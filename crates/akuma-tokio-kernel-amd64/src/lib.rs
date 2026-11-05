//! The machine half of `akuma-tokio-kernel` on amd64: a whole PVH guest kernel that runs unmodified
//! Linux musl binaries (busybox, sshd, git, rustc, tokio programs) over `akuma-runtime-tokio`.
//!
//! A kernel built from this is a short `main.rs` (`akuma-tokio-kernel`'s `entry!` and a [`Kernel`]
//! chain). This crate is everything else: the boot order, the traps and fault paths, the task
//! records and memory arms, processes and signals through `akuma-exec`, the scheduler's machine half,
//! SMP under one big kernel lock, and the default devices (COM1, ext2 on virtio-blk, virtio-net).
//! Each default device is a feature, wired by the matching [`Kernel`] method.
//!
//! Moved whole from Taoxichuan's `src/` (2026-11-02, B15 of `docs/archive/TAOXICHUAN_CLEANUP_PLAN.md`);
//! the history of each part is in Taoxichuan's docs and `docs/archive/`.
//!
//! # The boot order, and why
//!
//! Mirrors `akuma-amd64`'s `boot::early_init` order, which is load-bearing and
//! paid for: console → GDT (before the IDT loads; the CPU reads the GDT on
//! every exception delivery) → IDT (so everything after can *report* a fault
//! instead of triple-faulting into guest silence) → `CR0.WP` → machine
//! description → heap + PMM → LAPIC and the clock → the exec/threading runtime
//! → root filesystem → NIC → application processors → banner → init.

// The kernel's own target only: under the host's `cargo clippy`/`cargo test` this crate is empty, as
// the other amd64 machine crates are on a non-x86 host.
#![cfg(all(target_arch = "x86_64", target_os = "none"))]
#![no_std]
#![feature(alloc_error_handler)]
#![feature(abi_x86_interrupt)]
// Stage 1 registers every trap kind it does not specifically expect; until
// stage 2 gives some of them real jobs, that is the whole surface.
#![allow(function_casts_as_integer)]
// Same allow as akuma-boot-amd64: several entry-point docs open with a long
// paragraph on purpose (the why lives in the first block).
#![allow(clippy::too_long_first_doc_paragraph)]
// The Linux ABI is `u64` registers read as signed errno-style values and back;
// every `as i64`/`as i32` in the syscall arms is that conversion on purpose.
#![allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]

extern crate alloc;

mod child;
mod console;
mod exec_twin;
mod lazy;
mod net;
#[cfg(feature = "trace-err")]
use akuma_tokio_kernel_core::ring;
mod sched;
use akuma_tokio_kernel_core::shmpages;
mod smp;

use akuma_boot_amd64::{mem, paging, phys};
use akuma_idt_amd64 as idt_lib;
use akuma_trap_amd64 as trap;
use akuma_trap_amd64::{InterruptStackFrame, Kind, Trap};
use akuma_vfs::Filesystem;
use akuma_ring3_amd64 as ring3;

// PB1 linkage proof (session pb): the two M4 crates must be *referenced*, or
// the linker drops them and the size delta means nothing. `#[used]` makes the
// fn pointers GC roots; no runtime call, no behaviour change.
#[used]
static _PB1_THREADING_REGISTER: fn(akuma_threading::ThreadRuntime, akuma_threading::ThreadConfig) =
    akuma_threading::register;
#[used]
static _PB1_EXEC_INIT: fn(akuma_exec::ExecRuntime, akuma_exec::ExecConfig) = akuma_exec::init;
/// The heap. `#[global_allocator]` is a binary-level declaration; the crate
/// exports the implementation, this kernel installs it.
#[global_allocator]
static ALLOCATOR: akuma_alloc::KernelAllocator = akuma_alloc::KernelAllocator;

// ---------------------------------------------------------------------------
// The machine, as `akuma-ryzen-amd64` reads it: the physmap as a `PhysMem`.
// ---------------------------------------------------------------------------

/// Physical memory as this kernel sees it: the physmap, and nothing outside it
/// (the same bound and the same reasons as `akuma-amd64`'s `machine::Physmap`
/// — every address here comes from the VMM, and a read past the physmap is a
/// page fault during early boot, i.e. a guest that vanishes with no output).
struct Physmap;

impl akuma_ryzen_amd64::PhysMem for Physmap {
    fn read(&self, pa: u64, buf: &mut [u8]) -> bool {
        let Some(end) = pa.checked_add(buf.len() as u64) else {
            return false;
        };
        if end > phys::PHYSMAP_LIMIT {
            return false;
        }
        for (i, slot) in buf.iter_mut().enumerate() {
            // SAFETY: the whole range was just proved to be inside the physmap,
            // which `boot.s` maps before `kmain` runs.
            *slot = unsafe { phys::phys_ptr::<u8>(pa + i as u64).read_volatile() };
        }
        true
    }
}

/// The whole boot command line, copied into `buf` (empty when absent).
fn cmdline(start_info_pa: u64, buf: &mut [u8]) -> &str {
    let mut scratch = [0u8; 512];
    let Some(si) = akuma_ryzen_amd64::StartInfo::parse(&Physmap, start_info_pa) else {
        return "";
    };
    let Some(line) = si.cmdline(&Physmap, &mut scratch) else {
        return "";
    };
    let n = line.len().min(buf.len());
    buf[..n].copy_from_slice(&line.as_bytes()[..n]);
    core::str::from_utf8(&buf[..n]).unwrap_or("")
}

/// The value of one `token=` on the command line, copied into `buf`.
fn cmdline_token<'a>(start_info_pa: u64, prefix: &str, buf: &'a mut [u8]) -> Option<&'a str> {
    let mut scratch = [0u8; 512];
    let si = akuma_ryzen_amd64::StartInfo::parse(&Physmap, start_info_pa)?;
    let line = si.cmdline(&Physmap, &mut scratch)?;
    akuma_tokio_kernel_core::cmdline::token_into(line, prefix, buf)
}

// ---------------------------------------------------------------------------
// GDT/TSS: one core, the finished table shape.
// ---------------------------------------------------------------------------

/// The stack a ring-3 trap would switch to. Nothing runs in ring 3 yet, but
/// the TSS must be loadable for `ltr`, and a valid `rsp0` costs one static.
const TRAP_STACK_SIZE: usize = 16 * 1024;

#[repr(align(16))]
struct TrapStack(#[allow(dead_code)] [u8; TRAP_STACK_SIZE]);

/// One boot trap stack per CPU (`smp::MAX_CPUS`): each core's TSS `rsp0` until
/// its first switch arms a per-slot stack.
static mut TRAP_STACK: [TrapStack; smp::MAX_CPUS] = [const { TrapStack([0; TRAP_STACK_SIZE]) }; smp::MAX_CPUS];

/// Per-slot ring-3 trap stacks (session pf preemption, plan §5): a ring-3 trap
/// switches to the TSS `rsp0`, so a preempting timer switch must leave the
/// interrupted frame on a stack the next ring-3 trap cannot clobber — one per
/// slot. Indexed by the same numbers `switch_to` sees: shell slots `0..NSLOTS`,
/// `NSLOTS` = the idle thread. `gdt_init` arms the global `TRAP_STACK` first
/// (it serves every task's ring-3 traps until its first switch arms its own,
/// and by then all other ring-3 tasks own their private stacks).
static mut TRAP_STACKS: [TrapStack; crate::child::NSLOTS + 1] =
    [const { TrapStack([0; TRAP_STACK_SIZE]) }; crate::child::NSLOTS + 1];

/// Where a ring-3 trap lands while slot `slot` runs: its own per-slot stack (a preempting switch leaves the
/// interrupted frame on the interrupted task's stack, so the next ring-3 trap on any task must not land there),
/// or - for an idle thread, which never traps from ring 3 - this core's boot stack.
pub(crate) fn trap_top_for(slot: usize) -> u64 {
    // SAFETY: only the address of a static is taken; nothing is read through it.
    unsafe {
        if slot <= crate::child::NSLOTS {
            (&raw const TRAP_STACKS[slot]).cast::<u8>().add(TRAP_STACK_SIZE) as u64
        } else {
            (&raw const TRAP_STACK[ring3::cpu_index()]).cast::<u8>().add(TRAP_STACK_SIZE) as u64
        }
    }
}

/// Point THIS core's TSS `rsp0` at `top`.
pub(crate) fn set_tss_rsp0(top: u64) {
    // SAFETY: each core touches only its own TSS, IF clear on every caller path (a switch); reached
    // through a raw pointer, never a reference to the whole array.
    #[allow(clippy::deref_addrof)]
    unsafe {
        (*&raw mut TSS[ring3::cpu_index()]).set_rsp0(top);
    }
}
/// One TSS and one GDT per CPU: `ltr` marks a TSS descriptor busy, so two
/// cores cannot share either.
static mut TSS: [akuma_gdt::Tss; smp::MAX_CPUS] = [const { akuma_gdt::Tss::new() }; smp::MAX_CPUS];

/// Timer-preemption era flag (session pf, plan §5): set when init's thread is
/// published (`child::start_init_thread`) — the tick switch runs only once user
/// tasks exist. The pf-6
/// bisect showed the switch killing pre-ring-3 boots deterministically, in a
/// window with no legal switch target; before the first spawn there is
/// nothing to preempt and the tick must not switch at all.
pub(crate) static PREEMPT_ARMED: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);
static mut GDT: [[u64; akuma_gdt::SLOTS]; smp::MAX_CPUS] = [[0; akuma_gdt::SLOTS]; smp::MAX_CPUS];

/// Build the GDT and TSS, load both. One core; the entries are byte-identical
/// to `boot.s`'s at the same selectors, so `CS`/`SS` stay valid across the
/// `lgdt` and nothing needs a far return.
fn gdt_init() {
    gdt_init_cpu(0);
}

/// [`gdt_init`] for core `cpu`: its own GDT row, TSS and boot trap stack.
/// Each core touches only its own index.
pub(crate) fn gdt_init_cpu(cpu: usize) {
    // SAFETY: interrupts are masked on the calling core and only core `cpu`
    // touches index `cpu` of these statics. Reached only through raw
    // pointers, never references to the whole array.
    unsafe {
        let tss = &raw mut TSS[cpu];
        (*tss).set_rsp0((&raw mut TRAP_STACK[cpu]).cast::<u8>().add(TRAP_STACK_SIZE) as u64);
        let gdt = &raw mut GDT[cpu];
        *gdt = akuma_gdt::table(tss as u64);
        #[repr(C, packed)]
        struct Gdtr {
            limit: u16,
            base: u64,
        }
        let gdtr = Gdtr { limit: akuma_gdt::GDT_LIMIT, base: gdt as u64 };
        core::arch::asm!(
            "lgdt [{gdtr}]",
            "ltr {tr:x}",
            gdtr = in(reg) &raw const gdtr,
            tr = in(reg) akuma_gdt::TSS_SELECTOR,
            options(readonly, nostack, preserves_flags)
        );
    }
}

// ---------------------------------------------------------------------------
// IDT: every vector a way to *report*, not a way to vanish.
// ---------------------------------------------------------------------------

static IDT: idt_lib::Idt = idt_lib::Idt::new();

/// The one trap handler: name the kind, the rip, the cr2, and stop. A fault in
/// this kernel is a bug in this kernel; there is no userspace to deliver it to
/// and no scheduler to defer it through, so the honest report is the full
/// frame and a halt. The `[Fault]` token is the gate's vocabulary.
/// How far a user stack may grow (8 MiB); core's, shared with aarch64.
pub(crate) use akuma_tokio_kernel_core::loader::STACK_MAX;

/// A not-present fault just under a task's stack region is the stack growing
/// (core's `grow_user_stack`, over this machine's tree).
fn grow_user_stack(cr2: u64) -> bool {
    akuma_tokio_kernel_core::loader::grow_user_stack::<lazy::Amd64>(cr2)
}

// ---------------------------------------------------------------------------
// Copy-on-write fork: shared leaf frames, the bit-9 marker, one break path.
// ---------------------------------------------------------------------------

// x86 leaf PTE bits. Bit 9 is an AVL bit the MMU ignores: a page that was writable when
// `fork` shared it is demoted to read-only and marked, so a write fault can tell "break the
// share" from "this page is read-only" (`mprotect(PROT_READ)`, text) - `lazy::Amd64::cow_marked`.
use akuma_pte_amd64::ADDR_MASK as PTE_ADDR;

// W^X and the copy-on-write break are `akuma_tokio_kernel_core::cow`'s, over `lazy::Amd64`'s bits.
use akuma_tokio_kernel_core::cow;

fn mark_written(root: u64, va: u64) {
    cow::mark_written::<lazy::Amd64>(root, va);
}

/// `(shared, copied, taken-in-place, retried)` since boot, for the reap lines.
#[cfg_attr(not(feature = "trace-err"), allow(dead_code))]
pub(crate) fn cow_stats() -> (usize, usize, usize, usize) {
    cow::stats()
}

/// The 4 KiB leaf entry for `va` in `root`'s user half, if every level is
/// present and none is a huge page.
fn leaf_entry(root: u64, va: u64) -> Option<Pte> {
    // SAFETY: `root` is a user root (a task's `root_pa` or the live CR3): a
    // page-table frame.
    let mut table = unsafe { phys::Frame::from_pa(root & PTE_ADDR) };
    for shift in [39u32, 30, 21] {
        let e = table.entry(((va >> shift) & 0x1FF) as usize);
        if e & 1 == 0 || (shift != 39 && e & 0x80 != 0) {
            return None;
        }
        // SAFETY: a present, non-huge entry names the next-level table frame.
        table = unsafe { phys::Frame::from_pa(e & PTE_ADDR) };
    }
    Some(Pte { table, idx: ((va >> 12) & 0x1FF) as usize })
}

/// One leaf entry: the table frame and the slot in it.
struct Pte {
    table: phys::Frame,
    idx: usize,
}

impl Pte {
    fn read(&self) -> u64 {
        self.table.entry(self.idx)
    }

    fn write(&self, v: u64) {
        self.table.set_entry(self.idx, v);
    }
}

fn cow_resolve(root: u64, va: u64) -> akuma_cow::CowAction {
    cow::resolve::<lazy::Amd64>(root, va)
}

fn cow_prepare_write(root: u64, va: u64) -> bool {
    cow::prepare_write::<lazy::Amd64>(root, va)
}

fn fault_trap(t: &mut Trap<'_>) {
    let user = t.from_ring3();
    if user {
        smp::bkl_enter();
    }
    fault_trap_locked(t);
    if user {
        smp::bkl_leave();
    }
}

fn fault_trap_locked(t: &mut Trap<'_>) {
    let code = t.error_code.unwrap_or(0);
    // `CR2` is the core's, not the thread's: a file fill below can park on the
    // disk, and whatever faults on this core meanwhile overwrites it. Read once,
    // before anything can block.
    let cr2 = akuma_cpu::ctlreg::cr2();
    // A not-present user page that `mmap` reserved and nothing has touched yet:
    // fill it and retry. From ring 0 too (a kernel access that skipped
    // `validate_user_range`'s prefault), but only zero-fill there - a file fill
    // reads the filesystem, which a ring-0 fault may be holding. A failed fill
    // (no memory, an unreadable file) falls through to the kill.
    if code & 1 == 0 && cr2 < USER_TOP {
        let root = akuma_cpu::ctlreg::cr3() & PTE_ADDR;
        if lazy::fill(root, cr2, t.from_ring3()) == lazy::Fill::Present {
            return;
        }
    }
    if t.from_ring3() && code & 1 == 0 && grow_user_stack(cr2) {
        return;
    }
    // A write to a present user page, from EITHER ring: the copy-on-write
    // arm. Present + write (error code bits 0-1); ring 0 counts because
    // `akuma-syscalls-glue`'s arms write user memory directly through
    // `akuma-user-access` (`CR0.WP` makes that fault on a shared page), and
    // a kernel write into a forked child's buffer must break the share, not
    // fail. The live PTE decides, never the error code (`cow_resolve` re-reads
    // it); only `Fault` falls through.
    if code & 3 == 3 && cr2 < USER_TOP {
        let root = akuma_cpu::ctlreg::cr3() & PTE_ADDR;
        if cow_resolve(root, cr2).resolves() {
            return;
        }
    }
    // A ring-0 fault inside `akuma-user-access`'s copy loop is a bad user
    // pointer: resume at its `EFAULT` trampoline (amd64's `idt.rs` arm 2).
    if !t.from_ring3()
        && let Some(fixup) = akuma_user_access::user_copy_fixup(t.frame.rip)
    {
        t.frame.rip = fixup;
        return;
    }
    // A fault *from ring 3* is the task's death, not the kernel's: say which,
    // then end the task (below). What matters for the gate is that this prints
    // `[Fault]` instead of triple-faulting into guest silence.
    if t.from_ring3() {
        akuma_primitives::safe_print!(
            192,
            "[Fault] ring3 {:?} rip=0x{:016x} rsp=0x{:016x} cr2=0x{:016x} code=0x{:x} -- task killed\n",
            t.kind,
            t.frame.rip,
            t.frame.rsp,
            cr2,
            code
        );
        #[cfg(feature = "trace-err")]
        mm_dump_for(cr2);
        #[cfg(feature = "trace-err")]
        fault_code_dump(t.frame.rip);
        // A fork child or thread dies alone (SIGSEGV status to its waiter);
        // the machine runs on. The boot task has no slot to reap through, so
        // a fault there halts.
        if child::is_real_task() {
            akuma_primitives::safe_print!(64, "[Fault] slot {} killed (SIGSEGV)\n", exec_twin::current_slot());
            child::task_die_segv();
        }
        halt();
    }
    akuma_primitives::safe_print!(
        192,
        "[Fault] {:?} rip=0x{:016x} rsp=0x{:016x} cr2=0x{:016x} code=0x{:x}\n",
        t.kind,
        t.frame.rip,
        t.frame.rsp,
        cr2,
        code
    );
    halt();
}

/// Plain (callable) reporters: an `extern "x86-interrupt"` fn can only be
/// *entered* by the CPU, so the body lives here and each stub forwards to it —
/// the same split as `akuma-amd64`'s `fatal`.
fn report_unhandled(frame: &InterruptStackFrame) -> ! {
    akuma_primitives::safe_print!(
        160,
        "[Fault] unhandled vector frame rip=0x{:016x} rsp=0x{:016x}\n",
        frame.rip,
        frame.rsp
    );
    halt();
}

fn report_unhandled_code(frame: &InterruptStackFrame, code: u64) -> ! {
    akuma_primitives::safe_print!(
        160,
        "[Fault] unhandled vector frame rip=0x{:016x} rsp=0x{:016x} code=0x{:x}\n",
        frame.rip,
        frame.rsp,
        code
    );
    halt();
}

extern "x86-interrupt" fn double_fault(frame: InterruptStackFrame, code: u64) -> ! {
    report_unhandled_code(&frame, code)
}

/// Vectors no `Kind` owns, each with a stub that reports and halts — the same
/// shape as `akuma-amd64`'s `exception_stubs`, reduced to the unhandled
/// reporter.
macro_rules! plain_stubs {
    ($($name:ident = $vector:literal;)*) => {
        $(
            extern "x86-interrupt" fn $name(frame: InterruptStackFrame) {
                report_unhandled(&frame);
            }
        )*
    };
}

macro_rules! plain_stubs_code {
    ($($name:ident = $vector:literal;)*) => {
        $(
            extern "x86-interrupt" fn $name(frame: InterruptStackFrame, code: u64) {
                report_unhandled_code(&frame, code);
            }
        )*
    };
}

// No-error-code vectors outside `Kind::ALL`.
plain_stubs! {
    vec_nmi =  2;
    vec_of  =  4;
    vec_br  =  5;
    vec_nm  =  7;
    vec_cso =  9;
    vec_mc  = 18;
    vec_ve  = 20;
    vec_hv  = 28;
}
// Error-code vectors outside `Kind::ALL`.
plain_stubs_code! {
    vec_ts  = 10;
    vec_cp  = 21;
    vec_vc  = 29;
    vec_sx  = 30;
}

/// The LAPIC timer's IDT body (session pb). One implementation, two consumers:
/// the extracted crate counts the tick and acknowledges it; this kernel adds
/// nothing yet. **No task switch**: a tick that fires while ring 3 runs (the
/// only place IF is set — every kernel path runs with interrupts masked)
/// returns to the same instruction. The switch lands with `fork_process`.
fn timer_trap(t: &mut Trap<'_>) {
    smp::bkl_enter();
    timer_trap_locked(t);
    smp::bkl_leave();
}

fn timer_trap_locked(t: &Trap<'_>) {
    // The tick count is not the clock (that is the TSC, `net::uptime_us`); it
    // paces preemption and the trace dumps below. Only the boot core counts;
    // an AP's tick just acknowledges.
    if ring3::cpu_index() == 0 {
        let n = akuma_lapic_amd64::on_tick();
        let _ = n;
        // Trace builds: every 30 s of uptime (eight times) dump each slot's last
        // syscalls - the hang dump below only fires from an IDLE core, which a
        // livelocked (spinning) guest never reaches.
        #[cfg(feature = "trace-err")]
        if n % 3000 == 2999 && n < 3000 * 8 && t.from_ring3() {
            trace_hang_dump(6);
        }
    } else {
        akuma_lapic_amd64::eoi();
    }
    // A sibling `exit_group` asked to end, caught in a ring-3 loop that makes no
    // syscall: the tick is its kernel entry (`child::kill_siblings` waits for it).
    if t.from_ring3() && child::kill_requested() {
        child::finish_killed();
    }
    // A fatal pending signal (`SIGKILL`, or a default-terminate with no handler) ends a thread
    // looping in ring 3; handlers wait for its next syscall return.
    if t.from_ring3() {
        child::deliver_fatal_on_tick();
    }
    // M4 slice 3 preemption (session pf, plan §5): a tick that interrupted
    // ring 3 may switch — the same `x86_yield` the park loop and the idle lap
    // already run, on this handler's own (per-slot, `arm_trap_stack`) trap
    // stack; the interrupted frame survives the stack switch, and `on_tick`
    // has already EOI'd, so the LAPIC can deliver the next tick to whichever
    // task runs after this returns. Nothing else is READY ⇒ a documented
    // no-op. Kernel paths run IF-masked, so ring 0 is never switched here: a
    // ring-0 tick lands only in the idle window, which self-yields.
    //
    // **The user-era gate** (pf-6 bisect): pf-5/pf-6 died INSIDE net
    // calibrate — pre-ring-3, pre-first-wd, deterministically with this gate
    // live and never with it out — a window where no legal switch target
    // exists. So the switch arms when init's thread is published
    // (`PREEMPT_ARMED`, set in `child::start_init_thread`): before it the machine is
    // kernel-only and the tick behaves exactly like the boots that worked;
    // after it, ring 3 exists and preemption is the point.
    //
    // **The slotted-current gate** (pf-7's panic, named by the raw print):
    // `akuma-threading:3652 THREAD_STATES[cur]` with cur = usize::MAX — after
    // an exec'd shell's exit tears its slot down, a ring-3 tick can fire with
    // NO_SLOT current; the crate's switch engine indexes by current tid, so a
    // tick with no live slot must not switch (the same `may_park` discipline
    // every parking wait keeps).
    let slot = crate::exec_twin::current_slot();
    if t.from_ring3()
        && slot != crate::exec_twin::NO_SLOT
        && slot < crate::child::NSLOTS
        && PREEMPT_ARMED.load(core::sync::atomic::Ordering::Acquire)
    {
        akuma_threading::x86_yield();
    }
    // **The netpoll backstop** (session ph, boot ph-5's P2 stall): the guest's
    // other pumps are all task-context (sshd's serve-loop sleep, the DHCP
    // wait, the NetRuntime hooks) — and a ring-3 resident with NO_SLOT (the
    // boot image's init: wd's slot/tid=u64::MAX lines, permanent in every
    // gate window) can hold the core for seconds, because the gate above
    // must not switch it. Boot ph-5's P battery saw exactly that shape:
    // ~10 s with gw_frames_from_guest frozen (the stack stopped talking)
    // until the client's handshake watchdog gave up — KVM_RELAY_SYN_STALL's
    // symptom row. One drained lap per tick caps any such window at ~10 ms:
    // the by-construction servicing the other kernels get from their
    // interrupt face. netpoll guards itself (NET_UP/IN_LAP); see net.rs.
    crate::net::netpoll();
}

/// The tick source (session pb): the extracted crate's timer face over the
/// guest's xAPIC page. Mirrors amd64's `lapic::init` ordering — mask the
/// legacy PICs, set `IA32_APIC_BASE.APIC_GLOBAL_ENABLE`, map the page
/// device-uncached, then hand the (already-enabled) MSR value and the window
/// to the crate, which software-enables the LAPIC, calibrates against the
/// 8254 PIT and arms the periodic timer at `TIMER_VECTOR`.
fn lapic_init() -> bool {
    use akuma_cpu::msr::{self, Msr};
    akuma_port::PIC.mask_all();
    let base_msr = msr::read(Msr::ApicBase);
    let base = base_msr & akuma_lapic_amd64::APIC_BASE_MASK;
    if base == 0 {
        akuma_primitives::safe_print!(96, "  [lapic] IA32_APIC_BASE reports no LAPIC\n");
        return false;
    }
    // SAFETY: sets exactly one bit in an architectural MSR, leaving the
    // address field as found — the same write amd64's `lapic::init` makes.
    unsafe { msr::write(Msr::ApicBase, base_msr | akuma_lapic_amd64::APIC_GLOBAL_ENABLE) };
    if !map_device(base, 4096) {
        akuma_primitives::safe_print!(96, "  [lapic] could not map the LAPIC page\n");
        return false;
    }
    // SAFETY: the page at DEVMAP_BASE+base is the LAPIC register file, mapped
    // device-uncached just above for the kernel's lifetime, page-aligned.
    let win = unsafe {
        akuma_primitives::mmio::MmioWindow::new((akuma_boot_amd64::phys::DEVMAP_BASE + base) as usize, 4096)
    };
    if let Ok(info) = akuma_lapic_amd64::init(base_msr | akuma_lapic_amd64::APIC_GLOBAL_ENABLE, win) {
        akuma_primitives::safe_print!(
            160,
            "  lapic: id={} timer vector={} periodic; calibrated={} counts/tick={} tsc={} Hz\n",
            info.id,
            akuma_lapic_amd64::TIMER_VECTOR,
            u8::from(info.calibrated),
            info.timer_count,
            info.tsc_hz
        );
        true
    } else {
        akuma_primitives::safe_print!(96, "  [lapic] init failed\n");
        false
    }
}

#[cfg(feature = "smp")]
/// Load the (already filled) IDT on THIS core — an AP's `lidt`.
pub(crate) fn idt_load() {
    IDT.load();
}

#[cfg(feature = "smp")]
/// The control-register state every core needs: `CR0.WP`, SSE legal for ring 3
/// (`boot.s` sets the BSP's in long mode; the AP trampoline does not).
pub(crate) fn enable_cpu_features() {
    // SAFETY: setting architectural bits on this core; the same values the
    // BSP runs with.
    unsafe {
        core::arch::asm!(
            "mov {t}, cr0",
            "and {t}, -5",
            "or {t}, 2",
            "or {t}, {wp}",
            "mov cr0, {t}",
            "mov {t}, cr4",
            "or {t}, {sse}",
            "mov cr4, {t}",
            t = out(reg) _,
            wp = in(reg) 1u64 << 16,
            sse = in(reg) (1u64 << 9) | (1u64 << 10),
            options(nostack)
        );
    }
}

/// Fill and load the IDT: a reporter for every vector, the trap crate's stubs
/// for every `Kind` (whose handlers are registered first — `Registered`
/// panics naming itself if a stub fires with none, a boot-order bug rather
/// than a state to degrade through).
fn idt_init() {
    for kind in Kind::ALL {
        if kind.vector().is_some() {
            trap::register(kind, fault_trap);
        }
    }
    IDT.fill(double_fault as usize);
    IDT.set(2, vec_nmi as usize);
    IDT.set(4, vec_of as usize);
    IDT.set(5, vec_br as usize);
    IDT.set(7, vec_nm as usize);
    IDT.set(9, vec_cso as usize);
    IDT.set(10, vec_ts as usize);
    IDT.set(18, vec_mc as usize);
    IDT.set(20, vec_ve as usize);
    IDT.set(21, vec_cp as usize);
    IDT.set(28, vec_hv as usize);
    IDT.set(29, vec_vc as usize);
    IDT.set(30, vec_sx as usize);
    IDT.set(8, double_fault as usize);
    for kind in Kind::ALL {
        let Some(vector) = kind.vector() else { continue };
        let entry = trap::entry(kind);
        if kind.user_callable() {
            IDT.set_user(vector, entry);
        } else {
            IDT.set(vector, entry);
        }
    }
    // The LAPIC timer (session pb): `Kind::Timer`'s vector is a software
    // choice; the extracted crate fixes it at TIMER_VECTOR (32) and
    // `start_timer` programs the LVT with the same constant. Never a hand-set
    // stub pointer: the trap crate's own entry, placed by `IDT.set`.
    trap::register(Kind::Timer, timer_trap);
    IDT.set(akuma_lapic_amd64::TIMER_VECTOR, trap::entry(Kind::Timer));
    // The TLB-shootdown IPI (a ring-agnostic stub: `swapgs` on a ring-3 origin).
    IDT.set(smp::SHOOT_VECTOR, smp::shootdown_entry_addr());
    IDT.set(smp::WAKE_VECTOR, smp::shootdown_entry_addr());
    IDT.load();
}

// ---------------------------------------------------------------------------
// virtio-blk: map the announced transports, hand the geometry to the driver.
// ---------------------------------------------------------------------------

const PAGE_SIZE: u64 = 4096;

/// Map one device's register file into the device window, uncached — the
/// direct-window half of `akuma-amd64`'s `blk.rs`, pulled no further than
/// stage 1 needs (one transport class, no partitions).
fn map_device(base: u64, len: u64) -> bool {
    let first = base & !(PAGE_SIZE - 1);
    let last = (base + len).div_ceil(PAGE_SIZE) * PAGE_SIZE;
    let mut pa = first;
    while pa < last {
        if !paging::map_page(
            (phys::DEVMAP_BASE + pa) as usize,
            pa,
            paging::PteProt::KERNEL_RW,
            paging::MemAttr::Device,
        ) {
            return false;
        }
        pa += PAGE_SIZE;
    }
    true
}

/// Map the machine's virtio transports and set the slot window the block and
/// net drivers probe through. False when no device was announced (not an
/// error — a machine with neither a disk nor a NIC is a legitimate one).
#[cfg(any(feature = "disk", feature = "net"))]
fn map_virtio(devices: &akuma_ryzen_amd64::MmioDevices) -> bool {
    let devs = devices.as_slice();
    let Some(first) = devs.first() else {
        akuma_primitives::addr::clear_virtio_window();
        return false;
    };
    for d in devs {
        if !map_device(d.base, d.len) {
            akuma_primitives::safe_print!(96, "  [blk] could not map a virtio transport\n");
            return false;
        }
    }
    let Some((base, stride, slots)) = devices.geometry() else {
        return false;
    };
    akuma_primitives::addr::set_virtio_window(
        (phys::DEVMAP_BASE + base) as usize,
        stride as usize,
        slots,
    );
    akuma_primitives::safe_print!(
        128,
        "  virtio: {} slot(s) at pa 0x{:016x} stride 0x{:x} irq {}\n",
        slots,
        base,
        stride,
        first.irq
    );
    true
}

// ---------------------------------------------------------------------------
// The root filesystem: ext2 on virtio-blk by default (`disk`), or the root's own.
// ---------------------------------------------------------------------------

/// The virtio-blk device, as something `akuma-ext2` can read — device 0,
/// `vda`. No `flush`: the cache in front of the media is the *host's*, which a
/// guest cannot reach past (same reasoning as `akuma-amd64`'s `fs::VirtioBlk`).
#[cfg(feature = "disk")]
struct Vda;

#[cfg(feature = "disk")]
impl akuma_ext2::BlockDevice for Vda {
    fn read_bytes(&self, offset: u64, buf: &mut [u8]) -> Result<(), ()> {
        akuma_virtio::block::read_bytes(offset, buf).map_err(|_| ())
    }
    fn write_bytes(&self, offset: u64, data: &[u8]) -> Result<(), ()> {
        akuma_virtio::block::write_bytes(offset, data).map_err(|_| ())
    }
}

/// The ext2 root's inode clock: UTC once `epoch=` anchored it, else 0 (the
/// AArch64 kernel's mount answers the same way, `akuma-vfs-glue/src/ext2.rs`).
/// Read at each stamp, so mounting before the anchor is parsed is fine. Until
/// 2026-11-03 this was a constant 0: every file a guest made read `Jan 1 1970`,
/// and every inode it unlinked carried `dtime` 0, which `e2fsck` then reported
/// 15,254 times as "deleted inode has zero dtime" (`docs/archive/INFINITY_ON_TAOXICHUAN.md`).
#[cfg(feature = "disk")]
fn ext2_clock() -> u64 {
    akuma_primitives::clock::utc_time_us(net::uptime_us()).unwrap_or(0)
}

/// The default root: ext2 on the first virtio-blk device.
#[cfg(feature = "disk")]
fn disk_root() -> Option<alloc::sync::Arc<dyn Filesystem>> {
    if akuma_virtio::block::init().is_err() {
        console::puts("  [FATAL] no virtio block device -- the default root (ext2 on virtio-blk) cannot mount\n");
        return None;
    }
    let Ok(fs) = akuma_ext2::Ext2Filesystem::new(Vda, ext2_clock) else {
        console::puts("  fs:   /dev/vda holds no readable ext2 image\n");
        return None;
    };
    console::puts("  fs:   ext2 root on /dev/vda\n");
    Some(alloc::sync::Arc::new(fs))
}

/// The mounted root, from the boot path's mount and from every fd syscall that still
/// reads it directly (exec's image, file-backed `mmap`, `msync`).
///
/// Every touch is either before ring 3 exists or inside the syscall dispatcher, under
/// the big kernel lock.
static ROOT_FS: akuma_primitives::cell::BorrowCell<Option<&'static dyn Filesystem>> =
    akuma_primitives::cell::BorrowCell::new(None);

/// The mounted root, for the dispatcher. Panics if called before the mount —
/// which no caller does: the mount precedes the first ring-3 crossing.
fn root_fs() -> &'static dyn Filesystem {
    ROOT_FS.get().expect("root_fs before the mount")
}

/// Install the tokio surface and `akuma-vfs-glue`'s mount table — amd64's `fs::init_vfs`, minus
/// what this kernel does not build (no audio, no file-page cache).
fn install_surface() {
    // The tokio surface: glue's and `akuma-vfs-glue`'s hooks with a tokio kernel's answers
    // (no rump, console poller, mouse, tun, audio or file-page cache), the pipe wake sink and
    // the RNG. This kernel supplies its clock, its cores, RDRAND (SplitMix64 fallback, always
    // fills) and the sweep after a reap.
    akuma_runtime_tokio::install(akuma_runtime_tokio::Kernel {
        utc_time_us: || akuma_primitives::clock::utc_time_us(net::uptime_us()),
        online_cpus: smp::online_cpus,
        rng_fill: |b| {
            net::rng_fill(b);
            true
        },
        after_reap: child::collect,
    });
    // `uname` is glue's, which answers `Akuma` unless a kernel names itself. This is a
    // Linux-ABI kernel and programs branch on `uname -s`: `Linux`, the kernel's package name
    // as the node, and `<version>-<sha>-<name>` as the release (not a borrowed Linux number).
    {
        use akuma_syscalls_glue::{proc, version};
        let id = identity();
        proc::set_uts_identity(proc::UtsIdentity {
            sysname: proc::uts_join(&["Linux"]),
            nodename: proc::uts_join(&[id.nodename]),
            release: proc::uts_join(&[id.version, "-", version::GIT_SHA, "-", id.nodename]),
            version: proc::uts_join(&[version::BUILD_ID]),
        });
    }
    // A process `akuma-exec` ends (a fatal signal's default action inside glue)
    // must still reach this kernel's `wait4`: see `child::on_thread_terminated`.
    akuma_threading::set_slot_purge_callback(child::on_thread_terminated);
    akuma_vfs_glue::init();
    // A thread that dies holding a VFS lock has it released at its slot's
    // TERMINATED -> FREE transition (amd64's `install_reap_hooks`).
    akuma_threading::set_slot_reap_callback(akuma_vfs_glue::reap_dead_thread);
}

/// Mount `fs` at `/` in the shared mount table (what glue's arms resolve through) and keep a
/// `'static` borrow of the same instance for the arms that read it directly. Halts on
/// failure: a root the shared arms cannot resolve answers `ENOENT` for every path, silently
/// (`AKUMA_AMD64_C1_STEP4A_VFS_ADOPTION.md` § 1).
fn mount_root(fs: alloc::sync::Arc<dyn Filesystem>) {
    if akuma_vfs_glue::mount_with("/", Some("/dev/vda"), 0, fs.clone()).is_err() {
        console::puts("  [FATAL] could not mount the root into the shared mount table\n");
        halt();
    }
    // The front door (`akuma_vfs_glue::fs`) answers "not found" for every path
    // until this flag is set. AArch64's `fs::init` sets it; this kernel mounts
    // ext2 itself, as amd64 does, and sets it at the same point.
    akuma_vfs_glue::fs::mark_initialized();
    // The shared file-page cache (`lazy.rs`), and what keeps it honest when an
    // inode number is reissued. AArch64's `fs::init` does both; this kernel
    // mounts ext2 itself, so it does them here. Every write through
    // `akuma-vfs-glue` invalidates by inode already, and `shared_writeback`
    // does for the one write this kernel makes itself. The freed-inode hook
    // also drops what the writable shared-page table (`shmpages`) still holds
    // of the inode, and `shmpages::init` registers glue's write observers, so
    // a `write(2)` reaches every mapping of the page.
    #[cfg(feature = "disk")]
    akuma_ext2::init_inode_freed_hook(shmpages::inode_freed);
    akuma_fpcache::init(akuma_pmm::total_count() * 4096);
    shmpages::init();
    // `/proc`: the shared `ProcFilesystem`, rendering `akuma-exec`'s process
    // table (`<pid>/exe`, which musl's `$ORIGIN` expansion reads; `meminfo`;
    // `<pid>/fd`). Glue's `readlinkat` and friends answer through it.
    let proc_fs = alloc::sync::Arc::new(akuma_vfs_glue::proc::ProcFilesystem::new());
    if akuma_vfs_glue::mount_with("/proc", Some("proc"), 0, proc_fs).is_err() {
        console::puts("  fs:   /proc mount failed\n");
    }
    // The mount lives for the rest of the boot: leak one `Arc` to a `'static` borrow.
    let fs: &'static alloc::sync::Arc<dyn Filesystem> = alloc::boxed::Box::leak(alloc::boxed::Box::new(fs));
    ROOT_FS.set(Some(&**fs));
}
// ---------------------------------------------------------------------------
// The banner — printed at the init-spawn point, where the full kernel prints
// its own: the last thing on the console before the machine would hand over.
// ---------------------------------------------------------------------------

use akuma_tokio_kernel_core::{Identity, identity};

fn banner() {
    let id = identity();
    console::puts("\n");
    if !id.art.is_empty() {
        console::puts(id.art);
        console::puts("\n");
    }
    console::puts("  ");
    console::puts(id.name);
    console::puts(" ");
    console::puts(id.version);
    console::puts(" (akuma-tokio-kernel, amd64)\n\n");
}

// ---------------------------------------------------------------------------
// Stage 2: the task, its syscalls, and the crossing into ring 3.
// ---------------------------------------------------------------------------

// `USER_TOP` (the end of the canonical user half: nothing a task names at or above it may be
// mapped, read or written for it) is `akuma_tokio_kernel_core::task`'s.
use akuma_tokio_kernel_core::mm;
use akuma_tokio_kernel_core::task::{USER_TOP, Task};
/// Bisect mask for the mmap-slab rework, set from the kernel command line
/// (`mmdbg=N`, default 7): 1 = munmap frees + records holes, 2 = MAP_FIXED
/// releases what it replaces, 4 = mremap frees/moves the old range.
static MMDBG_V: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(7);
fn mmdbg() -> u32 {
    MMDBG_V.load(core::sync::atomic::Ordering::Relaxed)
}

/// The ext2 inode behind `fd` in the current process's descriptor table, if
/// it names a regular file: the pin `openat` recorded, or the path resolved
/// now for a descriptor opened without one. One mount, so the number is the
/// root's.
fn fd_inode(fd: u64) -> Option<u32> {
    use akuma_exec::process::FileDescriptor;
    let kf = akuma_exec::process::current_process_shared()?.with_fd(fd as u32, |d| match d {
        FileDescriptor::File(f) => Some(f.clone()),
        _ => None,
    })??;
    let inode = if kf.inode() != 0 { kf.inode() } else { root_fs().resolve_inode(&kf.path).ok()? };
    let md = root_fs().metadata_by_inode(inode).ok()?;
    (!md.is_dir).then_some(inode)
}

/// The task records (`akuma_tokio_kernel_core::task`), reached only through [`with_task`] /
/// [`with_task_slot`] / [`task_root_of`]: a closure never calls anything that reaches a task
/// record again (`user_ok`, the user copies, `add_region`, the page-table walkers, every
/// `child::` entry).
static TASKS: akuma_tokio_kernel_core::task::Tasks<{ crate::child::NSLOTS + 1 }, { smp::MAX_CPUS }> =
    akuma_tokio_kernel_core::task::Tasks::new();

// The memory arms are `akuma_tokio_kernel_core::mm`'s; these forward with the machine.
fn mprotect_arm(addr: u64, len: u64, prot: u64) -> u64 {
    mm::mprotect_arm::<lazy::Amd64>(addr, len, prot)
}

/// `--features trace-err`: the last 128 mmap-slab events (kind, start, end), so
/// a fault can name the unmap/replace that took its page away.
#[cfg(feature = "trace-err")]
static MM_EVENTS: ring::Ring<(u8, u64, u64), 128> = ring::Ring::new((0, 0, 0));

#[cfg(feature = "trace-err")]
fn mm_event(kind: u8, start: u64, end: u64) {
    MM_EVENTS.push((kind, start, end));
}
#[cfg(not(feature = "trace-err"))]
fn mm_event(_kind: u8, _start: u64, _end: u64) {}

/// What the faulting task actually executes at `rip`: the 16 bytes mapped there, the frame
/// that maps them, and that frame's share count. A fault the instruction at `rip` cannot raise
/// means those bytes are not the image's.
#[cfg(feature = "trace-err")]
fn fault_code_dump(rip: u64) {
    use akuma_tokio_kernel_core::mm::Mm;
    let root = task_root();
    let page = rip & !0xFFF;
    let pte = leaf_entry(root, page).map_or(0, |slot| slot.read());
    let pa = lazy::Amd64::translate(root, page).unwrap_or(0);
    let mut b = [0u8; 16];
    let ok = copy_from_user(&mut b, rip);
    akuma_primitives::safe_print!(
        256,
        "[fault-code] rip=0x{:x} pte=0x{:x} pa=0x{:x} refs={} read={} bytes={:02x?}\n",
        rip,
        pte,
        pa,
        if pa != 0 { akuma_pmm::cow_ref_get(pa as usize) } else { 0 },
        ok,
        b
    );
}

/// Print every recorded slab event that covers `va` (kinds: 1 munmap, 2
/// MAP_FIXED release, 3 mremap shrink, 4 mremap move-free, 5 mmap, 6 mremap-grow).
#[cfg(feature = "trace-err")]
fn mm_dump_for(va: u64) {
    MM_EVENTS.view(|head, ev| {
        for i in 0..128 {
            let (k, s, e) = ev[(head + i) % 128];
            if k != 0 && s <= va && va < e {
                akuma_primitives::safe_print!(96, "[mm] kind={} 0x{:x}..0x{:x} covers fault\n", k, s, e);
            }
        }
    });
}

fn msync_arm(addr: u64, len: u64) -> u64 {
    mm::msync_arm::<lazy::Amd64>(addr, len)
}

fn madvise_arm(addr: u64, len: u64, advice: u64) -> u64 {
    mm::madvise_arm::<lazy::Amd64>(addr, len, advice)
}

fn munmap_arm(addr: u64, len: u64) -> u64 {
    mm::munmap_arm::<lazy::Amd64>(addr, len)
}

/// Which task record the syscall path acts on now.
pub(crate) fn task_slot_index() -> usize {
    TASKS.current(ring3::cpu_index())
}

pub(crate) fn set_task_slot(i: usize) {
    TASKS.set_current(ring3::cpu_index(), i);
}

/// Run `f` on record `slot`.
pub(crate) fn with_task_slot<R>(slot: usize, f: impl FnOnce(&mut Task) -> R) -> R {
    TASKS.with_slot(slot, f)
}

/// Run `f` on the record the syscall path acts on now (`TASKS.current`): the dispatcher's task.
pub(crate) fn with_task<R>(f: impl FnOnce(&mut Task) -> R) -> R {
    with_task_slot(task_slot_index(), f)
}

/// The page-table root of record `slot`.
pub(crate) fn task_root_of(slot: usize) -> u64 {
    TASKS.root_of(slot)
}

/// The page-table root of the record the syscall path acts on now.
pub(crate) fn task_root() -> u64 {
    task_root_of(task_slot_index())
}

/// Is `[va, va+len)` fully inside a region this kernel mapped for the task?
/// The check that makes `write(1, ptr, len)` honest: a bogus pointer is
/// `-EFAULT`, not a kernel fault.
fn user_ok(va: u64, len: u64) -> bool {
    with_task(|t| t.layout.user_ok(va, len))
}

/// errno returns are the negated value in `RAX` — plain wrapping arithmetic
/// on `u64`, no signed casts.
fn e(code: u64) -> u64 {
    0u64.wrapping_sub(code)
}

/// The current task's pages, reached one at a time through the physmap: the effect half of
/// `akuma-user-walk` (the chunking, the string/array readers and the range rule are the crate's).
struct TaskWalk {
    root: u64,
}

impl akuma_user_walk::Walk for TaskWalk {
    fn read_page(&self, va: u64, out: &mut [u8]) -> bool {
        let Some(pa) = translate_filling(self.root, va) else {
            return false;
        };
        // SAFETY: `translate_in` named a mapped task frame.
        unsafe { phys::Frame::from_pa(pa) }.read_bytes((va & 0xFFF) as usize, out);
        true
    }
    fn write_page(&self, va: u64, src: &[u8]) -> bool {
        // Present first: `cow_prepare_write` judges the live entry, and a lazy
        // one has none to judge.
        if translate_filling(self.root, va).is_none() || !cow_prepare_write(self.root, va) {
            return false;
        }
        let Some(pa) = paging::translate_in(self.root, (va & !0xFFF) as usize) else {
            return false;
        };
        // SAFETY: `translate_in` named a mapped task frame, one this kernel mapped RW (or RO, for image
        // bytes - every caller writes only into stack/heap/scratch regions).
        unsafe { phys::Frame::from_pa(pa) }.write_bytes((va & 0xFFF) as usize, src);
        mark_written(self.root, va);
        true
    }
}

/// The frame behind the page holding `va`, filling it first if it is lazy: the
/// kernel's copies walk the page table and never fault, so they are where a
/// lazy page meets its first kernel access.
///
/// `None` (`EFAULT`) for a page ring 3 cannot reach either: a `PROT_NONE`
/// page keeps its frame with the user bit clear, and a syscall buffer there is
/// `EFAULT` on Linux too.
fn translate_filling(root: u64, va: u64) -> Option<u64> {
    let page = (va & !0xFFF) as usize;
    let pa = paging::translate_in(root, page).or_else(|| {
        (lazy::fill(root, va, true) == lazy::Fill::Present).then(|| paging::translate_in(root, page)).flatten()
    })?;
    leaf_entry(root, page as u64).is_some_and(|pte| pte.read() & akuma_pte_amd64::US != 0).then_some(pa)
}

fn task_walk() -> TaskWalk {
    TaskWalk { root: task_root() }
}

/// Copy `dst.len()` bytes from the task's address space at `src_va`. False (`-EFAULT`) when a page does not
/// translate - the regions check should make that unreachable, so it is a backstop, not the gate.
fn copy_from_user(dst: &mut [u8], src_va: u64) -> bool {
    akuma_user_walk::copy_from(&task_walk(), dst, src_va)
}

/// Copy `src` into the task's address space at `dst_va` - `copy_from_user`'s mirror, for syscalls that fill
/// user buffers (`getdents64`, `read`). False (`-EFAULT`) when a page does not translate.
fn copy_to_user(dst_va: u64, src: &[u8]) -> bool {
    akuma_user_walk::copy_to(&task_walk(), dst_va, src)
}

/// Resolve symlinks (bounded) on the root: `akuma_tokio_kernel_core::loader`'s, the one walker.
fn resolve_effective(path: &str) -> alloc::string::String {
    akuma_tokio_kernel_core::loader::resolve_effective::<lazy::Amd64>(path)
}

/// Read a NUL-terminated path from the task at `src_va` into `buf`, returning
/// the `&str` it held. `None` (`-EFAULT` at the call site) on a bad byte or
/// no terminator inside `buf.len()`. Only the `trace-err` `execve` line reads one.
#[cfg(feature = "trace-err")]
fn user_str(src_va: u64, buf: &mut [u8]) -> Option<&str> {
    let s = akuma_user_walk::read_cstr(&task_walk(), src_va, buf.len()).ok()?;
    buf[..s.len()].copy_from_slice(&s);
    core::str::from_utf8(&buf[..s.len()]).ok()
}


// ---------------------------------------------------------------------------
// Stage 8 session Q: eventfd(2) — libcurl's multi-stack wakeup fd.
// ---------------------------------------------------------------------------

use akuma_tokio_kernel_core::nosys::note as nosys_note;

/// The syscall landing: every ring 3 -> kernel entry holds the big kernel lock
/// until the matching return to ring 3 (nesting if this core already owns it).
fn dispatch(fr: &mut ring3::SyscallFrame) -> u64 {
    // The syscall stub parks the caller's user RSP in a per-CPU stash and
    // reloads it on the way out. Any switch inside the call (every blocking
    // wait — `akuma-syscalls-glue`'s park in `recv`/`poll`/a pipe read
    // included) lets the next thread's syscall overwrite it, so it is saved
    // here and restored before the return, for every call at once.
    let stash = ring3::user_rsp_stash();
    // Middleware's `own` tier runs here, outside the kernel lock (B22). The raw number and arguments: the
    // legacy fold has not happened yet, and no user memory is reachable from this tier.
    #[cfg(feature = "middleware")]
    let (own_nr, own_args) = (fr.rax, [fr.arg1(), fr.arg2(), fr.arg3(), fr.arg4(), fr.arg5(), fr.arg6()]);
    #[cfg(feature = "middleware")]
    let own = akuma_tokio_kernel_core::middleware::own_enter(own_nr, own_args);
    smp::bkl_enter();
    // A sibling `exit_group` asked to end leaves here, at entry or on the way back
    // to ring 3, rather than running on in a dying process (`child::kill_siblings`).
    if child::kill_requested() {
        child::finish_killed();
    }
    let nr = fr.rax;
    let r = dispatch_locked(fr);
    if child::kill_requested() {
        child::finish_killed();
    }
    // The stash is this thread's again: the `rsp` the call chose on purpose (a signal frame, or
    // `rt_sigreturn` putting the interrupted one back), else the one saved at entry - a park
    // inside the call let other threads overwrite it. Before delivery, which builds on it.
    ring3::set_user_rsp_stash(child::take_return_rsp().unwrap_or(stash));
    // Pending signals are taken here, on the way back to ring 3: a handler's frame, an ignore,
    // or a default action that ends the group (`child::deliver_at_return`). After an
    // `rt_sigreturn` too, as on Linux: that is how the next pending signal gets its turn.
    let r = child::deliver_at_return(fr, nr, r);
    // A frame pushed just now already set the stash; its override is spent.
    let _ = child::take_return_rsp();
    smp::bkl_leave();
    #[cfg(feature = "middleware")]
    akuma_tokio_kernel_core::middleware::own_leave(own, own_nr, own_args, r);
    r
}

fn dispatch_locked(fr: &mut ring3::SyscallFrame) -> u64 {
    #[cfg(feature = "trace-err")]
    {
        let (nr, a1, a2, a3) = (fr.rax, fr.rdi, fr.rsi, fr.rdx);
        if nr == 59 {
            let mut pb = [0u8; 160];
            let p = user_str(a1, &mut pb).unwrap_or("<bad>");
            akuma_primitives::safe_print!(224, "[execve] s{} path={}\n", crate::exec_twin::current_slot() as i64, p);
        }
        let rip = fr.user_rip;
        let islot = exec_twin::current_slot();
        if islot < child::NSLOTS {
            INFLIGHT.with_mut(|t| t[islot] = (nr, a1, a2, a3));
        }
        if nr == 202 {
            FXRING.push((islot as i32, a2, a1, a3, u64::MAX - 1));
        }
        let r = dispatch_inner(fr);
        if islot < child::NSLOTS {
            INFLIGHT.with_mut(|t| t[islot].0 = 0);
        }
        if nr == 202 {
            FXRING.push((islot as i32, a2, a1, a3, r));
        }
        trace_err(nr, r, a1, a2, a3);
        trace_ring(nr, r, a1, a2, a3, rip);
        r
    }
    #[cfg(not(feature = "trace-err"))]
    {
        dispatch_inner(fr)
    }
}

/// The syscall ring `trace_ring` fills and `trace_hang_dump` reads.
#[cfg(feature = "trace-err")]
const N: usize = 1024;
#[cfg(feature = "trace-err")]
static RING: ring::Ring<(i32, u64, u64, u64, u64, u64, u64), N> = ring::Ring::new((0, 0, 0, 0, 0, 0, 0));
/// Every futex call in global order: `(slot, op, uaddr, val_or_count, result)`.
#[cfg(feature = "trace-err")]
static FXRING: ring::Ring<(i32, u64, u64, u64, u64), 160> = ring::Ring::new((0, 0, 0, 0, 0));

/// The syscall each slot is in RIGHT NOW (`nr`, three args; `nr == 0` = none):
/// a parked thread is inside one that has not returned, which the ring cannot show.
#[cfg(feature = "trace-err")]
static INFLIGHT: akuma_primitives::cell::BorrowCell<[(u64, u64, u64, u64); child::NSLOTS + 1]> =
    akuma_primitives::cell::BorrowCell::new([(0, 0, 0, 0); child::NSLOTS + 1]);

/// The idle thread calls this when nothing has run for a while: every live
/// slot's last `per` syscalls, so a hang shows what each thread was waiting on.
#[cfg(feature = "trace-err")]
pub(crate) fn trace_hang_dump(per: usize) {
    akuma_primitives::safe_print!(64, "[hang] idle for 20 s; last futex calls, oldest first (ret=PENDING: entry):\n");
    FXRING.view(|head, ring| {
        for i in 0..160 {
            let (sl, op, ua, v, r) = ring[(head + i) % 160];
            if op != 0 || ua != 0 {
                if r == u64::MAX - 1 {
                    akuma_primitives::safe_print!(96, "[hang] fx s{} op=0x{:x} @0x{:x} v=0x{:x} PENDING\n", sl, op, ua, v);
                } else {
                    akuma_primitives::safe_print!(96, "[hang] fx s{} op=0x{:x} @0x{:x} v=0x{:x} -> {}\n", sl, op, ua, v, r as i64);
                }
            }
        }
    });
    akuma_primitives::safe_print!(64, "[hang] last syscalls per slot:\n");
    RING.view(|head, ring| {
        for slot in 0..child::NSLOTS {
            if !akuma_tokio_kernel_core::proc::slot_in_use(slot) {
                continue;
            }
            let (inr, ix, iy, iz) = INFLIGHT.with(|t| t[slot]);
            if inr != 0 {
                akuma_primitives::safe_print!(
                    112, "[hang] s{} IN nr={} a=0x{:x},0x{:x},0x{:x}\n", slot, inr, ix, iy, iz
                );
            }
            let mut shown = 0;
            for i in (0..N).rev() {
                if shown >= per {
                    break;
                }
                let (sl, n, x, y, z, ret, rp) = ring[(head + i) % N];
                if n != 0 && sl == slot as i32 {
                    shown += 1;
                    akuma_primitives::safe_print!(
                        112, "[hang] s{} nr={} a=0x{:x},0x{:x},0x{:x} -> {} rip=0x{:x}\n",
                        slot, n, x, y, z, ret as i64, rp
                    );
                }
            }
        }
    });
}

/// `--features trace-err`: the last 96 syscalls of every task, dumped when a
/// task writes the 4-byte value 17 (std's CLOEXEC error pipe reporting EEXIST).
/// Debug aid for "could not execute process"; the dump is the evidence.
#[cfg(feature = "trace-err")]
#[allow(clippy::many_single_char_names)] // register-argument names
fn trace_ring(nr: u64, r: u64, a1: u64, a2: u64, a3: u64, rip: u64) {
    static DUMPED: core::sync::atomic::AtomicU8 = core::sync::atomic::AtomicU8::new(0);
    let slot = crate::exec_twin::current_slot() as i32;
    RING.push((slot, nr, a1, a2, a3, r, rip));
    // What a task says on stderr (a panic/abort message), capped: the
    // evidence the next line of the console would otherwise lose.
    if nr == 1 && a1 == 2 && r > 0 {
        static SEEN_ERR: core::sync::atomic::AtomicU8 = core::sync::atomic::AtomicU8::new(0);
        if SEEN_ERR.fetch_add(1, core::sync::atomic::Ordering::Relaxed) < 120 {
            let mut b = [0u8; 120];
            let n = (r as usize).min(b.len());
            if copy_from_user(&mut b[..n], a2) {
                let txt = core::str::from_utf8(&b[..n]).unwrap_or("<non-utf8>");
                akuma_primitives::safe_print!(160, "[stderr] s{}: {}\n", slot, txt.trim_end());
            }
        }
    }
    if nr == 200 {
        akuma_primitives::safe_print!(64, "[ring] tkill by s{}; its last syscalls:\n", slot);
        ring_dump_slot(slot, 40);
    }
    // std's CLOEXEC error pipe: 4 bytes errno (big endian) + b"NOEX".
    if nr == 1 && a3 == 8 && r == 8 {
        let mut b = [0u8; 8];
        if copy_from_user(&mut b, a2) && &b[4..] == b"NOEX"
            && DUMPED.fetch_add(1, core::sync::atomic::Ordering::Relaxed) < 3
        {
            akuma_primitives::safe_print!(
                64, "[ring] exec failed in s{} errno={}; its last syscalls:\n",
                slot, u32::from_be_bytes([b[0], b[1], b[2], b[3]])
            );
            ring_dump_slot(slot, 60);
        }
    }
}

/// Print `slot`'s entries among the newest `last` of the syscall ring, oldest first.
#[cfg(feature = "trace-err")]
fn ring_dump_slot(slot: i32, last: usize) {
    RING.view(|head, ring| {
        for i in 0..N {
            let (sl, n, x, y, z, ret, rp) = ring[(head + i) % N];
            if n != 0 && sl == slot && i + last >= N {
                akuma_primitives::safe_print!(
                    112, "[ring] nr={} a=0x{:x},0x{:x},0x{:x} -> {} rip=0x{:x}\n",
                    n, x, y, z, ret as i64, rp
                );
            }
        }
    });
}

/// `--features trace-err`: name every syscall that answered a negative errno
/// (`[err] nr=N -E`), capped so a polling loop cannot flood the console. The
/// gap-finding view `[nosys]` is the always-on half of.
#[cfg(feature = "trace-err")]
fn trace_err(nr: u64, r: u64, a1: u64, a2: u64, a3: u64) {
    // Capped PER syscall number (40 lines each): one global cap let a polling
    // loop's ETIMEDOUT or a path walk's readlink EINVAL use the whole budget
    // before the failing execve was ever reached.
    static SEEN: [core::sync::atomic::AtomicU8; 512] =
        [const { core::sync::atomic::AtomicU8::new(0) }; 512];
    let v = r as i64;
    // EFAULT is never capped away: it is the answer a bad user range gets.
    let always = v == -14;
    if (-4095..0).contains(&v) && v != -11 && v != -2 && v != -10 && v != -25
        && v != -47 && v != -110
        && (always
            || (nr as usize) < SEEN.len()
                && SEEN[nr as usize].fetch_add(1, core::sync::atomic::Ordering::Relaxed) < 40)
    {
        akuma_primitives::safe_print!(96, "[err] s{} nr={} {} a=0x{:x},0x{:x},0x{:x}\n", crate::exec_twin::current_slot() as i64, nr, v, a1, a2, a3);
    }
}

/// `write`/`writev` of a channel-less process's stdio: the console
/// (`akuma_runtime_tokio::console`, shared with aarch64).
fn console_stdio_write(nr: u64, a: [u64; 6]) -> Option<u64> {
    let vectored = match nr {
        akuma_syscalls_abi::x86_64::WRITE => false,
        akuma_syscalls_abi::x86_64::WRITEV => true,
        _ => return None,
    };
    akuma_runtime_tokio::console::stdio_write(vectored, a, console::putb)
}

/// The tokio surface on this ABI: the shared-number calls, then the allow-list by the call
/// an x86_64 number names (`akuma-runtime-tokio`, arch-neutral), then the x86_64 fd family
/// (`akuma-syscalls-glue-amd64`: open flags, `struct stat`, the legacy numbers), which
/// answers `None` for what it does not know.
fn tokio_surface(nr: u64, args: [u64; 6]) -> Option<u64> {
    if let Some(r) = akuma_runtime_tokio::serve_shared(nr, args) {
        return Some(r);
    }
    if let Some(r) = akuma_syscalls_abi::Syscall::from_x86_64(nr).and_then(|c| akuma_runtime_tokio::serve(c, args)) {
        return Some(r);
    }
    akuma_syscalls_glue_amd64::dispatch(nr, args)
}

fn dispatch_inner(fr: &mut ring3::SyscallFrame) -> u64 {
    // M4 slice 3: the per-syscall capture `fork_process` step 6 consumes
    // (`get_saved_user_context` → the twin's `read_user_context`). The boot
    // task has no slot and skips; a slotted task pays one struct copy.
    crate::child::capture_user_context(fr);
    // The x86-only legacy path spellings (rename/mkdir/rmdir/link/unlink/
    // symlink/readlink/chmod/chown/lchown) mean the `*at` call with
    // `AT_FDCWD`; the fold is `akuma_syscalls_abi::legacy`, the same table
    // amd64 dispatches through. Rewritten in the frame so the one `*at` arm
    // below serves both spellings.
    #[allow(clippy::tuple_array_conversions)] // destructuring assignment into the frame's registers
    if let Some((call, a)) = akuma_syscalls_abi::legacy::fold(
        fr.rax,
        [fr.arg1(), fr.arg2(), fr.arg3(), fr.arg4(), fr.arg5(), fr.arg6()],
    ) {
        fr.rax = call.to_x86_64();
        (fr.rdi, fr.rsi, fr.rdx, fr.r10, fr.r8, fr.r9) = (a[0], a[1], a[2], a[3], a[4], a[5]);
    }
    // Middleware's `kernel` tier wraps the rest of the dispatch (B22), after the fold: a hook sees the canonical `*at`
    // number on either spelling. It gets the arguments by value and a read-only context: nothing a hook does can change
    // what the call below reads from the frame.
    #[cfg(feature = "middleware")]
    {
        let nr = fr.rax;
        let args = [fr.arg1(), fr.arg2(), fr.arg3(), fr.arg4(), fr.arg5(), fr.arg6()];
        akuma_tokio_kernel_core::middleware::run_kernel(nr, args, || dispatch_after_fold(fr))
    }
    #[cfg(not(feature = "middleware"))]
    {
        dispatch_after_fold(fr)
    }
}

/// `dispatch_inner` from the tokio surface on: everything after the legacy fold.
fn dispatch_after_fold(fr: &mut ring3::SyscallFrame) -> u64 {
    // The tokio surface (`akuma-runtime-tokio`): its allow-list to glue by the asm-generic
    // number, the fd family to the x86_64 ABI layer amd64 uses too. One local intercept
    // first: stdio of a process with no channel goes to the console (`write`/`writev` only,
    // neither of which is on the allow-list, so the order changes no routing).
    {
        let args = [fr.arg1(), fr.arg2(), fr.arg3(), fr.arg4(), fr.arg5(), fr.arg6()];
        if let Some(r) = console_stdio_write(fr.rax, args) {
            return r;
        }
        if let Some(r) = tokio_surface(fr.rax, args) {
            return r;
        }
    }
    match fr.rax {
        akuma_syscalls_abi::x86_64::EXIT | akuma_syscalls_abi::x86_64::EXIT_GROUP => {
            let code = fr.arg1() & 0xFF; // what wait(2) would see
            with_task(|t| t.exit = Some(code));
            // A slotted task's exit is a task exit — status published to its
            // parent, its pipe references dropped, the wait4-parked parent
            // woken, the threading slot terminated (never returns to the stub:
            // `x86_finish_current` switches away for good).
            if child::is_real_task() {
                // `exit` (60) from a thread ends that thread only; everything
                // else (exit_group, a lone process's exit) ends the group.
                let scope = akuma_syscalls_proc::exit::scope(
                    fr.rax == akuma_syscalls_abi::x86_64::EXIT_GROUP,
                    child::is_thread_task(),
                );
                if scope == akuma_syscalls_proc::exit::Scope::Thread {
                    child::task_exit_thread()
                }
                child::task_exit_real(code)
            }
            // Stage 3: not a halt — the report and the next spawn (or the
            // final gate line) live in `task_done`. This arm never returns to
            // the stub: the task is gone, and its parked `syscall` frame on
            // the syscall stack is abandoned by design (the stub restarts
            // from the stack top on the next task's first syscall).
            task_done(code)
        }
        akuma_syscalls_abi::x86_64::BRK => mm::brk_arm::<lazy::Amd64>(fr.arg1()),
        ring3::nr::ARCH_PRCTL => {
            let (code, addr) = (fr.arg1(), fr.arg2());
            match code {
                ring3::arch::ARCH_SET_FS => {
                    if addr >= USER_TOP {
                        return e(ring3::err::EPERM); // the kernel half is not a TLS base
                    }
                    // `akuma_cpu::msr::set_fs_base` is the closed wrmsr path,
                    // canonical-check included.
                    match akuma_cpu::msr::set_fs_base(addr) {
                        Ok(()) => {
                            // Session pi: keep the per-slot shadow in step —
                            // this MSR is per-core and `switch_to` installs
                            // the shadow, so a runtime TLS change must land
                            // in both or the next park-resume rolls back.
                            crate::sched::set_slot_fs(
                                crate::exec_twin::current_slot(),
                                addr,
                            );
                            0
                        }
                        Err(_) => e(ring3::err::ERANGE),
                    }
                }
                ring3::arch::ARCH_GET_FS => {
                    if !user_ok(addr, 8) {
                        return e(ring3::err::EFAULT);
                    }
                    let v = akuma_cpu::msr::read(akuma_cpu::msr::Msr::FsBase);
                    if !copy_to_user(addr, &v.to_ne_bytes()) {
                        return e(ring3::err::EFAULT);
                    }
                    0
                }
                _other => {
                    e(ring3::err::ENOSYS)
                }
            }
        }
        akuma_syscalls_abi::x86_64::MMAP => {
            let (flags, fd) = (fr.arg4(), fr.arg5());
            // The descriptor is the process's; the decision is core's (`mm::mmap_arm`).
            let file = if flags & ring3::mmap::MAP_ANONYMOUS == 0 { fd_inode(fd) } else { None };
            mm::mmap_arm::<lazy::Amd64>(fr.arg1(), fr.arg2(), fr.arg3(), flags, file, fr.arg6())
        }
        akuma_syscalls_abi::x86_64::MPROTECT => mprotect_arm(fr.arg1(), fr.arg2(), fr.arg3()),
        akuma_syscalls_abi::x86_64::MUNMAP => munmap_arm(fr.arg1(), fr.arg2()),
        akuma_syscalls_abi::x86_64::MSYNC => msync_arm(fr.arg1(), fr.arg2()),
        akuma_syscalls_abi::x86_64::MADVISE => madvise_arm(fr.arg1(), fr.arg2(), fr.arg3()),
        akuma_syscalls_abi::x86_64::SCHED_YIELD => {
            if crate::exec_twin::current_slot() < child::NSLOTS && akuma_threading::x86_runnable_exists() {
                let stash = ring3::user_rsp_stash();
                akuma_threading::x86_yield();
                ring3::set_user_rsp_stash(stash);
            }
            0
        }
        // x86_64 130. No signal is delivered here, so "a signal arrived" has two observable
        // causes: a `tkill` (the thread's interrupt bit) and a child's death (SIGCHLD).
        // Glue's, as on the other two kernels: they arm the restore-sigmask slot that
        // `child::deliver_at_return` reads. busybox ash's `wait` parks in `rt_sigsuspend`.
        akuma_syscalls_abi::x86_64::RT_SIGSUSPEND => akuma_syscalls_glue::handle_syscall(
            akuma_syscalls_abi::Syscall::RtSigsuspend.to_aarch64(),
            &[fr.arg1(), fr.arg2(), 0, 0, 0, 0],
        ),
        akuma_syscalls_abi::x86_64::RT_SIGPENDING => akuma_syscalls_glue::handle_syscall(
            akuma_syscalls_abi::Syscall::RtSigpending.to_aarch64(),
            &[fr.arg1(), fr.arg2(), 0, 0, 0, 0],
        ),
        ring3::nr::PAUSE => akuma_syscalls_glue::sys_pause(),
        akuma_syscalls_abi::x86_64::RT_SIGRETURN => child::sigreturn_arm(fr),
        // musl's realloc on an mmapped chunk (`akuma_tokio_kernel_core::mm`'s).
        akuma_syscalls_abi::x86_64::MREMAP => mm::mremap_arm::<lazy::Amd64>(fr.arg1(), fr.arg2(), fr.arg3(), fr.arg4()),
        // `clone` without `CLONE_THREAD`, `fork` and `vfork` all fork through
        // `fork_process`; `CLONE_THREAD` goes to `clone_thread` (child.rs).
        akuma_syscalls_abi::x86_64::CLONE | ring3::nr::FORK | ring3::nr::VFORK => child::fork_arm(fr),
        akuma_syscalls_abi::x86_64::EXECVE => {
            // A fork child (its own threading slot, published by
            // `fork_process`) execs the way Linux does — its image is
            // replaced, its fds survive, its thread keeps running. The boot
            // thread has no process to replace.
            if child::is_real_task() {
                return child::exec_real_task(fr);
            }
            e(ring3::err::ENOSYS)
        }
        // `tkill`/`tgkill`: the x86 kernels' shared bodies - pend on the thread, and interrupt
        // it only when the default action kills (setting the flag on the caller of musl's
        // `raise` made the mask restore after it fail with EINTR).
        akuma_syscalls_abi::x86_64::TKILL => akuma_syscalls_glue_amd64::kill::tkill(fr.arg1() as u32, fr.arg2() as u32),
        // `kill`: the x86 kernels' shared body - pend on the target group, each thread takes it
        // at its own next return to ring 3.
        akuma_syscalls_abi::x86_64::KILL => akuma_syscalls_glue_amd64::kill::kill(fr.arg1(), fr.arg2()),
        akuma_syscalls_abi::x86_64::TGKILL => {
            akuma_syscalls_glue_amd64::kill::tgkill(fr.arg1() as u32, fr.arg2() as u32, fr.arg3() as u32)
        }
        other => {
            // A guest task hit a number this table does not serve; the errno
            // is the answer (the arch-level checklist arm is above). The
            // first hit of each number is named on the console, so a probe
            // run (`rustc`, `git`, `cargo`) yields the gap list for free.
            nosys_note(other);
            e(ring3::err::ENOSYS)
        }
    }
}

// ---------------------------------------------------------------------------
// Init: the one boot program.
// ---------------------------------------------------------------------------

use akuma_tokio_kernel_core::init::{MAX_ARGV, MAX_ENV, Prog};

static INIT: akuma_primitives::cell::BorrowCell<Prog> = akuma_primitives::cell::BorrowCell::new(Prog::new());

/// Fill [`INIT`] from the boot command line. A sequence of programs is a shell script run as
/// init: init is a real process, it can fork.
fn parse_init(cmdline: &str) {
    INIT.with_mut(|p| p.parse(cmdline));
}

/// Load init and start it on its pool thread. Never returns.
fn run_init() -> ! {
    let prog = INIT.with(|p| *p);
    let path = prog.path_str();
    console::puts(identity().name);
    console::puts(": init spawn -- init=");
    console::puts(path);
    console::puts("\n");
    let Ok(image) = root_fs().read_file(path) else {
        console::puts("  init: NOT on the mounted root\n");
        halt();
    };
    spawn(&prog, &image)
}

/// Init exited (`child::start_init_thread` reports it from the parked boot
/// thread): say so and stop.
pub(crate) fn task_done(code: u64) -> ! {
    let prog = INIT.with(|p| *p);
    console::puts(identity().name);
    console::puts(": exit code ");
    console::put_dec(code);
    console::puts(" from ");
    console::puts(prog.path_str());
    console::puts("\n");
    console::puts(identity().name);
    console::puts(": all programs done\n");
    halt();
}

/// The fork child's address space, copy-on-write: `akuma_tokio_kernel_core::tree`'s clone
/// over this machine's tree (`lazy::Amd64`'s `UserTree`). `docs/reference/memory.md`.
pub(crate) fn clone_user_half(parent_root: u64, dst_root: u64) -> Option<()> {
    akuma_tokio_kernel_core::tree::clone_user_half::<lazy::Amd64>(parent_root, dst_root)
}

/// The kernel half of every task root is the live CR3's: one PML4 copy shares
/// every kernel subtree (physmap, device window, kernel image).
pub(crate) fn copy_kernel_half(dst: phys::Frame) {
    // SAFETY: CR3 holds the live PML4 frame.
    let k = unsafe { phys::Frame::from_pa(akuma_cpu::ctlreg::cr3() & PTE_ADDR) };
    for i in 256..512 {
        dst.set_entry(i, k.entry(i));
    }
}

/// Load an ELF image into the CURRENT task slot (`akuma_tokio_kernel_core::loader::load_task`):
/// shared by the boot spawn and `execve`.
fn load_task(argv: &[&str], env: &[&str], image: &[u8], path: &str) -> Option<TaskImage> {
    akuma_tokio_kernel_core::loader::load_task::<lazy::Amd64>(argv, env, image, path)
}


pub use akuma_tokio_kernel_core::loader::TaskImage;

/// Load the image, build the task's address space, and cross. Never returns:
/// the task ends in the dispatcher's `exit` or the trap handler's `[Fault]`.
fn spawn(p: &Prog, image: &[u8]) -> ! {
    let path = p.path_str();
    if image.len() < 4 || image[..4] != [0x7f, b'E', b'L', b'F'] {
        akuma_primitives::safe_print!(160, "  init: ELF refused: bad magic\n");
        halt();
    }
    let (argv, n_args, envv, n_env) = {
        let mut a: [&str; MAX_ARGV + 1] = [path; MAX_ARGV + 1];
        for k in 0..p.n_args() {
            a[k + 1] = p.arg_str(k);
        }
        let n = p.n_args();
        let mut owned: [&str; MAX_ARGV + 1] = [path; MAX_ARGV + 1];
        owned[1..=n].copy_from_slice(&a[1..=n]);
        let mut env: [&str; MAX_ENV] = [""; MAX_ENV];
        #[allow(clippy::needless_range_loop)] // `k` also indexes `p`
        for k in 0..p.n_env() {
            env[k] = p.env_str(k);
        }
        (owned, n, env, p.n_env())
    };
    let Some(ti) = load_task(&argv[..=n_args], &envv[..n_env], image, path) else {
        akuma_primitives::safe_print!(160, "  init: task load failed\n");
        halt();
    };

    // Arm the syscall path, then cross. `mov cr3` is the write the ctlreg
    // crate deliberately does not own (reads safe, writes unsafe at the call
    // site): it loads the task's root, flushing the non-global TLB — the
    // kernel half survives through the shared PML4 entries.
    ring3::enable_syscalls(dispatch);
    console::puts(identity().name);
    console::puts(": entering ring 3 -- ");
    console::puts(path);
    console::puts("\n");
    // Init is pid 1 on a real pool thread (a tid >= 1, a ring row, a registered `Process`), the way amd64's `run_init`
    // starts it, not on the boot stack: that is what lets it `fork`/`clone` and be preempted, so `sshd` runs one forked process
    // per session (`docs/archive/TAOXICHUAN_SWITCH_TO_CRATES.md` S1). The boot thread parks in `start_init_thread` and reports init's exit.
    child::start_init_thread(path, &ti)
}

// ---------------------------------------------------------------------------
// The kernel a root composes.
// ---------------------------------------------------------------------------

/// A kernel being composed: what a root's `main.rs` chains before [`Kernel::run_init`].
///
/// `akuma-tokio-kernel`'s `entry!` builds it from the boot handoff and the root package's name
/// and version. Each wiring method exists only with the feature that compiles its default; a
/// part left unwired is absent (no console output, loopback-only networking, one core). The
/// root filesystem is the one part the kernel cannot run without: `.disk()` or `.root(..)`.
#[must_use = "a Kernel does nothing until `run_init`"]
#[allow(clippy::struct_excessive_bools)] // one switch per builder method, not a state machine
pub struct Kernel {
    start_info: u64,
    identity: Identity,
    #[cfg(feature = "console")]
    console: bool,
    #[cfg(feature = "disk")]
    disk: bool,
    #[cfg(feature = "net")]
    net: bool,
    #[cfg(feature = "smp")]
    smp: bool,
    root: Option<fn() -> alloc::sync::Arc<dyn Filesystem>>,
    #[cfg(feature = "middleware")]
    middleware: Option<&'static dyn akuma_tokio_kernel_core::middleware::Dispatch>,
}

impl Kernel {
    /// The kernel `boot.s` handed `start_info` (the PVH `hvm_start_info` address), named
    /// `name` (`uname -n`, and the console) at `version` (`uname -r`).
    pub const fn new(start_info: u64, name: &'static str, version: &'static str) -> Self {
        Self {
            start_info,
            identity: Identity { name, nodename: name, version, art: "" },
            #[cfg(feature = "console")]
            console: false,
            #[cfg(feature = "disk")]
            disk: false,
            #[cfg(feature = "net")]
            net: false,
            #[cfg(feature = "smp")]
            smp: false,
            root: None,
            #[cfg(feature = "middleware")]
            middleware: None,
        }
    }

    /// The name on the console and in the banner, when it differs from the package name
    /// (which stays `uname -n`).
    pub const fn name(mut self, name: &'static str) -> Self {
        self.identity.name = name;
        self
    }

    /// Art printed above the banner line, just before init starts.
    pub const fn banner(mut self, art: &'static str) -> Self {
        self.identity.art = art;
        self
    }

    /// COM1: boot lines, the shared crates' `safe_print!`, and the stdio of a process with
    /// no channel of its own (init, and everything it forks before a session gives it one).
    #[cfg(feature = "console")]
    pub const fn console(mut self) -> Self {
        self.console = true;
        self
    }

    /// The root filesystem: ext2 on the first virtio-blk device, mounted at `/`.
    #[cfg(feature = "disk")]
    pub const fn disk(mut self) -> Self {
        self.disk = true;
        self
    }

    /// The root filesystem from `make` instead (called once the heap is up), mounted at `/`.
    /// Wins over [`Kernel::disk`].
    pub const fn root(mut self, make: fn() -> alloc::sync::Arc<dyn Filesystem>) -> Self {
        self.root = Some(make);
        self
    }

    /// The first virtio-net device, smoltcp on it, and a bounded wait (10 s) for a DHCP
    /// lease. Without it the socket layer is loopback-only.
    #[cfg(feature = "net")]
    pub const fn net(mut self) -> Self {
        self.net = true;
        self
    }

    /// Start the application processors the command line's `smp=N` asks for (all of them
    /// share this kernel under one big kernel lock). Without it, or without the token, one core.
    #[cfg(feature = "smp")]
    pub const fn smp(mut self) -> Self {
        self.smp = true;
        self
    }

    /// The kernel's one syscall middleware (`akuma_tokio_kernel_core::middleware`, B22). A second call panics:
    /// a kernel has one table, and composes several in its own function.
    #[cfg(feature = "middleware")]
    pub const fn middleware(mut self, mw: &'static dyn akuma_tokio_kernel_core::middleware::Dispatch) -> Self {
        assert!(self.middleware.is_none(), "one syscall middleware per kernel: compose in your own function");
        self.middleware = Some(mw);
        self
    }

    /// Boot, then load `init=` from the root and run it as pid 1. Never returns.
    pub fn run_init(self) -> ! {
        boot(&self)
    }
}

/// The boot sequence, in the order the crate doc gives and why.
fn boot(k: &Kernel) -> ! {
    let hvm_start_info = k.start_info;
    // The console first: everything after this reports through it, and the
    // shared crates' sink with it — the boot kit's memory bring-up reports
    // through `safe_print!`.
    #[cfg(feature = "console")]
    if k.console {
        console::init();
    }
    akuma_tokio_kernel_core::set_identity(k.identity);
    #[cfg(feature = "middleware")]
    if let Some(mw) = k.middleware {
        akuma_tokio_kernel_core::middleware::install(
            mw,
            akuma_tokio_kernel_core::middleware::Machine {
                decode: akuma_syscalls_abi::Syscall::from_x86_64,
                encode: akuma_syscalls_abi::Syscall::to_x86_64,
                copy_in: copy_from_user,
                copy_out: copy_to_user,
                pid: || akuma_exec::process::read_current_pid().unwrap_or(0),
            },
        );
    }

    console::puts("\n");
    console::puts(k.identity.name);
    console::puts("/amd64 -- long mode reached\n");
    console::puts("  uart: ");
    console::puts(if console::present() { "present\n" } else { "absent (reads report no data)\n" });
    console::puts("  hvm_start_info @ 0x");
    console::put_hex(hvm_start_info);
    console::puts("\n");

    // GDT, then IDT, then WP — the order of `boot::early_init`, minus the
    // steps this kernel has no caller for (per-CPU block, scheduler).
    gdt_init();
    // This core's per-CPU block, BEFORE the IDT: a handler that runs
    // `ring3::set_syscall_stack_top` reads `gs:[24]`. GS_BASE is the block,
    // KERNEL_GS_BASE the (absent) program's; every ring-3 entry `swapgs`es.
    let _ = ring3::install_percpu(0, 0);
    // The boot task runs on no spawn slot: the per-CPU slot word starts at 0,
    // which is a ring slot, so say NO_SLOT explicitly.
    exec_twin::set_current_slot(exec_twin::NO_SLOT);
    // The BSP boots holding the kernel lock (no AP exists yet to contend).
    smp::bkl_enter();
    idt_init();
    // `CR0.WP`: a kernel *write* to a read-only page honours the page tables.
    // Without it, `.rodata` protection is a fiction and copy-on-write later
    // becomes impossible to reason about. The only CR0 bit this kernel sets.
    // SAFETY: setting a single documented control-register bit; interrupts
    // are masked, one core, no thread of execution to race.
    unsafe {
        core::arch::asm!(
            "mov {tmp:r}, cr0",
            "or {tmp:r}, {wp:r}",
            "mov cr0, {tmp:r}",
            tmp = out(reg) _,
            wp = in(reg) 1u64 << 16,
            options(nostack, preserves_flags)
        );
    }

    // The machine's own description of itself.
    // The command line is parsed per-token below (init=, initargs=) straight
    // off the start info; a full copy exists only if a stage needs to grep it.
    let mut cmdline_buf = [0u8; 512];
    let _cmdline = cmdline(hvm_start_info, &mut cmdline_buf);
    let mut describe_buf = [0u8; 512];
    let Ok(machine) = akuma_ryzen_amd64::describe(&Physmap, hvm_start_info, &mut describe_buf) else {
        console::puts("  [FATAL] no hvm_start_info magic at the PVH handoff address\n");
        halt();
    };
    akuma_primitives::safe_print!(
        160,
        "  machine: {} MiB RAM, {} memmap entries, {} virtio-mmio slot(s)\n",
        machine.usable_ram() / 1024 / 1024,
        machine.start_info.memmap_entries,
        machine.virtio.as_slice().len()
    );

    // Heap then PMM. The no-op reapers are the honest hooks for a kernel with
    // no processes, no page cache and no retired list (each is a `|| 0`).
    if !mem::init(&machine, cfg!(feature = "pmm-forensics"), akuma_pmm::PmmHooks {
        heap_reclaim: || 0,
        drain_retired: akuma_exec::process::reclaim::drain_retired_under_pressure,
        evict_clean_file_pages: |_| 0,
        shrink_page_cache: |_| 0,
    }) {
        console::puts("\n  memory bring-up FAILED\n");
        halt();
    }

    smp::record_kernel_root();
    // The root disk, and the ext2 root on it.
    // session pb: after PMM/heap are up - map_device allocates page-table frames
    lapic_init();
    smp::record_bsp_lapic_id(akuma_lapic_amd64::apic_id());
    // The one clock (the TSC, `net::uptime_us`): calibrated before anything
    // registers it, so it never jumps backwards under a deadline. After
    // `lapic_init`, whose own TSC measurement is the fallback when there is
    // no PIT (KVM, `microvm`).
    net::calibrate();
    // Session pc, slice 1: the exec/threading runtime twin. Registered here,
    // not earlier: the clock must be calibrated before it is anyone's clock,
    // and PMM/heap must be up before the first allocation behind registration. Nothing shared runs
    // before the first spawn, so this is also "before any syscall can reach
    // the tables" (amd64's ordering rule).
    exec_twin::init();
    install_surface();
    // The virtio-mmio window is the disk's and the NIC's: map it whichever is compiled in.
    #[cfg(any(feature = "disk", feature = "net"))]
    let virtio = map_virtio(&machine.virtio);
    // One instance, two names: `akuma-vfs-glue`'s mount table holds it at `/`
    // (what `akuma-syscalls-glue`'s arms resolve through), and `ROOT_FS` holds
    // a `'static` borrow of the same `Arc` for the arms still served here. Two
    // instances would be two block caches and two locks over one disk.
    let root = k.root.map(|make| make());
    #[cfg(feature = "disk")]
    let root = root.or_else(|| if k.disk && virtio { disk_root() } else { None });
    let Some(fs) = root else {
        console::puts("  [FATAL] no root filesystem: wire `.disk()` or `.root(..)`\n");
        halt();
    };
    mount_root(fs);

    // The network path — smoltcp on virtio-net, wait (bounded, pumping the NIC) for DHCP. A
    // guest with no NIC or no lease still boots; sshd will bind and simply
    // never answer, which is a legible failure, not a hang. Unwired: loopback only.
    #[cfg(feature = "net")]
    let nic = k.net && virtio && net::net_init();
    #[cfg(not(feature = "net"))]
    let nic = false;
    if !nic {
        net::loopback_init();
    }

    // `smp=N` starts N-1 application processors; absent or 1 keeps one core.
    #[cfg(feature = "smp")]
    if k.smp {
        let smp_cap = cmdline_token(hvm_start_info, "smp=", &mut [0u8; 8])
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(1);
        if smp_cap > 1 {
            smp::start_secondaries(smp_cap);
            akuma_primitives::safe_print!(64, "  smp: {} of {} requested\n", smp::online_cpus(), smp_cap);
        }
    }

    // ---- The init-spawn point ------------------------------------------------
    //
    // The full kernel resolves `init=` against the root, loads the ELF, builds
    // the address space and enters user mode. Stage 1's gate is *reaching*
    // this point on a mounted root: the banner goes here (the full kernel
    // prints its own in exactly this position, right before `run_init` starts
    // init), and the spawn's evidence is that init's bytes are on the disk and
    // are an ELF. Loading and entering user mode is stage 2's line.
    banner();

    // Stage 8 (M1): the wall clock anchor. `epoch=<unix seconds>` on the
    // guest command line (boot_dev.sh injects a fresh one into both herd
    // conf copies at every boot) is this kernel's one truth about wall
    // time: it is stored as a UTC offset in the shared
    // `akuma_primitives::clock` offset store — the same store ring 3's
    // clock_settime would write on the metal kernel — anchored at the
    // uptime sampled here. No token: the store stays unset and CLOCK_REALTIME
    // / gettimeofday keep their stage-4 answers. This is *written new*
    // (guest-cmdline policy): the metal's anchors are RTC/NTP mechanisms
    // (`akuma-cmos-rtc`, `akuma-sntp`), neither reachable under this guest.
    if let Some(n) = cmdline_token(hvm_start_info, "mmdbg=", &mut [0u8; 8]).and_then(|v| v.parse::<u32>().ok()) {
        MMDBG_V.store(n, core::sync::atomic::Ordering::Relaxed);
    }
    if let Some(e) = cmdline_token(hvm_start_info, "epoch=", &mut [0u8; 32]) {
        match e.parse::<u64>() {
            Ok(secs) => {
                let at = net::uptime_us();
                akuma_primitives::clock::set_utc_time_us(secs.saturating_mul(1_000_000), at);
                akuma_primitives::safe_print!(
                    96,
                    "  [clock] epoch={} anchored at uptime={}us\n",
                    secs,
                    at
                );
            }
            Err(_) => console::puts("  [clock] bad epoch= token, ignored\n"),
        }
    }

    let mut init_buf = [0u8; 512];
    let cmdline_str = cmdline(hvm_start_info, &mut init_buf);
    if cmdline_token(hvm_start_info, "init=", &mut [0u8; 128]).is_none() {
        console::puts("  init: no init= on the command line -- nothing to spawn\n");
        halt();
    }
    parse_init(cmdline_str);
    run_init();
}

/// The multiboot2 entry's answer — `boot.s` calls the root's `kmain_mb2` (from `entry!`) when
/// GRUB's magic says the info block is a multiboot2 one. A kernel built from this crate is a VMM
/// guest (PVH, virtio-mmio); rather than growing a second boot protocol, it says so and stops.
pub fn boot_multiboot2() -> ! {
    raw_uart(b"\nakuma-tokio-kernel: multiboot2 boot -- PVH/VMM only\n");
    halt();
}

/// Park the core forever with interrupts masked.
pub fn halt() -> ! {
    loop {
        // SAFETY: `cli` and `hlt` are unconditionally safe to execute at ring
        // 0 and this function never returns, so masking interrupts permanently
        // is the intent rather than a leaked side effect.
        unsafe {
            core::arch::asm!("cli; hlt", options(nomem, nostack, preserves_flags));
        }
    }
}

/// Out of memory. The honest response at this stage is to say so and stop —
/// there is no process to kill and no pressure ladder to climb.
#[alloc_error_handler]
fn alloc_error_handler(layout: core::alloc::Layout) -> ! {
    akuma_primitives::safe_print!(
        96,
        "[OOM] allocation of {} bytes failed\n",
        layout.size()
    );
    halt();
}

/// Session pf: straight at the UART, no budget, no allocator — the panic
/// path's first words, so a panicking kernel can never die silent again
/// (pf-5's VM exit printed nothing).
pub(crate) fn raw_uart(bytes: &[u8]) {
    for &b in bytes {
        akuma_port::COM1.write(akuma_port::UartReg::Data, b);
    }
}

fn raw_dec(mut v: u64) {
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    loop {
        i -= 1;
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    raw_uart(&buf[i..]);
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    raw_uart(b"\n[PANIC] ");
    if let Some(loc) = info.location() {
        raw_uart(loc.file().as_bytes());
        raw_uart(b":");
        raw_dec(u64::from(loc.line()));
    }
    raw_uart(b"\n");
    akuma_primitives::safe_print!(256, "       {}\n", info.message());
    halt();
}
