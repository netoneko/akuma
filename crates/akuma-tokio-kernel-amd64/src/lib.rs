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
mod ring;
mod sched;
mod shmpages;
mod smp;

use akuma_boot_amd64::{mem, paging, phys};
use akuma_idt_amd64 as idt_lib;
use akuma_ring3_amd64::err;
use akuma_trap_amd64 as trap;
use akuma_trap_amd64::{InterruptStackFrame, Kind, Trap};
use akuma_vfs::Filesystem;
use akuma_boot_amd64::paging::{MemAttr, PteProt};
use akuma_ring3_amd64 as ring3;

// PB1 linkage proof (session pb): the two M4 crates must be *referenced*, or
// the linker drops them and the size delta means nothing. `#[used]` makes the
// fn pointers GC roots; no runtime call, no behaviour change.
#[used]
static _PB1_THREADING_REGISTER: fn(akuma_threading::ThreadRuntime, akuma_threading::ThreadConfig) =
    akuma_threading::register;
#[used]
static _PB1_EXEC_INIT: fn(akuma_exec::ExecRuntime, akuma_exec::ExecConfig) = akuma_exec::init;
use alloc::vec::Vec;
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
    let value = line.split_ascii_whitespace().find_map(|t| t.strip_prefix(prefix))?;
    let n = value.len().min(buf.len());
    buf[..n].copy_from_slice(&value.as_bytes()[..n]);
    core::str::from_utf8(&buf[..n]).ok()
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
/// How far a user stack may grow below its initial pages: 8 MiB, the Linux
/// default `ulimit -s`. cargo's main thread goes deeper than the 64 KiB
/// every image starts with.
pub(crate) const STACK_MAX: u64 = 8 << 20;

/// A not-present fault just under a task's stack region is the stack growing:
/// map the zeroed pages between the fault and the region, widen the region,
/// and let the instruction retry. False for any other fault.
fn grow_user_stack(cr2: u64) -> bool {
    let Some((root, (first, lo))) =
        with_task(|tk| Some((tk.root_pa, tk.layout.growth_below(USER_STACK_TOP, STACK_MAX, cr2)?)))
    else {
        return false;
    };
    let mut page = lo;
    while page > first {
        page -= 4096;
        let Some(pa) = akuma_pmm::alloc_page_zeroed() else {
            return false;
        };
        if !paging::map_page_in(root, page as usize, pa as u64, PteProt::USER_RW, MemAttr::WriteBack) {
            return false;
        }
    }
    with_task(|tk| tk.layout.extend_down(USER_STACK_TOP, first))
}

// ---------------------------------------------------------------------------
// Copy-on-write fork: shared leaf frames, the bit-9 marker, one break path.
// ---------------------------------------------------------------------------

/// x86 leaf PTE bits. Bit 9 is an AVL bit the MMU ignores: a page that was
/// writable when `fork` shared it is demoted to read-only and marked, so a
/// write fault can tell "break the share" from "this page is read-only"
/// (`mprotect(PROT_READ)`, text) — `akuma_cow::CowFault::marked`.
use akuma_pte_amd64::{ADDR_MASK as PTE_ADDR, COW as PTE_COW, D as PTE_D, RW as PTE_W};

/// W^X (`akuma_syscalls_mem::wx`): no page is writable and executable, and a page written since it was
/// mapped cannot be made executable. On by default; `wx=0` on the kernel command line turns it off for a
/// guest that needs a JIT. Stricter than Linux on purpose.
static WX_ENFORCE: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(true);
fn wx_on() -> bool {
    WX_ENFORCE.load(core::sync::atomic::Ordering::Relaxed)
}

/// Has the page at `va` in `root` been written since it was mapped (user store or kernel store)?
fn page_written(root: u64, va: u64) -> bool {
    leaf_entry(root, va & !0xFFF).is_some_and(|pte| pte.read() & PTE_D != 0)
}

/// Record that the kernel stored into the user page at `va` - the CPU sets the dirty bit for a user store,
/// nothing sets it for ours.
fn mark_written(root: u64, va: u64) {
    if let Some(pte) = leaf_entry(root, va & !0xFFF) {
        let v = pte.read();
        if v & 1 != 0 && v & PTE_D == 0 {
            pte.write(v | PTE_D);
        }
    }
}

static COW_SHARED: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
static COW_COPY: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
static COW_TAKE: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
static COW_RETRY: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// `(shared, copied, taken-in-place, retried)` since boot, for the reap lines.
#[cfg_attr(not(feature = "trace-err"), allow(dead_code))]
pub(crate) fn cow_stats() -> (usize, usize, usize, usize) {
    use core::sync::atomic::Ordering::Relaxed;
    (COW_SHARED.load(Relaxed), COW_COPY.load(Relaxed), COW_TAKE.load(Relaxed), COW_RETRY.load(Relaxed))
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

    /// The frame this entry maps, if it is present.
    fn frame(&self) -> Option<phys::Frame> {
        let v = self.read();
        // SAFETY: a present leaf of a user page table (`leaf_entry` walked to it) names a user frame the PMM
        // handed out.
        (v & 1 != 0).then(|| unsafe { phys::Frame::from_pa(v & PTE_ADDR) })
    }
}

/// THE copy-on-write break: re-read the live PTE for `va` in `root`, ask
/// `akuma_cow::decide`, and carry the answer out. The write-fault arm and the
/// kernel's own user-memory writers both come through here — one break path.
/// Single core, `IF` clear (a trap or a syscall): nothing races the walk.
/// `invlpg` after every PTE write is the whole shootdown.
fn cow_resolve(root: u64, va: u64) -> akuma_cow::CowAction {
    use akuma_cow::CowAction;
    use core::sync::atomic::Ordering::Relaxed;
    let va = va & !0xFFF;
    let Some(slot) = leaf_entry(root, va) else {
        return CowAction::Fault;
    };
    let pte = slot.read();
    if pte & 1 == 0 {
        return CowAction::Fault;
    }
    let old = pte & PTE_ADDR;
    let action = akuma_cow::CowFault {
        pte_writable: pte & PTE_W != 0,
        marked: pte & PTE_COW != 0,
        refs: share_count(pte),
    }
    .decide();
    match action {
        CowAction::Retry => {
            COW_RETRY.fetch_add(1, Relaxed);
            akuma_cpu::tlb::invlpg(va as usize);
        }
        CowAction::Fault => {}
        CowAction::TakeInPlace => {
            slot.write((pte | PTE_W) & !PTE_COW);
            akuma_cpu::tlb::invlpg(va as usize);
            COW_TAKE.fetch_add(1, Relaxed);
        }
        CowAction::Copy => {
            let (Some(new), Some(src)) = (phys::Frame::alloc_zeroed(), slot.frame()) else {
                // Out of frames: the caller's kill path reports it.
                return CowAction::Fault;
            };
            new.copy_page_from(src);
            slot.write(((pte & !PTE_ADDR) | PTE_W | new.pa()) & !PTE_COW);
            akuma_cpu::tlb::invlpg(va as usize);
            // A peer running a thread of this space may still map the old
            // frame: tell it before the old frame's count drops.
            smp::shootdown();
            // Drops this space's count on the old frame (frees it only if it
            // was somehow the last holder).
            akuma_pmm::free_page(old as usize, akuma_primitives::preempt::current_tid() as u32);
            COW_COPY.fetch_add(1, Relaxed);
        }
    }
    action
}

/// Before the kernel writes into the user page at `va` (through the physmap, which never faults): decide
/// whether the write may happen. `false` is `EFAULT`, as on Linux, where a kernel write to a page the user
/// could not write faults.
///
/// * A marked read-only page is a CoW share: break it first. If the break cannot get a frame the write is
///   refused - it used to be attempted anyway, into the still-shared frame, corrupting the other holder.
/// * An UNMARKED read-only page that several spaces hold (shared text, or an `mprotect(PROT_READ)` page after a
///   fork) must not be written: the write would land in every holder. Refused.
/// * An unshared read-only page is left writable to the kernel (the only holder is the one asking).
fn cow_prepare_write(root: u64, va: u64) -> bool {
    use akuma_cow::KernelWrite;
    let Some(slot) = leaf_entry(root, va & !0xFFF) else {
        return true; // not mapped: the translate that follows reports it
    };
    let pte = slot.read();
    match akuma_cow::kernel_write(pte & 1 != 0, pte & PTE_W != 0, pte & PTE_COW != 0, share_count(pte)) {
        KernelWrite::Allow => true,
        KernelWrite::Break => !matches!(cow_resolve(root, va), akuma_cow::CowAction::Fault),
        KernelWrite::Refuse => false,
    }
}

/// The PMM share count for the frame a PTE names, as `akuma-cow` takes it.
fn share_count(pte: u64) -> u16 {
    akuma_pmm::cow_ref_get((pte & PTE_ADDR) as usize)
}

/// `mprotect` is about to make `va` writable: a frame several spaces hold
/// (marked or not - a shared read-only text page is unmarked) must be private
/// first, or the write lands in every holder.
fn cow_privatize(root: u64, va: u64) -> bool {
    let Some(slot) = leaf_entry(root, va) else {
        return true;
    };
    let pte = slot.read();
    if !akuma_cow::needs_private_copy(pte & 1 != 0, share_count(pte)) {
        return true;
    }
    // Force the Copy arm for a share the PTE does not mark.
    slot.write((pte & !PTE_W) | PTE_COW);
    !matches!(cow_resolve(root, va), akuma_cow::CowAction::Fault)
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
    // A not-present user page that `mmap` reserved and nothing has touched yet:
    // fill it and retry. From ring 0 too (a kernel access that skipped
    // `validate_user_range`'s prefault), but only zero-fill there - a file fill
    // reads the filesystem, which a ring-0 fault may be holding. A failed fill
    // (no memory, an unreadable file) falls through to the kill.
    if code & 1 == 0 && akuma_cpu::ctlreg::cr2() < USER_TOP {
        let root = akuma_cpu::ctlreg::cr3() & PTE_ADDR;
        if lazy::fill(root, akuma_cpu::ctlreg::cr2(), t.from_ring3()) == lazy::Fill::Present {
            return;
        }
    }
    if t.from_ring3() && code & 1 == 0 && grow_user_stack(akuma_cpu::ctlreg::cr2()) {
        return;
    }
    // A write to a present user page, from EITHER ring: the copy-on-write
    // arm. Present + write (error code bits 0-1); ring 0 counts because
    // `akuma-syscalls-glue`'s arms write user memory directly through
    // `akuma-user-access` (`CR0.WP` makes that fault on a shared page), and
    // a kernel write into a forked child's buffer must break the share, not
    // fail. The live PTE decides, never the error code (`cow_resolve` re-reads
    // it); only `Fault` falls through.
    let cr2 = akuma_cpu::ctlreg::cr2();
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
            akuma_cpu::ctlreg::cr2(),
            code
        );
        #[cfg(feature = "trace-err")]
        mm_dump_for(akuma_cpu::ctlreg::cr2());
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
        akuma_cpu::ctlreg::cr2(),
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

/// Who the kernel is: its name on the console, its node name and release in `uname`, its
/// banner. Set once at the top of the boot from the [`Kernel`] the root built.
#[derive(Clone, Copy)]
struct Identity {
    name: &'static str,
    nodename: &'static str,
    version: &'static str,
    art: &'static str,
}

static IDENTITY: akuma_primitives::OnceCopy<Identity> = akuma_primitives::OnceCopy::new();

fn identity() -> Identity {
    IDENTITY.get().unwrap_or(Identity { name: "akuma", nodename: "akuma", version: "0", art: "" })
}

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

/// The end of the canonical user half: every user address is below it, and
/// nothing a task names at or above it may be mapped, read or written for it.
pub(crate) const USER_TOP: u64 = 0x0000_8000_0000_0000;

/// Top of the task's stack: canonical user half, far from anything the kernel
/// places. 16 pages — far more than musl's startup touches, and cheap.
const USER_STACK_TOP: u64 = 0x0000_7000_0000_0000;
const USER_STACK_PAGES: u64 = 16;
/// `brk` and anonymous `mmap` each get a fixed slab, registered as user
/// regions up front so every user-pointer check is one range scan. Stage 6:
/// Sized by what the heaviest guest needs, not by what hello needs: rustc's
/// `--version` maps ~1 GiB of address space (measured; 64 MiB made it
/// `ENOMEM` before it printed). The cost is only pages actually touched, and
/// the range is far from PIE code (`PIE_BASE`) and the stack. The bounds are
/// constants for now; the per-process memory model comes with stage 12.
///
/// 16 GiB since 2026-10-30: compiling this kernel in the guest needs more than 1 GiB of
/// address space in ONE rustc - it maps the 311 MB `librustc_driver`, a ~17 MB stack per
/// codegen thread and every dependency's rlib - and the old 1 GiB slab answered `ENOMEM` to
/// the stack `mmap`, which musl's `pthread_create` reports as `EAGAIN` ("failed to spawn work
/// thread", an ICE) with 2 GiB of RAM still free (`docs/archive/TAOXICHUAN_BUGS_FOUND_2026_10_30.md`).
/// Address space only: a page costs memory when it is mapped, not when it is reserved.
const MMAP_SLAB: u64 = 0x4_0000_0000;
/// Hole-table size per task (see `Task::holes`).
const MAX_HOLES: usize = 64;
/// `MAP_SHARED` mappings tracked per task: writable file ones (written back) and
/// anonymous ones (shared across fork).
const MAX_SHARED: usize = 32;
/// errnos `akuma_ring3_amd64::err` does not carry.
pub(crate) const EINTR: u64 = 4;
/// Bisect mask for the mmap-slab rework, set from the kernel command line
/// (`mmdbg=N`, default 7): 1 = munmap frees + records holes, 2 = MAP_FIXED
/// releases what it replaces, 4 = mremap frees/moves the old range.
static MMDBG_V: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(7);
fn mmdbg() -> u32 {
    MMDBG_V.load(core::sync::atomic::Ordering::Relaxed)
}
/// `brk` sits above the image at `PIE_BASE` (0x1000_0000); the dynamic linker
/// is placed at `INTERP_BASE` (0x3000_0000), so the brk window must stay under
/// 512 MiB less the image: 256 MiB, not the 512 the plan first named.
const BRK_SLAB: u64 = 0x1000_0000;
const MMAP_BASE: u64 = 0x0000_6000_0000_0000;
/// Cap on one `write(2)`: the console is a serial line, not a disk.
const WRITE_LIMIT: u64 = 1 << 20;

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

/// A regular file's current length, as ext2 has it — one size per inode, the
/// way Linux keeps it, so every description of the file agrees. `None` when the
/// inode cannot be read (the callers answer as for an empty file or `EIO`).
fn inode_size(inode: u32) -> Option<u64> {
    root_fs().metadata_by_inode(inode).ok().map(|md| md.size)
}

/// A task's memory record: its root page table, its layout (the user ranges a pointer may
/// name, the brk window, the mmap slab and its holes, the writable shared file mappings -
/// `akuma-user-layout`, whose decisions the memory arms below act on), and the exit code once
/// captured. Descriptors, cwd and signal state are the registered `akuma-exec` Process's.
struct Task {
    root_pa: u64,
    layout: UserLayout,
    exit: Option<u64>,
}

/// 16 user regions, [`MAX_HOLES`] slab holes, [`MAX_SHARED`] shared file mappings.
type UserLayout = akuma_user_layout::Layout<16, MAX_HOLES, MAX_SHARED>;

/// The task records. A [`BorrowCell`](akuma_primitives::cell::BorrowCell): reached only through [`with_task`] /
/// [`with_task_slot`] / [`with_tasks`] / [`task_root_of`], so two overlapping borrows are a panic naming the bug. A closure
/// therefore never calls anything that reaches a task record again: `user_ok`, `copy_from_user`/`copy_to_user`, `add_region`,
/// the page-table walkers and every `child::` entry run with the borrow CLOSED — read what you need out, drop the borrow, then call.
static TASKS: akuma_primitives::cell::BorrowCell<[Task; crate::child::NSLOTS + 1]> =
    akuma_primitives::cell::BorrowCell::new([const { Task::new() }; crate::child::NSLOTS + 1]);
/// Which task record the syscall path on each core acts on: the running
/// thread's slot, set on every switch-in (`sched.rs`, `exec_twin`); 0 is the
/// boot thread.
static TASK_SLOT: [core::sync::atomic::AtomicUsize; smp::MAX_CPUS] =
    [const { core::sync::atomic::AtomicUsize::new(0) }; smp::MAX_CPUS];

impl Task {
    const fn new() -> Self {
        Self { root_pa: 0, layout: UserLayout::new(MMAP_BASE, MMAP_SLAB), exit: None }
    }
}

/// `mprotect(addr, len, prot)`: re-map each present page of the range with
/// the requested `R/W/X`, and give each lazy page (`lazy.rs`) the same
/// permissions for when it is first touched. This is what makes
/// `tcc -run` and every JIT work: the code is written RW, then flipped to RX,
/// and a page mapped RW-non-exec faults on the fetch.
///
/// `PROT_NONE` keeps a present page's frame and clears its user bit: ring 3
/// faults on any access, the kernel's copies refuse it (`EFAULT`), and fork,
/// teardown and the share counts see an ordinary present page, so a later
/// `mprotect(RW)` gives the contents back. A lazy page's record is marked
/// `none` and is never filled.
fn mprotect_arm(addr: u64, len: u64, prot: u64) -> u64 {
    if addr & 0xFFF != 0 {
        return e(ring3::err::EINVAL);
    }
    if len == 0 {
        return 0;
    }
    let pages = len.div_ceil(4096);
    if len > u64::MAX / 2 || !user_ok(addr, pages * 4096) {
        return e(ring3::err::ENOMEM);
    }
    #[allow(clippy::verbose_bit_mask)] // PROT_NONE is spelled as the bits it lacks
    let none = prot & 7 == 0;
    let want =
        PteProt { write: prot & ring3::mmap::PROT_WRITE != 0, exec: prot & ring3::mmap::PROT_EXEC != 0, user: !none };
    let root = task_root();
    // W^X: decided for the whole range before any page changes, so a refusal leaves it as it was.
    if wx_on()
        && let Some(no) = akuma_syscalls_mem::wx::check_range(prot, |i| page_written(root, addr + i * 4096), pages).errno()
    {
        return e(no);
    }
    for i in 0..pages {
        let va = (addr + i * 4096) as usize;
        if lazy::reprotect(root, va as u64, want.write, want.exec, none) {
            continue;
        }
        // The remap below builds a fresh PTE; carry "written" across it or the policy forgets.
        let was_written = page_written(root, va as u64);
        // A shared mapping's frame is meant to be in every holder.
        if want.write && !in_shared_span(va as u64) && !cow_privatize(root, va as u64) {
            return e(ring3::err::ENOMEM);
        }
        let Some(pa) = paging::translate_in(root, va) else {
            continue;
        };
        let _ = paging::unmap_page_in(root, va);
        if !paging::map_page_in(root, va, pa, want, MemAttr::WriteBack) {
            return e(ring3::err::ENOMEM);
        }
        if was_written {
            mark_written(root, va as u64);
        }
    }
    // A tightened protection must reach peers' TLBs before the call returns.
    smp::shootdown();
    0
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

/// Unmap every present page of `[start, end)` in `root` and give its frame
/// back (`free_page` honours a CoW count), and clear every lazy record in it.
///
/// A writable `MAP_SHARED` file page's frame is in `shmpages`' table too: when
/// the range overlaps such a mapping, the table gives back whatever nobody maps
/// now. The caller has written those pages back (`shared_writeback`).
///
/// A file whose last unfilled record in this space just went is unpinned
/// (`lazy::unpin_released`), so an `unlink` after `munmap` frees its blocks at
/// once, as on Linux, instead of when the process is reaped.
fn release_pages(root: u64, start: u64, end: u64) {
    let tid = akuma_primitives::preempt::current_tid() as u32;
    let mut va = start;
    while va < end {
        if let Some(pa) = paging::unmap_page_in(root, va as usize) {
            akuma_pmm::free_page(pa as usize, tid);
        } else {
            lazy::clear(root, va);
        }
        va += 4096;
    }
    if with_task(|t| t.layout.shared().iter().any(|f| !f.is_anon() && f.overlaps(start, end))) {
        shmpages::sweep();
    }
    // A shared file mapping left in the pin's range writes back by inode.
    lazy::unpin_released(root, start, end, |inode, lo, hi| {
        with_task(|t| t.layout.shared().iter().any(|f| !f.is_anon() && f.inode == inode && f.overlaps(lo, hi)))
    });
    // Peers may still hold translations for the frames just freed: no frame is
    // reused before they have dropped them (the kernel lock is held throughout).
    smp::shootdown();
}

/// Copy the pages of every writable `MAP_SHARED` file mapping that overlap
/// `[lo, hi)` back to their files (clamped to the file's size at map time).
/// A frame several mappers share is written by each of them; the bytes are the
/// same. Anonymous shared mappings have no file and are skipped.
#[allow(clippy::many_single_char_names)] // s/e are the clipped range
fn shared_writeback(lo: u64, hi: u64) {
    // The records and the root are copied out and the task borrow is closed for the whole walk: the loop does file I/O
    // and `read_bytes`, and nothing in it may find the record borrowed. The (possibly shortened) table goes back at the end.
    let (root_pa, layout) = with_task(|t| (t.root_pa, t.layout));
    for f in layout.shared().iter().filter(|f| !f.is_anon()) {
        let (base, len, ino, off, fsize) = (f.base, f.len, f.inode, f.offset, f.size);
        let (s, e) = (base.max(lo), (base + len).min(hi));
        if s < e {
            let mut va = s & !0xFFF;
            while va < e {
                if let Some(frame) = leaf_entry(root_pa, va).and_then(|p| p.frame()) {
                    let file_off = off + (va - base);
                    if file_off < fsize {
                        let n = (fsize - file_off).min(4096) as usize;
                        let mut page = [0u8; 4096];
                        frame.read_bytes(0, &mut page[..n]);
                        let _ = root_fs().write_at_by_inode(ino, file_off as usize, &page[..n]);
                        // A cached read-only page of this file now holds the old bytes.
                        akuma_fpcache::invalidate_inode(ino);
                    }
                }
                va += 4096;
            }
        }
    }
}

/// Write back every shared file mapping the running task still holds (exit,
/// exec): the mapping dies with the address space. Its pages stay mapped until
/// the root is freed (`free_user_root`), whose sweep then gives `shmpages` the
/// entries nobody maps any more.
pub(crate) fn shared_flush_all() {
    if with_task(|t| !t.layout.shared().is_empty()) {
        shared_writeback(0, u64::MAX);
        with_task(|t| t.layout.forget_shared_within(0, u64::MAX));
    }
}

/// `msync(addr, len, flags)`: the POSIX flush point lld uses.
fn msync_arm(addr: u64, len: u64) -> u64 {
    if addr & 0xFFF != 0 {
        return e(ring3::err::EINVAL);
    }
    shared_writeback(addr, addr.saturating_add(len));
    0
}

/// `madvise(addr, len, advice)`: the advice decode is `akuma-syscalls-mem`'s, shared with the other kernels
/// (`MADV_FREE` is `EINVAL`, which musl's allocator reads as "not supported"; the hints are no-ops, and so is
/// `MADV_WILLNEED`: a lazy page fills on its first touch either way).
///
/// `MADV_DONTNEED` makes the next read of each page in the mmap slab return zero, per
/// [`akuma_syscalls_mem::madvise::dontneed_page_action`]: a present page's leaf becomes a zero-fill lazy
/// record with the page's permissions (`lazy.rs`), and the frame goes back - or, if a fork peer also maps it,
/// only this space's count on it. A page not yet touched keeps its record, so a private file page nobody
/// wrote still reads the file, as on Linux. A writable `MAP_SHARED` file page is written back and kept, which
/// is what Linux's drop and refault reads. Pinned divergences: a present private file page refaults as
/// zeros, where Linux rereads the file (the record of which file it came from went when it was filled);
/// pages outside the slab (the image, `brk`, the main stack) are left as they are.
fn madvise_arm(addr: u64, len: u64, advice: u64) -> u64 {
    use akuma_syscalls_mem::madvise::{self, Action, PageAction};
    if !madvise::range_fits_user_va(addr as usize, len as usize, USER_TOP as usize) {
        return e(ring3::err::EINVAL);
    }
    match madvise::action(advice as i32) {
        Action::Fail(errno) => errno,
        Action::Ignore | Action::Willneed => 0,
        Action::Dontneed => {
            let (start, pages) = madvise::dontneed_zero_range(addr as usize, len as usize);
            let (start, end) = (start as u64, (start + pages * 4096) as u64);
            if pages == 0 {
                return 0;
            }
            if with_task(|t| t.layout.shared().iter().any(|f| f.base < end && start < f.base + f.len)) {
                shared_writeback(start, end);
            }
            // Only the used part of the slab: nothing above the cursor has a leaf.
            let (lo, hi) = (start.max(MMAP_BASE), end.min(with_task(|t| t.layout.mmap_cur())));
            let root = task_root();
            // Frames whose leaves were just replaced, given back in batches once peers' TLBs have dropped them.
            let mut dropped = [0u64; 64];
            let mut n = 0;
            for va in (lo..hi).step_by(4096) {
                if let (false, Some(slot)) = (with_task(|t| t.layout.in_shared(va)), leaf_entry(root, va)) {
                    let pte = slot.read();
                    match madvise::dontneed_page_action(pte & 1 != 0, share_count(pte)) {
                        PageAction::Nothing => {}
                        // Shared or not, the answer is the same here: this space lets go of the frame (a peer
                        // keeps its count) and the page refaults as zeros.
                        PageAction::ZeroInPlace | PageAction::BreakSharing => {
                            let Some(record) = lazy::anon_like(pte).encode() else {
                                continue;
                            };
                            slot.write(record);
                            akuma_cpu::tlb::invlpg(va as usize);
                            dropped[n] = pte & PTE_ADDR;
                            n += 1;
                            if n == dropped.len() {
                                free_dropped(&dropped[..n]);
                                n = 0;
                            }
                        }
                    }
                }
            }
            free_dropped(&dropped[..n]);
            0
        }
    }
}

/// Give back frames whose leaves this core just rewrote, after every peer has dropped its translations of
/// them (as `cow_resolve`'s copy: the shootdown comes before the count drops).
fn free_dropped(frames: &[u64]) {
    if frames.is_empty() {
        return;
    }
    smp::shootdown();
    let tid = akuma_primitives::preempt::current_tid() as u32;
    for &f in frames {
        akuma_pmm::free_page(f as usize, tid);
    }
}

/// `munmap(addr, len)` for the mmap slab: free the frames, record the hole.
/// Ranges outside the slab (the brk area, a stack) are accepted and ignored.
fn munmap_arm(addr: u64, len: u64) -> u64 {
    let (addr, end) = match with_task(|t| t.layout.munmap(addr, len)) {
        Err(_) => return e(ring3::err::EINVAL),
        Ok(None) => return 0,
        Ok(Some(r)) => r,
    };
    if mmdbg() & 1 == 0 {
        return 0;
    }
    shared_writeback(addr, end);
    mm_event(1, addr, end);
    release_pages(task_root(), addr, end);
    with_task(|t| {
        t.layout.forget_shared_within(addr, end);
        t.layout.release(addr, end);
    });
    0
}


/// Which task record the syscall path acts on now.
pub(crate) fn task_slot_index() -> usize {
    TASK_SLOT[ring3::cpu_index()].load(core::sync::atomic::Ordering::Relaxed)
}

pub(crate) fn set_task_slot(i: usize) {
    TASK_SLOT[ring3::cpu_index()].store(i, core::sync::atomic::Ordering::Relaxed);
}

/// Run `f` on record `slot`.
pub(crate) fn with_task_slot<R>(slot: usize, f: impl FnOnce(&mut Task) -> R) -> R {
    TASKS.with_mut(|t| f(&mut t[slot]))
}

/// Run `f` on the record the syscall path acts on now (`TASK_SLOT`): the dispatcher's task.
pub(crate) fn with_task<R>(f: impl FnOnce(&mut Task) -> R) -> R {
    with_task_slot(task_slot_index(), f)
}

/// The page-table root of record `slot`.
pub(crate) fn task_root_of(slot: usize) -> u64 {
    TASKS.with(|t| t[slot].root_pa)
}

/// The page-table root of the record the syscall path acts on now.
pub(crate) fn task_root() -> u64 {
    task_root_of(task_slot_index())
}

fn add_region(start: u64, end: u64) {
    if !with_task(|t| t.layout.add_region(start, end)) {
        akuma_primitives::safe_print!(96, "[exec] region table full: [{:#x}, {:#x}) is not user memory\n", start, end);
    }
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

/// The ext2 lookup does not follow symlinks (a deliberate crate boundary:
/// `akuma-ext2` resolves components, `akuma-vfs::path` owns the chain
/// walker). The serve image's `/bin/sh` — what sshd's exec path spawns — is
/// a symlink, so every path that names a program goes through the chain
/// walker with ext2 as the effect provider: `is_symlink` to decide, the
/// link inode's bytes for the target (relative targets joined by the
/// walker), 8 hops to ELOOP. The stage-3 `open` path gets the same walker,
/// because `/etc/sshd/sshd.conf`'s `shell = /bin/sh` is the exact case.
fn read_link_target(path: &str) -> Option<alloc::string::String> {
    let fs = root_fs();
    if !fs.is_symlink(path) {
        return None;
    }
    // `read_symlink`, never `read_file`: a fast symlink (every busybox link
    // on the serve image — `/bin/sh`, `/bin/apk` — fits the 60-byte inline
    // form) keeps its target in the inode's pointer-word window and has no
    // data blocks. `read_file` handed those first target bytes to the block
    // mapper as a bogus block number, the read failed, spawn answered ENOENT,
    // and sshd fell back to built-in paws on every exec — three stages of
    // gates quietly ran on shell builtins because of this one provider.
    // `read_symlink` returns the target verbatim (`read_symlink_inode`
    // handles both the fast inline form and the slow block form); strip one
    // trailing newline only as belt-and-braces.
    fs.read_symlink(path)
        .ok()
        .map(|s| alloc::string::String::from(s.trim_end_matches('\n')))
}

/// Resolve symlinks (bounded), returning the effective path to look up.
fn resolve_effective(path: &str) -> alloc::string::String {
    akuma_vfs::resolve_symlink_chain(path, 8, read_link_target)
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

/// Stage 6: dirfd-relative path resolution. Absolute paths pass through; a
/// relative path resolves against an open directory fd's path (apk opens the
/// db / cache directory and then passes its fd to `openat`/`renameat`); a
/// relative path at `AT_FDCWD` resolves against `/` — this kernel has no
/// working directory, and every task it boots runs at the root. `None` means
/// the caller sees `EBADF` (a non-directory fd where a dirfd was required).
/// A whole file for the ELF loader. `Filesystem::read_file` refuses anything
/// over 16 MiB ("use read_at"), and cargo is 47 MB, cc1 38 MB: past that,
/// read it in 1 MiB steps through the inode. One heap block of the file's
/// size either way - the loader wants the image as a slice.
pub(crate) fn read_image(path: &str) -> Result<Vec<u8>, akuma_vfs::FsError> {
    let fs = root_fs();
    let md = fs.metadata(path)?;
    if md.size <= 16 * 1024 * 1024 {
        return fs.read_file(path);
    }
    let ino = fs.resolve_inode(path)?;
    let mut data: Vec<u8> = Vec::new();
    data.try_reserve_exact(md.size as usize).map_err(|_| akuma_vfs::FsError::NoSpace)?;
    data.resize(md.size as usize, 0);
    let mut off = 0usize;
    while off < data.len() {
        let end = (off + (1 << 20)).min(data.len());
        let n = fs.read_at_by_inode(ino, off, &mut data[off..end])?;
        if n == 0 {
            return Err(akuma_vfs::FsError::Internal);
        }
        off += n;
    }
    Ok(data)
}

// ---------------------------------------------------------------------------
// Stage 8 session Q: eventfd(2) — libcurl's multi-stack wakeup fd.
// ---------------------------------------------------------------------------

/// One bit per syscall number below 512 already named by `nosys_note`.
static NOSYS_SEEN: [core::sync::atomic::AtomicU64; 8] =
    [const { core::sync::atomic::AtomicU64::new(0) }; 8];

/// Name an unserved syscall number on the console, once per number.
fn nosys_note(nr: u64) {
    use core::sync::atomic::Ordering::Relaxed;
    if nr >= 512 {
        akuma_primitives::safe_print!(64, "[nosys] nr={}\n", nr);
        return;
    }
    let bit = 1u64 << (nr & 63);
    if NOSYS_SEEN[(nr >> 6) as usize].fetch_or(bit, Relaxed) & bit == 0 {
        akuma_primitives::safe_print!(64, "[nosys] nr={}\n", nr);
    }
}

/// The syscall landing: every ring 3 -> kernel entry holds the big kernel lock
/// until the matching return to ring 3 (nesting if this core already owns it).
fn dispatch(fr: &mut ring3::SyscallFrame) -> u64 {
    // The syscall stub parks the caller's user RSP in a per-CPU stash and
    // reloads it on the way out. Any switch inside the call (every blocking
    // wait — `akuma-syscalls-glue`'s park in `recv`/`poll`/a pipe read
    // included) lets the next thread's syscall overwrite it, so it is saved
    // here and restored before the return, for every call at once.
    let stash = ring3::user_rsp_stash();
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
            if !child::slot_in_use(slot) {
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

/// `write`/`writev` on a `Stdout`/`Stderr`/`DevTty` descriptor of a process
/// with no I/O channel of its own: the console. Glue would buffer it in a
/// channel nothing drains here; init (sshd) and every process forked from it
/// write their stdio this way. `None` for
/// every other call, so the fd layer serves it.
fn console_stdio_write(nr: u64, a: [u64; 6]) -> Option<u64> {
    use akuma_exec::process::FileDescriptor as F;
    let (fd, iov, cnt) = match nr {
        akuma_syscalls_abi::x86_64::WRITE => (a[0], 0, 0),
        akuma_syscalls_abi::x86_64::WRITEV => (a[0], a[1], a[2]),
        _ => return None,
    };
    // `Process.channel`, not `current_channel()`: the latter falls back to the
    // per-thread exit channel every forked child is registered with for its
    // parent's wait, and glue would buffer the child's stdout there, unread.
    let p = akuma_exec::process::current_process_shared()?;
    if p.channel.is_some() {
        return None;
    }
    if !p.with_fd(fd as u32, |d| matches!(d, F::Stdout | F::Stderr | F::DevTty))? {
        return None;
    }
    let emit = |base: u64, len: u64| -> u64 {
        let mut chunk = [0u8; 256];
        let mut done = 0u64;
        while done < len {
            let n = ((len - done) as usize).min(chunk.len());
            if akuma_user_access::copy_from_user(&mut chunk[..n], base + done).is_err() {
                return if done == 0 { e(ring3::err::EFAULT) } else { done };
            }
            for &b in &chunk[..n] {
                console::putb(b);
            }
            done += n as u64;
        }
        done
    };
    if nr == akuma_syscalls_abi::x86_64::WRITE {
        return Some(emit(a[1], a[2].min(WRITE_LIMIT)));
    }
    let mut total = 0u64;
    for i in 0..cnt.min(1024) {
        let mut entry = [0u64; 2];
        if akuma_user_access::read_user_into(&mut entry, iov + i * 16).is_err() {
            return Some(if total == 0 { e(ring3::err::EFAULT) } else { total });
        }
        let n = emit(entry[0], entry[1].min(WRITE_LIMIT));
        if n.cast_signed() < 0 {
            return Some(if total == 0 { n } else { total });
        }
        total += n;
    }
    Some(total)
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
    // The tokio surface (`akuma-runtime-tokio`): its allow-list to glue by the asm-generic
    // number, the fd family to the x86_64 ABI layer amd64 uses too. One local intercept
    // first: stdio of a process with no channel goes to the console (`write`/`writev` only,
    // neither of which is on the allow-list, so the order changes no routing).
    {
        let args = [fr.arg1(), fr.arg2(), fr.arg3(), fr.arg4(), fr.arg5(), fr.arg6()];
        if let Some(r) = console_stdio_write(fr.rax, args) {
            return r;
        }
        if let Some(r) = akuma_runtime_tokio::dispatch(fr.rax, args) {
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
        akuma_syscalls_abi::x86_64::BRK => {
            let want = fr.arg1();
            let (plan, brk_cur, root) = with_task(|t| (t.layout.brk_plan(want), t.layout.brk(), t.root_pa));
            // Linux's failure value is the *old* break; a shrink is bookkeeping only (no unmap).
            if plan == akuma_user_layout::Brk::Refuse {
                return brk_cur;
            }
            if let akuma_user_layout::Brk::Grow { from, to: end } = plan {
                // A shrink left its pages mapped, and a regrow must read zeros: release whatever is
                // there (it used to be overwritten, leaking the frame and keeping nothing), then
                // reserve the range lazily.
                release_pages(root, from, end);
                if !lazy::reserve(root, from, end - from, lazy::Lazy::anon(true, false)) {
                    release_pages(root, from, end);
                    return brk_cur;
                }
            }
            with_task(|t| t.layout.set_brk(want));
            want
        }
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
            let (addr, len, prot, flags) = (fr.arg1(), fr.arg2(), fr.arg3(), fr.arg4());
            let fd = fr.arg5();
            let off = fr.arg6();
            // Linux refuses an offset that is not a page multiple, mapping or not.
            if len == 0 || off & 0xFFF != 0 {
                return e(ring3::err::EINVAL);
            }
            if wx_on() && let Some(no) = akuma_syscalls_mem::wx::check_prot(prot).errno() {
                return e(no);
            }
            // M5: two kinds of mapping. MAP_ANONYMOUS — the only kind this
            // kernel served before — and, now, MAP_PRIVATE of an open file:
            // musl's ldso reserves a library's span anonymous and then
            // re-maps each PT_LOAD over it MAP_FIXED from the library's fd
            // (`map_library`). Zeroed pages filled from the inode at `off`
            // is exactly that, minus COW (writes are private to this task
            // because its address space dies with it).
            let anon = flags & ring3::mmap::MAP_ANONYMOUS != 0;
            let mut fnode: Option<u32> = None;
            let mut fsize = 0u64;
            if !anon {
                // A regular file in the process's table; anything else
                // (unbound, socket, pipe, directory) has no file to map.
                match fd_inode(fd) {
                    Some(node) => {
                        fnode = Some(node);
                        fsize = inode_size(node).unwrap_or(0);
                    }
                    _ => return e(ring3::err::ENODEV),
                }
            }
            let root = task_root();
            let Some(pages) = akuma_user_layout::page_span(len) else {
                return e(ring3::err::ENOMEM);
            };
            let base = if flags & ring3::mmap::MAP_FIXED != 0 && with_task(|t| t.layout.in_slab(addr, pages)) {
                // Replacing whatever was there (musl's ldso maps each PT_LOAD
                // over its anonymous reservation): the old frames go back, and
                // the range leaves the hole table.
                if mmdbg() & 2 != 0 {
                    mm_event(2, addr, addr + pages);
                    // A shared file mapping under it keeps what was written to it.
                    if overlaps_shared(addr, addr + pages) {
                        shared_writeback(addr, addr + pages);
                    }
                    release_pages(root, addr, addr + pages);
                    with_task(|t| {
                        t.layout.forget_shared_within(addr, addr + pages);
                        t.layout.carve(addr, addr + pages);
                    });
                }
                addr
            } else {
                match with_task(|t| t.layout.alloc(pages)) {
                    Some(b) => b,
                    None => return e(ring3::err::ENOMEM),
                }
            };
            // Writable only when asked, as `mprotect` already was: a `PROT_READ` file page is what the
            // shared file-page cache can hand to every mapper (`lazy::fill`), and it was never shared
            // while this widened it to RW. With W^X on, a `PROT_EXEC` mapping is RX - `W|X` was refused
            // above, so it asked for no write - and a page can only become executable if the program
            // never wrote it. With `wx=0` the old widening to RWX is back.
            let (write, exec) = if prot & ring3::mmap::PROT_EXEC == 0 {
                (prot & ring3::mmap::PROT_WRITE != 0, false)
            } else if wx_on() {
                (false, true)
            } else {
                (true, true)
            };
            // `PROT_NONE`: a record nothing fills (or, eagerly, a page without the user bit).
            #[allow(clippy::verbose_bit_mask)] // PROT_NONE is spelled as the bits it lacks
            let none = prot & 7 == 0;
            let pte = if none { PteProt { write: false, exec: false, user: false } } else { PteProt { write, exec, user: true } };
            const MAP_SHARED: u64 = 0x1;
            let shared_write = fnode.is_some() && flags & MAP_SHARED != 0 && prot & ring3::mmap::PROT_WRITE != 0;
            let shared_anon = anon && flags & MAP_SHARED != 0;
            // Lazy (`lazy.rs`): one record per page, a frame on the first touch. A writable shared file
            // page's record says so (`shared`), and its fill maps the one frame every mapper of that page
            // maps (`shmpages`); fork shares the span's present frames as they are. An anonymous shared
            // mapping is eager: two lazy records would fill two frames, so it has its frames before the
            // first fork. So is a file mapping past what a record holds, or whose inode cannot be pinned.
            let first = lazy::Lazy {
                write,
                exec,
                none,
                file: fnode.map(|inode| lazy::FilePage { inode, index: off / 4096, shared: shared_write }),
            };
            let lazy_ok =
                !shared_anon && lazy::fits(first, pages / 4096) && fnode.is_none_or(|node| lazy::pin(root, node, base, base + pages));
            if lazy_ok {
                if !lazy::reserve(root, base, pages, first) {
                    release_pages(root, base, base + pages);
                    return e(ring3::err::ENOMEM);
                }
            } else {
                let mut page = base & !0xFFF;
                while page < base + pages {
                    let Some(frame) = phys::Frame::alloc_zeroed() else {
                        return e(ring3::err::ENOMEM);
                    };
                    if !paging::map_page_in(root, page as usize, frame.pa(), pte, MemAttr::WriteBack) {
                        return e(ring3::err::ENOMEM);
                    }
                    if let Some(node) = fnode {
                        // File-backed: fill this page straight from the inode. A short read (the page
                        // runs past EOF) leaves the zeroed tail - the same bytes a mapping of the file
                        // would have there.
                        let foff = off + (page - base);
                        let mut done = 0usize;
                        while done < 4096 {
                            let want = (4096 - done).min(1024);
                            let mut chunk = [0u8; 1024];
                            let got = match root_fs().read_at_by_inode(
                                node,
                                (foff + done as u64) as usize,
                                &mut chunk[..want],
                            ) {
                                Ok(g) => g,
                                Err(_) => return e(ring3::err::EIO),
                            };
                            if got == 0 {
                                break;
                            }
                            frame.write_bytes(done, &chunk[..got]);
                            done += got;
                        }
                    }
                    page += 4096;
                }
            }
            // A MAP_FIXED mapping lands inside an earlier reservation, BELOW
            // `mmap_cur`: only a mapping above the cursor moves it.
            with_task(|t| t.layout.note_mapped(base + pages));
            if let Some(node) = fnode
                && shared_write
            {
                let f = akuma_user_layout::SharedFile { base, len: pages, inode: node, offset: off, size: fsize };
                let recorded = with_task(|t| t.layout.add_shared(f));
                if !recorded {
                    akuma_primitives::safe_print!(96, "[mmap] shared-file table full: writes to this mapping are lost\n");
                }
            }
            if shared_anon && !with_task(|t| t.layout.add_shared(akuma_user_layout::SharedFile::anon(base, pages))) {
                akuma_primitives::safe_print!(96, "[mmap] shared-file table full: a fork copies this mapping\n");
            }
            base
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
        akuma_syscalls_abi::x86_64::MREMAP => {
            // musl's realloc on an mmapped chunk. Shrink frees the tail; grow
            // extends in place when the range ends at the slab cursor, else
            // moves (MAYMOVE, which the caller accepts by using the returned
            // address): a slab mapping by moving its leaves, anything else by
            // copying the overlap.
            // MREMAP_FIXED is refused: no fixed targets.
            let (old, old_sz, new_sz, flags) = (fr.arg1(), fr.arg2(), fr.arg3(), fr.arg4());
            if new_sz == 0 || flags & 2 != 0 || old & 0xFFF != 0 {
                return e(ring3::err::EINVAL);
            }
            if old_sz == 0 || !user_ok(old, old_sz) {
                return e(ring3::err::EFAULT);
            }
            // A `MAP_SHARED` record names its range; moving or resizing the pages under it would leave it
            // naming the wrong ones. Refused rather than followed (a pinned divergence: Linux allows it).
            if overlaps_shared(old, old.saturating_add(old_sz)) {
                return e(ring3::err::EINVAL);
            }
            let root = task_root();
            let Ok(plan) = with_task(|t| t.layout.mremap(old, old_sz, new_sz)) else {
                return e(ring3::err::EINVAL);
            };
            // Both page-rounded; `mremap` above refused a size that does not round.
            let (old_pages, new_pages) = (old_sz.div_ceil(4096) * 4096, new_sz.div_ceil(4096) * 4096);
            if let akuma_user_layout::Mremap::Shrink { release } = plan {
                if let Some((lo, hi)) = release
                    && mmdbg() & 4 != 0
                {
                    mm_event(3, lo, hi);
                    release_pages(root, lo, hi);
                    with_task(|t| t.layout.release(lo, hi));
                }
                return old;
            }
            // In place: the range is the topmost thing in the slab.
            if let akuma_user_layout::Mremap::InPlace { from, to } = plan {
                if !lazy::reserve(root, from, to - from, lazy::Lazy::anon(true, false)) {
                    release_pages(root, from, to);
                    return e(ring3::err::ENOMEM);
                }
                with_task(|t| t.layout.note_mapped(to));
                return old;
            }
            let release_old = matches!(plan, akuma_user_layout::Mremap::Move { release_old: true });
            let Some(base) = with_task(|t| t.layout.alloc(new_pages)) else {
                return e(ring3::err::ENOMEM);
            };
            // A slab mapping moves by its leaves: present or lazy, each page keeps its frame and its
            // bits at the new address, so nothing is copied and nothing untouched gets filled. The
            // tail past the old length is fresh zero-fill.
            if release_old && mmdbg() & 4 != 0 {
                if !lazy::reserve(root, base + old_pages, new_pages - old_pages, lazy::Lazy::anon(true, false)) {
                    release_pages(root, base, base + new_pages);
                    return e(ring3::err::ENOMEM);
                }
                if !lazy::move_range(root, old, base, old_pages) {
                    release_pages(root, base, base + new_pages);
                    return e(ring3::err::ENOMEM);
                }
                lazy::move_pins(root, old, base, old_pages);
                // Peers may still translate the old addresses to the moved frames.
                smp::shootdown();
                mm_event(4, old, old + old_pages);
                with_task(|t| t.layout.release(old, old + old_pages));
                return base;
            }
            if !lazy::reserve(root, base, new_pages, lazy::Lazy::anon(true, false)) {
                release_pages(root, base, base + new_pages);
                return e(ring3::err::ENOMEM);
            }
            let copy = old_sz.min(new_sz);
            let mut chunk = [0u8; 1024];
            let mut done = 0u64;
            while done < copy {
                let n = ((copy - done) as usize).min(chunk.len());
                if !copy_from_user(&mut chunk[..n], old + done)
                    || !copy_to_user(base + done, &chunk[..n])
                {
                    return e(ring3::err::EFAULT);
                }
                done += n as u64;
            }
            base
        }
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

/// argv entries beyond `argv[0]`, and each one's byte budget. The command
/// line is 512 bytes total.
const MAX_ARGV: usize = 16;
const ARG_LEN: usize = 96;
/// Environment entries (`initenv=`, comma-separated `KEY=value`, no comma or
/// space in a value).
const MAX_ENV: usize = 8;

/// The init program from the command line (`init=`, `initargs=`, `initenv=`).
/// Fixed slots, no heap: parsed before ring 3 exists, read again when init exits.
#[derive(Clone, Copy)]
struct Prog {
    path: [u8; ARG_LEN],
    args: [[u8; ARG_LEN]; MAX_ARGV],
    n_args: usize,
    env: [[u8; ARG_LEN]; MAX_ENV],
    n_env: usize,
}

impl Prog {
    const fn new() -> Self {
        Self { path: [0; ARG_LEN], args: [[0; ARG_LEN]; MAX_ARGV], n_args: 0, env: [[0; ARG_LEN]; MAX_ENV], n_env: 0 }
    }

    /// One environment entry.
    fn env_str(&self, i: usize) -> &str {
        str_until_nul(&self.env[i])
    }

    /// argv[0], as the string it was stored as.
    fn path_str(&self) -> &str {
        str_until_nul(&self.path)
    }

    /// One argv entry beyond `argv[0]`.
    fn arg_str(&self, i: usize) -> &str {
        str_until_nul(&self.args[i])
    }
}

/// The `&str` a NUL-padded slot holds ("" when unset).
fn str_until_nul(buf: &[u8]) -> &str {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    core::str::from_utf8(&buf[..end]).unwrap_or("")
}

static INIT: akuma_primitives::cell::BorrowCell<Prog> = akuma_primitives::cell::BorrowCell::new(Prog::new());

/// Fill [`INIT`] from the boot command line. `initargs`/`initenv` are
/// comma-separated (`initargs=sh,/etc/guest.sh`). A sequence of programs is a
/// shell script run as init: init is a real process, it can fork.
fn parse_init(cmdline: &str) {
    INIT.with_mut(|p| {
        for token in cmdline.split_ascii_whitespace() {
            if let Some(v) = token.strip_prefix("init=") {
                store_slot(&mut p.path, v);
            } else if let Some(v) = token.strip_prefix("initargs=") {
                for a in v.split(',') {
                    if p.n_args < MAX_ARGV {
                        store_slot(&mut p.args[p.n_args], a);
                        p.n_args += 1;
                    }
                }
            } else if let Some(v) = token.strip_prefix("initenv=") {
                for a in v.split(',') {
                    if p.n_env < MAX_ENV {
                        store_slot(&mut p.env[p.n_env], a);
                        p.n_env += 1;
                    }
                }
            }
        }
    });
}

fn store_slot(dst: &mut [u8], s: &str) {
    let b = s.as_bytes();
    let n = b.len().min(dst.len() - 1);
    dst[..n].copy_from_slice(&b[..n]);
    dst[n] = 0;
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

/// The fork child's address space, copy-on-write since 2026-10-30.
/// The child's user page tables are cloned subtree-by-subtree, but the LEAF
/// frames are shared with the parent (`clone_user_tree`): a writable leaf is
/// demoted to read-only + `PTE_COW` in both tables, a read-only leaf is shared
/// as is, and the first write by either side faults into `cow_resolve`
/// (`fault_trap`), which copies or takes the frame in place. Only writable
/// `MAP_SHARED` file mappings are still copied eagerly. The root is built the
/// same way `load_task` builds one (kernel half copied from the live CR3, user
/// half empty). Huge (PS) leaves are refused. `docs/reference/memory.md`.
pub(crate) fn clone_user_half(parent_root: u64, dst_root: u64) -> Option<()> {
    // The `fork_share_memory` pass, INTO a root that already exists —
    // `fork_process` step 1 handed the child a fresh `UserAddressSpace` and
    // the hook fills it. The kernel half copies wholesale from the live CR3
    // (the same upper-half copy `load_task` gives every task root — the
    // fresh root from the crate shares only amd64's trio 256/257/511, and
    // this kernel's convention is the full upper half). The user half clones
    // the parent's subtrees leaf-by-leaf through `clone_user_tree`.
    // SAFETY: both roots are PMM-resident page-table frames; the forking parent
    // is parked and this kernel is single-core with IF clear — the discipline
    // that owns every page-table touch here.
    let (src, dst) = unsafe { (phys::Frame::from_pa(parent_root), phys::Frame::from_pa(dst_root)) };
    copy_kernel_half(dst);
    // The child inherits the parent's lazy file pages, so it holds their inodes too.
    if !lazy::fork_pins(parent_root, dst_root) || clone_user_half_into(src, dst).is_none() {
        lazy::drop_pins(dst_root);
        return None;
    }
    Some(())
}

/// The kernel half of every task root is the live CR3's: one PML4 copy shares
/// every kernel subtree (physmap, device window, kernel image).
fn copy_kernel_half(dst: phys::Frame) {
    // SAFETY: CR3 holds the live PML4 frame.
    let k = unsafe { phys::Frame::from_pa(akuma_cpu::ctlreg::cr3() & PTE_ADDR) };
    for i in 256..512 {
        dst.set_entry(i, k.entry(i));
    }
}

/// Clone the user half of `src` (a parked parent's PML4) into `dst`, one PML4
/// subtree at a time through [`clone_user_tree`]. On failure everything this
/// pass linked into `dst` is released; the root frame itself stays the caller's.
fn clone_user_half_into(src: phys::Frame, dst: phys::Frame) -> Option<()> {
    use akuma_user_tree::{ClonePlan, SharedSpans, X86_64};
    for i in 0..256 {
        let entry = src.entry(i);
        // PML4 entries sit three levels above the leaves.
        match X86_64.plan_clone(3, entry, false, SharedSpans::Copy) {
            ClonePlan::Skip => {}
            ClonePlan::Descend { table } => {
                let Some(sub) = clone_user_tree(table, 2, (i as u64) << 39) else {
                    release_cloned_prefix(dst, i);
                    return None;
                };
                dst.set_entry(i, X86_64.relink(entry, sub));
            }
            // A 1 GiB leaf: see fork_share_memory's note. Release what this pass linked first.
            ClonePlan::Refuse | ClonePlan::CopyLeaf { .. } | ClonePlan::ShareLeaf { .. } => {
                release_cloned_prefix(dst, i);
                return None;
            }
        }
    }
    Some(())
}

/// Give back the first `upto` user PML4 slots a failed clone pass linked into `dst`; the root frame
/// itself stays the caller's.
fn release_cloned_prefix(dst: phys::Frame, upto: usize) {
    for j in 0..upto {
        if let Some(akuma_user_tree::FreeStep::Table(t)) = akuma_user_tree::X86_64.plan_free(3, dst.entry(j)) {
            free_subtree(t, 2);
            dst.set_entry(j, 0);
        }
    }
}

/// One page-table level of [`fork_share_memory`]'s clone: `depth` counts the
/// levels BELOW this table (2 = a PDPT, 0 = a leaf PT naming 4 KiB frames) and
/// `va_base` is the address the table's first entry covers.
///
/// Leaves are SHARED, not copied (copy-on-write): the child's entry names the
/// parent's frame, `cow_ref_inc` counts the second holder, and a writable
/// leaf is demoted to read-only + `PTE_COW` in BOTH tables (the parent's
/// `invlpg`'d — a stale writable translation would write straight into the
/// shared frame, silently). A leaf that was already read-only keeps its flags
/// (text stays RX; no marker, so a write to it is still a kill). The one
/// exception is a writable `MAP_SHARED` file mapping (`t.shared`): those stay
/// eager per-process copies, the writeback model they run under. Interior
/// entries get a cloned subtree; huge (PS) leaves are refused. Single core,
/// `IF` clear, the forking parent parked.
///
/// On failure the partial subtree is released (through `free_page`, so the
/// counts taken so far are dropped) — a refused fork leaks nothing.
///
/// `table_pa` must be a PMM-resident page-table frame whose table belongs to the
/// USER half of a parked task's root (every caller reads it from a present entry
/// of one).
fn clone_user_tree(table_pa: u64, depth: u32, va_base: u64) -> Option<u64> {
    use akuma_user_tree::{ClonePlan, SharedSpans, X86_64};
    let dst = phys::Frame::alloc_zeroed()?;
    let out = dst.pa();
    let shift = 12 + 9 * depth;
    // SAFETY: contract above.
    let src = unsafe { phys::Frame::from_pa(table_pa) };
    for i in 0..512 {
        let entry = src.entry(i);
        let va = va_base | ((i as u64) << shift);
        // The decisions (skip / refuse / descend / copy / share-and-demote) are `akuma-user-tree`'s, host-tested
        // against a fake tree; this loop performs them. A `MAP_SHARED` span (a writable file mapping, or an anonymous
        // one) is shared as it stands, writable and unmarked (`SharedSpans::ShareWritable`): parent and child map one frame.
        // A lazy leaf is not present, so the plan would skip it: the child must inherit the record.
        if depth == 0 && lazy::is_lazy(entry) {
            dst.set_entry(i, entry);
            continue;
        }
        let plan = X86_64.plan_clone(depth, entry, depth == 0 && in_shared_span(va), SharedSpans::ShareWritable);
        match plan {
            ClonePlan::Skip => {}
            ClonePlan::Refuse => {
                free_subtree(out, depth);
                return None; // a huge page: see fork_share_memory's note
            }
            ClonePlan::Descend { table } => {
                let Some(sub) = clone_user_tree(table, depth - 1, va) else {
                    free_subtree(out, depth);
                    return None;
                };
                dst.set_entry(i, X86_64.relink(entry, sub));
            }
            ClonePlan::CopyLeaf { old } => {
                let Some(frame) = phys::Frame::alloc_zeroed() else {
                    free_subtree(out, depth);
                    return None;
                };
                // SAFETY: `old` is the frame this present leaf maps.
                frame.copy_page_from(unsafe { phys::Frame::from_pa(old) });
                dst.set_entry(i, X86_64.relink(entry, frame.pa()));
            }
            ClonePlan::ShareLeaf { frame, parent_entry, child_entry, demoted } => {
                akuma_pmm::cow_ref_inc(frame as usize);
                if demoted {
                    // The parent's entry changes: its TLB may hold the page writable.
                    src.set_entry(i, parent_entry);
                    akuma_cpu::tlb::invlpg(va as usize);
                }
                dst.set_entry(i, child_entry);
                COW_SHARED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            }
        }
    }
    Some(out)
}

/// Does `[lo, hi)` overlap a `MAP_SHARED` mapping of the running task?
fn overlaps_shared(lo: u64, hi: u64) -> bool {
    with_task(|t| t.layout.shared().iter().any(|f| f.overlaps(lo, hi)))
}

/// Is `va` inside a `MAP_SHARED` mapping of the running task (a writable file
/// one, or an anonymous one)?
fn in_shared_span(va: u64) -> bool {
    with_task(|t| t.layout.in_shared(va))
}

/// Release a page-table subtree built by [`clone_user_tree`]: `depth` levels
/// below `table_pa` (0 = a leaf table). Leaves go through `free_page` (a
/// shared frame only loses a count); the tables are never shared.
///
/// SAFETY-adjacent contract: `table_pa` is a table frame of the caller's own
/// half-built tree, unreachable from any live root.
fn free_subtree(table_pa: u64, depth: u32) {
    use akuma_user_tree::{FreeStep, X86_64};
    let tid = akuma_primitives::preempt::current_tid() as u32;
    // SAFETY: the frame is a PMM page-table frame owned by the caller.
    let table = unsafe { phys::Frame::from_pa(table_pa) };
    for i in 0..512 {
        match X86_64.plan_free(depth, table.entry(i)) {
            None => {}
            Some(FreeStep::Leaf(f)) => akuma_pmm::free_page(f as usize, tid),
            Some(FreeStep::Table(t)) => free_subtree(t, depth - 1),
        }
    }
    akuma_pmm::free_page(table_pa as usize, tid);
}

/// Map one placed `PT_LOAD` into `root`, page by page: a fresh zeroed frame,
/// mapped, with the segment's file-backed slice copied in through the physmap
/// (the zero-filled `.bss` tail needs nothing more). Returns the page-rounded
/// span it covered, `None` if a frame or a mapping failed or the segment
/// reaches past the user half (a hostile image must not map into the kernel
/// half every root shares). `akuma_elf_exec::load_segments` checked that
/// `image[offset..offset + filesz]` is in bounds.
///
/// Protection: writable wins over executable, so a `W|X` segment (no image
/// this kernel runs has one) is `USER_RW`.
///
/// With `inode` (the image's, on the root filesystem) a page that is wholly
/// one thing is left lazy (`lazy.rs`): every byte from the file becomes a file
/// record - a read-only one then fills from the file-page cache, one frame for
/// every process running the image - and every byte past the file a zero-fill
/// one. A page the segment's start or its file end cuts is filled here, as
/// every page was until 2026-11-03. Divergence: a lazy page reads the file when
/// first touched, and nothing refuses a write to a running image (`ETXTBSY`),
/// so rewriting one in place shows the running copy the new bytes.
fn place_segment(root: u64, image: &[u8], seg: &akuma_elf_exec::LoadSeg, inode: Option<u32>) -> Option<(u64, u64)> {
    let pte = if seg.flags & akuma_elf_exec::PF_W != 0 {
        PteProt::USER_RW
    } else if seg.flags & akuma_elf_exec::PF_X != 0 {
        PteProt::USER_RX
    } else {
        PteProt::USER_RO
    };
    let end = seg.vaddr.checked_add(seg.memsz)?.checked_add(0xFFF)?;
    let (first, last) = (seg.vaddr & !0xFFF, end & !0xFFF);
    if last > USER_TOP {
        return None;
    }
    // A file record names a whole file page, so the segment's pages and the
    // file's must line up (`p_offset` congruent to `p_vaddr`, as the ELF spec
    // requires for a loadable image).
    let inode = inode.filter(|&node| seg.offset.wrapping_sub(seg.vaddr).trailing_zeros() >= 12 && lazy::pin(root, node, first, last));
    let file_end = seg.vaddr + seg.filesz;
    let mut pva = first;
    while pva < last {
        let lazy = if pva >= seg.vaddr && pva + 4096 <= file_end {
            inode.map(|node| lazy::Lazy {
                write: pte.write,
                exec: pte.exec,
                none: false,
                file: Some(lazy::FilePage { inode: node, index: (seg.offset + (pva - seg.vaddr)) / 4096, shared: false }),
            })
        } else if pva >= file_end {
            Some(lazy::Lazy::anon(pte.write, pte.exec))
        } else {
            None
        };
        if let Some(rec) = lazy.filter(|&rec| lazy::fits(rec, 1)) {
            if !lazy::reserve(root, pva, 4096, rec) {
                return None;
            }
            pva += 4096;
            continue;
        }
        let frame = phys::Frame::alloc_zeroed()?;
        if !paging::map_page_in(root, pva as usize, frame.pa(), pte, MemAttr::WriteBack) {
            return None;
        }
        // The page's overlap with the segment's file-backed range. The file
        // slice starts at `p_offset`, not at the vaddr-relative offset: the two
        // differ in every Alpine dynamic binary's text segment.
        let from = pva.max(seg.vaddr);
        let to = (pva + 4096).min(seg.vaddr + seg.filesz);
        if from < to {
            let off = (seg.offset + (from - seg.vaddr)) as usize;
            frame.write_bytes((from - pva) as usize, &image[off..off + (to - from) as usize]);
        }
        pva += 4096;
    }
    Some((first, last))
}

/// Load an ELF image into the CURRENT task slot: fresh page root (the
/// kernel's upper half shared by PML4 copy), every `PT_LOAD` placed eagerly
/// at its link address, a Linux initial stack with argc/argv/envp/auxv on
/// it, heap slabs registered, the fd table reset. Returns where to cross:
/// the root to load into CR3, the entry, the initial `rsp`.
///
/// Shared by the boot spawn (`spawn`) and `execve` (`child.rs`) — one loader,
/// one placement story. Runs with the kernel
/// half live (the physmap does all the copying); the caller chooses the
/// CR3 moment.
fn load_task(argv: &[&str], env: &[&str], image: &[u8], path: &str) -> Option<TaskImage> {
    const NO_SEG: akuma_elf_exec::LoadSeg = akuma_elf_exec::LoadSeg { vaddr: 0, offset: 0, filesz: 0, memsz: 0, flags: 0 };
    let mut segs = [NO_SEG; 16];
    let (prog, n) = akuma_elf_exec::program(image, &mut segs).ok()?;
    with_task(|t| {
        t.exit = None;
        // A fresh address space, a fresh region list: `user_ok` checks against
        // exactly what THIS image mapped.
        t.layout.reset_for_exec();
    });

    // The task's root: a fresh page table whose upper half is the kernel's
    // (one PML4 copy shares every kernel subtree — physmap, device window,
    // kernel image), whose lower half holds only the task. The kernel's own
    // root keeps its identity map; the task's root simply has none.
    // The task's tables are not live until the CR3 load below.
    let root_frame = phys::Frame::alloc_zeroed()?;
    copy_kernel_half(root_frame);
    let root = root_frame.pa();
    with_task(|t| t.root_pa = root);

    // Every PT_LOAD at its placed address (a static-PIE's moved to `PIE_BASE`).
    // `read_image` read `path` from the root filesystem, so its inode there is
    // the image's: what the lazy file pages fill from.
    let image_inode = root_fs().resolve_inode(path).ok();
    for seg in &segs[..n] {
        let (first, last) = place_segment(root, image, seg, image_inode)?;
        add_region(first, last);
    }

    // M5: a `PT_INTERP` names the dynamic linker. The *decision* — including
    // the one-NUL-byte static-PIE case — is the shared `akuma-elf-exec` rule
    // (amd64's loader rides the same), and so is the parse; the mapping is
    // `place_segment` again. Placement: `INTERP_BASE` = 0x3000_0000, the base
    // akuma-elf uses — above a static-PIE program at `PIE_BASE` (an image
    // would have to be 512 MiB to reach it) and far below `MMAP_BASE`, so
    // image and linker cannot meet. A hit here changes three auxv/cross facts
    // at once: the entry ring 3 crosses is the linker's, `AT_BASE` is where it
    // was placed, and `AT_ENTRY` stays the program's.
    let (cross_entry, at_base) = match akuma_elf_exec::interp_path(image) {
        Ok(Some(path)) => {
            let ipath = core::str::from_utf8(path).ok()?;
            // Read through the same bounded symlink chain as the program:
            // Alpine's `/lib/ld-musl-x86_64.so.1` is a link, not a file.
            let eff = resolve_effective(ipath);
            let Ok(ldso) = read_image(&eff) else {
                console::puts("  exec: PT_INTERP unreadable: ");
                console::puts(&eff);
                console::puts("\n");
                return None;
            };
            let mut isegs = [NO_SEG; 8];
            let Ok(n) = akuma_elf_exec::load_segments(&ldso, akuma_elf_exec::INTERP_BASE, &mut isegs) else {
                console::puts("  exec: interpreter refused by the exec core\n");
                return None;
            };
            let mut hi = akuma_elf_exec::INTERP_BASE;
            let ldso_inode = root_fs().resolve_inode(&eff).ok();
            for seg in &isegs[..n] {
                hi = hi.max(place_segment(root, &ldso, seg, ldso_inode)?.1);
            }
            add_region(akuma_elf_exec::INTERP_BASE, hi);
            let Ok(ientry) = akuma_elf_exec::interp_entry(&ldso, akuma_elf_exec::INTERP_BASE) else {
                console::puts("  exec: interpreter has no entry\n");
                return None;
            };
            (ientry, akuma_elf_exec::INTERP_BASE)
        }
        // Static or static-PIE: the program's own entry.
        Ok(None) => (prog.entry, 0),
        // `program` already walked the same table, so this is unreachable.
        Err(_) => return None,
    };

    // argc/argv/envp/auxv. `AT_RANDOM`/`AT_EXECFN` values are the builder's
    // to fill.
    const AUX_N: usize = 14;
    let mut aux = [(0u64, 0u64); AUX_N];
    aux[0] = (ring3::AT_BASE, at_base); // the interpreter's base, 0 if none
    aux[1] = (ring3::AT_PHDR, prog.phdr_va);
    aux[2] = (ring3::AT_PHENT, akuma_elf_exec::PHENT);
    aux[3] = (ring3::AT_PHNUM, prog.phnum);
    aux[4] = (ring3::AT_PAGESZ, 4096);
    aux[5] = (ring3::AT_ENTRY, prog.entry);
    aux[6] = (ring3::AT_UID, 0);
    aux[7] = (ring3::AT_EUID, 0);
    aux[8] = (ring3::AT_GID, 0);
    aux[9] = (ring3::AT_EGID, 0);
    aux[10] = (ring3::AT_SECURE, 0);
    aux[11] = (ring3::AT_HWCAP, 0);
    aux[12] = (ring3::AT_CLKTCK, 100);
    aux[13] = (ring3::AT_RANDOM, 0);
    // The block is sized by its contents: `stack_block_len` pages, ending at
    // `USER_STACK_TOP`. A fixed block (12 pages until 2026-11-03) is what the
    // exec path's entry caps were derived from, and a 570-argument `rustc`
    // line got `E2BIG` (`docs/archive/TAOXICHUAN_EXEC_ARGV_LIMIT.md`). The
    // exec path bounds the total (`child::ARG_BYTES_MAX`); the allocation is
    // fallible because that bound is a megabyte.
    // (The argv slice is exactly argv: an older builder sliced one slot past
    // it, `..=`, and apk 3's `add` read the duplicated argv[0] as a package.)
    let block_pages = ring3::stack_block_len(argv, env, aux.len()).div_ceil(4096);
    let mut block: Vec<u8> = Vec::new();
    block.try_reserve_exact(block_pages * 4096).ok()?;
    block.resize(block_pages * 4096, 0);
    let rsp = ring3::build_stack(&mut block, USER_STACK_TOP, argv, env, &aux);

    // The stack: at least `USER_STACK_PAGES`, and room for the block plus
    // four pages of real stack under it; the fault path grows it from there.
    // The block fills the top `block_pages` pages; the stack below it is lazy,
    // a zero page on the first touch (it was all allocated here until
    // 2026-11-03: `USER_STACK_PAGES` of zeros per exec, mostly never touched).
    let stack_pages = (USER_STACK_PAGES as usize).max(block_pages + 4);
    let stack_base = USER_STACK_TOP - stack_pages as u64 * 4096;
    let first_block_page = stack_pages - block_pages;
    if !lazy::reserve(root, stack_base, first_block_page as u64 * 4096, lazy::Lazy::anon(true, false)) {
        return None;
    }
    for p in first_block_page..stack_pages {
        let frame = phys::Frame::alloc_zeroed()?;
        let k = p - first_block_page;
        frame.write_bytes(0, &block[k * 4096..(k + 1) * 4096]);
        let pva = stack_base + p as u64 * 4096;
        if !paging::map_page_in(root, pva as usize, frame.pa(), PteProt::USER_RW, MemAttr::WriteBack) {
            return None;
        }
    }
    add_region(stack_base, USER_STACK_TOP);
    drop(block);

    // The heap slabs: `brk` above the image, `mmap` in its own window. Both
    // registered now, so a pointer check is one scan.
    let brk0 = (prog.end + 0xFFF) & !0xFFF;
    with_task(|t| t.layout.set_brk_window(brk0, BRK_SLAB));
    add_region(brk0, brk0 + BRK_SLAB);
    add_region(MMAP_BASE, MMAP_BASE + MMAP_SLAB);

    Some(TaskImage { root_pa: root, entry: cross_entry, rsp })
}

/// Free a lower-half address space the loader built by hand: every leaf frame
/// (through `free_page`, so a CoW-shared frame only drops its count), every
/// page-table frame, and the root. The upper half is the kernel's, shared
/// with every root, and is never touched. Returns the pages released
/// (leaves + tables + root). The root must not be the active CR3.
pub(crate) fn free_user_root(root: u64) -> usize {
    const PRESENT: u64 = 1;
    const LARGE: u64 = 1 << 7;
    const ADDR: u64 = 0x000f_ffff_ffff_f000;
    if root == 0 || root == (akuma_cpu::ctlreg::cr3() & ADDR) {
        return 0;
    }
    lazy::drop_pins(root);
    let tid = akuma_primitives::preempt::current_tid() as u32;
    let mut n = 0usize;
    // Every frame named below is a page-table frame this walk reached from a
    // present, non-large entry of a root the loader built.
    let table = |pa: u64| {
        // SAFETY: as above.
        unsafe { phys::Frame::from_pa(pa) }
    };
    let l4 = table(root);
    for i4 in 0..256 {
        let e4 = l4.entry(i4);
        if e4 & PRESENT == 0 {
            continue;
        }
        let p3 = e4 & ADDR;
        for i3 in 0..512 {
            let e3 = table(p3).entry(i3);
            if e3 & PRESENT == 0 {
                continue;
            }
            if e3 & LARGE != 0 {
                continue; // never built by the loader; not ours to guess
            }
            let p2 = e3 & ADDR;
            for i2 in 0..512 {
                let e2 = table(p2).entry(i2);
                if e2 & PRESENT == 0 || e2 & LARGE != 0 {
                    continue;
                }
                let p1 = e2 & ADDR;
                for i1 in 0..512 {
                    let e1 = table(p1).entry(i1);
                    if e1 & PRESENT != 0 {
                        akuma_pmm::free_page((e1 & ADDR) as usize, tid);
                        n += 1;
                    }
                }
                akuma_pmm::free_page(p1 as usize, tid);
                n += 1;
            }
            akuma_pmm::free_page(p2 as usize, tid);
            n += 1;
        }
        akuma_pmm::free_page(p3 as usize, tid);
        n += 1;
    }
    akuma_pmm::free_page(root as usize, tid);
    // The root's shared file pages were written back at its exit or exec; any
    // whose last mapping this was leave `shmpages` now.
    shmpages::sweep();
    n + 1
}

/// Where a loaded task starts: the root for CR3, the entry, the stack.
pub struct TaskImage {
    pub root_pa: u64,
    pub entry: u64,
    pub rsp: u64,
}

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
        for k in 0..p.n_args {
            a[k + 1] = p.arg_str(k);
        }
        let n = p.n_args;
        let mut owned: [&str; MAX_ARGV + 1] = [path; MAX_ARGV + 1];
        owned[1..=n].copy_from_slice(&a[1..=n]);
        let mut env: [&str; MAX_ENV] = [""; MAX_ENV];
        #[allow(clippy::needless_range_loop)] // `k` also indexes `p`
        for k in 0..p.n_env {
            env[k] = p.env_str(k);
        }
        (owned, n, env, p.n_env)
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
    IDENTITY.set(k.identity);

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
    if !mem::init(&machine, false, akuma_pmm::PmmHooks {
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
    if cmdline_token(hvm_start_info, "wx=", &mut [0u8; 4]) == Some("0") {
        WX_ENFORCE.store(false, core::sync::atomic::Ordering::Relaxed);
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
