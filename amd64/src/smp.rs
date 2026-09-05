//! SMP: per-CPU state, the Big Kernel Lock, and application-processor bring-up.
//!
//! Stage U. Until this existed the amd64 target was one vCPU and every module
//! said so in its `SAFETY` comments — "single core" — as the whole justification
//! for a `static mut` table reached through raw pointers. This is what replaces
//! that justification. It is the amd64 counterpart of the aarch64 kernel's
//! `smp-shared` feature: **one shared kernel across all cores, serialised by one
//! lock** (`docs/reference/subsystems/smp-shared.md`), chosen over fine-grained
//! locking for the same reason it was chosen there — it turns "every `static
//! mut` in the tree is a data race" into "kernel code runs on one core at a
//! time", and lets user code run in parallel from day one.
//!
//! # The three things this module owns
//!
//! **Per-CPU state, reached through `%gs`.** A [`PerCpu`] block per core holds
//! the running task, its `UserCtx` pointer, the BKL depth and the reschedule
//! flag — everything that was a bare `static` while there was one core. Kernel
//! code finds its own block through `IA32_GS_BASE`; user code never sees it
//! because the `syscall`/`sysret` path brackets itself in `swapgs`, and the
//! interrupt stubs `swapgs` when — and only when — the interrupted `CS` was ring
//! 3. The invariant that makes `gs:[..]` safe to read anywhere in kernel Rust is
//! therefore: **in ring 0, `GS_BASE` is this core's [`PerCpu`]; in ring 3 it is
//! whatever the program set (`arch_prctl(ARCH_SET_GS)`, kept in
//! `IA32_KERNEL_GS_BASE` while the kernel runs)**. Both halves are per-core
//! registers, so a task that migrates cores finds the right block on arrival
//! without anyone doing anything.
//!
//! **The Big Kernel Lock.** A fair ticket spinlock with a per-core recursion
//! depth. Held by every core while it executes kernel code; released on the way
//! back to ring 3 and in the idle loop's `hlt` window. A context switch
//! *transfers* the hold: the outgoing task's depth is saved in its `Task`, the
//! incoming task's restored, and the lock itself never changes hands — the
//! core keeps it, and the next task on that core is the one that eventually
//! lets go. That is what makes every "single core" `SAFETY` comment in
//! `sched.rs`, `usermode.rs`, `fd.rs` and friends true again under a different
//! reading: single *kernel* core.
//!
//! A kernel path that waits for another core's progress must not spin holding
//! it — that is a livelock, not a slow path — so every `sched::yield_now` opens
//! a [`bkl_drop_window`] before it does anything else. Every blocking syscall in
//! this kernel waits by yielding, which is what makes that the single point
//! that needed it; `sched::try_switch` records the livelock that taught it.
//!
//! **Application-processor bring-up.** The MADT names every enabled LAPIC; the
//! BSP sends each one INIT then STARTUP through its own LAPIC's ICR, and the AP
//! starts in 16-bit real mode at the 4 KiB page the STARTUP vector names. The
//! trampoline there (`boot.s`, copied to physical `0x8000` at run time because a
//! STARTUP vector cannot name anything above 1 MiB) walks it up to long mode on
//! a page-table root the BSP built for exactly this — the kernel's own PML4 plus
//! an identity map of the first gigabyte, so the low code can turn paging on
//! and the high code is already there — and jumps to [`ap_entry64`], which
//! throws the identity map away and finishes as a Rust function.
//!
//! # What is deliberately missing
//!
//! - **No TLB shootdown.** `invlpg` is core-local. It is complete here because
//!   an address space is only ever active on the core running its one task
//!   (processes are single-threaded — no `CLONE_VM`), and every switch between
//!   different roots writes `CR3`, which flushes. Kernel-half mappings change
//!   only at boot, before any AP runs. Both are properties of today's kernel,
//!   not guarantees: the first `clone(CLONE_VM)` needs an IPI.
//! - **No wake IPI.** A core in `hlt` learns of new work at its next timer tick.
//! - **No kernel-mode preemption.** The tick preempts ring 3 and the idle loop;
//!   a kernel task yields when it chooses (see `sched::preempt_if_needed` for
//!   why the old behaviour was a latent deadlock, not a feature).

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use akuma_ryzen_amd64::acpi::Madt;
use akuma_ryzen_amd64::MachineDescription;
use akuma_selftest::Suite;

use crate::phys::phys_ptr;
use crate::usermode::UserCtx;
use crate::{gdt, idt, lapic, paging, sched, serial, uaccess, usermode};

/// Most cores this kernel will run. The MADT parser reports up to 32; the
/// per-CPU table is sized for what a devbox is given rather than that ceiling.
pub const MAX_CPUS: usize = 16;

/// `IA32_GS_BASE`: the `%gs` base the CPU uses right now.
const IA32_GS_BASE: u32 = 0xC000_0101;
/// `IA32_KERNEL_GS_BASE`: the other one — what `swapgs` swaps in.
pub const IA32_KERNEL_GS_BASE: u32 = 0xC000_0102;

/// "No core" — the BKL's owner when free, and a task's pin when unpinned.
pub const NO_CPU: u32 = u32::MAX;

/// One core's private state.
///
/// `repr(C)` because `syscall_entry` and `enter_user_mode` index the first
/// three fields by hand as `gs:[0]`, `gs:[8]` and `gs:[16]`. Reordering them
/// silently changes what that assembly reads; [`OFFSETS_PINNED`] fails the
/// build if the offsets move. Everything is an atomic so the table can be a
/// plain `static`: a block is written by its own core, and by the BSP once
/// before that core is started — never by two cores at once — so `Relaxed`
/// is the right ordering everywhere except the `online` handshake.
#[repr(C, align(64))]
pub struct PerCpu {
    /// Address of this block. Offset 0, `gs:[0]`: how kernel Rust finds its
    /// own block with one load.
    self_ptr: AtomicU64,
    /// The running task's `UserCtx`. Offset 8, `gs:[8]` — what `syscall_entry`
    /// reads before it has a stack.
    current_uctx: AtomicU64,
    /// One word of scratch for the syscall stubs. Offset 16, `gs:[16]`. Was a
    /// global `SYSCALL_SCRATCH`, which two cores in `syscall_entry` at once
    /// would have clobbered.
    scratch: AtomicU64,
    /// This core's index into [`PERCPU`]; 0 is the BSP.
    index: AtomicU32,
    /// This core's LAPIC id, as the MADT (and `lapic::apic_id`) report it.
    lapic_id: AtomicU32,
    /// The `sched` task slot running on this core.
    current_task: AtomicUsize,
    /// The task slot that idles this core when nothing else is runnable. 0 (the
    /// boot task) on the BSP; a dedicated slot on every AP.
    idle_task: AtomicUsize,
    /// How many times this core has entered the BKL without leaving.
    bkl_depth: AtomicU32,
    /// Set by this core's timer tick, consumed by its next `yield_now`.
    need_resched: AtomicBool,
    /// Set by the AP once it can take interrupts; the BSP waits on it.
    online: AtomicBool,
    /// Timer ticks this core has taken.
    ticks: AtomicU64,
    /// Top of the stack the core was started on (its idle task's), so
    /// `ap_entry64` can hand it to the TSS without deriving it from `rsp`.
    stack_top: AtomicU64,
}

impl PerCpu {
    const fn new() -> Self {
        Self {
            self_ptr: AtomicU64::new(0),
            current_uctx: AtomicU64::new(0),
            scratch: AtomicU64::new(0),
            index: AtomicU32::new(0),
            lapic_id: AtomicU32::new(0),
            current_task: AtomicUsize::new(0),
            idle_task: AtomicUsize::new(0),
            bkl_depth: AtomicU32::new(0),
            need_resched: AtomicBool::new(false),
            online: AtomicBool::new(false),
            ticks: AtomicU64::new(0),
            stack_top: AtomicU64::new(0),
        }
    }
}

/// The assembly's view of [`PerCpu`], pinned at compile time.
const OFFSETS_PINNED: () = {
    assert!(core::mem::offset_of!(PerCpu, self_ptr) == 0);
    assert!(core::mem::offset_of!(PerCpu, current_uctx) == 8);
    assert!(core::mem::offset_of!(PerCpu, scratch) == 16);
};

static PERCPU: [PerCpu; MAX_CPUS] = [const { PerCpu::new() }; MAX_CPUS];

/// How many cores are running the kernel — the BSP plus every AP that came
/// online. 1 until [`start_secondaries`] runs.
static ONLINE: AtomicUsize = AtomicUsize::new(1);

/// # Safety
/// Writing an MSR reconfigures the CPU. The writes here set the two `%gs`
/// bases and nothing else.
unsafe fn wrmsr(msr: u32, val: u64) {
    // SAFETY: caller's obligation.
    unsafe {
        core::arch::asm!("wrmsr", in("ecx") msr, in("eax") val as u32,
                         in("edx") (val >> 32) as u32,
                         options(nostack, preserves_flags));
    }
}

/// Point this core's `%gs` at `PERCPU[idx]`.
///
/// `IA32_KERNEL_GS_BASE` is set to 0 as well: it holds the *user's* GS base
/// while the kernel runs, and a fresh core has no user yet. The scheduler keeps
/// it current per task from then on (`UserCtx::gs_base`).
fn install_percpu(idx: usize) {
    let block = &PERCPU[idx];
    let addr = core::ptr::from_ref::<PerCpu>(block) as u64;
    block.self_ptr.store(addr, Ordering::Relaxed);
    block.index.store(idx as u32, Ordering::Relaxed);
    // SAFETY: the two `%gs` base MSRs. The block is a `static`, so the address
    // is valid for the life of the kernel, and nothing reads `gs:` before this.
    unsafe {
        wrmsr(IA32_GS_BASE, addr);
        wrmsr(IA32_KERNEL_GS_BASE, 0);
    }
}

/// Has this core's `%gs` been pointed at a [`PerCpu`] yet?
///
/// Reads `IA32_GS_BASE` rather than `gs:[0]`: before `install_percpu` the base
/// is 0, and a `gs:`-relative load through it would be the very fault this
/// exists to avoid. For the one caller (`halt`) that can run at any point in
/// the boot, including before the block exists.
#[must_use]
pub fn percpu_installed() -> bool {
    let (lo, hi): (u32, u32);
    // SAFETY: reading an architectural MSR has no side effect.
    unsafe {
        core::arch::asm!("rdmsr", in("ecx") IA32_GS_BASE, out("eax") lo, out("edx") hi,
                         options(nomem, nostack, preserves_flags));
    }
    (u64::from(hi) << 32) | u64::from(lo) != 0
}

/// This core's block, through `gs:[0]`.
///
/// Valid only in ring 0 with the invariant from the module header in force —
/// which is every line of kernel Rust after [`init_bsp`] / [`ap_entry64`].
#[inline]
fn this_cpu() -> &'static PerCpu {
    let p: u64;
    // SAFETY: `gs:[0]` is `PerCpu::self_ptr`, written by `install_percpu` on
    // this core before any code that can reach here runs; the block it names is
    // a `static`.
    unsafe {
        core::arch::asm!("mov {}, qword ptr gs:[0]", out(reg) p,
                         options(nostack, preserves_flags, readonly));
        &*(p as *const PerCpu)
    }
}

/// This core's index: 0 for the BSP.
#[inline]
#[must_use]
pub fn cpu_index() -> usize {
    this_cpu().index.load(Ordering::Relaxed) as usize
}

/// [`cpu_index`] as the `u32` the networking runtime's `current_core_id` hook
/// wants.
#[must_use]
pub fn cpu_index_u32() -> u32 {
    this_cpu().index.load(Ordering::Relaxed)
}

/// Cores running the kernel right now.
#[must_use]
pub fn online_cpus() -> usize {
    ONLINE.load(Ordering::Acquire)
}

/// Timer ticks taken by core `idx`.
#[must_use]
pub fn ticks_on(idx: usize) -> u64 {
    PERCPU.get(idx).map_or(0, |c| c.ticks.load(Ordering::Relaxed))
}

/// Count one timer tick on this core. Called from the timer vector.
pub fn this_cpu_tick() {
    this_cpu().ticks.fetch_add(1, Ordering::Relaxed);
}

/// The running task's `UserCtx`, or null before the scheduler is initialised.
#[must_use]
pub fn current_uctx() -> *mut UserCtx {
    this_cpu().current_uctx.load(Ordering::Relaxed) as *mut UserCtx
}

/// Repoint this core's `UserCtx` — the scheduler, on every switch.
pub fn set_current_uctx(p: *mut UserCtx) {
    this_cpu().current_uctx.store(p as u64, Ordering::Relaxed);
}

/// The task slot running on this core.
#[must_use]
pub fn current_task() -> usize {
    this_cpu().current_task.load(Ordering::Relaxed)
}

pub fn set_current_task(slot: usize) {
    this_cpu().current_task.store(slot, Ordering::Relaxed);
}

/// The slot that idles this core.
#[must_use]
pub fn idle_task() -> usize {
    this_cpu().idle_task.load(Ordering::Relaxed)
}

pub fn set_need_resched() {
    this_cpu().need_resched.store(true, Ordering::Relaxed);
}

/// Read and clear this core's reschedule request.
pub fn take_need_resched() -> bool {
    this_cpu().need_resched.swap(false, Ordering::Relaxed)
}

#[must_use]
pub fn need_resched() -> bool {
    this_cpu().need_resched.load(Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// The Big Kernel Lock
// ---------------------------------------------------------------------------

/// A fair ticket lock. Fairness is not decoration: with a plain test-and-set,
/// the core that just released — its cache line hot — wins the next acquire
/// almost every time, and a peer spinning in its timer handler can wait for
/// tens of milliseconds behind a busy syscall loop. The aarch64 kernel learned
/// that the hard way (`docs/archive/BKL_VFS_CARVE_OUT.md` §8).
struct Bkl {
    next_ticket: AtomicU32,
    now_serving: AtomicU32,
    /// The core holding it, or [`NO_CPU`]. Written by the holder only, after
    /// acquire and before release; read by the same core to detect recursion
    /// and by anyone for the stuck diagnostic.
    owner: AtomicU32,
}

static BKL: Bkl = Bkl {
    next_ticket: AtomicU32::new(0),
    now_serving: AtomicU32::new(0),
    owner: AtomicU32::new(NO_CPU),
};

/// Spins before the stuck diagnostic prints. Big enough that a legitimate long
/// hold (a whole-file ext2 read through polled virtio) does not trip it; small
/// enough that a real deadlock is named within seconds under TCG.
const STUCK_SPINS: u64 = 1 << 27;

/// Spins a core waits with the lock dropped in [`bkl_drop_window`] before it
/// takes the lock back. Small on purpose: the lock is FIFO, so a peer that was
/// already spinning for it is served the moment it is released and this core
/// queues behind it whatever the gap — the gap only matters for a peer that
/// arrives during it. A yield pays this on every call (`sched::try_switch`), so
/// hundreds of `pause`s here would be a visible tax on every wait loop.
const DROP_WINDOW_SPINS: u32 = 4;

fn pause() {
    core::hint::spin_loop();
}

fn bkl_acquire(me: u32) {
    let ticket = BKL.next_ticket.fetch_add(1, Ordering::AcqRel);
    let mut spins = 0u64;
    let mut reported = false;
    while BKL.now_serving.load(Ordering::Acquire) != ticket {
        pause();
        spins += 1;
        if spins == STUCK_SPINS && !reported {
            reported = true;
            serial::puts("[BKL] stuck: cpu ");
            serial::put_dec(u64::from(me));
            serial::puts(" waiting on owner ");
            serial::put_dec(u64::from(BKL.owner.load(Ordering::Relaxed)));
            serial::puts("\n");
        }
    }
    BKL.owner.store(me, Ordering::Relaxed);
}

fn bkl_release() {
    BKL.owner.store(NO_CPU, Ordering::Relaxed);
    BKL.now_serving.fetch_add(1, Ordering::Release);
}

/// Enter the kernel: take the BKL, or deepen this core's hold on it.
///
/// Recursive per core, not per task — a timer tick that lands while this core
/// already holds the lock (kernel task code with `IF` set) nests rather than
/// deadlocks. The depth travels with the task across a context switch
/// (`sched::yield_now`), so a task always leaves as many times as it entered.
pub fn bkl_enter() {
    let cpu = this_cpu();
    let me = cpu.index.load(Ordering::Relaxed);
    if BKL.owner.load(Ordering::Relaxed) == me {
        cpu.bkl_depth.fetch_add(1, Ordering::Relaxed);
        return;
    }
    bkl_acquire(me);
    cpu.bkl_depth.store(1, Ordering::Relaxed);
}

/// Leave the kernel: undo one [`bkl_enter`]; release at depth zero.
pub fn bkl_leave() {
    let cpu = this_cpu();
    let d = cpu.bkl_depth.load(Ordering::Relaxed);
    debug_assert!(d > 0, "bkl_leave without a matching enter");
    if d <= 1 {
        cpu.bkl_depth.store(0, Ordering::Relaxed);
        bkl_release();
    } else {
        cpu.bkl_depth.store(d - 1, Ordering::Relaxed);
    }
}

/// This core's hold depth. Saved into the outgoing task on a switch.
#[must_use]
pub fn bkl_depth() -> u32 {
    this_cpu().bkl_depth.load(Ordering::Relaxed)
}

/// Install the incoming task's hold depth on a switch. The lock itself stays
/// with the core; only the count of pending `leave`s changes hands.
pub fn set_bkl_depth(d: u32) {
    this_cpu().bkl_depth.store(d, Ordering::Relaxed);
}

/// Give the lock up for good, if this core holds it — for a core that is about
/// to stop executing kernel code forever (`halt`, the bare-metal colour cycle).
///
/// Without this a BSP that finishes its boot holding the lock — a failed
/// self-test verdict ends in `halt()`, which is `cli; hlt` at depth 1 — leaves
/// every AP spinning in its tick handler for a lock that will never be
/// released, printing `[BKL] stuck … owner 0` once each (measured 2026-09-05 in
/// the OVMF rig). Nothing was wrong; the diagnostic was right. A core that
/// stops should let go. Safe to call from a core that never held it.
pub fn bkl_abandon() {
    let cpu = this_cpu();
    if BKL.owner.load(Ordering::Relaxed) == cpu.index.load(Ordering::Relaxed) {
        cpu.bkl_depth.store(0, Ordering::Relaxed);
        bkl_release();
    }
}

/// Let the other cores in: drop the lock completely, spin briefly, take it
/// back at the same depth.
///
/// Called by every `sched::yield_now` — the point every kernel wait loop in
/// this kernel passes through (a pipe read, a socket, a child exit, the boot
/// task's drive loops). Holding the lock through such a loop is not slowness,
/// it is a livelock: the peer's syscall spins for the lock this core is
/// spinning inside. A no-op when the lock is not held.
pub fn bkl_drop_window() {
    let cpu = this_cpu();
    let d = cpu.bkl_depth.load(Ordering::Relaxed);
    if d == 0 {
        return;
    }
    cpu.bkl_depth.store(0, Ordering::Relaxed);
    bkl_release();
    for _ in 0..DROP_WINDOW_SPINS {
        pause();
    }
    bkl_acquire(cpu.index.load(Ordering::Relaxed));
    cpu.bkl_depth.store(d, Ordering::Relaxed);
}

/// Run `f` with the lock released, then take it back at the same depth.
///
/// What ring 3 gets for free, offered to kernel code that wants to behave like
/// it: the SMP self-test's workers spin inside this so two of them can really
/// be executing at once.
pub fn bkl_run_unlocked<R>(f: impl FnOnce() -> R) -> R {
    let cpu = this_cpu();
    let d = cpu.bkl_depth.load(Ordering::Relaxed);
    if d > 0 {
        cpu.bkl_depth.store(0, Ordering::Relaxed);
        bkl_release();
    }
    let r = f();
    if d > 0 {
        bkl_acquire(cpu.index.load(Ordering::Relaxed));
        cpu.bkl_depth.store(d, Ordering::Relaxed);
    }
    r
}

// ---------------------------------------------------------------------------
// Bring-up
// ---------------------------------------------------------------------------

/// Make the BSP core 0: install its per-CPU block and take the BKL.
///
/// Called right after `gdt::init`, before anything reads `gs:` — and before
/// any AP exists, so the acquire cannot contend. From here on the boot task
/// holds the lock at depth 1, which is what every kernel task is born holding.
pub fn init_bsp() {
    let () = OFFSETS_PINNED;
    install_percpu(0);
    PERCPU[0].online.store(true, Ordering::Release);
    bkl_enter();
}

/// Record the BSP's LAPIC id once the LAPIC is mapped.
pub fn set_bsp_lapic_id(id: u32) {
    PERCPU[0].lapic_id.store(id, Ordering::Relaxed);
}

/// Where the trampoline is copied. A STARTUP IPI's vector is the page number of
/// a 4 KiB page below 1 MiB (`0x08` → `0x8000`); this one is clear of every
/// boot structure both VMMs place — QEMU's `hvm_start_info` at `0x1580`,
/// Firecracker's at `0x6000` with its memory map at `0x7000` and command line
/// at `0x20000` — and of the PMM, which is handed the region *containing the
/// kernel* (from 1 MiB) and never the low one.
const AP_TRAMPOLINE_PA: u64 = 0x8000;
const AP_STARTUP_VECTOR: u8 = (AP_TRAMPOLINE_PA >> 12) as u8;

/// The mailbox the trampoline reads: fixed offsets inside the trampoline page,
/// mirrored as `AP_MB_*` in `boot.s`.
const AP_MB_CR3: u64 = AP_TRAMPOLINE_PA + 0xF00;
const AP_MB_STACK: u64 = AP_TRAMPOLINE_PA + 0xF08;
const AP_MB_ENTRY: u64 = AP_TRAMPOLINE_PA + 0xF10;
const AP_MB_INDEX: u64 = AP_TRAMPOLINE_PA + 0xF18;

/// Kernel stack for an AP's idle task, and therefore for every interrupt it
/// takes while idle. Same size as a scheduler task's.
const AP_STACK_SIZE: usize = 32 * 1024;

/// LAPIC timer counts to wait after INIT and after a STARTUP that did not
/// take. The architectural minimums are 10 ms and 200 µs; this is ~16 ms on
/// QEMU's 1 GHz APIC clock at divide-by-16, and only shorter on a part whose
/// clock is faster. Bring-up is serial and this is paid once per core.
const AP_INIT_DELAY: u32 = 1_000_000;

/// Spins to wait for an AP to report in before the STARTUP is repeated.
const AP_ONLINE_BUDGET: u64 = 200_000_000;

unsafe extern "C" {
    static ap_trampoline_start: u8;
    static ap_trampoline_end: u8;
}

/// Is the trampoline page ordinary RAM, and clear of everything the boot
/// loader placed?
///
/// [`AP_TRAMPOLINE_PA`] is a claim about two VMMs' layouts. A firmware boot is
/// a different machine: UEFI's map fragments low memory, and GRUB puts its
/// information block wherever it likes. Copying the trampoline over either
/// would corrupt something the kernel is still reading — so the caller names
/// what it placed (`keep_out`, as `[start, end)` byte ranges) and this refuses
/// unless the page is inside a RAM region and outside all of them. A refusal
/// means "stay single-core", which is a configuration, not a failure.
#[must_use]
pub fn trampoline_page_available(machine: &MachineDescription, keep_out: &[(u64, u64)]) -> bool {
    let (page_start, page_end) = (AP_TRAMPOLINE_PA, AP_TRAMPOLINE_PA + 4096);
    let in_ram = machine
        .regions()
        .iter()
        .any(|r| r.kind == 1 && r.addr <= page_start && r.addr + r.size >= page_end);
    let clear = keep_out
        .iter()
        .all(|&(s, e)| e <= page_start || s >= page_end);
    in_ram && clear
}

/// Present + writable, for the intermediate entries of the AP boot tables.
const PTE_P_RW: u64 = 0x3;
/// ...plus page-size, for a 2 MiB identity leaf.
const PTE_P_RW_PS: u64 = 0x83;

/// The page-table root APs enable paging on: the kernel's PML4 with slot 0
/// pointing at an identity map of the first gigabyte. Three frames.
struct ApBootTables {
    pml4: usize,
    pdpt: usize,
    pd: usize,
}

impl ApBootTables {
    fn build(kernel_root: u64) -> Option<Self> {
        let pml4 = akuma_pmm::alloc_page()?;
        let Some(pdpt) = akuma_pmm::alloc_page() else {
            akuma_pmm::free_page(pml4, 0);
            return None;
        };
        let Some(pd) = akuma_pmm::alloc_page() else {
            akuma_pmm::free_page(pml4, 0);
            akuma_pmm::free_page(pdpt, 0);
            return None;
        };
        // SAFETY: three fresh PMM frames, reached through the physmap; the
        // kernel root is a live PML4 that is likewise inside the physmap.
        unsafe {
            core::ptr::copy_nonoverlapping(
                phys_ptr::<u8>(kernel_root),
                phys_ptr::<u8>(pml4 as u64),
                4096,
            );
            core::ptr::write_bytes(phys_ptr::<u8>(pdpt as u64), 0, 4096);
            let pd_ptr = phys_ptr::<u64>(pd as u64);
            for i in 0..512u64 {
                pd_ptr.add(i as usize).write_volatile((i << 21) | PTE_P_RW_PS);
            }
            phys_ptr::<u64>(pdpt as u64).write_volatile(pd as u64 | PTE_P_RW);
            phys_ptr::<u64>(pml4 as u64).write_volatile(pdpt as u64 | PTE_P_RW);
        }
        Some(Self { pml4, pdpt, pd })
    }

    /// Give the frames back. Safe once every AP has switched to the kernel root,
    /// which each does as its first act in [`ap_entry64`].
    fn free(self) {
        akuma_pmm::free_page(self.pml4, 0);
        akuma_pmm::free_page(self.pdpt, 0);
        akuma_pmm::free_page(self.pd, 0);
    }
}

/// Copy the trampoline to its page. Returns false if it does not fit below the
/// mailbox, which is a build error surfacing at run time.
fn install_trampoline() -> bool {
    // Two linker-visible symbols bracketing a `.rodata` blob in the kernel
    // image; the difference is its length.
    let (src, len) = {
        let s = &raw const ap_trampoline_start;
        let e = &raw const ap_trampoline_end;
        (s, e as usize - s as usize)
    };
    if len > (AP_MB_CR3 - AP_TRAMPOLINE_PA) as usize {
        return false;
    }
    // SAFETY: the destination page is RAM inside the physmap and belongs to no
    // one — see `AP_TRAMPOLINE_PA`.
    unsafe {
        core::ptr::copy_nonoverlapping(src, phys_ptr::<u8>(AP_TRAMPOLINE_PA), len);
    }
    true
}

fn mailbox_write(pa: u64, v: u64) {
    // SAFETY: inside the trampoline page, which `install_trampoline` owns.
    unsafe { phys_ptr::<u64>(pa).write_volatile(v) };
}

/// Start every AP the MADT lists. Returns how many came online.
///
/// Serial, one core at a time through one mailbox: simpler than an atomic
/// stack-claiming protocol, and bring-up is not a hot path. The BSP's timer
/// must be stopped on entry (the INIT delay borrows it) and every AP's timer is
/// running on exit.
pub fn start_secondaries(madt: Option<&Madt>) -> usize {
    let Some(madt) = madt else {
        serial::puts("  smp:  no MADT — single core\n");
        return 0;
    };
    let bsp_id = lapic::apic_id();
    set_bsp_lapic_id(bsp_id);
    let aps = madt.cpus().iter().filter(|&&id| u32::from(id) != bsp_id).count();
    if aps == 0 {
        serial::puts("  smp:  MADT lists one CPU — single core\n");
        return 0;
    }
    if aps > MAX_CPUS - 1 {
        serial::puts("  smp:  [WARN] more CPUs than MAX_CPUS; starting the first ");
        serial::put_dec((MAX_CPUS - 1) as u64);
        serial::puts("\n");
    }

    let kernel_root = sched::kernel_root();
    let Some(tables) = ApBootTables::build(kernel_root) else {
        serial::puts("  smp:  [FAIL] no frames for the AP boot tables\n");
        return 0;
    };
    if !install_trampoline() {
        serial::puts("  smp:  [FAIL] trampoline does not fit its page\n");
        tables.free();
        return 0;
    }

    let mut started = 0usize;
    let mut idx = 0usize;
    for &id in madt.cpus() {
        let id = u32::from(id);
        if id == bsp_id {
            continue;
        }
        idx += 1;
        if idx >= MAX_CPUS {
            break;
        }
        let cpu = &PERCPU[idx];
        cpu.lapic_id.store(id, Ordering::Relaxed);
        cpu.index.store(idx as u32, Ordering::Relaxed);

        // The AP's stack; leaked, like every task stack — it is the idle task's
        // for the life of the kernel. The top is offset by one word so the
        // trampoline's `jmp` lands with `rsp ≡ 8 (mod 16)`, the state System V
        // defines for a function entered by `call`.
        let stack = alloc::vec![0u8; AP_STACK_SIZE].leak();
        let stack_top = (stack.as_ptr() as usize + AP_STACK_SIZE) & !0xf;
        cpu.stack_top.store(stack_top as u64, Ordering::Relaxed);

        let Some(idle_slot) = sched::register_idle_task(idx) else {
            serial::puts("  smp:  [FAIL] no task slot for an idle task\n");
            break;
        };
        cpu.idle_task.store(idle_slot, Ordering::Relaxed);
        cpu.current_task.store(idle_slot, Ordering::Relaxed);
        cpu.current_uctx.store(sched::uctx_ptr(idle_slot) as u64, Ordering::Relaxed);

        mailbox_write(AP_MB_CR3, tables.pml4 as u64);
        mailbox_write(AP_MB_STACK, (stack_top - 8) as u64);
        #[allow(function_casts_as_integer)]
        mailbox_write(AP_MB_ENTRY, ap_entry64 as usize as u64);
        mailbox_write(AP_MB_INDEX, idx as u64);
        core::sync::atomic::fence(Ordering::SeqCst);

        lapic::send_init(id);
        lapic::delay_counts(AP_INIT_DELAY);
        lapic::send_startup(id, AP_STARTUP_VECTOR);
        if !wait_online(idx) {
            // The second STARTUP the architecture asks for; most VMMs never
            // need it and real parts sometimes do.
            lapic::send_startup(id, AP_STARTUP_VECTOR);
            lapic::delay_counts(AP_INIT_DELAY);
            if !wait_online(idx) {
                serial::puts("  smp:  [FAIL] cpu ");
                serial::put_dec(idx as u64);
                serial::puts(" (lapic id ");
                serial::put_dec(u64::from(id));
                serial::puts(") did not come online\n");
                continue;
            }
        }
        started += 1;
    }
    tables.free();
    ONLINE.store(1 + started, Ordering::Release);
    serial::puts("  smp:  ");
    serial::put_dec((1 + started) as u64);
    serial::puts(" cpus online\n");
    started
}

fn wait_online(idx: usize) -> bool {
    let mut spins = 0u64;
    while !PERCPU[idx].online.load(Ordering::Acquire) {
        if spins >= AP_ONLINE_BUDGET {
            return false;
        }
        spins += 1;
        pause();
    }
    true
}

/// Make SSE legal, exactly as `boot.s` does for the BSP — `CR0.EM=0`,
/// `CR0.MP=1`, `CR4.OSFXSR`, `CR4.OSXMMEXCPT`. Ring 3 needs it; the kernel is
/// soft-float and does not.
fn enable_sse() {
    // SAFETY: the same four control-register bits `boot.s` sets on the BSP,
    // with the same justification (see `long_mode_start` there).
    unsafe {
        core::arch::asm!(
            "mov rax, cr0",
            "and rax, -5",
            "or rax, 2",
            "mov cr0, rax",
            "mov rax, cr4",
            "or rax, (1 << 9) | (1 << 10)",
            "mov cr4, rax",
            out("rax") _,
            options(nostack, preserves_flags)
        );
    }
}

/// Where the trampoline jumps: an AP in long mode, on the boot tables, with
/// its stack set and `index` in `rdi`. Finishes bringing the core up and never
/// returns.
///
/// The order is the BSP's, minus what is shared: GDT and TSS are per core
/// (`ltr` needs its own), the IDT is one table loaded on each, the syscall
/// MSRs and `CR4.SMAP/SMEP` are per core, and the LAPIC is this core's own.
#[unsafe(no_mangle)]
extern "C" fn ap_entry64(index: u64) -> ! {
    let idx = index as usize;
    // First, off the boot tables: the identity map in slot 0 is the lower half
    // and belongs to userspace. The kernel root maps everything this function
    // touches — the image, the physmap this stack is in, the device window.
    // SAFETY: the kernel root shares the upper half with the boot tables, so
    // the instruction after the write is mapped.
    unsafe { paging::activate(sched::kernel_root()) };
    enable_sse();
    gdt::init_cpu(idx, PERCPU[idx].stack_top.load(Ordering::Relaxed));
    idt::load();
    install_percpu(idx);
    uaccess::init_smap();
    usermode::init_syscall();
    lapic::init_ap();

    // Report in BEFORE taking the BKL: the BSP holds the lock while it waits
    // for this flag, and only lets go inside `yield_now`. Entering first would
    // be a two-party deadlock with a straight face.
    PERCPU[idx].online.store(true, Ordering::Release);
    serial::puts("  smp:  cpu ");
    serial::put_dec(index);
    serial::puts(" online (lapic id ");
    serial::put_dec(u64::from(lapic::apic_id()));
    serial::puts(")\n");

    bkl_enter();
    sched::idle_loop()
}

// ---------------------------------------------------------------------------
// Self-test
// ---------------------------------------------------------------------------

const WORKERS: usize = 4;
const WORKER_ROUNDS: u32 = 8;
/// Spins per round with the BKL dropped. Long enough that two workers on two
/// cores overlap; short enough (~0.25 s each under TCG for all rounds) that the
/// suite does not crawl.
const WORKER_SPINS: u32 = 200_000;

/// Which CPUs each worker saw itself run on, one bit per CPU.
static WORKER_CPUS: [AtomicU32; WORKERS] = [const { AtomicU32::new(0) }; WORKERS];
/// Workers inside their unlocked spin right now, and the most ever at once.
static IN_FLIGHT: AtomicU32 = AtomicU32::new(0);
static MAX_IN_FLIGHT: AtomicU32 = AtomicU32::new(0);
static WORKERS_DONE: AtomicU32 = AtomicU32::new(0);

fn worker_body(id: usize) -> ! {
    for _ in 0..WORKER_ROUNDS {
        WORKER_CPUS[id].fetch_or(1 << cpu_index(), Ordering::Relaxed);
        bkl_run_unlocked(|| {
            let now = IN_FLIGHT.fetch_add(1, Ordering::AcqRel) + 1;
            MAX_IN_FLIGHT.fetch_max(now, Ordering::AcqRel);
            for _ in 0..WORKER_SPINS {
                pause();
            }
            IN_FLIGHT.fetch_sub(1, Ordering::AcqRel);
        });
        sched::yield_now();
    }
    WORKERS_DONE.fetch_add(1, Ordering::Release);
    sched::finish();
}

extern "C" fn worker0() -> ! {
    worker_body(0);
}
extern "C" fn worker1() -> ! {
    worker_body(1);
}
extern "C" fn worker2() -> ! {
    worker_body(2);
}
extern "C" fn worker3() -> ! {
    worker_body(3);
}

/// Prove the secondaries are up, ticking, and scheduling.
///
/// Three properties, each of which fails independently:
///
/// 1. Every CPU the MADT lists came online.
/// 2. Every AP takes timer interrupts of its own — its tick counter climbs
///    while the BSP waits.
/// 3. Kernel tasks run on more than one core, and two of them execute *at the
///    same time*: four workers spin with the BKL dropped, and the high-water
///    mark of workers inside that spin must reach 2. A scheduler that merely
///    migrated tasks between cores one at a time would pass the first half of
///    this and fail the second.
///
/// On a one-CPU machine the parallelism checks are skipped and noted rather
/// than failed: single core is a configuration, not a bug.
pub fn smoke_test(t: &mut Suite, expected_aps: usize, started: usize) {
    t.check_eq("smp: secondaries online", started as u64, expected_aps as u64);
    t.check_eq("smp: online count agrees", online_cpus() as u64, (1 + started) as u64);
    if started == 0 {
        t.note("smp: single core, parallelism checks skipped", 1);
        return;
    }

    // 2. Ticks on every AP. Each AP's timer has been running since it came
    //    online and its idle loop sleeps with interrupts on, so its counter
    //    climbs on its own; the BSP only watches. With the BKL dropped: an
    //    idle AP's tick handler takes the lock to look for work, and this loop
    //    holding it would turn "no ticks" into "one tick, then a core stuck in
    //    its handler". The BSP's own interrupts are off here (they have been
    //    since `sched::smoke_test`'s `cli`), which is why its timer is not used
    //    to pace the wait.
    const TICK_WAIT_BUDGET: u64 = 400_000_000;
    let before: [u64; MAX_CPUS] = core::array::from_fn(ticks_on);
    let all_ticked = |before: &[u64; MAX_CPUS]| {
        before.iter().enumerate().take(started + 1).skip(1).all(|(idx, &was)| ticks_on(idx) > was)
    };
    let mut spins = 0u64;
    bkl_run_unlocked(|| {
        while !all_ticked(&before) && spins < TICK_WAIT_BUDGET {
            spins += 1;
            pause();
        }
    });
    t.note("smp: spins until every secondary had ticked", spins);
    let mut all_ticking = true;
    for (idx, &was) in before.iter().enumerate().take(started + 1).skip(1) {
        if ticks_on(idx) <= was {
            all_ticking = false;
            serial::puts("  smp:  cpu ");
            serial::put_dec(idx as u64);
            serial::puts(" took no ticks\n");
        }
    }
    t.check("smp: every secondary takes timer interrupts", all_ticking);
    // The BSP's timer, for the workers below: they yield, and a tick-driven
    // reschedule request on the BSP is what keeps the boot task from hogging
    // it between their rounds.
    lapic::start_timer();

    // 3. Parallel execution.
    let spawned = [worker0 as extern "C" fn() -> !, worker1, worker2, worker3]
        .into_iter()
        .filter(|&f| sched::spawn(f).is_some())
        .count();
    if t.check_eq("smp: workers spawned", spawned as u64, WORKERS as u64) {
        let mut spins = 0u64;
        while WORKERS_DONE.load(Ordering::Acquire) < WORKERS as u32 && spins < 2_000_000_000 {
            spins += 1;
            sched::yield_now();
        }
        t.check_eq(
            "smp: every worker finished",
            u64::from(WORKERS_DONE.load(Ordering::Acquire)),
            WORKERS as u64,
        );
        let seen = WORKER_CPUS
            .iter()
            .fold(0u32, |acc, c| acc | c.load(Ordering::Relaxed));
        t.note("smp: cpu mask the workers ran on", u64::from(seen));
        t.check("smp: workers ran on more than one core", seen.count_ones() >= 2);
        t.note("smp: most workers in flight at once", u64::from(MAX_IN_FLIGHT.load(Ordering::Relaxed)));
        t.check(
            "smp: two workers executed simultaneously",
            MAX_IN_FLIGHT.load(Ordering::Relaxed) >= 2,
        );
    }
    lapic::stop_timer();
    for idx in 0..=started {
        t.note("smp: ticks on a cpu", ticks_on(idx));
    }
}
