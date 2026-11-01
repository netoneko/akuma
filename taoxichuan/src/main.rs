//! Taoxichuan (淘熙川) — the minimal composed Akuma amd64 kernel.
//!
//! The composition root of the minimal-self-hosting experiment's metal track
//! (`docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT.md`): a kernel built by
//! *pulling crates*, one line per stage, with as few crates as will serve —
//! and where the logic was still stuck in `amd64/src/`, extracting it into a
//! crate first and pulling that (`akuma-boot-amd64`, `akuma-comconsole` are
//! stage 1's extractions).
//!
//! # Stage 2 — ring 3
//!
//! The gate: the same static musl `hello` the host built runs in ring 3,
//! prints its line, and exits 0 — the kernel reports the captured exit code,
//! and the zero-`[Fault]` grep still holds. The spawn point stage 1 stopped
//! at is now the real thing: a fresh root page table (the kernel's upper half
//! shared by PML4 copy, the identity map left behind in the kernel's own
//! root), the ELF's `PT_LOAD`s placed eagerly at their link addresses, a
//! Linux initial stack, and `sysretq`. Syscalls come back through
//! `akuma-ring3-amd64`'s stub: `write` into the console, `exit_group`
//! captured and reported, `brk`/`mmap`/`arch_prctl`/`set_tid_address` enough
//! for musl's startup. One task, one core, no scheduler — the task that only
//! exits does not need one.
//!
//! # Stage 3 — busybox, sequentially
//!
//! The gate: the guest runs `busybox echo <fixed string>` and
//! `busybox ls <fixed directory on the ext2 root>` as kernel-spawned
//! programs, each byte-identical to the reference (the same binary run on the
//! box against the same file set), exit code 0, zero fault lines. No shell,
//! no pty, no `fork`: the two programs are *sequential kernel-reaped tasks* —
//! spawn, run to `exit`, then spawn the next. `exit_group` does not halt the
//! machine any more; it reports, and the boot sequence continues with the
//! next program on the command line (`init=`/`initargs=` for the first,
//! `then=`/`thenargs=` for each follow-up). What the applets needed beyond
//! stage 2: an fd table (pulled, not written: `akuma-fdtable`, the extracted
//! fd-*decision* crate — this kernel supplies the effects, a few fixed slots
//! of it per task), and the syscalls their startup and their work ask for:
//! `openat` (absolute paths only, `AT_FDCWD` only, read-only), `getdents64`
//! (served from the directory listing cached on the open description),
//! `close`, `read` (a file fd, or `EOF` on the console's stdin — there is no
//! input path), and `rt_sigaction`/`rt_sigprocmask` as accepted no-ops: there
//! is no signal delivery to register against, and saying so beats failing
//! startup on a service the kernel never promised.
//!
//! # Stage 1 — boots in KVM
//!
//! The gate: reach init spawn, print the banner on the guest console, zero
//! `[Fault]`/`PANIC`/`[sig!]` lines in the ring. **Userspace actually running
//! is stage 2** — this kernel stops at the spawn point, having proved
//! everything the spawn stands on: the boot, the console, the machine's own
//! description, heap + PMM, virtio-blk, and an ext2 root it can read init's
//! bytes off.
//!
//! # What is deliberately NOT here (pinned exclusions, load-bearing)
//!
//! No pty layer, no users/multi-user machinery, no herd — init is
//! kernel-spawned (the extreme-size pattern), single vCPU, no PCI walk
//! (Firecracker-style virtio-mmio only), and no scheduler: nothing yields,
//! because at this stage there is nothing to yield to. The BKL is not even
//! compiled in — every crate here is built without `smp-shared`, where the
//! lock's entry points are no-ops, which is *correct* for one core rather than
//! a compromise.
//!
//! # The boot order, and why
//!
//! Mirrors `akuma-amd64`'s `boot::early_init` order, which is load-bearing and
//! paid for: console → GDT (before the IDT loads; the CPU reads the GDT on
//! every exception delivery) → IDT (so everything after can *report* a fault
//! instead of triple-faulting into guest silence) → `CR0.WP` → machine
//! description → heap + PMM → virtio-blk → ext2 → banner → init spawn.
//!
//! The identity map is NOT dropped, deliberately: that retirement exists to
//! hand the lower half to userspace, and stage 1 has none. Keeping it also
//! keeps `boot.s`'s low GDT reachable, so fault delivery has one fewer way to
//! be wrong. Stage 2 drops it when the lower half starts earning its keep.

#![no_std]
#![no_main]
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
mod exec_twin;
mod futex;
mod net;
#[cfg(feature = "trace-err")]
mod ring;
mod sched;
mod smp;

use akuma_boot_amd64::{mem, paging, phys};
use akuma_comconsole as console;
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

/// Timer-preemption era flag (session pf, plan §5): set at the first SPAWN
/// crossing — the tick switch runs only once user tasks exist. The pf-6
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
const STACK_MAX: u64 = 8 << 20;

/// A not-present fault just under a task's stack region is the stack growing:
/// map the zeroed pages between the fault and the region, widen the region,
/// and let the instruction retry. False for any other fault.
fn grow_user_stack(cr2: u64) -> bool {
    let limit = USER_STACK_TOP - STACK_MAX;
    if cr2 >= USER_STACK_TOP || cr2 < limit {
        return false;
    }
    let Some((root, ri, lo)) = with_task(|tk| {
        let ri = tk.regions[..tk.n_regions].iter().position(|&(_, e)| e == USER_STACK_TOP)?;
        Some((tk.root_pa, ri, tk.regions[ri].0))
    }) else {
        return false;
    };
    if cr2 >= lo {
        return false;
    }
    let first = cr2 & !0xFFF;
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
    with_task(|tk| tk.regions[ri].0 = first);
    true
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
            let Some(new) = phys::Frame::alloc_zeroed() else {
                // Out of frames: the caller's kill path reports it.
                return CowAction::Fault;
            };
            // SAFETY: `old` is the frame this present leaf maps.
            new.copy_page_from(unsafe { phys::Frame::from_pa(old) });
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
    if code & 3 == 3 && cr2 < 0x0000_8000_0000_0000 {
        let root = akuma_cpu::ctlreg::cr3() & PTE_ADDR;
        if cow_resolve(root, cr2).resolves() {
            return;
        }
    }
    // A ring-0 fault inside `akuma-user-access`'s copy loop is a bad user
    // pointer: resume at its `EFAULT` trampoline (amd64's `idt.rs` arm 2).
    if !t.from_ring3() {
        if let Some(fixup) = akuma_user_access::user_copy_fixup(t.frame.rip) {
            t.frame.rip = fixup;
            return;
        }
    }
    // Stage 2: a fault *from ring 3* is the task's death, not the kernel's —
    // say which, and park. There is no scheduler to reap through and no other
    // task to keep running; what matters for the gate is that this prints
    // `[Fault]` and stops instead of triple-faulting into guest silence.
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
        // A real fork child or thread dies alone (SIGSEGV status to its
        // waiter); the machine runs on. The shell and the boot task are the
        // M2 shapes with no slot to reap through: those halt as before.
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

fn timer_trap_locked(t: &mut Trap<'_>) {
    // The count is the twin's uptime clock (exec_twin::uptime_us); the trap
    // itself needs nothing back — a tick returns to the same instruction. The
    // `let _` is the sanctioned discard of the crate's `#[must_use]` count.
    // The count is the twin's uptime clock (exec_twin::uptime_us); the trap
    // itself needs nothing back — a tick returns to the same instruction. The
    // `let _` is the sanctioned discard of the crate's `#[must_use]` count.
    // Only the boot core advances the clock; an AP's tick just acknowledges.
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
    // exists. So the switch arms with the FIRST SPAWN crossing
    // (`PREEMPT_ARMED`, set in `spawn_arm`): before it the machine is
    // kernel-only and the tick behaves exactly like the boots that worked;
    // after it, ring 3 exists and preemption is the point.
    //
    // **The slotted-current gate** (pf-7's panic, named by the raw print):
    // `akuma-threading:3652 THREAD_STATES[cur]` with cur = usize::MAX — after
    // an exec'd shell's exit tears its slot down, a ring-3 tick can fire with
    // NO_SLOT current; the crate's switch engine indexes by current tid, so a
    // tick with no live slot must not switch (the same `may_park` discipline
    // the pipe arms and `wait4_real` keep).
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

/// Load the (already filled) IDT on THIS core — an AP's `lidt`.
pub(crate) fn idt_load() {
    IDT.load();
}

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

/// Map the machine's virtio transports, set the slot window, bring up the
/// block driver. False when no device was announced (not an error — a
/// diskless boot is a legitimate machine, just not this stage's gate).
fn blk_init(devices: &akuma_ryzen_amd64::MmioDevices) -> bool {
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
        "  blk:  {} virtio slot(s) at pa 0x{:016x} stride 0x{:x} irq {}\n",
        slots,
        base,
        stride,
        first.irq
    );
    akuma_virtio::block::init().is_ok()
}

// ---------------------------------------------------------------------------
// ext2 root on that disk.
// ---------------------------------------------------------------------------

/// The virtio-blk device, as something `akuma-ext2` can read — device 0,
/// `vda`. No `flush`: the cache in front of the media is the *host's*, which a
/// guest cannot reach past (same reasoning as `akuma-amd64`'s `fs::VirtioBlk`).
struct Vda;

impl akuma_ext2::BlockDevice for Vda {
    fn read_bytes(&self, offset: u64, buf: &mut [u8]) -> Result<(), ()> {
        akuma_virtio::block::read_bytes(offset, buf).map_err(|_| ())
    }
    fn write_bytes(&self, offset: u64, data: &[u8]) -> Result<(), ()> {
        akuma_virtio::block::write_bytes(offset, data).map_err(|_| ())
    }
}

/// No wall clock: nothing at this stage stamps an inode (the root is mounted
/// read-only in spirit — no syscall path can write), and `|| 0` is the honest
/// pre-sync answer the full kernel documents, not a guess dressed up as one.
fn no_clock() -> u64 {
    0
}

/// The mounted ext2 root, from `kmain`'s mount point and from every fd
/// syscall. Stage 3's move out of `kmain`'s frame: `openat`/`getdents64`
/// answer from ring-0 syscall context, which has no access to a local.
///
/// SAFETY: one core; every touch is either before ring 3 exists or inside
/// the syscall dispatcher (`IF` clear from `syscall`'s own masking) — the
/// same discipline `task()` documents. `Ext2Filesystem` is `Send + Sync`
/// (`akuma-vfs`'s `Filesystem` supertraits); the interior lock is a spinlock
/// that never blocks on anything this kernel runs.
static ROOT_FS: akuma_primitives::cell::BorrowCell<Option<&'static akuma_ext2::Ext2Filesystem<Vda>>> =
    akuma_primitives::cell::BorrowCell::new(None);

/// The mounted root, for the dispatcher. Panics if called before the mount —
/// which no caller does: the mount precedes the first ring-3 crossing.
fn root_fs() -> &'static akuma_ext2::Ext2Filesystem<Vda> {
    ROOT_FS.get().expect("root_fs before the mount")
}

/// Install `akuma-vfs-glue`'s hooks, create its mount table and mount the
/// ext2 root at `/` — amd64's `fs::init_vfs` + `mount_root_on`, minus what this
/// kernel does not build (no audio, no file-page cache, no `/proc` yet). Halts
/// on failure: a root the shared arms cannot resolve answers `ENOENT` for every
/// path, silently (`AKUMA_AMD64_C1_STEP4A_VFS_ADOPTION.md` § 1).
fn mount_vfs_glue(fs: alloc::sync::Arc<akuma_ext2::Ext2Filesystem<Vda>>) {
    akuma_vfs_glue::set_hooks(akuma_vfs_glue::VfsGlueHooks {
        audio_is_available: || false,
        fs_exists: akuma_vfs_glue::exists,
        probed_core_count: smp::online_cpus,
        utc_time_us: || akuma_primitives::clock::utc_time_us(net::uptime_us()),
        // No file-page cache on this kernel: mmap populates eagerly.
        fpcache_init: |_| {},
        // `/proc` is not mounted here, so nothing asks for map rows.
        pid_map_rows: |_| None,
    });
    // `akuma-syscalls-glue`'s own table: the wall clock and the core count are
    // real answers; there is no rump kernel, console poller, mouse or tap
    // device here (amd64's `boot::install_shared_sinks` registers the same set).
    akuma_syscalls_glue::set_hooks(akuma_syscalls_glue::SyscallHooks {
        box_is_rump: |_| false,
        mark_box_rump: |_| {},
        attach_server: |_, _| {},
        intercept_box_syscall: |_, _| None,
        rump_socket_readable: |_| false,
        utc_time_us: || akuma_primitives::clock::utc_time_us(net::uptime_us()),
        probed_core_count: smp::online_cpus,
        poll_console_state: |_| None,
        dev_mouse_has_events: || false,
        dev_tun_has_frame: || false,
    });
    // A process `akuma-exec` ends (a fatal signal's default action inside glue)
    // must still reach this kernel's `wait4`: see `child::on_thread_terminated`.
    akuma_threading::set_slot_purge_callback(child::on_thread_terminated);
    // Glue's pipe wakes go to the slot by number, with a fresh handle — the
    // waker amd64 registers (`pipe::wake_sink` -> `sched::wake`). The handle a
    // parked writer recorded is not trusted to still name its slot.
    akuma_syscalls_glue::pipe::set_wake_sink(|tid, _handle| {
        akuma_exec::threading::wake_by_handle(akuma_exec::threading::wake_handle_for_thread(tid));
    });
    // Glue's `getrandom` reads the primitives hook; this kernel's source is
    // `net::rng_fill` (RDRAND, SplitMix64 fallback), which always fills.
    akuma_primitives::rng::set_rng_hook(|b| {
        net::rng_fill(b);
        true
    });
    akuma_vfs_glue::init();
    if akuma_vfs_glue::mount_with("/", Some("/dev/vda"), 0, fs).is_err() {
        console::puts("  [FATAL] could not mount the ext2 root into the shared mount table\n");
        halt();
    }
    // The front door (`akuma_vfs_glue::fs`) answers "not found" for every path
    // until this flag is set. AArch64's `fs::init` sets it; this kernel mounts
    // ext2 itself, as amd64 does, and sets it at the same point.
    akuma_vfs_glue::fs::mark_initialized();
    // A thread that dies holding a VFS lock has it released at its slot's
    // TERMINATED -> FREE transition (amd64's `install_reap_hooks`).
    akuma_threading::set_slot_reap_callback(akuma_vfs_glue::reap_dead_thread);
}
// ---------------------------------------------------------------------------
// The banner — printed at the init-spawn point, where the full kernel prints
// its own: the last thing on the console before the machine would hand over.
// ---------------------------------------------------------------------------

/// The banner is `banner.txt`, the user's own art, included verbatim.
const ART: &str = include_str!("banner.txt");

fn banner() {
    console::puts("\n");
    console::puts(ART);
    console::puts("\n  Taoxichuan ");
    console::puts(env!("CARGO_PKG_VERSION"));
    console::puts(" (Akuma/amd64 crates) x86_64 -- stage 3\n\n");
}

// ---------------------------------------------------------------------------
// Stage 2: the task, its syscalls, and the crossing into ring 3.
// ---------------------------------------------------------------------------

/// Top of the task's stack: canonical user half, far from anything the kernel
/// places. 16 pages — far more than musl's startup touches, and cheap.
const USER_STACK_TOP: u64 = 0x0000_7000_0000_0000;
const USER_STACK_PAGES: u64 = 16;
/// Pages at the stack top that hold the argv/envp/auxv block `load_task` builds.
pub(crate) const ARG_BLOCK_PAGES: usize = 12;
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
/// Writable shared file mappings tracked per task (see `Task::shared`).
const MAX_SHARED: usize = 8;
/// errnos `akuma_ring3_amd64::err` does not carry.
pub(crate) const EINTR: u64 = 4;
pub(crate) const ESRCH: u64 = 3;
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

/// The one task: its root page table, the user ranges a pointer may name, the
/// heap cursors, its fd table's three `akuma-fdtable` parts, and the exit
/// code once captured.
struct Task {
    root_pa: u64,
    regions: [(u64, u64); 16],
    n_regions: usize,
    brk_base: u64,
    brk_cur: u64,
    brk_end: u64,
    mmap_cur: u64,
    /// The path of the running image, what `readlink("/proc/self/exe")`
    /// answers (rust-lld's wrapper and `std::env::current_exe` ask).
    exe: alloc::string::String,
    /// Writable `MAP_SHARED` file mappings: `(base, len, inode, file offset,
    /// file size at map time)`. The pages are private frames filled from the
    /// file (there is no page cache), so they are copied back on `munmap`,
    /// `msync`, exec and exit - what `rust-lld`'s `FileOutputBuffer` needs to
    /// leave a real ELF behind (`docs/archive/AKUMA_SELF_HOSTING.md`).
    shared: [(u64, u64, u32, u64, u64); MAX_SHARED],
    n_shared: usize,
    /// Unmapped holes below `mmap_cur` in the mmap slab, sorted by nothing,
    /// merged when they touch: `munmap` records them, `mmap` reuses them
    /// first-fit before bumping the cursor. Full table: the hole is leaked
    /// (the frames are still freed).
    holes: akuma_mmap::HoleSet<MAX_HOLES>,
    exit: Option<u64>,
}

/// The task records. A [`BorrowCell`](akuma_primitives::cell::BorrowCell): reached only through [`with_task`] /
/// [`with_task_slot`] / [`with_tasks`] / [`task_root_of`], so two overlapping borrows are a panic naming the bug. A closure
/// therefore never calls anything that reaches a task record again: `user_ok`, `copy_from_user`/`copy_to_user`, `add_region`,
/// the page-table walkers and every `child::` entry run with the borrow CLOSED — read what you need out, drop the borrow, then call.
static TASKS: akuma_primitives::cell::BorrowCell<[Task; crate::child::NSLOTS + 1]> =
    akuma_primitives::cell::BorrowCell::new([const { Task::new() }; crate::child::NSLOTS + 1]);
/// Which slot `task()` hands out: 0 is the boot's init task (sshd for this
/// stage's gate), 1 is the child running inside a SPAWN (`child.rs`), and 2
/// is a fork child running inside that child (child.rs's M2 scaffold).
static TASK_SLOT: [core::sync::atomic::AtomicUsize; smp::MAX_CPUS] =
    [const { core::sync::atomic::AtomicUsize::new(0) }; smp::MAX_CPUS];

impl Task {
    const fn new() -> Self {
        Self {
            root_pa: 0,
            regions: [(0, 0); 16],
            n_regions: 0,
            brk_base: 0,
            brk_cur: 0,
            brk_end: 0,
            mmap_cur: MMAP_BASE,
            exe: alloc::string::String::new(),
            shared: [(0, 0, 0, 0, 0); MAX_SHARED],
            n_shared: 0,
            holes: akuma_mmap::HoleSet::new(),
            exit: None,
        }
    }
}

/// `mprotect(addr, len, prot)`: re-map each present page of the range with
/// the requested `R/W/X` (anonymous memory is eagerly mapped here, so a page
/// either has a frame or is not part of the task). This is what makes
/// `tcc -run` and every JIT work: the code is written RW, then flipped to RX,
/// and a page mapped RW-non-exec faults on the fetch.
///
/// `PROT_NONE` stays a no-op: guard pages are not enforced, and unmapping the
/// frame would lose the contents a later `mprotect(RW)` must give back.
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
    if prot & 7 == 0 {
        return 0;
    }
    let want = PteProt { write: prot & ring3::mmap::PROT_WRITE != 0, exec: prot & ring3::mmap::PROT_EXEC != 0, user: true };
    let root = task_root();
    // W^X: decided for the whole range before any page changes, so a refusal leaves it as it was.
    if wx_on()
        && let Some(no) = akuma_syscalls_mem::wx::check_range(prot, |i| page_written(root, addr + i * 4096), pages).errno()
    {
        return e(no);
    }
    for i in 0..pages {
        let va = (addr + i * 4096) as usize;
        // The remap below builds a fresh PTE; carry "written" across it or the policy forgets.
        let was_written = page_written(root, va as u64);
        if want.write && !cow_privatize(root, va as u64) {
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
/// back (`free_page` honours a CoW count). Pages that were never faulted in
/// are skipped.
fn release_pages(root: u64, start: u64, end: u64) {
    let tid = akuma_primitives::preempt::current_tid() as u32;
    let mut va = start;
    while va < end {
        if let Some(pa) = paging::unmap_page_in(root, va as usize) {
            akuma_pmm::free_page(pa as usize, tid);
        }
        va += 4096;
    }
    // Peers may still hold translations for the frames just freed: no frame is
    // reused before they have dropped them (the kernel lock is held throughout).
    smp::shootdown();
}

/// Record `[start, end)` of the mmap slab as unmapped: lower the cursor when
/// the hole touches it, else add it to the hole table, merging with any hole
/// it overlaps or abuts. The algebra is `akuma_mmap::HoleSet`.
fn add_hole(t: &mut Task, start: u64, end: u64) {
    t.holes.add(&mut t.mmap_cur, start, end);
}

/// Take `[start, end)` out of the hole table (a `MAP_FIXED` landed there).
fn carve_hole(t: &mut Task, start: u64, end: u64) {
    t.holes.carve(start, end);
}

/// A fresh `bytes`-long (page multiple) range of the mmap slab: first-fit from
/// the holes, else the bump cursor. `None` when the slab is exhausted.
fn mmap_alloc(t: &mut Task, bytes: u64) -> Option<u64> {
    t.holes.alloc(&mut t.mmap_cur, MMAP_BASE + MMAP_SLAB, bytes)
}

/// Copy the pages of every writable `MAP_SHARED` file mapping that overlap
/// `[lo, hi)` back to their files (clamped to the file's size at map time),
/// and with `forget` drop the records wholly inside the range.
#[allow(clippy::many_single_char_names)] // s/e are the clipped range
fn shared_writeback(lo: u64, hi: u64, forget: bool) {
    // The records and the root are copied out and the task borrow is closed for the whole walk: the loop does file I/O
    // and `read_bytes`, and nothing in it may find the record borrowed. The (possibly shortened) table goes back at the end.
    let (root_pa, mut shared, mut n_shared) = with_task(|t| (t.root_pa, t.shared, t.n_shared));
    let mut i = 0;
    while i < n_shared {
        let (base, len, ino, off, fsize) = shared[i];
        let (s, e) = (base.max(lo), (base + len).min(hi));
        if s < e {
            let mut va = s & !0xFFF;
            while va < e {
                if let Some(pa) = paging::translate_in(root_pa, va as usize) {
                    let file_off = off + (va - base);
                    if file_off < fsize {
                        let n = (fsize - file_off).min(4096) as usize;
                        let mut page = [0u8; 4096];
                        // SAFETY: `translate_in` named a mapped user frame.
                        unsafe { phys::Frame::from_pa(pa) }.read_bytes(0, &mut page[..n]);
                        let _ = root_fs().write_at_by_inode(ino, file_off as usize, &page[..n]);
                    }
                }
                va += 4096;
            }
            if forget && s == base && e == base + len {
                n_shared -= 1;
                shared[i] = shared[n_shared];
                continue;
            }
        }
        i += 1;
    }
    if forget {
        with_task(|t| {
            t.shared = shared;
            t.n_shared = n_shared;
        });
    }
}

/// Write back every shared file mapping the running task still holds (exit,
/// exec): the mapping dies with the address space.
pub(crate) fn shared_flush_all() {
    if with_task(|t| t.n_shared) != 0 {
        shared_writeback(0, u64::MAX, true);
    }
}

/// `msync(addr, len, flags)`: the POSIX flush point lld uses.
fn msync_arm(addr: u64, len: u64) -> u64 {
    if addr & 0xFFF != 0 {
        return e(ring3::err::EINVAL);
    }
    shared_writeback(addr, addr.saturating_add(len), false);
    0
}

/// `munmap(addr, len)` for the mmap slab: free the frames, record the hole.
/// Ranges outside the slab (the brk area, a stack) are accepted and ignored.
fn munmap_arm(addr: u64, len: u64) -> u64 {
    if addr & 0xFFF != 0 || len == 0 {
        return e(ring3::err::EINVAL);
    }
    let Some(bytes) = len.checked_add(0xFFF).map(|l| l & !0xFFF) else {
        return e(ring3::err::EINVAL);
    };
    let Some(end) = addr.checked_add(bytes) else {
        return e(ring3::err::EINVAL);
    };
    if addr < MMAP_BASE || end > MMAP_BASE + MMAP_SLAB {
        return 0;
    }
    if mmdbg() & 1 == 0 {
        return 0;
    }
    shared_writeback(addr, end, true);
    mm_event(1, addr, end);
    release_pages(task_root(), addr, end);
    with_task(|t| add_hole(t, addr, end));
    0
}

const RT_SIGSUSPEND: u64 = 130;

/// `tkill(tid, sig)` / `tgkill(tgid, tid, sig)`. No signal is delivered on this
/// kernel, so a signal's only effect is the one thing a thread can observe
/// without a handler running: its blocked `poll`/pipe `read` returns `EINTR`.
/// That is exactly what the jobserver crate relies on - cargo sends `SIGUSR1`
/// to its helper thread to pull it out of a blocking wait and then joins it
/// (the ENOSYS this answered before left cargo hung forever after any error).
/// `tid` is the threading slot (what `clone` returned). Signal 0 only probes.
fn tkill_arm(tid: u64, sig: u64) -> u64 {
    if sig > 64 {
        return e(ring3::err::EINVAL);
    }
    let tid = tid as usize;
    if tid >= child::NSLOTS || !child::slot_in_use(tid) {
        return e(ESRCH);
    }
    if sig != 0 {
        akuma_threading::set_thread_interrupted(tid);
        akuma_threading::get_waker_for_thread(tid).wake();
    }
    0
}

/// Switch which task slot the syscall path acts on (the SPAWN arm, the
/// child exit, and slice 3's `twin_enter_user` — which points the record at
/// the incoming real task).
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
    with_task(|t| {
        t.regions[t.n_regions] = (start, end);
        t.n_regions += 1;
    });
}

/// Is `[va, va+len)` fully inside a region this kernel mapped for the task?
/// The check that makes `write(1, ptr, len)` honest: a bogus pointer is
/// `-EFAULT`, not a kernel fault.
fn user_ok(va: u64, len: u64) -> bool {
    with_task(|t| akuma_user_walk::in_regions(&t.regions[..t.n_regions], va, len))
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
        let Some(pa) = paging::translate_in(self.root, (va & !0xFFF) as usize) else {
            return false;
        };
        // SAFETY: `translate_in` named a mapped task frame.
        unsafe { phys::Frame::from_pa(pa) }.read_bytes((va & 0xFFF) as usize, out);
        true
    }
    fn write_page(&self, va: u64, src: &[u8]) -> bool {
        if !cow_prepare_write(self.root, va) {
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

/// View a `repr(C)` plain-old-data wire struct as its own bytes, the form
/// `copy_to_user` takes. Trim step 2 made this the shared shape: every struct
/// the dispatcher now pulls from `akuma-syscalls-linux` goes out through here
/// (sock.rs's sockaddr_in writer was the first local precedent).
fn struct_bytes<T>(t: &T) -> &[u8] {
    // SAFETY: every caller passes a `repr(C)` plain-old-data wire struct;
    // viewing it as its own bytes is well defined.
    unsafe {
        core::slice::from_raw_parts(
            core::ptr::addr_of!(*t).cast::<u8>(),
            core::mem::size_of::<T>(),
        )
    }
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
/// no terminator inside `buf.len()`.
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
    let r = dispatch_locked(fr);
    smp::bkl_leave();
    ring3::set_user_rsp_stash(stash);
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
    let always = v == -14 || nr == ring3::nr::SPAWN;
    // SPAWN's answer is never filtered: it sits above `SEEN`'s 512 numbers (the private base), so its EPERM from a
    // pool-thread init was invisible here for a week.
    let spawn = nr == ring3::nr::SPAWN;
    if (-4095..0).contains(&v) && (spawn || (v != -11 && v != -2 && v != -10 && v != -25
        && v != -47 && v != -110))
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

/// Whether `akuma-syscalls-glue` serves `call` on this kernel. One place, so
/// the folded set is readable at a glance and a batch is one diff here.
const fn served_by_glue(call: akuma_syscalls_abi::Syscall) -> bool {
    use akuma_syscalls_abi::Syscall as S;
    matches!(
        call,
        // Batch A (leaf tier): identity, host names, limits, randomness,
        // whole-filesystem sync. No fd table, no task record.
        S::Uname
            | S::Sethostname
            | S::Setdomainname
            | S::Getpid
            | S::Getppid
            | S::Gettid
            | S::Getuid
            | S::Geteuid
            | S::Getgid
            | S::Getegid
            | S::Getsid
            | S::Setsid
            | S::Getpgid
            | S::Setpgid
            | S::SchedGetaffinity
            | S::Getrandom
            | S::Membarrier
            | S::Sync
            | S::Syncfs
            | S::Sysinfo
            | S::Getrusage
            | S::Getgroups
            | S::Times
            // Signal dispositions and masks (one table: glue's `tkill` and
            // SIGPIPE read it, and Rust programs ignore SIGPIPE through it).
            | S::RtSigaction
            | S::RtSigprocmask
            | S::Sigaltstack
            // Thread naming and the exit-time tid clear (`Process.clear_child_tid`,
            // which this kernel's thread exit reads).
            | S::Prctl
            | S::SetTidAddress
            // Sockets (step 2b, with the fd table): AF_INET and AF_UNIX are
            // glue's; `sockaddr` and `msghdr` have one layout on both ABIs.
            | S::Socket
            | S::Bind
            | S::Listen
            | S::Accept
            | S::Accept4
            | S::Connect
            | S::Sendto
            | S::Recvfrom
            | S::Sendmsg
            | S::Recvmsg
            | S::Setsockopt
            | S::Getsockopt
            | S::Shutdown
            | S::Getsockname
            | S::Getpeername
    )
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
    // The arms `akuma-syscalls-glue` serves: the bodies both other kernels
    // run, reached by their asm-generic number (`to_aarch64`), exactly as
    // amd64's `to_glue`. The list grows batch by batch
    // (TAOXICHUAN_CLEANUP_PLAN.md step 2c); an arm that moves here is deleted
    // from the match below in the same change.
    if let Some(call) = akuma_syscalls_abi::Syscall::from_x86_64(fr.rax)
        && served_by_glue(call)
    {
        let args = [fr.arg1(), fr.arg2(), fr.arg3(), fr.arg4(), fr.arg5(), fr.arg6()];
        return akuma_syscalls_glue::handle_syscall(call.to_aarch64(), &args);
    }
    // The fd family over the process's `akuma-exec` table (step 2b): the
    // x86_64 ABI layer amd64 uses too, after the one local intercept — stdio
    // of a process with no channel goes to the console.
    {
        let args = [fr.arg1(), fr.arg2(), fr.arg3(), fr.arg4(), fr.arg5(), fr.arg6()];
        if let Some(r) = console_stdio_write(fr.rax, args) {
            return r;
        }
        if let Some(r) = akuma_syscalls_glue_amd64::dispatch(fr.rax, args) {
            return r;
        }
    }
    match fr.rax {
        ring3::nr::EXIT | ring3::nr::EXIT_GROUP => {
            let code = fr.arg1() & 0xFF; // what wait(2) would see
            with_task(|t| t.exit = Some(code));
            // Slice 3 FIRST: a REAL fork child's exit is a task exit — status
            // into the slot, its pipe references dropped, the wait4-parked
            // parent woken, the threading slot terminated (never returns to
            // the stub: `x86_finish_current` switches away for good). This
            // test must outrank `nested_active()`: a real child runs at its
            // own slot (1..NSLOTS), which made `nested_active()` true for it
            // too and sent its exit_group down the M2 coroutine path — whose
            // `exit_cross` restores a CrossSave `bind_child_task` never wrote
            // and `ret`s through a zeroed return address (session pe: pd's
            // deterministic rip=0).
            if child::is_real_task() {
                // `exit` (60) from a thread ends that thread only; everything
                // else (exit_group, a lone process's exit) ends the group.
                if fr.rax == ring3::nr::EXIT && child::is_thread_task() {
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
        ring3::nr::BRK => {
            let want = fr.arg1();
            let (brk_base, brk_end, brk_cur, root) = with_task(|t| (t.brk_base, t.brk_end, t.brk_cur, t.root_pa));
            if want < brk_base || want >= brk_end {
                return brk_cur; // failure: Linux returns the *old* break
            }
            if want > brk_cur {
                // Grow: map fresh zeroed pages. Shrink is a bookkeeping move
                // (no unmap) — a `printf`-and-exit task never asks for it.
                let mut page = brk_cur & !0xFFF;
                let end = (want + 0xFFF) & !0xFFF;
                while page < end {
                    let Some(pa) = akuma_pmm::alloc_page_zeroed() else {
                        return brk_cur;
                    };
                    if !paging::map_page_in(root, page as usize, pa as u64, PteProt::USER_RW, MemAttr::WriteBack) {
                        return brk_cur;
                    }
                    page += 4096;
                }
            }
            with_task(|t| t.brk_cur = want);
            want
        }
        ring3::nr::ARCH_PRCTL => {
            let (code, addr) = (fr.arg1(), fr.arg2());
            match code {
                ring3::arch::ARCH_SET_FS => {
                    if addr >= 0x0000_8000_0000_0000 {
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
                            crate::child::set_slot_fs(
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
        akuma_syscalls_abi::x86_64::FUTEX => {
            futex::sys_futex(fr.arg1(), fr.arg2(), fr.arg3(), fr.arg4(), fr.arg5(), fr.arg6())
        }
        ring3::nr::MMAP => {
            let (addr, len, prot, flags) = (fr.arg1(), fr.arg2(), fr.arg3(), fr.arg4());
            let fd = fr.arg5();
            let off = fr.arg6();
            if len == 0 {
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
            let pages = (len + 0xFFF) & !0xFFF;
            let base = if flags & ring3::mmap::MAP_FIXED != 0
                && addr >= MMAP_BASE
                && addr + pages <= MMAP_BASE + MMAP_SLAB
            {
                // Replacing whatever was there (musl's ldso maps each PT_LOAD
                // over its anonymous reservation): the old frames go back, and
                // the range leaves the hole table.
                if mmdbg() & 2 != 0 {
                    mm_event(2, addr, addr + pages);
                    release_pages(root, addr, addr + pages);
                    with_task(|t| carve_hole(t, addr, addr + pages));
                }
                addr
            } else {
                match with_task(|t| mmap_alloc(t, pages)) {
                    Some(b) => b,
                    None => return e(ring3::err::ENOMEM),
                }
            };
            // Mappings are RW (the kernel fills them), except that exec is exec: with W^X on, a
            // `PROT_EXEC` mapping is RX - `W|X` was refused above, so it asked for no write - and a
            // page can only become executable if the program never wrote it. With `wx=0` the old
            // widening to RWX is back.
            let pte = if prot & ring3::mmap::PROT_EXEC == 0 {
                PteProt::USER_RW
            } else if wx_on() {
                PteProt::USER_RX
            } else {
                PteProt::USER_RWX
            };
            let mut page = base & !0xFFF;
            while page < base + pages {
                let Some(frame) = phys::Frame::alloc_zeroed() else {
                    return e(ring3::err::ENOMEM);
                };
                if !paging::map_page_in(root, page as usize, frame.pa(), pte, MemAttr::WriteBack) {
                    return e(ring3::err::ENOMEM);
                }
                if let Some(node) = fnode {
                    // File-backed private: fill this page straight from the
                    // inode. A short read (the page runs past EOF) leaves
                    // the zeroed tail — the same bytes a private mapping of
                    // the file would have there.
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
            // A MAP_FIXED mapping lands inside an earlier reservation, BELOW
            // `mmap_cur`: only a mapping above the cursor moves it.
            with_task(|t| {
                if base + pages > t.mmap_cur {
                    t.mmap_cur = base + pages;
                }
            });
            if let Some(node) = fnode {
                const MAP_SHARED: u64 = 0x1;
                if flags & MAP_SHARED != 0 && prot & ring3::mmap::PROT_WRITE != 0 {
                    let recorded = with_task(|t| {
                        if t.n_shared < MAX_SHARED {
                            t.shared[t.n_shared] = (base, pages, node, off, fsize);
                            t.n_shared += 1;
                            true
                        } else {
                            false
                        }
                    });
                    if !recorded {
                        akuma_primitives::safe_print!(96, "[mmap] shared-file table full: writes to this mapping are lost\n");
                    }
                }
            }
            base
        }
        ring3::nr::MPROTECT => mprotect_arm(fr.arg1(), fr.arg2(), fr.arg3()),
        ring3::nr::MUNMAP => munmap_arm(fr.arg1(), fr.arg2()),
        26 => msync_arm(fr.arg1(), fr.arg2()),
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
        RT_SIGSUSPEND => {
            let (a1, a2) = (fr.arg1(), fr.arg2());
            child::sigsuspend_arm(fr, a1, a2)
        }
        15 => child::sigreturn_arm(fr),
        ring3::nr::CLOCK_GETTIME => {
            // Stage 4: the calibrated TSC gives both clocks real elapsed
            // time (net.rs). Stage 8 (M1): CLOCK_REALTIME (id 0) adds the
            // UTC offset store once a boot-time `epoch=` anchor installed
            // one (akuma_primitives::clock::realtime_us falls back to the
            // boot-relative reading, i.e. this arm's stage-4 behaviour,
            // when no anchor exists: no epoch= token, no behaviour change).
            // Every other clock id keeps its boot-relative meaning; musl's
            // `struct timespec` is two u64s.
            let (clk, tp) = (fr.arg1(), fr.arg2());
            let us = if clk == 0 {
                akuma_primitives::clock::realtime_us(net::uptime_us())
            } else {
                net::uptime_us()
            };
            if !user_ok(tp, 16) {
                return e(ring3::err::EFAULT);
            }
            // Trim step 2: the wire shape is the crate's `Timespec`
            // (`repr(C)`, 16 bytes), `from_bits` its word packing — the two
            // u64 words musl reads go out unchanged.
            let ts = akuma_syscalls_linux::time::Timespec::from_bits(
                us / 1_000_000,
                (us % 1_000_000) * 1_000,
            );
            if !copy_to_user(tp, struct_bytes(&ts)) {
                return e(ring3::err::EFAULT);
            }
            0
        }
        ring3::nr::NANOSLEEP | ring3::nr::CLOCK_NANOSLEEP => {
            // The serve loop's tick: `sleep_ms(1)` lands here, and every lap
            // pumps the NIC (net.rs) — this arm is what keeps TCP alive
            // while sshd idles. Both spellings accepted, the same sniff the
            // full kernel makes: `a0` below one page is the raw
            // (seconds, nanos) register pair libakuma's non-linux-abi path
            // sends; anything above is a `*const timespec`.
            let (a0, a1, a2, a3) = (fr.arg1(), fr.arg2(), fr.arg3(), fr.arg4());
            let (sec, nanos) = if a0 < 4096 {
                (a0, a1)
            } else {
                let mut ts = [0u8; 16];
                if !user_ok(a0, 16) || !copy_from_user(&mut ts, a0) {
                    return e(ring3::err::EFAULT);
                }
                (
                    u64::from_le_bytes(ts[0..8].try_into().unwrap()),
                    u64::from_le_bytes(ts[8..16].try_into().unwrap()),
                )
            };
            let _ = (a2, a3);
            // Cap: a task may sleep at most 60 s per call. Nothing on this
            // kernel can signal a sleeper awake, so an unbounded request
            // would be an unbounded stall of the whole machine.
            let total_us = sec
                .saturating_mul(1_000_000)
                .saturating_add(nanos / 1_000)
                .min(60_000_000);
            child::sleep_us(total_us);
            0
        }
        ring3::nr::MREMAP => {
            // musl's realloc on an mmapped chunk. Shrink frees the tail; grow
            // extends in place when the range ends at the slab cursor, else
            // moves (MAYMOVE, which the caller accepts by using the returned
            // address), copies the overlap and frees the old range.
            // MREMAP_FIXED is refused: no fixed targets.
            let (old, old_sz, new_sz, flags) = (fr.arg1(), fr.arg2(), fr.arg3(), fr.arg4());
            if new_sz == 0 || flags & 2 != 0 || old & 0xFFF != 0 {
                return e(ring3::err::EINVAL);
            }
            if old_sz == 0 || !user_ok(old, old_sz) {
                return e(ring3::err::EFAULT);
            }
            let old_pages = (old_sz + 0xFFF) & !0xFFF;
            let new_pages = (new_sz + 0xFFF) & !0xFFF;
            let root = task_root();
            let mmap_cur = with_task(|t| t.mmap_cur);
            let in_slab = old >= MMAP_BASE && old + old_pages <= MMAP_BASE + MMAP_SLAB;
            if new_pages <= old_pages {
                if in_slab && new_pages < old_pages && mmdbg() & 4 != 0 {
                    mm_event(3, old + new_pages, old + old_pages);
                    release_pages(root, old + new_pages, old + old_pages);
                    with_task(|t| add_hole(t, old + new_pages, old + old_pages));
                }
                return old;
            }
            // In place: the range is the topmost thing in the slab.
            if in_slab && old + old_pages == mmap_cur
                && old + new_pages <= MMAP_BASE + MMAP_SLAB
            {
                let mut page = old + old_pages;
                while page < old + new_pages {
                    let Some(pa) = akuma_pmm::alloc_page_zeroed() else {
                        return e(ring3::err::ENOMEM);
                    };
                    if !paging::map_page_in(root, page as usize, pa as u64, PteProt::USER_RW, MemAttr::WriteBack) {
                        return e(ring3::err::ENOMEM);
                    }
                    page += 4096;
                }
                with_task(|t| t.mmap_cur = old + new_pages);
                return old;
            }
            let Some(base) = with_task(|t| mmap_alloc(t, new_pages)) else {
                return e(ring3::err::ENOMEM);
            };
            let mut page = base & !0xFFF;
            while page < base + new_pages {
                let Some(pa) = akuma_pmm::alloc_page_zeroed() else {
                    return e(ring3::err::ENOMEM);
                };
                if !paging::map_page_in(root, page as usize, pa as u64, PteProt::USER_RW, MemAttr::WriteBack) {
                    return e(ring3::err::ENOMEM);
                }
                page += 4096;
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
            if in_slab && mmdbg() & 4 != 0 {
                mm_event(4, old, old + old_pages);
                release_pages(root, old, old + old_pages);
                with_task(|t| add_hole(t, old, old + old_pages));
            }
            base
        }
        // Akuma private: libakuma's sshd/sh ask for uptime; the clock arm's
        // elapsed-time source, in microseconds.
        ring3::nr::UPTIME => net::uptime_us(),
        // CLONE/FORK/VFORK + WAIT4 (stage 8, process-model M2): the blocking
        // fork and the real reap — the arms busybox ash needs to run any
        // non-final command of a list. child.rs's M2 scaffold.
        ring3::nr::CLONE | 57 | 58 => child::fork_arm(fr),
        ring3::nr::WAIT4 => child::wait4_arm(fr.arg1(), fr.arg2(), fr.arg3(), fr.arg4()),
        ring3::nr::GETTIMEOFDAY => {
            // Same clock as CLOCK_GETTIME (stage 8, M1): with an `epoch=`
            // anchor in the offset store, tv is wall time; without one
            // utc_time_us answers None and today's all-zeros answer stands.
            // Struct timeval is two u64s, `tz` is optional and never filled.
            let (tv, tz) = (fr.arg1(), fr.arg2());
            if let Some(us) = akuma_primitives::clock::utc_time_us(net::uptime_us()) {
                if !user_ok(tv, 16) {
                    return e(ring3::err::EFAULT);
                }
                // Trim step 2: `Timeval` is the crate's shape (tv_usec, not
                // tv_nsec — same clock, different second word).
                let tvbuf = akuma_syscalls_linux::time::Timeval::from_bits(us / 1_000_000, us % 1_000_000);
                if !copy_to_user(tv, struct_bytes(&tvbuf)) {
                    return e(ring3::err::EFAULT);
                }
            } else if !user_ok(tv, 16) || !copy_to_user(tv, &[0u8; 16]) {
                return e(ring3::err::EFAULT);
            }
            if tz != 0 && !user_ok(tz, 8) {
                return e(ring3::err::EFAULT);
            }
            0
        }
        ring3::nr::EXECVE => {
            // Slice 3: a REAL fork child (its own threading slot, published
            // by `fork_process`) execs the way Linux does — its image is
            // replaced, its fds survive, its thread keeps running. The M2
            // nested-crossing arm below stays for the shell (the SPAWN
            // crossing's tail-exec handoff).
            if child::is_real_task() {
                return child::exec_real_task(fr);
            }
            e(ring3::err::ENOSYS)
        }
        // One CPU, no limits: what the toolchain's thread-count and stack
        // probes read. RLIMIT_STACK reports the 8 MiB the stack may grow to.
        akuma_syscalls_abi::x86_64::PRLIMIT64 | 97 => {
            let (res, old) = if fr.rax == 97 { (fr.arg1(), fr.arg2()) } else { (fr.arg2(), fr.arg4()) };
            if old != 0 {
                let cur = if res == 3 { STACK_MAX } else { u64::MAX };
                let mut b = [0u8; 16];
                b[..8].copy_from_slice(&cur.to_le_bytes());
                b[8..].copy_from_slice(&u64::MAX.to_le_bytes());
                if !copy_to_user(old, &b) {
                    return e(ring3::err::EFAULT);
                }
            }
            0
        }
        akuma_syscalls_abi::x86_64::TKILL => tkill_arm(fr.arg1(), fr.arg2()),
        akuma_syscalls_abi::x86_64::TGKILL => tkill_arm(fr.arg2(), fr.arg3()),
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
// Stage 3: the program list, and the sequential kernel-reaped tasks.
// ---------------------------------------------------------------------------

/// Programs the boot will run, one after another, each to `exit`. The gate
/// needs two (busybox echo, busybox ls); the list is the general shape and
/// the command line fills it (`init=`/`initargs=`, then `then=`/`thenargs=`
/// per follow-up). Fixed slots, no heap: the dispatcher's reaper reads these
/// after the task that booted them is gone.
const MAX_PROGRAMS: usize = 4;
/// argv entries per program beyond `argv[0]`, and their byte budget each.
/// The command line is 512 bytes total; 8×95 cannot overflow it.
const MAX_ARGV: usize = 16;
const ARG_LEN: usize = 96;
/// Environment entries per program (`initenv=`/`thenenv=`, comma-separated,
/// `KEY=value` with no comma or space in the value).
const MAX_ENV: usize = 8;

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
        Self {
            path: [0; ARG_LEN],
            args: [[0; ARG_LEN]; MAX_ARGV],
            n_args: 0,
            env: [[0; ARG_LEN]; MAX_ENV],
            n_env: 0,
        }
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

/// The boot program list, how many entries it holds, which one runs now, and
/// each one's exit code. Written by `parse_programs` before ring 3 exists, then
/// by the reaper; read in whole copies so no borrow outlives a call.
struct Programs {
    list: [Prog; MAX_PROGRAMS],
    count: usize,
    current: usize,
    exit_codes: [u64; MAX_PROGRAMS],
}

static PROGRAMS: akuma_primitives::cell::BorrowCell<Programs> =
    akuma_primitives::cell::BorrowCell::new(Programs {
        list: [Prog::new(); MAX_PROGRAMS],
        count: 0,
        current: 0,
        exit_codes: [0; MAX_PROGRAMS],
    });

/// Fill the program list from the boot command line: `init=`/`initargs=`
/// name the first program (the stage-1/2 token pair, unchanged), each
/// `then=`/`thenargs=` pair appends one. `initargs`/`thenargs` are
/// comma-separated (the stage-1 busybox line's convention,
/// `initargs=sh,/etc/guest.sh`).
fn parse_programs(cmdline: &str) {
    let mut idx = 0usize;
    for token in cmdline.split_ascii_whitespace() {
        let mut set_env = None;
        let (set_path, set_args) = if let Some(v) = token.strip_prefix("init=") {
            (Some(v), None)
        } else if let Some(v) = token.strip_prefix("initargs=") {
            (None, Some(v))
        } else if let Some(v) = token.strip_prefix("initenv=") {
            set_env = Some(v);
            (None, None)
        } else if let Some(v) = token.strip_prefix("then=") {
            idx += 1;
            (Some(v), None)
        } else if let Some(v) = token.strip_prefix("thenargs=") {
            (None, Some(v))
        } else if let Some(v) = token.strip_prefix("thenenv=") {
            set_env = Some(v);
            (None, None)
        } else {
            (None, None)
        };
        if idx >= MAX_PROGRAMS {
            break;
        }
        PROGRAMS.with_mut(|t| {
        let p = &mut t.list[idx];
        if let Some(v) = set_path {
            store_slot(&mut p.path, v);
        }
        if let Some(v) = set_args {
            for a in v.split(',').take(MAX_ARGV) {
                if p.n_args < MAX_ARGV {
                    store_slot(&mut p.args[p.n_args], a);
                    p.n_args += 1;
                }
            }
        }
        if let Some(v) = set_env {
            for a in v.split(',').take(MAX_ENV) {
                if p.n_env < MAX_ENV {
                    store_slot(&mut p.env[p.n_env], a);
                    p.n_env += 1;
                }
            }
        }
        });
    }
    PROGRAMS.with_mut(|t| {
        t.count = idx + 1;
        t.current = 0;
    });
}

fn store_slot(dst: &mut [u8], s: &str) {
    let b = s.as_bytes();
    let n = b.len().min(dst.len() - 1);
    dst[..n].copy_from_slice(&b[..n]);
    dst[n] = 0;
}

/// Spawn program `i` from the list, printing the spawn-evidence lines the
/// stage-1 gate was pinned on (they are per-program now: two tasks, two spawn
/// lines). Never returns.
fn run_program(i: usize) -> ! {
    // `current` is read by the dispatcher's reaper, and this write precedes the
    // first crossing of the program it names. The entry is copied out so no
    // borrow of the table outlives the call.
    let prog = PROGRAMS.with_mut(|t| {
        t.current = i;
        t.list[i]
    });
    let (path, n_args) = (prog.path_str(), prog.n_args);
    console::puts("Taoxichuan: init spawn -- init=");
    console::puts(path);
    if n_args > 0 {
        console::puts(" argv=[");
        let mut args = [""; MAX_ARGV];
        for (k, slot) in args.iter_mut().enumerate() {
            if k < n_args {
                *slot = prog.arg_str(k);
            }
        }
        for (k, a) in args.iter().enumerate() {
            if k >= n_args {
                break;
            }
            if k > 0 {
                console::puts(",");
            }
            console::puts(a);
        }
        console::puts("]");
    }
    console::puts("\n");

    let fs = root_fs();
    if !fs.exists(path) {
        console::puts("  init: NOT on the mounted root\n");
        halt();
    }
    let image = fs.read_file(path).unwrap_or_default();
    let elf = image.len() >= 4 && image[..4] == [0x7f, b'E', b'L', b'F'];
    console::puts("  init: ");
    console::put_dec(image.len() as u64);
    console::puts(" bytes on the ext2 root, ELF magic ");
    console::puts(if elf { "ok" } else { "MISSING" });
    console::puts("\n");
    console::puts("Taoxichuan: stage 1 gate reached -- init spawn (userspace is stage 2)\n");

    spawn(&prog, &image)
}

/// The dispatcher's reaper, in place of stage 2's `halt()`: report the exit,
/// then either spawn the next program or close the boot out. Runs on the
/// syscall stack inside the dying task's dispatcher frame; the stack the
/// stub parked is abandoned with the task (the stub restarts at its top on
/// the next task's first syscall).
pub(crate) fn task_done(code: u64) -> ! {
    let (i, n, prog) = PROGRAMS.with_mut(|t| {
        t.exit_codes[t.current] = code;
        (t.current, t.count, t.list[t.current])
    });
    let path = prog.path_str();
    // The stage-2 gate's own evidence lines, verbatim, on the boot that
    // re-checks that gate; every other program gets the generic line.
    if path == "/bin/hello" {
        console::puts("Taoxichuan: hello exit code ");
        console::put_dec(code);
        console::puts("\n");
        if code == 0 {
            console::puts("Taoxichuan: stage 2 gate reached -- ring-3 ran, exited clean, output above\n");
        }
    } else {
        console::puts("Taoxichuan: exit code ");
        console::put_dec(code);
        console::puts(" from ");
        console::puts(path);
        console::puts("\n");
    }
    if i + 1 < n {
        run_program(i + 1);
    }
    let all_zero = PROGRAMS.with(|t| t.exit_codes[..n].iter().all(|&c| c == 0));
    if n >= 2 && all_zero {
        // The stage-3 gate's own line: two kernel-reaped tasks, both clean.
        console::puts("Taoxichuan: stage 3 gate reached -- sequential tasks ran clean, outputs above\n");
    }
    console::puts("Taoxichuan: all programs done\n");
    halt();
}
/// Load an ELF image into the CURRENT task slot: fresh page root (the
/// kernel's upper half shared by PML4 copy), every `PT_LOAD` placed eagerly
/// at its link address, a Linux initial stack with argc/argv/envp/auxv on
/// it, heap slabs registered, the fd table reset. Returns where to cross:
/// the root to load into CR3, the entry, the initial `rsp`.
///
/// Shared by the boot spawn (`spawn`) and the SPAWN syscall's child
/// (`child.rs`) — one loader, one placement story. Runs with the kernel
/// half live (the physmap does all the copying); the caller chooses the
/// CR3 moment.
/// M2 (H3-f), copy-on-write since 2026-10-30: the fork child's address space.
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
    clone_user_half_into(src, dst)
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
        // against a fake tree; this loop performs them. A writable `MAP_SHARED` file span is copied (`SharedSpans::Copy`).
        let plan = X86_64.plan_clone(depth, entry, depth == 0 && in_shared_span(va), SharedSpans::Copy);
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

/// Is `va` inside a writable `MAP_SHARED` file mapping of the running task?
fn in_shared_span(va: u64) -> bool {
    with_task(|t| t.shared[..t.n_shared].iter().any(|&(base, len, ..)| va >= base && va < base + len))
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

fn load_task(argv: &[&str], env: &[&str], image: &[u8]) -> Option<TaskImage> {
    let img = ring3::Image::parse(image).ok()?;
    with_task(|t| {
        t.exit = None;
        t.mmap_cur = MMAP_BASE;
        t.holes.clear();
        t.n_shared = 0;
        // A fresh address space, a fresh region list: `user_ok` checks against
        // exactly what THIS image mapped. Without the reset the 16 region slots
        // fill across spawns and an exec would panic on the append.
        t.n_regions = 0;
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

    // Place every PT_LOAD at its link address, page by page: fresh zeroed
    // frame, map, copy the file-backed slice through the physmap. The zeroed
    // tail (`memsz > filesz` — `.bss`) needs nothing more.
    let mut placed = 0u64;
    for seg in img.segments() {
        let pte = match seg.prot {
            ring3::Prot::Ro => PteProt::USER_RO,
            ring3::Prot::Rx => PteProt::USER_RX,
            ring3::Prot::Rw => PteProt::USER_RW,
        };
        let first = seg.vaddr & !0xFFF;
        let last = (seg.vaddr + seg.memsz + 0xFFF) & !0xFFF;
        let file = img.segment_bytes(seg);
        let mut pva = first;
        while pva < last {
            let frame = phys::Frame::alloc_zeroed()?;
            if !paging::map_page_in(root, pva as usize, frame.pa(), pte, MemAttr::WriteBack) {
                return None;
            }
            // The page's overlap with the segment's file-backed range.
            let from = pva.max(seg.vaddr);
            let to = (pva + 4096).min(seg.vaddr + seg.filesz);
            if from < to {
                let src = &file[(from - seg.vaddr) as usize..(to - seg.vaddr) as usize];
                // `from - pva` stays inside the frame because `from >= pva` and
                // `to <= pva + 4096` (`write_bytes` checks it anyway).
                frame.write_bytes((from - pva) as usize, src);
            }
            placed += 1;
            pva += 4096;
        }
        add_region(first, last);
    }

    // M5: a `PT_INTERP` names the dynamic linker. The *decision* — including
    // the one-NUL-byte static-PIE case — is the shared `akuma-elf-exec` rule
    // (amd64's loader rides the same); the *mapping* is this kernel's page
    // loop again. Placement: `INTERP_BASE` = 0x3000_0000, the base akuma-elf
    // uses — above a static-PIE program at `PIE_BASE` (an image would have to
    // be 512 MiB to reach it) and far below `MMAP_BASE`, so image and linker
    // cannot meet. A hit here changes three auxv/cross facts at once: the
    // entry ring 3 crosses is the linker's, `AT_BASE` is where it was placed,
    // and `AT_ENTRY` stays the program's (already in the table below).
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
            let mut segs =
                [akuma_elf_exec::LoadSeg { vaddr: 0, offset: 0, filesz: 0, memsz: 0, flags: 0 }; 8];
            let Ok(n) =
                akuma_elf_exec::load_segments(&ldso, akuma_elf_exec::INTERP_BASE, &mut segs)
            else {
                console::puts("  exec: interpreter refused by the exec core\n");
                return None;
            };
            let mut hi = akuma_elf_exec::INTERP_BASE;
            for seg in &segs[..n] {
                let first = seg.vaddr & !0xFFF;
                let last = (seg.vaddr + seg.memsz + 0xFFF) & !0xFFF;
                let mut pva = first;
                while pva < last {
                    let frame = phys::Frame::alloc_zeroed()?;
                    let pte = if seg.flags & akuma_elf_exec::PF_X != 0 {
                        PteProt::USER_RX
                    } else if seg.flags & akuma_elf_exec::PF_W != 0 {
                        PteProt::USER_RW
                    } else {
                        PteProt::USER_RO
                    };
                    if !paging::map_page_in(root, pva as usize, frame.pa(), pte, MemAttr::WriteBack) {
                        return None;
                    }
                    // The page's overlap with the segment's file-backed range.
                    let from = pva.max(seg.vaddr);
                    let to = (pva + 4096).min(seg.vaddr + seg.filesz);
                    if from < to {
                        // `seg.offset` is `p_offset`: the file slice for
                        // vaddr `v - p_vaddr` starts at
                        // `p_offset + (v - p_vaddr)`, NOT at `v - p_vaddr`.
                        // The whole-image Vec is indexed from the file's
                        // start; the program loop above does not need this
                        // because `img.segment_bytes(seg)` re-bases the slice
                        // at the segment's own `p_offset`.
                        let off = seg.offset + (from - seg.vaddr);
                        let end = seg.offset + (to - seg.vaddr);
                        let bytes = &ldso[off as usize..end as usize];
                        frame.write_bytes((from - pva) as usize, bytes);
                    }
                    if last > hi {
                        hi = last;
                    }
                    pva += 4096;
                }
            }
            add_region(akuma_elf_exec::INTERP_BASE, hi);
            let Ok(ientry) = akuma_elf_exec::interp_entry(&ldso, akuma_elf_exec::INTERP_BASE) else {
                console::puts("  exec: interpreter has no entry\n");
                return None;
            };
            (ientry, akuma_elf_exec::INTERP_BASE)
        }
        // Static or static-PIE: exactly the answer this loader always gave.
        Ok(None) => (img.entry(), 0),
        // A malformed phdr table the shared core refuses — the same refusal
        // akuma-elf's parser would make. Not reachable for anything
        // `ring3::Image::parse` accepted with a sane `e_phentsize`.
        Err(_) => return None,
    };

    // The stack, then the initial block on it. The block is < half a page and
    // ends at `USER_STACK_TOP`, so it lands entirely in the top page.
    let stack_base = USER_STACK_TOP - USER_STACK_PAGES * 4096;
    let mut stack_frames = [None::<phys::Frame>; USER_STACK_PAGES as usize];
    let mut pva = stack_base;
    while pva < USER_STACK_TOP {
        let frame = phys::Frame::alloc_zeroed()?;
        stack_frames[((pva - stack_base) / 4096) as usize] = Some(frame);
        if !paging::map_page_in(root, pva as usize, frame.pa(), PteProt::USER_RW, MemAttr::WriteBack) {
            return None;
        }
        pva += 4096;
    }
    add_region(stack_base, USER_STACK_TOP);

    // argc/argv/envp/auxv. `AT_RANDOM`/`AT_EXECFN` values are the builder's
    // to fill.
    const AUX_N: usize = 14;
    let mut aux = [(0u64, 0u64); AUX_N];
    aux[0] = (ring3::AT_BASE, at_base); // the interpreter's base, 0 if none
    aux[1] = (ring3::AT_PHDR, img.phdr_va());
    aux[2] = (ring3::AT_PHENT, img.phent());
    aux[3] = (ring3::AT_PHNUM, img.phnum());
    aux[4] = (ring3::AT_PAGESZ, 4096);
    aux[5] = (ring3::AT_ENTRY, img.entry());
    aux[6] = (ring3::AT_UID, 0);
    aux[7] = (ring3::AT_EUID, 0);
    aux[8] = (ring3::AT_GID, 0);
    aux[9] = (ring3::AT_EGID, 0);
    aux[10] = (ring3::AT_SECURE, 0);
    aux[11] = (ring3::AT_HWCAP, 0);
    aux[12] = (ring3::AT_CLKTCK, 100);
    aux[13] = (ring3::AT_RANDOM, 0);
    // `..n_args`, not `..=n_args`: the array is exactly argv-sized and the
    // slice must be too. The old builder pre-filled every slot with argv[0]
    // and then sliced one slot PAST the copied prefix (`..=`), so the new
    // image's argv ended with a duplicate of argv[0] — apk 3's `add` then
    // parsed that duplicate as a package name ("unable to select packages:
    // apk.static (no such package)") while `--version` (G2) and `sh -c`
    // (G1, the dup landed as $0) never noticed.
    // No fixed argv/env caps here: the block below is sized in pages. rustc's
    // linker line is ~50 arguments and `collect2` hands `ld` all of them (the
    // old 16-entry copy truncated it: `ld: no input files`).
    let full_argv: &[&str] = argv;
    let n_args = argv.len();
    // One full page, not the old 512: `build_stack` lays the block out from
    // `buf.len()` backwards and `apk`'s long argv/env (`--allow-untrusted add
    // /pkgs/...` plus sshd's environment) overran 512 by exactly the 8 bytes
    // of one pointer word — the underflowed slice index panicked the kernel
    // on the first real install. The block still ends at `USER_STACK_TOP` and
    // now fills the top page exactly, which the copy below already handles.
    const BLOCK_PAGES: usize = ARG_BLOCK_PAGES;
    let mut block = alloc::vec![0u8; BLOCK_PAGES * 4096];
    let rsp = ring3::build_stack(&mut block, USER_STACK_TOP, &full_argv[..n_args], env, &aux);

    // The block ends at `USER_STACK_TOP`, so it fills the top `BLOCK_PAGES` pages.
    for k in 0..BLOCK_PAGES {
        if let Some(frame) = stack_frames[USER_STACK_PAGES as usize - BLOCK_PAGES + k] {
            frame.write_bytes(0, &block[k * 4096..(k + 1) * 4096]);
        }
    }

    // The heap slabs: `brk` above the image, `mmap` in its own window. Both
    // registered now, so a pointer check is one scan.
    let brk0 = (img.image_end() + 0xFFF) & !0xFFF;
    with_task(|t| {
        t.brk_base = brk0;
        t.brk_cur = brk0;
        t.brk_end = brk0 + BRK_SLAB;
    });
    add_region(brk0, brk0 + BRK_SLAB);
    add_region(MMAP_BASE, MMAP_BASE + MMAP_SLAB);

    let _ = placed;
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
    let Some(ti) = load_task(&argv[..=n_args], &envv[..n_env], image) else {
        akuma_primitives::safe_print!(160, "  init: task load failed\n");
        halt();
    };

    // Arm the syscall path, then cross. `mov cr3` is the write the ctlreg
    // crate deliberately does not own (reads safe, writes unsafe at the call
    // site): it loads the task's root, flushing the non-global TLB — the
    // kernel half survives through the shared PML4 entries.
    ring3::enable_syscalls(dispatch);
    console::puts("Taoxichuan: entering ring 3 -- ");
    console::puts(path);
    console::puts("\n");
    // Init is pid 1 on a real pool thread (a tid >= 1, a ring row, a registered `Process`), the way amd64's `run_init`
    // starts it, not on the boot stack: that is what lets it `fork`/`clone` and be preempted, so `sshd` runs one forked process
    // per session (`docs/reference/switch-to-crates.md` S1). The boot thread parks in `start_init_thread` and reports init's exit.
    child::start_init_thread(path, &ti);
    // SAFETY: one documented control-register write; interrupts are masked,
    // one core, and the root was built entirely before this line.
    unsafe {
        core::arch::asm!(
            "mov cr3, {r}",
            r = in(reg) ti.root_pa,
            options(nostack, preserves_flags)
        );
    }
    let ef = ring3::EntryFrame {
        r15: 0, r14: 0, r13: 0, r12: 0, rbp: 0, rbx: 0, r10: 0, r8: 0, r9: 0,
        rdx: 0, rsi: 0, rdi: 0, rax: 0,
        rip: ti.entry,
        rflags: 0x202, // IF set, reserved bit 1 set
        rsp: ti.rsp,
    };
    // SAFETY: the address space maps the kernel half (shared PML4 entries)
    // and the task's pages; `ef` names image-placed code on the built stack.
    let _ = smp::bkl_leave_all();
    unsafe { ring3::enter_user(&ef) }
}

// ---------------------------------------------------------------------------
// Entry.
// ---------------------------------------------------------------------------

/// Long-mode entry, called from `boot.s` (in `akuma-boot-amd64`) with the
/// `hvm_start_info` pointer in `%rdi`.
#[unsafe(no_mangle)]
pub extern "C" fn kmain(hvm_start_info: u64) -> ! {
    // The console first: everything after this reports through it.
    console::init();
    // And the shared crates' sink, before the first one of them can print:
    // the boot kit's memory bring-up reports through `safe_print!`.
    akuma_primitives::console::set_print_hook(console::puts);

    console::puts("\nTaoxichuan/amd64 -- long mode reached\n");
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
        console::puts("\nTaoxichuan/amd64 -- memory bring-up FAILED\n");
        halt();
    }

    smp::record_kernel_root();
    // The root disk, and the ext2 root on it.
    // session pb: after PMM/heap are up - map_device allocates page-table frames
    lapic_init();
    smp::record_bsp_lapic_id(akuma_lapic_amd64::apic_id());
    // Session pc, slice 1: the exec/threading runtime twin. Registered here,
    // not earlier: the LAPIC tick must exist before it can be anyone's clock
    // (exec_twin's uptime_us is ticks × 10 ms), and PMM/heap must be up
    // before the first allocation behind registration. Nothing shared runs
    // before the first spawn, so this is also "before any syscall can reach
    // the tables" (amd64's ordering rule).
    exec_twin::init();
    if !blk_init(&machine.virtio) {
        console::puts("  [FATAL] no virtio block device -- the pinned root (ext2 on virtio-blk) cannot mount\n");
        halt();
    }
    let Ok(fs) = akuma_ext2::Ext2Filesystem::new(Vda, no_clock) else {
        console::puts("  fs:   /dev/vda holds no readable ext2 image\n");
        halt();
    };
    // One instance, two names: `akuma-vfs-glue`'s mount table holds it at `/`
    // (what `akuma-syscalls-glue`'s arms resolve through), and `ROOT_FS` holds
    // a `'static` borrow of the same `Arc` for the arms still served here. Two
    // instances would be two block caches and two locks over one disk.
    let fs = alloc::sync::Arc::new(fs);
    mount_vfs_glue(fs.clone());
    console::puts("  fs:   ext2 root mounted on /dev/vda\n");
    // The mount lives for the rest of the boot: leak the `Arc` to a `'static` borrow.
    ROOT_FS.set(Some(&**alloc::boxed::Box::leak(alloc::boxed::Box::new(fs))));

    // Stage 4: the network path — calibrate the TSC against the PIT, pull up
    // smoltcp on virtio-net, wait (bounded, pumping the NIC) for DHCP. A
    // guest with no NIC or no lease still boots; sshd will bind and simply
    // never answer, which is a legible failure, not a hang.
    let _ = net::net_init();

    // SMP (stage S-b): `smp=N` starts N-1 application processors; absent or 1
    // keeps the single-core kernel byte-for-byte (the default until S-d is green).
    let smp_cap = cmdline_token(hvm_start_info, "smp=", &mut [0u8; 8])
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(1);
    if smp_cap > 1 {
        smp::start_secondaries(smp_cap);
        akuma_primitives::safe_print!(64, "  smp: {} of {} requested\n", smp::online_cpus(), smp_cap);
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
        console::puts("Taoxichuan: stage 1 gate NOT reached (no init)\n");
        halt();
    }
    // Stage 3: the whole program list comes off the command line here
    // (`init=`/`initargs=` for the first program, `then=`/`thenargs=` for
    // each follow-up), so the dispatcher's reaper never parses anything
    // after a task is live.
    parse_programs(cmdline_str);

    // Stage 2: the spawn is real now. Load, cross, and the task's own exit
    // (or its fault) is the next line on this console; stage 3 chains the
    // next program from there.
    run_program(0);
}

/// The multiboot2 entry — called by `boot.s` when GRUB's magic says the info
/// block in `%rdi` is a multiboot2 one. This composition is a VMM guest
/// (Firecracker-style virtio-mmio assumptions are pinned in the experiment
/// brief); rather than growing a second boot protocol, it says so and stops.
/// The symbol must exist: `boot.s` names it unconditionally.
#[unsafe(no_mangle)]
pub extern "C" fn kmain_mb2(_info_phys: u64) -> ! {
    console::init();
    console::puts("\nTaoxichuan: multiboot2 boot -- not composed in this kernel (PVH/VMM only)\n");
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
