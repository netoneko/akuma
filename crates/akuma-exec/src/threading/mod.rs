// Preemptive threading with fixed-size thread pool
// Supports per-thread stack sizes and stack overflow detection

#![allow(dead_code)]

// Physical address where the kernel binary is loaded (RAM_BASE + text_offset).
// Must match KERNEL_PHYS_BASE in src/config.rs and KERNEL_PHYS_BASE in linker.ld.
const KERNEL_PHYS_BASE: usize = 0x4010_0000;

pub mod sigframe;
pub mod types;

pub use types::*;

use alloc::boxed::Box;
use alloc::vec::Vec;
#[cfg(target_os = "none")]
use core::arch::global_asm;
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use spinning_top::Spinlock;

use crate::runtime::{runtime, config, with_irqs_disabled, IrqGuard};
// This module's ~39 `safe_print!` calls used to resolve to a `macro_rules!`
// defined a few lines below them. The macro now lives in `akuma-primitives`
// (one copy for the tree instead of three), so it needs importing.
use crate::safe_print;

/// Set the current exception stack pointer (TPIDR_EL1).
#[cfg(target_os = "none")]
#[inline]
pub fn set_current_exception_stack(stack_top: u64) {
    unsafe { core::arch::asm!("msr tpidr_el1, {}", in(reg) stack_top); }
}

#[cfg(not(target_os = "none"))]
#[inline]
pub fn set_current_exception_stack(_stack_top: u64) {}

// `StackWriter` and this crate's `safe_print!` copy both moved to
// `akuma-primitives::console` — one writer and one macro for the whole tree
// instead of five and three. `crate::safe_print!` still resolves: `lib.rs`
// re-exports the macro. See that module's header for the census.

// ============================================================================
// Lock-Free Thread State Management
// ============================================================================

/// Cleanup callback (e.g. for process unregistration)
/// Stored as usize (function pointer cast) to allow atomic access
static CLEANUP_CALLBACK: AtomicUsize = AtomicUsize::new(0);

/// Set a callback to be invoked when a thread is cleaned up (recycled).
/// The callback receives the thread ID (index) of the cleaned up thread.
pub fn set_cleanup_callback(cb: fn(usize)) {
    CLEANUP_CALLBACK.store(cb as usize, Ordering::SeqCst);
}

/// Kernel-registered purge for per-tid state this crate cannot reach.
///
/// `scrub_thread_slot` covers the per-slot arrays that live in this module, but other
/// subsystems register a *thread id* and rely on that thread to deregister itself —
/// `FUTEX_WAITERS` (`src/syscall/sync.rs`) most importantly, where a tid left queued by a
/// thread that died while parked is inherited by the slot's next occupant and silently
/// absorbs that address's next wake. Those tables live in the kernel crate, so the kernel
/// registers a hook here and the recycler calls it alongside `CLEANUP_CALLBACK`.
static SLOT_PURGE_CALLBACK: AtomicUsize = AtomicUsize::new(0);

/// Register the per-tid purge invoked when a thread slot is recycled. Runs with no lock
/// of this module held (same point as `CLEANUP_CALLBACK`), so the hook may take its own.
pub fn set_slot_purge_callback(cb: fn(usize)) {
    SLOT_PURGE_CALLBACK.store(cb as usize, Ordering::SeqCst);
}

/// Thread ID of the network polling loop (run_async_main).
/// Set at boot by set_network_thread_id(). usize::MAX means not yet registered.
static NETWORK_THREAD_ID: AtomicUsize = AtomicUsize::new(usize::MAX);

pub fn set_network_thread_id(tid: usize) {
    NETWORK_THREAD_ID.store(tid, Ordering::Relaxed);
}

/// Number of thread slots that actually get a stack allocated — the user-process
/// slots `[reserved_threads, thread_limit())` plus the reserved system slots.
/// `MAX_THREADS` stays the compile-time array size; this is the runtime cap so
/// the PMM-backed stack pool fits on small machines (see `compute_thread_limit`
/// in src/main.rs and docs/LOW_MEMORY_ENVIRONMENT.md). Defaults to the full
/// `MAX_THREADS` until `set_thread_limit` is called during early boot.
static THREAD_LIMIT: AtomicUsize = AtomicUsize::new(MAX_THREADS);

// ── Lazy thread-stack allocation (size profile) ──────────────────────────────
// On the size profile the per-slot stacks (128 KB system / 64 KB user from PMM)
// are not all pre-allocated at boot — that reserved ~1.28 MB at thread_limit=14
// while only ~3 threads run at idle. Instead we keep a small WARM FLOOR of FREE
// pre-allocated stacks per class so the common single-session/single-process
// spawn is always warm and can't fail, and allocate/free the rest on demand
// (ensure_slot_stack on claim, free-above-floor in cleanup_terminated). On
// release every slot is still pre-allocated (no per-spawn alloc, guaranteed
// availability). Idle saving at 8 MB ≈ 0.8 MB. See docs/LOW_MEMORY_ENVIRONMENT.md.
// extreme: no warm floor at all. System threads (async-main, SSH) spawn once at
// boot and never recycle, so a warm *system* reserve is dead weight. The warm
// *user* stack is the 64 KB that lingers after a process exits (the post-workload
// step we measured at the floor); free it too. The next user spawn re-allocates
// its 16 contiguous pages — at the floor workloads are ~serial, so the just-freed
// stack is immediately available. extreme implies kernel_profile_extreme, so the
// size branch below must exclude it.
#[cfg(kernel_profile_extreme)]
const WARM_FREE_SYSTEM: usize = 0;
#[cfg(kernel_profile_extreme)]
const WARM_FREE_USER: usize = 0;

// ── Stack high-water probe ───────────────────────────────────────────────────
// To right-size the 128 KB system / 64 KB user kernel stacks for the extreme
// target we need the *true* peak usage. When this is on, freshly-allocated
// stacks are painted with `STACK_SENTINEL`; `stack_high_water` then scans for the
// deepest write — an UPPER bound on usage (any write, even a zero, breaks the
// sentinel), so sizing to peak+margin is safe. Costs a memset per stack alloc;
// keep `false` in production and flip on only for a measurement boot.
const STACK_USAGE_PROBE: bool = true;
const STACK_SENTINEL: u64 = 0xABAB_ABAB_ABAB_ABAB;

// Recorded peak usage per class (bytes), so a short-lived user thread's
// high-water survives the free that WARM_FREE_USER=0 does immediately on exit.
// Updated in `free_stack_for_slot` (teardown) and `report_stack_high_water`
// (live long-running system threads). Only meaningful when STACK_USAGE_PROBE.
static STACK_PEAK_SYSTEM: AtomicUsize = AtomicUsize::new(0);
static STACK_PEAK_USER: AtomicUsize = AtomicUsize::new(0);

// Per-slot latch for `report_overrun_stack_canaries`: the stack base this slot's
// overflow was last announced for, so a periodic sweep prints once per broken
// stack rather than once per sweep. `0` = nothing reported yet (no stack is ever
// based at 0 — `StackInfo::empty()` uses it as the unallocated marker).
#[allow(clippy::declare_interior_mutable_const)]
const ATOMIC_USIZE_ZERO: AtomicUsize = AtomicUsize::new(0);
static CANARY_REPORTED_BASE: [AtomicUsize; MAX_THREADS] = [ATOMIC_USIZE_ZERO; MAX_THREADS];
// Boot-stack (thread 0) bounds, recorded by `paint_boot_stack` so the report can
// scan it. The boot stack is the 1 MB elephant; its true high-water decides
// whether it can be halved for the extreme target.
static BOOT_STACK_BASE: AtomicUsize = AtomicUsize::new(0);
static BOOT_STACK_TOP: AtomicUsize = AtomicUsize::new(0);

/// Paint the *currently-unused* lower part of the boot stack with the sentinel so
/// `report_stack_high_water` can later report thread 0's peak. Call once, early in
/// boot (the deep work — tests, init — happens after and overwrites the sentinel
/// down to the true high-water). Leaves a generous headroom below the live SP so
/// this function's own frame is never clobbered. No-op unless STACK_USAGE_PROBE.
pub fn paint_boot_stack(base: usize, top: usize) {
    if !STACK_USAGE_PROBE || base == 0 || top <= base {
        return;
    }
    BOOT_STACK_BASE.store(base, Ordering::Relaxed);
    BOOT_STACK_TOP.store(top, Ordering::Relaxed);
    let sp: usize;
    #[cfg(target_os = "none")]
    unsafe { core::arch::asm!("mov {}, sp", out(reg) sp); }
    #[cfg(not(target_os = "none"))]
    { sp = top; }
    let paint_end = sp.saturating_sub(8 * 1024) & !7; // 8 KB headroom below live SP
    let mut addr = (base + 7) & !7;
    if paint_end <= addr {
        return;
    }
    unsafe {
        while addr + 8 <= paint_end {
            (addr as *mut u64).write_volatile(STACK_SENTINEL);
            addr += 8;
        }
    }
}

fn record_stack_peak(slot: usize, used: usize) {
    let reserved = config().reserved_threads;
    let cell = if slot >= 1 && slot < reserved {
        &STACK_PEAK_SYSTEM
    } else if slot >= reserved {
        &STACK_PEAK_USER
    } else {
        return; // slot 0 = boot stack, measured separately
    };
    cell.fetch_max(used, Ordering::Relaxed);
}

/// Paint `[base + canary, top)` with the sentinel (skips the canary words at the
/// base so `check_stack_canary` still sees its own pattern).
fn fill_stack_sentinel(base: usize, top: usize) {
    if base == 0 || top <= base {
        return;
    }
    let canary_bytes = config().canary_words * 8;
    let start = (base + canary_bytes + 7) & !7; // 8-byte aligned
    let mut addr = start;
    unsafe {
        while addr + 8 <= top {
            (addr as *mut u64).write_volatile(STACK_SENTINEL);
            addr += 8;
        }
    }
}

/// Peak stack usage for a slot, as `(used_bytes, total_bytes)`. Requires
/// `STACK_USAGE_PROBE`; returns `None` if the slot has no painted stack. Scans up
/// from the canary for the first broken sentinel word — the deepest (lowest) the
/// stack ever reached — and returns `top - that_addr`.
pub fn stack_high_water(slot: usize) -> Option<(usize, usize)> {
    if !STACK_USAGE_PROBE || slot >= MAX_THREADS {
        return None;
    }
    with_irqs_disabled(|| {
        let pool = POOL.lock();
        let stack = &pool.stacks[slot];
        if !stack.is_allocated() {
            return None;
        }
        let base = stack.base;
        let size = stack.size;
        let top = base + size;
        let canary_bytes = config().canary_words * 8;
        let mut addr = (base + canary_bytes + 7) & !7;
        let mut first_used = top;
        unsafe {
            while addr + 8 <= top {
                if (addr as *const u64).read_volatile() != STACK_SENTINEL {
                    first_used = addr;
                    break;
                }
                addr += 8;
            }
        }
        Some((top - first_used, size))
    })
}

/// Print a `[Stack]` high-water line: the deepest-used slot in each class and its
/// peak vs the configured stack size. Drives the extreme stack right-sizing.
pub fn report_stack_high_water() {
    if !STACK_USAGE_PROBE {
        return;
    }
    let limit = thread_limit();
    // Fold currently-live slots into the global peaks (long-running system
    // threads never recycle, so their high-water is only ever seen live).
    for i in 1..limit {
        if let Some((used, _size)) = stack_high_water(i) {
            record_stack_peak(i, used);
        }
    }
    let sys_peak = STACK_PEAK_SYSTEM.load(Ordering::Relaxed);
    let usr_peak = STACK_PEAK_USER.load(Ordering::Relaxed);
    // Boot stack: scan from base for the first broken sentinel = thread 0's peak.
    let boot_base = BOOT_STACK_BASE.load(Ordering::Relaxed);
    let boot_top = BOOT_STACK_TOP.load(Ordering::Relaxed);
    let mut boot_used = 0usize;
    let boot_size = boot_top.saturating_sub(boot_base);
    if boot_base != 0 && boot_top > boot_base {
        // Skip the canary words at the base (init_stack_canary writes them AFTER
        // paint_boot_stack runs, so they read as non-sentinel and would otherwise
        // pin the high-water at "full").
        let canary_bytes = config().canary_words * 8;
        let mut addr = (boot_base + canary_bytes + 7) & !7;
        let mut first_used = boot_top;
        unsafe {
            while addr + 8 <= boot_top {
                if (addr as *const u64).read_volatile() != STACK_SENTINEL {
                    first_used = addr;
                    break;
                }
                addr += 8;
            }
        }
        boot_used = boot_top - first_used;
    }
    safe_print!(200,
        "[Stack] sys peak {}KB/{}KB | user peak {}KB/{}KB | boot peak {}KB/{}KB (probe; lower=more trim headroom)\n",
        sys_peak / 1024, config().system_thread_stack_size / 1024,
        usr_peak / 1024, config().user_thread_stack_size / 1024,
        boot_used / 1024, boot_size / 1024);
}

/// Set the runtime thread-slot limit. Clamped to `[reserved+1, MAX_THREADS]` so
/// there is always the full system-thread set plus at least one user slot.
pub fn set_thread_limit(limit: usize) {
    let lo = config().reserved_threads + 1;
    THREAD_LIMIT.store(limit.clamp(lo, MAX_THREADS), Ordering::Release);
}

/// Current runtime thread-slot limit (`<= MAX_THREADS`).
pub fn thread_limit() -> usize {
    THREAD_LIMIT.load(Ordering::Acquire)
}

/// Atomic thread states - lock-free access
/// Each thread's state can be read/modified without holding any lock
static THREAD_STATES: [AtomicU8; MAX_THREADS] = {
    const INIT: AtomicU8 = AtomicU8::new(thread_state::FREE);
    [INIT; MAX_THREADS]
};

/// Per-thread current trap frame pointer (set during EL0 sync handler)
/// Used by fork to capture full register state from the parent's trap frame.
static CURRENT_TRAP_FRAME: [AtomicU64; MAX_THREADS] = {
    const INIT: AtomicU64 = AtomicU64::new(0);
    [INIT; MAX_THREADS]
};

/// ON-CPU gate for the cross-core stack-sharing races (the SMP=4 boot-storm EL1
/// crashes / BKL_RUSTC_SCALING_BASELINE.md §5.1 corruption family).
///
/// Set while a thread is (or may still be) executing on some core; a thread may
/// be PICKED by a scheduler only when its gate is clear. Two distinct races
/// require this, and thread STATE alone catches neither:
///
/// 1. **Switch-out tail.** `commit_switch` marks the outgoing thread READY and
///    POOL is released when `sgi_scheduler_handler_with_sp` returns — but the
///    switching core keeps EXECUTING ON THE OUTGOING THREAD'S KERNEL STACK
///    until the vector asm's `mov sp, x0` (Rust epilogues, BKL reconcile and
///    tag bookkeeping all run on it after POOL is gone). A peer picking the
///    thread in that window resumes it onto a stack still in use.
/// 2. **Wake-before-switch-out.** `schedule_blocking` stores WAITING (often
///    with an already-expired deadline) while the thread is STILL RUNNING,
///    before its yield-SGI fires. A peer's wake-pass / `mark_thread_ready`
///    flips it READY and a peer scheduler resumes it from its STALE `ctx.sp`
///    (the previous switch-out's frame) while it is still running elsewhere —
///    a double-run on one stack. Observed as EL1 `ret` into kernel data /
///    `ELR=0x8` with SP in another thread's stack, and as §5.1's ERET to EL0
///    with a kernel register file.
///
/// Protocol: `commit_switch` sets the gate for the INCOMING thread (and
/// defensively re-asserts the outgoing thread's, which stays set from its own
/// switch-in); the outgoing tid is recorded per-core and its gate is cleared by
/// `rust_switch_finished`, called from the vector asm immediately AFTER
/// `mov sp, x0`. Bringup sites (boot thread, per-core idle claim) set the gate
/// for the thread they start life running. Pickers and the slot recycler skip
/// gated threads.
static ON_CPU: [AtomicU8; MAX_THREADS] = {
    const INIT: AtomicU8 = AtomicU8::new(0);
    [INIT; MAX_THREADS]
};

/// Per-core: the tid whose [`ON_CPU`] gate this core must clear once the
/// vector asm has switched SP (`usize::MAX` = none pending). Sized by
/// `types::MAX_CORES`, the SMP bringup cap.
static PER_CORE_OFFCPU: [AtomicUsize; MAX_CORES] = {
    const INIT: AtomicUsize = AtomicUsize::new(usize::MAX);
    [INIT; MAX_CORES]
};

/// Per-thread *expected* TTBR0 L0 base — the physical page-table root this
/// thread's user context is supposed to run under. 0 = unknown (kernel/idle
/// threads and slots between scrub and first context write): checks skip it.
///
/// Written wherever the canonical value changes hands: `update_thread_context`
/// when a parent builds a child's context, and `UserAddressSpace::
/// activate`/`deactivate` (via [`note_current_expected_l0`]) when a thread
/// installs tables itself. The ASID half of TTBR0 is deliberately masked out —
/// a cloned thread legitimately runs under the parent's L0 with a different
/// ASID (see debug-thread-spawn-segv.md §2f "Read the flag carefully").
///
/// Consumed by the context-switch tripwires in `sgi_scheduler_handler_with_sp`:
/// they catch a thread being switched out under (or resumed into) page tables
/// that are not its own — the §2f `AS MISMATCH` — at the switch where the
/// corruption happens, with both tids, instead of at the EL0 fault it causes
/// later.
static EXPECTED_L0: [AtomicU64; MAX_THREADS] = {
    const INIT: AtomicU64 = AtomicU64::new(0);
    [INIT; MAX_THREADS]
};

/// Physical L0-base bits of a TTBR0 value (ASID and CnP masked off).
pub const TTBR0_L0_MASK: u64 = 0x0000_FFFF_FFFF_F000;

/// Per-slot generation counter — bumped once per slot *lifetime*, in
/// `scrub_thread_slot` (which every claim path runs under the winning
/// FREE→INITIALIZING CAS, so there is exactly one bumper per rebirth).
///
/// This gives tids the property pids already get for free from their
/// monotonic allocator: **staleness is detectable**. A bare tid is an index
/// into a recycled 256-slot array, so a tid held across the slot's death —
/// in a futex/pipe/msgqueue wait queue, or in a waker preempted mid-flight —
/// is indistinguishable from a live one, and acting on it acts on whoever
/// owns the slot now (see `ThreadWaker::wake` for the corruption that
/// enabled). A `(generation, tid)` pair ([`WakeHandle`]) names one
/// *incarnation*: if the generation no longer matches, the wake is refused
/// before any side effect.
///
/// Slots that never recycle (boot/idle/system threads) stay at generation 0
/// forever, which is fine — a handle minted for them always validates.
static SLOT_GEN: [AtomicU64; MAX_THREADS] = {
    const INIT: AtomicU64 = AtomicU64::new(0);
    [INIT; MAX_THREADS]
};

/// Current generation of `tid`'s slot (0 for out-of-range).
pub fn thread_generation(tid: usize) -> u64 {
    if tid < MAX_THREADS {
        SLOT_GEN[tid].load(Ordering::Acquire)
    } else {
        0
    }
}

/// Bits of a [`WakeHandle`] that hold the tid. `MAX_THREADS` is 256 (64 on
/// host), so 16 bits leaves the upper 48 for the generation — at a pathological
/// million spawns/second that wraps after ~9 years of uptime.
const WAKE_HANDLE_TID_BITS: u32 = 16;
const _: () = assert!(MAX_THREADS <= 1 << WAKE_HANDLE_TID_BITS);

/// A generation-tagged wake target: names one *incarnation* of a thread slot,
/// not the slot itself. Mint one with [`current_wake_handle`] when a waiter
/// enqueues itself (the tid is self-evidently live), or
/// [`wake_handle_for_thread`] when the caller otherwise knows the tid is
/// currently live. Wake it with [`wake_by_handle`]; a handle whose generation
/// has passed is refused with no side effect at all.
///
/// Wait queues must store THIS, not a bare `usize` tid — a bare tid dequeued
/// and then held across a preemption (or simply left behind by a dead waiter)
/// wakes whoever owns the slot by then.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct WakeHandle(u64);

impl WakeHandle {
    fn new(tid: usize, generation: u64) -> Self {
        Self((generation << WAKE_HANDLE_TID_BITS) | tid as u64)
    }

    /// The slot index this handle points at. Only meaningful for bookkeeping
    /// keyed by slot (queue purges, diagnostics) — never act on the thread
    /// through the bare tid.
    pub fn tid(self) -> usize {
        (self.0 & ((1 << WAKE_HANDLE_TID_BITS) - 1)) as usize
    }

    fn generation(self) -> u64 {
        self.0 >> WAKE_HANDLE_TID_BITS
    }

    /// `true` while the slot still hosts the incarnation this handle names.
    pub fn is_current(self) -> bool {
        thread_generation(self.tid()) == self.generation()
    }
}

/// Handle for the calling thread — what a waiter stores in a wait queue.
pub fn current_wake_handle() -> WakeHandle {
    wake_handle_for_thread(get_current_thread_register())
}

/// Handle for `tid`'s CURRENT incarnation. Only meaningful where the caller
/// knows `tid` is live right now (e.g. it holds the registration that created
/// it); calling this at wake time on a tid stored long ago just launders a
/// stale tid into a fresh-looking handle.
pub fn wake_handle_for_thread(tid: usize) -> WakeHandle {
    WakeHandle::new(tid, thread_generation(tid))
}

/// Generation-validated wake: the WAITING→READY transition fires only if the
/// slot still hosts the incarnation the handle names. Refusal has no side
/// effect — not even the sticky `WOKEN_STATES` flag, which on a stale tid
/// would spend a phantom wake on the slot's next occupant.
pub fn wake_by_handle(handle: WakeHandle) {
    ThreadWaker { handle }.wake();
}

/// Record the expected L0 base for `tid` (0 clears / disables the check).
pub fn note_thread_expected_l0(tid: usize, ttbr0: u64) {
    if tid < MAX_THREADS {
        EXPECTED_L0[tid].store(ttbr0 & TTBR0_L0_MASK, Ordering::Release);
    }
}

/// Diagnostic accessor for the fault dump: `tid`'s current expected L0 base
/// (0 = unknown/no-check — see [`EXPECTED_L0`]). An `AS MISMATCH` fault with
/// this reading 0 means the thread ran without ever passing a checked
/// switch-in with a known baseline; reading the correct L0 means the tripwire
/// SHOULD have fired and the corruption entered somewhere it cannot see.
pub fn thread_expected_l0(tid: usize) -> u64 {
    if tid < MAX_THREADS {
        EXPECTED_L0[tid].load(Ordering::Acquire)
    } else {
        0
    }
}

/// Per-slot count of checked scheduler switch-ins since the slot was scrubbed.
/// Discriminates two `AS MISMATCH` stories the fault dump alone cannot: 0 means
/// the thread reached EL0 purely via its first-entry path (trampoline →
/// activate → eret) and the live TTBR0 diverged with NO switch in between;
/// >0 means every one of those restores was checked against `EXPECTED_L0`.
static SWITCH_INS: [AtomicU32; MAX_THREADS] = {
    const INIT: AtomicU32 = AtomicU32::new(0);
    [INIT; MAX_THREADS]
};

/// Diagnostic accessor for [`SWITCH_INS`].
pub fn thread_switch_ins(tid: usize) -> u32 {
    if tid < MAX_THREADS {
        SWITCH_INS[tid].load(Ordering::Relaxed)
    } else {
        0
    }
}

/// Record the expected L0 base for the calling thread — hook for
/// `UserAddressSpace::activate`/`deactivate`, which install tables for the
/// thread that is executing them.
pub fn note_current_expected_l0(ttbr0: u64) {
    note_thread_expected_l0(get_current_thread_register(), ttbr0);
}

/// Per-thread user-copy fault handler address (set around copy_from/to_user).
///
/// Lock-free, for the same reason as `CURRENT_TRAP_FRAME`: it is read by the DATA
/// ABORT exception handler (`get_user_copy_fault_handler`) to recover a faulting
/// user copy. Taking `POOL.lock()` there self-deadlocks the single CPU if the
/// fault occurred while `POOL` was already held (a nested fault) — observed as a
/// hang spinning on the pool spinlock in the copy-fault path under a multithreaded
/// process (curl's resolver + main thread). A per-thread atomic is safe: a thread
/// only reads/writes its OWN slot, and its own fault can't race its own set.
static USER_COPY_FAULT_HANDLER: [AtomicU64; MAX_THREADS] = {
    const INIT: AtomicU64 = AtomicU64::new(0);
    [INIT; MAX_THREADS]
};

/// Atomic wake times for WAITING threads - scheduler checks these
/// Value is 0 for threads that are not waiting, otherwise it's the wake deadline in microseconds
static WAKE_TIMES: [AtomicU64; MAX_THREADS] = {
    const INIT: AtomicU64 = AtomicU64::new(0);
    [INIT; MAX_THREADS]
};

/// Atomic total CPU time in microseconds for each thread
static TOTAL_CPU_TIMES: [AtomicU64; MAX_THREADS] = {
    const INIT: AtomicU64 = AtomicU64::new(0);
    [INIT; MAX_THREADS]
};

/// Last core each thread ran on (MPIDR aff0). 0xFF = never scheduled.
/// Lock-free like THREAD_STATES/TOTAL_CPU_TIMES so sys_get_cpu_stats reads it
/// without POOL.lock (see USER_COPY_FAULT_HANDLER note above).
static LAST_CORE: [AtomicU8; MAX_THREADS] = {
    const INIT: AtomicU8 = AtomicU8::new(0xFF);
    [INIT; MAX_THREADS]
};

/// Per-THREAD current syscall number (`!0` = not in a syscall). Set by the
/// syscall dispatch at entry and cleared at exit, keyed by thread id. Unlike
/// `Process.current_syscall` (keyed by the address-space owner / leader, so a
/// CLONE_VM/vfork child's syscalls are accounted to its parent), this is exact
/// per-thread — needed to see which syscall a parked child is blocked in.
static THREAD_CURRENT_SYSCALL: [AtomicU64; MAX_THREADS] = {
    const INIT: AtomicU64 = AtomicU64::new(u64::MAX);
    [INIT; MAX_THREADS]
};

/// Set the calling thread's current syscall number (or `!0` to clear at exit).
pub fn set_thread_current_syscall(nr: u64) {
    let tid = current_thread_id();
    if tid < MAX_THREADS {
        THREAD_CURRENT_SYSCALL[tid].store(nr, Ordering::Relaxed);
    }
}

/// Read a thread's current syscall number (`!0` if not in a syscall).
pub fn thread_current_syscall(tid: usize) -> u64 {
    if tid < MAX_THREADS {
        THREAD_CURRENT_SYSCALL[tid].load(Ordering::Relaxed)
    } else {
        u64::MAX
    }
}

/// Atomic "sticky wake" flags - set when wake() is called, cleared when thread resumes
static WOKEN_STATES: [AtomicBool; MAX_THREADS] = {
    const INIT: AtomicBool = AtomicBool::new(false);
    [INIT; MAX_THREADS]
};

/// Wakeup-locality hint: the tid of a thread just promoted READY by an explicit
/// `wake()` (futex/cond signal), which the scheduler prefers to run NEXT (once)
/// instead of waiting a full round-robin cycle. Set in [`ThreadWaker::wake`],
/// consumed (swapped to `MAX_THREADS`) in `schedule_indices`.
///
/// GATED OFF ([`WAKEUP_LOCALITY_HINT`] = false): tried as a rump-sysproxy latency
/// fix; measured NO improvement (the per-syscall cost is not the woken-thread
/// scheduling delay — woken threads already run promptly via the SGI), so it
/// stays disabled to keep baseline round-robin behavior. Kept for a future,
/// more-targeted use. The real latency lever is the fiber rumpuser backend.
static PREEMPT_WAKE_TID: AtomicUsize = AtomicUsize::new(MAX_THREADS);

/// Master switch for the [`PREEMPT_WAKE_TID`] wakeup-locality experiment. Off:
/// the hint is never set, so the scheduler is exactly baseline round-robin.
const WAKEUP_LOCALITY_HINT: bool = false;

/// Real shared-kernel SMP: which thread slots are per-core idle threads. Idle threads
/// are skipped by the round-robin scan — a core only ever runs ITS OWN idle, chosen
/// explicitly as the fallback in `schedule_indices` — so one core can never pick
/// another core's idle. Slot 0 (the boot/idle thread) is marked idle for core 0 at init.
static IS_IDLE_THREAD: [AtomicBool; MAX_THREADS] = {
    const INIT: AtomicBool = AtomicBool::new(false);
    [INIT; MAX_THREADS]
};

/// Per-core idle thread slot (index into the pool), or -1 if none assigned. Core 0 is
/// slot 0 (the boot thread); each secondary registers its own via
/// [`register_core_idle`]. `schedule_indices` falls back to this core's idle when no
/// non-idle thread is READY.
static IDLE_SLOT_FOR_CORE: [AtomicI32; MAX_CORES] = {
    const INIT: AtomicI32 = AtomicI32::new(-1);
    [INIT; MAX_CORES]
};

/// Register `slot` as the idle thread for `core_id` (real shared-kernel SMP). Called by
/// the BSP for core 0 at init and by each secondary during bringup. Idempotent.
pub fn register_core_idle(core_id: usize, slot: usize) {
    if slot < MAX_THREADS {
        IS_IDLE_THREAD[slot].store(true, Ordering::Release);
    }
    if core_id < MAX_CORES {
        IDLE_SLOT_FOR_CORE[core_id].store(slot as i32, Ordering::Release);
    }
}

/// Per-thread pending signal bitmask.  Bit N set = signal (N+1) pending.
/// Multiple signals can be pending simultaneously (unlike the old single-slot).
/// Set by pend_signal_for_thread via fetch_or.
/// Cleared one-at-a-time by take_pending_signal via fetch_and.
static PENDING_SIGNALS: [AtomicU64; MAX_THREADS] = {
    const INIT: AtomicU64 = AtomicU64::new(0);
    [INIT; MAX_THREADS]
};

/// Per-thread **signal mask** (blocked-signal set). Bit N set = signal (N+1)
/// blocked. This MUST be per-thread, not per-process: Linux/POSIX signal masks
/// are per-thread (`pthread_sigmask`), and `read_current_pid()` collapses every
/// CLONE_THREAD sibling onto the owner PID — so a `Process::signal_mask` is shared
/// across all siblings, letting one thread's `rt_sigprocmask`/`sigreturn` clear a
/// block another thread installed. That defeats per-thread masking used to gate
/// async signals (e.g. rustc's SIGUSR1 storm) to safe points, delivering a signal
/// mid-critical-section and corrupting a register — the long-standing
/// signal/register-corruption bug (docs/SIGNAL_DELIVERY_FORKTEST_EVIDENCE.md §D).
static THREAD_SIGNAL_MASK: [AtomicU64; MAX_THREADS] = {
    const INIT: AtomicU64 = AtomicU64::new(0);
    [INIT; MAX_THREADS]
};

/// Current thread's blocked-signal mask.
pub fn thread_signal_mask() -> u64 {
    let tid = current_thread_id();
    if tid < MAX_THREADS { THREAD_SIGNAL_MASK[tid].load(Ordering::Acquire) } else { 0 }
}

/// Set the current thread's blocked-signal mask (SIGKILL/SIGSTOP can't be blocked).
pub fn set_thread_signal_mask(mask: u64) {
    let tid = current_thread_id();
    if tid < MAX_THREADS {
        let unblockable = (1u64 << 8) | (1u64 << 18); // SIGKILL(9), SIGSTOP(19)
        THREAD_SIGNAL_MASK[tid].store(mask & !unblockable, Ordering::Release);
    }
}

/// OR bits into the current thread's blocked-signal mask; returns the new mask.
pub fn or_thread_signal_mask(bits: u64) -> u64 {
    let tid = current_thread_id();
    if tid < MAX_THREADS {
        let unblockable = (1u64 << 8) | (1u64 << 18);
        let prev = THREAD_SIGNAL_MASK[tid].fetch_or(bits & !unblockable, Ordering::AcqRel);
        prev | (bits & !unblockable)
    } else {
        0
    }
}

/// Seed a specific thread slot's signal mask (used at clone/spawn so a child
/// inherits the parent thread's mask, matching Linux `CLONE` semantics).
pub fn seed_thread_signal_mask(tid: usize, mask: u64) {
    if tid < MAX_THREADS {
        THREAD_SIGNAL_MASK[tid].store(mask, Ordering::Release);
    }
}

/// Blocked-signal mask of a specific thread slot (0 for out-of-range).
/// Used by `tkill`/`tgkill`, which target a thread by id.
pub fn thread_signal_mask_of(tid: usize) -> u64 {
    if tid < MAX_THREADS { THREAD_SIGNAL_MASK[tid].load(Ordering::Acquire) } else { 0 }
}

/// Raw pending-signal bitset of a thread slot (0 for out-of-range). Read-only —
/// does not consume. Used by `rt_sigsuspend` to test for a deliverable signal
/// without draining it (delivery happens at syscall return).
pub fn pending_signals_raw(slot: usize) -> u64 {
    if slot < MAX_THREADS { PENDING_SIGNALS[slot].load(Ordering::Acquire) } else { 0 }
}

// ---------------------------------------------------------------------------
// Restore-sigmask (Linux TIF_RESTORE_SIGMASK analogue)
//
// `rt_sigsuspend` installs a temporary mask and must, after the woken signal's
// handler returns, restore the mask that existed *before* the call — not the
// temporary one. We stash the to-restore mask per thread and set a pending flag;
// the next signal-frame setup (`try_deliver_signal`) consumes it and writes it
// as `uc_sigmask`, so `rt_sigreturn` restores the original mask.
// ---------------------------------------------------------------------------
static THREAD_RESTORE_SIGMASK: [AtomicU64; MAX_THREADS] = {
    const INIT: AtomicU64 = AtomicU64::new(0);
    [INIT; MAX_THREADS]
};
static THREAD_RESTORE_SIGMASK_PENDING: [AtomicBool; MAX_THREADS] = {
    const INIT: AtomicBool = AtomicBool::new(false);
    [INIT; MAX_THREADS]
};

/// Last SIGCHLD payload for each thread slot, packed as
/// `(child_pid: u32 << 32) | (exit_code as i32 as u32)`. Written by
/// [`raise_sigchld_for_parent`] immediately before pending signal 17; read by
/// `try_deliver_signal` when it builds the `siginfo_t` for a delivered SIGCHLD,
/// so an `SA_SIGINFO` handler sees a real `si_pid`/`si_status` instead of zeros.
///
/// Last-writer-wins under a burst: `PENDING_SIGNALS` is a bitmask, so N child
/// exits already collapse to one delivered SIGCHLD — matching Linux for
/// non-realtime signals, and why correct reapers loop on `waitpid(WNOHANG)`
/// rather than counting signals. Peek (load) rather than take (swap) so that a
/// signal which re-pends (e.g. `SA_ONSTACK` before `sigaltstack`) still finds
/// its payload on the next delivery attempt.
static LAST_SIGCHLD: [AtomicU64; MAX_THREADS] = {
    const INIT: AtomicU64 = AtomicU64::new(0);
    [INIT; MAX_THREADS]
};

/// Per-thread deferred-kill request (real shared-kernel SMP).
///
/// Set by [`request_thread_kill`] when a peer core's `kill_thread_group` wants
/// this thread dead. The thread self-terminates at its next EL1→EL0 boundary
/// ([`take_thread_kill_request`], checked in the sync-EL0 exception wrapper),
/// AFTER every kernel lock held by the unwound call stack has been released —
/// never while it may hold a spinlock. Hard-marking a sibling `TERMINATED`
/// while it was preempted mid-critical-section (e.g. holding `BLOCK_DEVICE`
/// during a demand-paging disk read) stranded the lock forever, freezing every
/// later disk-dependent path (the sshd "freeze"; see
/// `docs/runbooks/debug-smp-go-stress-corruption.md`).
static PENDING_KILL: [AtomicBool; MAX_THREADS] = {
    const INIT: AtomicBool = AtomicBool::new(false);
    [INIT; MAX_THREADS]
};

/// Arm the restore-sigmask for the current thread: the next delivered signal's
/// frame saves `saved` as `uc_sigmask` (so `sigreturn` restores it).
pub fn set_restore_sigmask(saved: u64) {
    let tid = current_thread_id();
    if tid < MAX_THREADS {
        THREAD_RESTORE_SIGMASK[tid].store(saved, Ordering::Release);
        THREAD_RESTORE_SIGMASK_PENDING[tid].store(true, Ordering::Release);
    }
}

/// Consume the current thread's armed restore-sigmask, if any. Returns the saved
/// mask and clears the pending flag; `None` if not armed.
pub fn take_restore_sigmask() -> Option<u64> {
    let tid = current_thread_id();
    if tid < MAX_THREADS && THREAD_RESTORE_SIGMASK_PENDING[tid].swap(false, Ordering::AcqRel) {
        Some(THREAD_RESTORE_SIGMASK[tid].load(Ordering::Acquire))
    } else {
        None
    }
}

/// Record the payload for a SIGCHLD about to be pended on `slot`: the exiting
/// child's pid and its raw exit code (negative ⇒ killed by signal `-code`).
/// Called by [`crate::process::raise_sigchld_for_parent`] immediately before
/// `pend_signal_for_thread`, so delivery reads a consistent `(pid, code)`.
pub fn set_last_sigchld(slot: usize, child_pid: u32, exit_code: i32) {
    if slot < MAX_THREADS {
        let packed = ((child_pid as u64) << 32) | (exit_code as u32 as u64);
        LAST_SIGCHLD[slot].store(packed, Ordering::Release);
    }
}

/// Peek (do NOT consume) the last SIGCHLD payload pended on `slot`. Returns
/// `(child_pid, exit_code)` if one was ever recorded, else `None`. Delivery
/// peeks rather than takes so an `SA_ONSTACK` re-pend still finds its payload.
pub fn peek_last_sigchld(slot: usize) -> Option<(u32, i32)> {
    if slot < MAX_THREADS {
        let packed = LAST_SIGCHLD[slot].load(Ordering::Acquire);
        if packed != 0 {
            return Some(((packed >> 32) as u32, packed as u32 as i32));
        }
    }
    None
}

/// Per-thread alternate signal stack base address (0 = not set).
/// Indexed by kernel thread slot so each CLONE_VM thread has its own sigaltstack.
static THREAD_SIGALTSTACK_SP: [AtomicU64; MAX_THREADS] = {
    const INIT: AtomicU64 = AtomicU64::new(0);
    [INIT; MAX_THREADS]
};

/// Per-thread alternate signal stack size.
static THREAD_SIGALTSTACK_SIZE: [AtomicU64; MAX_THREADS] = {
    const INIT: AtomicU64 = AtomicU64::new(0);
    [INIT; MAX_THREADS]
};

/// Per-thread alternate signal stack flags (SS_DISABLE=2 means disabled).
/// Stored as u32 but semantically i32 (cast on read/write).
static THREAD_SIGALTSTACK_FLAGS: [AtomicU32; MAX_THREADS] = {
    const INIT: AtomicU32 = AtomicU32::new(2); // SS_DISABLE
    [INIT; MAX_THREADS]
};

/// Per-thread `ITIMER_REAL` deadline in uptime microseconds (0 = disarmed).
/// Owned here (not by `src/syscall/time.rs`, the only caller) so that
/// [`scrub_thread_slot`] — the single place a recycled slot's per-occupant
/// state gets cleared — can reset it like every other per-slot register.
/// Before this lived here, a slot that last held a process which armed
/// `alarm()`/`setitimer()` and then exited without disarming it (e.g. busybox
/// `wget -T`, which implements its timeout via `alarm()`) kept ticking after
/// the process was gone. The next unrelated process to land in that reused
/// slot inherited an already-long-expired deadline and got SIGALRM delivered
/// as fatal (no handler installed yet) at its very first timer tick —
/// `docs/archive/GIT_CLONE_STALE_ITIMER_SIGALRM.md`.
static ITIMER_DEADLINE: [AtomicU64; MAX_THREADS] = {
    const INIT: AtomicU64 = AtomicU64::new(0);
    [INIT; MAX_THREADS]
};

/// Per-thread `ITIMER_REAL` periodic interval in microseconds (0 = one-shot).
static ITIMER_INTERVAL: [AtomicU64; MAX_THREADS] = {
    const INIT: AtomicU64 = AtomicU64::new(0);
    [INIT; MAX_THREADS]
};

/// Read a thread slot's `(deadline, interval)` ITIMER_REAL state, in uptime
/// microseconds. `(0, 0)` means disarmed.
pub fn get_itimer(tid: usize) -> (u64, u64) {
    if tid < MAX_THREADS {
        (
            ITIMER_DEADLINE[tid].load(Ordering::Relaxed),
            ITIMER_INTERVAL[tid].load(Ordering::Relaxed),
        )
    } else {
        (0, 0)
    }
}

/// Set a thread slot's ITIMER_REAL `(deadline, interval)`, in uptime
/// microseconds. `deadline = 0` disarms (interval is stored regardless, matching
/// `setitimer`'s "value 0 disarms, interval is still recorded" contract).
pub fn set_itimer(tid: usize, deadline: u64, interval: u64) {
    if tid < MAX_THREADS {
        ITIMER_DEADLINE[tid].store(deadline, Ordering::Relaxed);
        ITIMER_INTERVAL[tid].store(interval, Ordering::Relaxed);
    }
}

/// Current running thread - stored in TPIDRRO_EL0 register
/// Using a CPU register avoids race conditions with global atomics.
/// TPIDRRO_EL0 is accessible from EL1 and provides per-CPU thread tracking.
/// It is read-only from EL0 (user mode), which is fine as userspace shouldn't
/// need to modify its own thread ID directly.

/// Set the current thread ID in TPIDRRO_EL0
#[cfg(target_os = "none")]
#[inline]
fn set_current_thread_register(tid: usize) {
    unsafe {
        core::arch::asm!("msr tpidrro_el0, {}", in(reg) tid as u64);
    }
    // BKL-hold profiler only (no-op unless `bkl-profile` turned it on; absent entirely
    // outside `smp-shared`). This is THE choke point for "the current thread changed" —
    // every switch path funnels through it (`commit_switch`, the network-thread boost in
    // `schedule_indices`, per-core idle adoption, boot) — so re-pointing the per-core
    // sampling cache here is what makes BKL attribution follow the thread across
    // preemption and cross-core migration instead of being smeared into `irq/sched`.
    // See `crate::sync::ThreadTagTable`.
    #[cfg(kernel_smp_shared)]
    crate::sync::load_thread_tag_to_core(crate::bkl::current_core_id(), tid);
}

#[cfg(not(target_os = "none"))]
#[inline]
fn set_current_thread_register(_tid: usize) {}

/// Get the current thread ID from TPIDRRO_EL0.
///
/// The *read* lives in `akuma_primitives::preempt::current_tid` — it is a
/// bounds-checked `mrs` with no scheduler state behind it, and moving it is what
/// let the preemption counters move too. The *write*
/// (`set_current_thread_register`, above) deliberately stays here: it also
/// re-points the per-core BKL attribution cache, which is scheduler business.
#[inline]
fn get_current_thread_register() -> usize {
    akuma_primitives::preempt::current_tid()
}

/// Reset every per-slot register to its power-on value, so a recycled slot cannot leak
/// its previous occupant's state into the next thread.
///
/// This exists because the scrub lists had drifted apart: `claim_free_slot` cleared the
/// signal mask, the restore-mask flag and the trap frame; the direct claim in
/// `ThreadPool::spawn_user_closure_initializing` (the path *every real `pthread_create`*
/// takes) cleared only the trap frame; and `cleanup_terminated_internal` cleared a third,
/// different set. Anything on one list and not another was inherited. The worst of those
/// was `THREAD_SIGNAL_MASK`: a `clone_thread` slot inherited the dead thread's blocked
/// set, so a signal the new thread never blocked was silently never delivered.
///
/// Call it from every path that takes a slot FREE → INITIALIZING, and once more when a
/// slot goes back to FREE. Adding per-slot state? Add it here, not to a call site.
///
/// Deliberately NOT reset here:
/// - `THREAD_STATES` — the caller's CAS owns the state machine.
/// - `IS_IDLE_THREAD` — a permanent property of the per-core idle slots, not occupant state.
/// - `ON_CPU` — owned by the scheduler's run/parked bookkeeping.
/// - `THREAD_CONTEXTS` — bulk register file, zeroed by its own `Context::zero()`.
#[inline]
fn scrub_thread_slot(i: usize) {
    if i >= MAX_THREADS { return; }

    // Signal state. A stale mask or restore-mask is invisible until a signal is
    // *not* delivered, which reads as a hang rather than an error.
    PENDING_SIGNALS[i].store(0, Ordering::Release);
    PENDING_KILL[i].store(false, Ordering::Release);
    THREAD_SIGNAL_MASK[i].store(0, Ordering::Release);
    THREAD_RESTORE_SIGMASK[i].store(0, Ordering::Release);
    THREAD_RESTORE_SIGMASK_PENDING[i].store(false, Ordering::Release);
    LAST_SIGCHLD[i].store(0, Ordering::Release);
    THREAD_SIGALTSTACK_SP[i].store(0, Ordering::Release);
    THREAD_SIGALTSTACK_SIZE[i].store(0, Ordering::Release);
    THREAD_SIGALTSTACK_FLAGS[i].store(2, Ordering::Release); // SS_DISABLE

    // ITIMER_REAL (alarm()/setitimer()). A stale non-zero deadline here outlives
    // its process (e.g. exit without disarming) and fires as an immediate,
    // fatal SIGALRM against whatever unrelated process next claims this slot —
    // docs/archive/GIT_CLONE_STALE_ITIMER_SIGALRM.md.
    ITIMER_DEADLINE[i].store(0, Ordering::Release);
    ITIMER_INTERVAL[i].store(0, Ordering::Release);

    // Blocking/scheduling state. `WOKEN_STATES` is the sticky wake flag consumed on
    // entry to `schedule_blocking` — a stale `true` spends a phantom wake on the new
    // occupant's first park.
    WOKEN_STATES[i].store(false, Ordering::Release);
    WAKE_TIMES[i].store(0, Ordering::Release);
    // The three preemption records live in `akuma_primitives::preempt` now; a
    // recycled slot inheriting a non-zero disable count would be a thread the
    // scheduler silently never preempts.
    akuma_primitives::preempt::scrub_slot(i);

    // Kernel-entry state. A stale `USER_COPY_FAULT_HANDLER` is a fixup address into a
    // syscall frame that no longer exists — a fault before the new occupant installs
    // its own would jump there.
    CURRENT_TRAP_FRAME[i].store(0, Ordering::Release);
    USER_COPY_FAULT_HANDLER[i].store(0, Ordering::Release);

    // TTBR0 tripwire baseline: unknown until the new occupant's context is written
    // (a stale expected-L0 would flag the new occupant's first switch as a mismatch).
    EXPECTED_L0[i].store(0, Ordering::Release);
    SWITCH_INS[i].store(0, Ordering::Relaxed);

    // New incarnation: every WakeHandle minted for the previous occupant goes
    // stale here, before the new occupant is observable. Exactly one bumper per
    // rebirth — scrub runs under the claim path's winning FREE→INITIALIZING CAS.
    SLOT_GEN[i].fetch_add(1, Ordering::AcqRel);

    // Diagnostics. Stale values here are not a correctness bug but a *debugging* one:
    // `[THR-DUMP]`/`[PSTATS]` would attribute the previous occupant's syscall and core
    // to a thread that never made it, which is exactly the kind of evidence a hang hunt
    // reads as ground truth.
    THREAD_CURRENT_SYSCALL[i].store(u64::MAX, Ordering::Release);
    LAST_CORE[i].store(0xFF, Ordering::Release);
    TOTAL_CPU_TIMES[i].store(0, Ordering::Release);
    TERMINATION_TIME[i].store(0, Ordering::Release);

    // Reclaim re-entrancy guard. A thread killed inside `drain_retired` leaves this set,
    // and the next occupant then takes the "already draining" early return forever.
    crate::process::reclaim::clear_draining(i);

    // Profiler attribution: without this the new thread lends the previous occupant's
    // tag to any peer sampling this slot before its first kernel entry.
    #[cfg(kernel_smp_shared)]
    crate::sync::reset_thread_tag(i);
}

/// Atomically claim a free slot in the given range
/// Returns the slot index if successful, None if no free slots
/// NOTE: Sets state to INITIALIZING, not READY - caller must set to READY after context setup!
fn claim_free_slot(start: usize, end: usize) -> Option<usize> {
    for i in start..end {
        // Try to atomically change FREE -> INITIALIZING
        // We use INITIALIZING (not READY) to prevent scheduler from picking up
        // the thread before its context is fully set up
        if THREAD_STATES[i]
            .compare_exchange(
                thread_state::FREE,
                thread_state::INITIALIZING,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_ok()
        {
            // A slot may reach FREE by a route that skipped the recycler, so scrub every
            // per-slot register here rather than trusting the previous owner's teardown.
            // Clone/fork re-seed what they need afterwards (POSIX inheritance).
            scrub_thread_slot(i);
            return Some(i);
        }
    }
    None
}

/// Ensure a just-claimed slot has a stack allocated (lazy stacks). A no-op when
/// the stack is already present — always the case on release (every slot is
/// pre-allocated) and for warm-floor slots on the size profile. Returns false if
/// the PMM cannot back the stack, in which case the caller must release the slot.
fn ensure_slot_stack(slot_idx: usize, size: usize) -> bool {
    let _guard = IrqGuard::new();
    let mut pool = POOL.lock();
    if pool.stacks[slot_idx].is_allocated() {
        return true;
    }
    pool.allocate_stack_for_slot(slot_idx, size)
}

/// Per-thread termination timestamp (for cooldown tracking)
static TERMINATION_TIME: [AtomicU64; MAX_THREADS] = {
    const INIT: AtomicU64 = AtomicU64::new(0);
    [INIT; MAX_THREADS]
};

/// Cross-thread terminations seen so far; rate-limits the tracer in
/// [`mark_thread_terminated`].
static CROSS_KILLS: AtomicUsize = AtomicUsize::new(0);

/// Rate-limits the `[TERM]` site tracer in [`mark_thread_terminated`].
static TERM_TRACES: AtomicUsize = AtomicUsize::new(0);

/// Mark a thread as terminated (lock-free)
///
/// `#[track_caller]` so the tracer below can name the *site* that killed a slot.
/// There are a dozen call sites across teardown, signals, spawn failure and the
/// trampoline, and "which one ran" is the first question every "process survives
/// with no thread" hang asks. Propagated through [`mark_current_terminated`].
#[track_caller]
pub fn mark_thread_terminated(idx: usize) {
    if idx != IDLE_THREAD_IDX && idx < MAX_THREADS {
        // Attribute EVERY termination to its site, not just cross-thread ones.
        // The `[kill]` tracer below fires only when `killer != idx`, which under
        // `kernel_smp_shared` misses the whole deferred-kill path: PHASE 1 posts
        // `request_thread_kill` and the victim self-marks at its EL1→EL0 boundary,
        // so `killer == idx` and a thread killed from outside dies with no trace
        // at all. That blind spot is why a "zero `[kill]` lines" reading cannot be
        // used to rule out an external kill (docs/runbooks/debug-futex-lost-wakeup.md §4).
        // Owner comes from THREAD_PID_MAP, never the `p.thread_id` table scan.
        if TERM_TRACES.fetch_add(1, Ordering::Relaxed) < 4096 {
            let loc = core::panic::Location::caller();
            let killer = get_current_thread_register();
            safe_print!(224,
                "[TERM] tid={} pid={:?} by_tid={} state={} pending_kill={} at={}:{}\n",
                idx, crate::process::table::pid_for_thread(idx), killer,
                THREAD_STATES[idx].load(Ordering::SeqCst),
                PENDING_KILL[idx].load(Ordering::Acquire),
                loc.file(), loc.line());
        }
        // Cross-thread kill tracer: whoever terminates a slot that is not its own
        // is killing a thread that may since have been recycled to an unrelated
        // process. Prints the victim's owning pid and its live state so a
        // "process survives with no thread" hang can be attributed to its killer.
        //
        // A thread killed from outside never runs its own exit epilogue, so anything
        // only *its* userspace publishes is lost — most visibly musl's
        // `pthread_join`, which parks on `&t->detach_state` and has no kernel-side
        // substitute. That is a hang with no futex-table evidence at all (the waiter
        // stays correctly queued forever), so this line is the only place it becomes
        // attributable to a killer. Normal `exit_group` teardown also lands here, so
        // it is rate-limited rather than gated: the first few are the interesting ones.
        let killer = get_current_thread_register();
        if killer != idx
            && CROSS_KILLS.fetch_add(1, Ordering::Relaxed) < 32
            && let Some(victim_pid) = crate::process::find_pid_by_thread(idx)
        {
            safe_print!(160,
                "[kill] tid={} (pid={}) terminated by tid={} (pid={}) victim_state={}\n",
                idx, victim_pid, killer,
                crate::process::find_pid_by_thread(killer).unwrap_or(0),
                THREAD_STATES[idx].load(Ordering::SeqCst));
        }
        // Record termination time for cooldown tracking
        TERMINATION_TIME[idx].store((runtime().uptime_us)(), Ordering::SeqCst);
        THREAD_STATES[idx].store(thread_state::TERMINATED, Ordering::SeqCst);

        // Drop cross-subsystem tid registrations NOW, not at recycle. The slot stays
        // TERMINATED for at least the cooldown (10 ms) and often far longer under load,
        // and for that entire window a tid left in `FUTEX_WAITERS` is still a wake target:
        // `futex_do_wake` pops it, counts it toward `max_wake`, and wakes a thread that is
        // never going to run again — so a `FUTEX_WAKE(uaddr, 1)` is consumed and the real
        // waiter stays parked. Purging at recycle alone would leave that window open.
        //
        // Safe to drop this early precisely because a queue entry is of no further use to
        // a terminated thread — unlike its trap frame, kernel stack or sigaltstack, which
        // the terminal park may still touch and which therefore stay until the recycler.
        // Holds no lock of this module, so the hook may take its own.
        let purge_addr = SLOT_PURGE_CALLBACK.load(Ordering::Relaxed);
        if purge_addr != 0 {
            let purge: fn(usize) = unsafe { core::mem::transmute(purge_addr) };
            purge(idx);
        }
    }
}

/// Request that thread `tid` terminate itself at its next kernel-exit boundary
/// (real shared-kernel SMP). The thread is NOT marked `TERMINATED` here — it
/// stays schedulable so it can finish any in-flight kernel work, release every
/// held lock, and then self-terminate at the EL1→EL0 boundary
/// ([`take_thread_kill_request`], checked in the sync-EL0 handler wrapper).
/// Hard-terminating a thread preempted mid-critical-section leaks its locks
/// (the sshd "freeze" root cause).
///
/// Wakes the thread if parked so it reaches a boundary promptly. No-op for the
/// idle thread and out-of-range slots.
pub fn request_thread_kill(tid: usize) {
    if tid != IDLE_THREAD_IDX && tid < MAX_THREADS {
        PENDING_KILL[tid].store(true, Ordering::Release);
        get_waker_for_thread(tid).wake();
    }
}

/// Atomically take (clear) the current thread's deferred-kill request. Returns
/// `true` if a kill was pending — the caller (the sync-EL0 handler wrapper)
/// must then self-terminate instead of returning to EL0.
pub fn take_thread_kill_request() -> bool {
    take_kill_request_via_tid(get_current_thread_register())
}

/// Tid-explicit core of [`take_thread_kill_request`]. Also used by host tests
/// and the kernel self-test (where `current_thread_id()` is always the idle
/// thread 0 on host, or not the target tid in a runtime self-test).
pub fn take_kill_request_via_tid(tid: usize) -> bool {
    if tid < MAX_THREADS {
        PENDING_KILL[tid].swap(false, Ordering::AcqRel)
    } else {
        false
    }
}

/// `true` if a deferred-kill request is still pending for `tid` (not yet
/// consumed by the target reaching its boundary). Used by `kill_thread_group`'s
/// grace-wait loop to know when a sibling has acted on the request.
pub fn has_pending_kill(tid: usize) -> bool {
    tid < MAX_THREADS && PENDING_KILL[tid].load(Ordering::Acquire)
}

/// Mark a thread as ready (lock-free). The publish half of the spawn protocol:
/// the owner (clone/fork/vfork) calls this exactly once, after the slot's
/// context is fully written, to flip it INITIALIZING -> READY.
///
/// Refuses to overwrite TERMINATED: `kill_thread_group` /
/// `mark_thread_terminated` run cross-thread with no lock, so a group kill can
/// land on a half-built child between context setup and this publish. A plain
/// READY store would resurrect it — a peer core would then run a thread whose
/// process teardown (address space, fds) is already in progress.
pub fn mark_thread_ready(idx: usize) {
    if idx < MAX_THREADS {
        let _ = THREAD_STATES[idx].fetch_update(
            Ordering::SeqCst,
            Ordering::SeqCst,
            |s| (s != thread_state::TERMINATED).then_some(thread_state::READY),
        );
    }
}

/// Spawn a user thread but keep it in INITIALIZING state
pub fn spawn_user_thread_initializing(
    trampoline_fn: extern "C" fn() -> !,
    data_ptr: *mut (),
) -> Result<usize, &'static str> {
    let trampoline_casted = unsafe {
        core::mem::transmute::<extern "C" fn() -> !, fn(*mut ()) -> !>(trampoline_fn)
    };

    let attempt = || {
        with_irqs_disabled(|| {
            let mut pool = POOL.lock();
            pool.spawn_user_closure_initializing(trampoline_casted, data_ptr)
        })
    };

    // On slot exhaustion, collect cooled-down terminated slots ourselves and retry
    // once before failing — the pool is usually not exhausted, just uncollected (see
    // `reclaim_terminated_slots`). This is the same fallback
    // `spawn_user_thread_fn_internal` / `spawn_system_thread_fn` already do; without
    // it, this path — every fork/vfork/clone_thread, i.e. every real pthread_create —
    // reports EAGAIN to userspace while the slots it needs sit TERMINATED waiting for
    // an idle loop that a busy system never reaches. A tight
    // pthread_create/pthread_join loop failed deterministically at ~iteration 58 of
    // 200 with MAX_THREADS=64.
    //
    // The reclaim must happen HERE and not inside `spawn_user_closure_initializing`:
    // that runs with the POOL lock held, and `reclaim_terminated_slots` takes POOL
    // itself, so calling it there would deadlock on the non-reentrant spinlock.
    match attempt() {
        Ok(slot) => {
            note_user_thread_highwater();
            Ok(slot)
        }
        Err("No free user thread slots") => {
            // Census BEFORE the reclaim, so the log distinguishes the two very different
            // states that produce the same error: genuinely full (`live` at the ceiling)
            // versus merely uncollected (`terminated` holding the slots). Only the first
            // is a real capacity limit.
            // NB: `free` is 0 by construction here — this arm only runs because the scan
            // just failed to find a FREE slot. The diagnostic value is the live/terminated
            // split: `terminated` high means the slots exist and are merely uncollected
            // (collection is lazy — TERMINATED→FREE needs the cooldown AND someone to run
            // a reclaim pass), whereas `live` at the ceiling is a genuine capacity limit.
            let (live, terminated, _free) = user_slot_census();
            let reclaimed = reclaim_terminated_slots();
            if reclaimed == 0 {
                safe_print!(192,
                    "[threads] SPAWN FAILED: live={} terminated={} ceiling={} — nothing \
                     reclaimable (high-water {})\n",
                    live, terminated, thread_limit() - config().reserved_threads,
                    USER_THREAD_HIGHWATER.load(Ordering::Relaxed));
                return Err("No free user thread slots");
            }
            safe_print!(192,
                "[threads] slots exhausted (live={} terminated={} ceiling={}) — reclaimed {} \
                 and retrying\n",
                live, terminated, thread_limit() - config().reserved_threads, reclaimed);
            let r = attempt();
            if r.is_ok() {
                note_user_thread_highwater();
            }
            r
        }
        other => other,
    }
}

/// Highest number of simultaneously-live user thread slots seen this boot.
static USER_THREAD_HIGHWATER: AtomicUsize = AtomicUsize::new(0);

/// Count user slots (i.e. excluding the reserved system range) by state.
/// Returns `(live, terminated, free)`, where `live` is anything occupied and not yet
/// terminated — the number that actually competes for the ceiling.
fn user_slot_census() -> (usize, usize, usize) {
    let (mut live, mut terminated, mut free) = (0, 0, 0);
    for i in config().reserved_threads..thread_limit() {
        match THREAD_STATES[i].load(Ordering::Relaxed) {
            thread_state::FREE => free += 1,
            thread_state::TERMINATED => terminated += 1,
            _ => live += 1,
        }
    }
    (live, terminated, free)
}

/// Record a new simultaneous-live-thread high-water mark, logging each time it rises.
/// This is how many threads this kernel actually sustained at once, as opposed to the
/// `MAX_THREADS` ceiling — the two diverge because a TERMINATED slot still holds its
/// index for a ~10 ms cooldown, so a spawn-heavy workload runs out of *usable* slots
/// well before `live` reaches the ceiling.
fn note_user_thread_highwater() {
    let (live, terminated, free) = user_slot_census();
    let prev = USER_THREAD_HIGHWATER.load(Ordering::Relaxed);
    if live > prev
        && USER_THREAD_HIGHWATER
            .compare_exchange(prev, live, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
    {
        safe_print!(160,
            "[threads] new high-water: {} live user threads (terminated={} free={} ceiling={})\n",
            live, terminated, free, thread_limit() - config().reserved_threads);
    }
}

impl ThreadPool {
    /// Internal helper to spawn a user closure without marking it READY
    pub fn spawn_user_closure_initializing(
        &mut self,
        trampoline_fn: fn(*mut ()) -> !,
        closure_ptr: *mut (),
    ) -> Result<usize, &'static str> {
        if !self.initialized { return Err("Thread pool not initialized"); }

        for i in config().reserved_threads..thread_limit() {
            if THREAD_STATES[i].load(Ordering::SeqCst) == thread_state::FREE {
                // Claim the slot atomically
                if THREAD_STATES[i].compare_exchange(thread_state::FREE, thread_state::INITIALIZING, Ordering::SeqCst, Ordering::SeqCst).is_err() {
                    continue;
                }

                // This path claims a slot directly instead of via `claim_free_slot`, and
                // it is the one every real `pthread_create` takes — so it must run the
                // same scrub. It previously repeated only the trap-frame clear, which is
                // why a cloned thread inherited the dead occupant's blocked signal mask.
                scrub_thread_slot(i);

                // Lazy stacks: on the extreme profile WARM_FREE_USER=0, so a freshly
                // claimed slot has no stack (StackInfo::empty(), top=0). Allocate it on
                // demand before computing stack_top — otherwise `0 - EXCEPTION_STACK_SIZE`
                // wraps to a near-null VA and setup_fake_irq_frame's write_bytes faults in
                // EL1 (EC=0x25). The POOL lock is already held here, so call the
                // lock-free allocate_stack_for_slot directly (not ensure_slot_stack).
                if !self.stacks[i].is_allocated()
                    && !self.allocate_stack_for_slot(i, config().user_thread_stack_size)
                {
                    THREAD_STATES[i].store(thread_state::FREE, Ordering::SeqCst);
                    return Err("Failed to allocate user thread stack from PMM");
                }

                let stack = &self.stacks[i];
                let stack_top = ((stack.top - EXCEPTION_STACK_SIZE) & !0xF) as u64;
                let boot_ttbr0 = crate::mmu::get_boot_ttbr0();

                let sp = setup_fake_irq_frame(
                    stack_top,
                    thread_start_closure as *const () as u64,
                    trampoline_fn as *const () as u64,
                    closure_ptr as u64,
                    0,
                );

                unsafe {
                    let ctx = &mut *get_context_mut(i);
                    ctx.magic = CONTEXT_MAGIC;
                    ctx.sp = sp;
                    ctx.ttbr0 = boot_ttbr0;
                    ctx.x19 = trampoline_fn as *const () as u64;
                    ctx.x20 = closure_ptr as u64;
                    ctx.x30 = thread_start_closure as *const () as u64;
                    ctx.elr = thread_start_closure as *const () as u64;
                    ctx.spsr = 0x00000345;
                    ctx.is_user_process = 1; // Mark as user process thread
                }

                self.slots[i].start_time_us = 0;

                // NOTE: We do NOT store READY here. Caller must call mark_thread_ready().
                return Ok(i);
            }
        }
        Err("No free user thread slots")
    }
}

/// Mark a thread as running (lock-free)
#[track_caller]
fn mark_thread_running(idx: usize) {
    if idx < MAX_THREADS {
        let prev = THREAD_STATES[idx].swap(thread_state::RUNNING, Ordering::SeqCst);
        if prev == thread_state::TERMINATED {
            let loc = core::panic::Location::caller();
            safe_print!(160, "[REVIVE] tid={} TERMINATED->RUNNING at={}:{} by_tid={}\n",
                idx, loc.file(), loc.line(), get_current_thread_register());
        }
    }
}

/// Mark a thread as waiting with a wake time (lock-free)
#[track_caller]
fn mark_thread_waiting(idx: usize, wake_time_us: u64) {
    if idx < MAX_THREADS {
        WAKE_TIMES[idx].store(wake_time_us, Ordering::SeqCst);
        let prev = THREAD_STATES[idx].swap(thread_state::WAITING, Ordering::SeqCst);
        if prev == thread_state::TERMINATED {
            // F8 evidence tripwire: an unconditional WAITING publication can
            // overwrite a cross-thread TERMINATED (the reap-mid-exit race); a
            // waker's WAITING->READY CAS then completes a resurrection the
            // guarded paths (`mark_thread_ready`, `commit_switch`,
            // `resume_running_unless_terminated`) all refuse.
            let loc = core::panic::Location::caller();
            safe_print!(160, "[REVIVE] tid={} TERMINATED->WAITING at={}:{} by_tid={}\n",
                idx, loc.file(), loc.line(), get_current_thread_register());
        }
    }
}

/// Get thread wake time (lock-free read)
fn get_wake_time(idx: usize) -> u64 {
    if idx < MAX_THREADS {
        WAKE_TIMES[idx].load(Ordering::Relaxed)
    } else {
        0
    }
}

/// Get thread state (lock-free read)
pub fn get_thread_state(idx: usize) -> u8 {
    if idx < MAX_THREADS {
        THREAD_STATES[idx].load(Ordering::SeqCst)
    } else {
        thread_state::FREE
    }
}

/// Check if a thread is terminated or freed (lock-free).
/// Returns true if the thread is dead (TERMINATED or FREE state).
/// Used for orphaned lock detection - a thread holding a lock is considered
/// dead if it's terminated or if its slot has been reclaimed.
pub fn is_thread_terminated(thread_id: usize) -> bool {
    let state = get_thread_state(thread_id);
    state == thread_state::TERMINATED || state == thread_state::FREE
}

/// Test helper: set a thread's state directly (lock-free).
pub fn set_thread_state(idx: usize, state: u8) {
    if idx < MAX_THREADS {
        THREAD_STATES[idx].store(state, Ordering::SeqCst);
    }
}

/// Test-only: atomically claim up to `n` genuinely-FREE user thread slots so a
/// test can use them as sibling thread IDs without corrupting real threads.
///
/// Hardcoding fake TIDs is unsafe two ways: low TIDs (< reserved_threads) collide
/// with live system threads, and TIDs >= MAX_THREADS are silently ignored by
/// `mark_thread_terminated` / `get_thread_state` (so the slot's state can never be
/// observed). Claimed slots are parked in INITIALIZING — never dispatched by the
/// scheduler and never handed out by `spawn_*` (which only takes FREE). Release
/// each slot with `release_test_thread_slot` when the test finishes.
pub fn claim_test_thread_slots(n: usize) -> Vec<usize> {
    let mut out = Vec::new();
    for i in config().reserved_threads..thread_limit() {
        if out.len() == n { break; }
        if THREAD_STATES[i]
            .compare_exchange(
                thread_state::FREE,
                thread_state::INITIALIZING,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_ok()
        {
            out.push(i);
        }
    }
    out
}

/// Test-only: return a slot claimed by `claim_test_thread_slots` to the FREE pool.
pub fn release_test_thread_slot(idx: usize) {
    if idx < MAX_THREADS {
        THREAD_STATES[idx].store(thread_state::FREE, Ordering::SeqCst);
    }
}

/// Test helper: get the sticky woken flag for a thread.
pub fn get_woken_state(idx: usize) -> bool {
    if idx < MAX_THREADS {
        WOKEN_STATES[idx].load(Ordering::SeqCst)
    } else {
        false
    }
}

/// Test helper: set the sticky woken flag for a thread.
pub fn set_woken_state(idx: usize, val: bool) {
    if idx < MAX_THREADS {
        WOKEN_STATES[idx].store(val, Ordering::SeqCst);
    }
}

/// Get the last core a thread ran on (MPIDR aff0). 0xFF = never scheduled.
pub fn get_thread_last_core(idx: usize) -> u8 {
    if idx < MAX_THREADS {
        LAST_CORE[idx].load(Ordering::Relaxed)
    } else {
        0xFF
    }
}

/// Role name for a KERNEL thread — one with no owning userspace process — for display
/// in `top` / cpu stats. Without this such threads (per-core idle, the network poller,
/// other system threads) show up blank. Lock-free: reads only atomics + `config()`, so
/// it is safe to call from `sys_get_cpu_stats`, which must not take `POOL.lock` (see the
/// `USER_COPY_FAULT_HANDLER` note above).
pub fn kernel_thread_name(idx: usize) -> &'static str {
    if idx >= MAX_THREADS {
        return "?";
    }
    if idx == 0 {
        return "kernel";
    }
    if IS_IDLE_THREAD[idx].load(Ordering::Relaxed) {
        return "idle";
    }
    if idx == NETWORK_THREAD_ID.load(Ordering::Relaxed) {
        return "network";
    }
    if idx < config().reserved_threads {
        return "system";
    }
    "kernel-thread"
}

/// Get total CPU time for a thread in microseconds
pub fn get_thread_cpu_time(idx: usize) -> u64 {
    if idx < MAX_THREADS {
        let mut total = TOTAL_CPU_TIMES[idx].load(Ordering::Relaxed);
        
        // If the thread is currently running, add the time since it started
        if get_thread_state(idx) == thread_state::RUNNING {
            let start_time = with_irqs_disabled(|| {
                let pool = POOL.lock();
                pool.slots[idx].start_time_us
            });
            if start_time > 0 {
                let now = (runtime().uptime_us)();
                total += now.saturating_sub(start_time);
            }
        }
        total
    } else {
        0
    }
}

/// Count free slots in range (lock-free)
fn count_free_slots(start: usize, end: usize) -> usize {
    (start..end)
        .filter(|&i| THREAD_STATES[i].load(Ordering::Relaxed) == thread_state::FREE)
        .count()
}

/// Cleanup terminated threads - atomically mark as free (lock-free)
/// Returns number of threads cleaned up
///
/// When DEFERRED_THREAD_CLEANUP is enabled:
/// - Only cleans up if called from thread 0 (main thread)
/// - Respects THREAD_CLEANUP_COOLDOWN_US before recycling slots
pub fn cleanup_terminated_lockfree() -> usize {
    cleanup_terminated_internal(false, false)
}

/// Force cleanup of terminated threads - bypasses thread check and cooldown
/// Use for tests or when you know it's safe to recycle immediately
pub fn cleanup_terminated_force() -> usize {
    cleanup_terminated_internal(true, true)
}

/// Reclaim cooled-down terminated slots **from any thread**, keeping the cooldown.
///
/// The deferred-cleanup design has one collector — thread 0 — and its only
/// steady-state caller is thread 0's *idle* loop, which by definition does not run
/// while the system is busy. Under sustained process churn that starves reclamation
/// completely: slots sat TERMINATED for a measured p50 of 24 s (max 192 s) against a
/// 10 ms cooldown, `fork` stalled for minutes, and spawns eventually failed with
/// "No free user thread slots" while gigabytes of RAM were free. See
/// docs/archive/BKL_VFS_CARVE_OUT.md §11.4.
///
/// Dropping only the caller gate is safe; the two things that make recycling correct
/// are kept:
/// - **The cooldown stays.** A thread marks itself TERMINATED while still executing on
///   its own kernel stack and only leaves it at the next context switch. The cooldown —
///   not the caller's identity — is what guarantees it is gone before the slot's stack
///   and context are reused. `cleanup_terminated_force` (tests only) is the one caller
///   that may bypass it.
/// - **The `TERMINATED → INITIALIZING` CAS stays.** It is what actually excludes a
///   concurrent `claim_free_slot`, and it equally serializes two concurrent reclaimers:
///   only one can win a given slot, and the loser skips it.
///
/// The caller's own slot can never be a candidate — a running thread is not TERMINATED.
pub fn reclaim_terminated_slots() -> usize {
    cleanup_terminated_internal(true, false)
}

/// Internal cleanup implementation.
///
/// `any_caller` drops the "only thread 0 collects" gate; `ignore_cooldown` drops the
/// post-termination settling time. They are independent: `reclaim_terminated_slots`
/// takes the first without the second, which is the combination that is safe in
/// production (see its docs).
fn cleanup_terminated_internal(any_caller: bool, ignore_cooldown: bool) -> usize {
    // In deferred mode, only thread 0 collects unless the caller opts out.
    if !any_caller && config().deferred_thread_cleanup {
        let current = get_current_thread_register();
        if current != IDLE_THREAD_IDX {
            // Not main thread - skip cleanup
            return 0;
        }
    }

    let now = (runtime().uptime_us)();
    let mut count = 0;

    for i in 1..MAX_THREADS {
        // Check if thread is terminated
        if THREAD_STATES[i].load(Ordering::SeqCst) != thread_state::TERMINATED {
            continue;
        }

        // The core that switched away from this thread may still be executing on
        // its kernel stack (commit → `mov sp, x0` window). Don't recycle the slot
        // — and above all don't free the stack — until the gate clears; the next
        // pass gets it. See ON_CPU's doc for the race.
        if ON_CPU[i].load(Ordering::SeqCst) != 0 {
            continue;
        }

        // In deferred mode, respect the settling time unless the caller opts out.
        if !ignore_cooldown && config().deferred_thread_cleanup {
            let term_time = TERMINATION_TIME[i].load(Ordering::SeqCst);
            if term_time > 0 && now.saturating_sub(term_time) < config().thread_cleanup_cooldown_us {
                // Thread hasn't been terminated long enough - skip
                continue;
            }
        }

        // CRITICAL: Use INITIALIZING as intermediate state to prevent race with spawn!
        // 
        // Race condition without this:
        // 1. Cleanup: TERMINATED -> FREE
        // 2. Spawn: claim_free_slot sees FREE, changes to INITIALIZING
        // 3. Spawn: sets up context in THREAD_CONTEXTS[i]
        // 4. Cleanup: still running, zeros THREAD_CONTEXTS[i] -> OVERWRITES spawn's context!
        // 5. Spawn: sets state to READY
        // 6. Scheduler: switches to thread with zeroed context -> CRASH
        //
        // Solution: Use INITIALIZING so spawn's claim_free_slot fails while cleanup runs.
        if THREAD_STATES[i]
            .compare_exchange(
                thread_state::TERMINATED,
                thread_state::INITIALIZING,  // Block spawns from claiming this slot
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_ok()
        {
            // Calculate cooldown before clearing
            let term_time = TERMINATION_TIME[i].load(Ordering::SeqCst);
            let cooldown = now.saturating_sub(term_time);
            
            // Clear termination time
            TERMINATION_TIME[i].store(0, Ordering::SeqCst);
            
            // CRITICAL: Zero the context to prevent stale ELR/SPSR/TTBR0 from leaking
            // This prevents a newly spawned thread from accidentally getting user-mode
            // ELR/SPSR from a previous process execution.
            //
            // We must disable IRQs while holding the pool lock to prevent deadlock:
            // if a timer fires while we hold the lock, the SGI handler will try to
            // acquire the same lock and spin forever (single CPU = deadlock).
            {
                let _guard = IrqGuard::new();
                
                // Zero the context in THREAD_CONTEXTS
                unsafe {
                    *get_context_mut(i) = Context::zero();
                }
                
                // Clear slot state
                let mut pool = POOL.lock();
                pool.slots[i].start_time_us = 0;
            }

            // Clear any pending signal from the previous occupant of this slot.
            // Without this, a stale SIGURG (or any signal) pended by a Go goroutine
            // would be delivered to the next process that runs on this thread slot,
            // triggering signal delivery code in an unexpected context (e.g. /bin/hello
            // which never registered a signal handler), causing an EL1 data abort.
            PENDING_SIGNALS[i].store(0, Ordering::Release);
            // Clear a stale deferred-kill request so a recycled slot's next occupant
            // is not wrongly self-terminated at its first EL1→EL0 boundary.
            PENDING_KILL[i].store(false, Ordering::Release);
            // Reset the per-thread preemption-disable records. A thread that died with
            // a disable outstanding (e.g. a lifecycle op that never released — see
            // process/lifecycle.rs "No-return callers") must not poison the slot's next
            // occupant with permanently-deferred preemption. They live in
            // `akuma_primitives::preempt` now; this clears the disabled-at location too,
            // which the old two-store version left stale.
            akuma_primitives::preempt::scrub_slot(i);
            // Drop the dead occupant's EL0 trap-frame pointer. `clear_current_trap_frame`
            // (the SVC epilogue) is the only other clear, and no exit path reaches it:
            // `return_to_kernel` never returns to the epilogue, and a peer-killed thread
            // never runs it at all. The frame lives on THIS slot's kernel stack, which
            // `free_stack_for_slot` below hands back to the PMM — so leaving the entry set
            // arms every reader (`get_saved_user_context`, `current_trap_frame_elr`,
            // `dump_thread_resume_points`, none of which validate) with a pointer into
            // freed memory the moment the slot is recycled. Must precede the stack free.
            CURRENT_TRAP_FRAME[i].store(0, Ordering::Release);
            // Reset per-thread sigaltstack so the next occupant starts clean.
            THREAD_SIGALTSTACK_SP[i].store(0, Ordering::Release);
            THREAD_SIGALTSTACK_SIZE[i].store(0, Ordering::Release);
            THREAD_SIGALTSTACK_FLAGS[i].store(2, Ordering::Release); // SS_DISABLE

            // Lazy stacks (size profile): return this slot's stack to the PMM
            // unless that would drop the warm free-stack floor for its class.
            // Safe here — the thread has terminated and cooled down, so it is no
            // longer executing on this stack. Done BEFORE the canary re-init
            // below, which then naturally skips the now-empty stack (base == 0).
            #[cfg(kernel_profile_extreme)]
            {
                let _guard = IrqGuard::new();
                let mut pool = POOL.lock();
                let reserved = config().reserved_threads;
                let (start, end, floor) = if i < reserved {
                    (1, reserved, WARM_FREE_SYSTEM)
                } else {
                    (reserved, thread_limit(), WARM_FREE_USER)
                };
                // Count stacks already kept warm (FREE + allocated) in this class,
                // excluding slot `i` which is still TERMINATED at this point.
                let warm_free = (start..end)
                    .filter(|&j| {
                        j != i
                            && THREAD_STATES[j].load(Ordering::SeqCst) == thread_state::FREE
                            && pool.stacks[j].is_allocated()
                    })
                    .count();
                if warm_free >= floor {
                    pool.free_stack_for_slot(i);
                }
            }

            // Re-initialize canary for reuse
            if config().enable_stack_canaries {
                // Must disable IRQs when acquiring POOL lock to prevent deadlock
                // if timer fires - SGI handler would try to acquire the same lock
                let stack_base = {
                    let _guard = IrqGuard::new();
                    POOL.lock().stacks[i].base
                };
                if stack_base != 0 {
                    init_stack_canary(stack_base);
                }
            }

            // Invoke cleanup callback (if any)
            let cb_addr = CLEANUP_CALLBACK.load(Ordering::Relaxed);
            if cb_addr != 0 {
                let cb: fn(usize) = unsafe { core::mem::transmute(cb_addr) };
                cb(i);
            }

            // Drop per-tid registrations held by subsystems outside this crate before the
            // slot can be re-claimed — otherwise the next occupant inherits them under the
            // same tid. Same lock context as the callback above (none of this module's
            // locks held), so the hook is free to take its own.
            let purge_addr = SLOT_PURGE_CALLBACK.load(Ordering::Relaxed);
            if purge_addr != 0 {
                let purge: fn(usize) = unsafe { core::mem::transmute(purge_addr) };
                purge(i);
            }
            
            // Clear any dropped-BKL window this slot's late occupant left open. A
            // thread killed while parked inside a converted (BKL-opted-out) syscall
            // never reaches its window close — the ledger is tid-indexed, so the next
            // occupant of this slot would inherit the depth and run its EL1 excursions
            // BKL-free until the EL0-entry tripwire healed it. Must happen before the
            // slot goes FREE, i.e. before any spawn can claim it.
            let _stale_depth = crate::bkl::clear_dropped_windows_for_dead_thread(i);

            // Final scrub before the slot becomes claimable. The individual clears above
            // stay because some are order-sensitive (CURRENT_TRAP_FRAME must precede the
            // stack free); this is the catch-all that keeps the recycler's list from
            // drifting away from the claim paths' again.
            scrub_thread_slot(i);

            // NOW set to FREE - cleanup is complete, spawn can safely claim this slot
            THREAD_STATES[i].store(thread_state::FREE, Ordering::SeqCst);
            
            // Safe print without heap allocation
            safe_print!(128, "[Cleanup] Thread {} recycled after {}us cooldown\n", i, cooldown);
            
            count += 1;
        }
    }
    count
}

// ============================================================================
// Preemption Control (Per-Thread)
// ============================================================================
//
// The per-thread disable counters, the two diagnostic arrays beside them, the
// six accessors and the watchdog all moved to `akuma_primitives::preempt`, and
// are re-exported below so no call site changed.
//
// Why they could move: `PreemptGuard` was the only reason `akuma-ext2` and
// `akuma-net` depended on this crate at all, and the three things the counters
// needed from outside `core` — a console (the FATAL corrupt-tid halt), a clock
// (the 0->1 diagnostic timestamp) and IRQ masking — are all available to a leaf
// crate now. See that module's header, and
// docs/archive/TRIM_FAT_EMBARASSING_DUPLICATIONS.md 5.555.
pub use akuma_primitives::preempt::{
    check_preemption_watchdog, disable_preemption, enable_preemption, is_preemption_disabled,
    preemption_disabled_at, preemption_disabled_count,
};

/// Real shared-kernel SMP: adopt the CURRENT execution context — a secondary core's
/// boot/trampoline context, running on its boot stack — as that core's idle thread, so
/// the one shared scheduler can switch away from it and back like any thread. Claims a
/// free slot, marks it the RUNNING, preemptible idle for `core_id`, points
/// `TPIDRRO_EL0` at it, and registers it via [`register_core_idle`] (so no other core's
/// round-robin scan ever picks it). The context's saved SP/TTBR0 are captured on the
/// first switch-out. Returns the slot, or `None` if no slot is free.
///
/// `exc_stack_top` becomes the slot's exception-stack top (loaded into `TPIDR_EL1` when
/// the scheduler switches back to this idle); `stack_base`/`stack_size` describe the
/// boot stack so `validate_current_sp` recognizes it. Must be called with IRQs masked
/// and before this core enables interrupts.
#[cfg(target_os = "none")]
pub fn adopt_current_as_core_idle(
    core_id: usize,
    exc_stack_top: u64,
    stack_base: usize,
    stack_size: usize,
) -> Option<usize> {
    let _guard = IrqGuard::new();
    let mut pool = POOL.lock();
    let slot = claim_free_slot(1, MAX_THREADS)?;
    pool.slots[slot].start_time_us = (runtime().uptime_us)();
    pool.slots[slot].exception_stack_top = exc_stack_top;
    pool.stacks[slot] = StackInfo::new(stack_base, stack_size);
    // Seed a valid context; sp/ttbr0 are captured on the first switch-out from idle.
    unsafe {
        let ctx = &mut *get_context_mut(slot);
        ctx.magic = CONTEXT_MAGIC;
        ctx.ttbr0 = crate::mmu::get_boot_ttbr0();
        ctx.is_user_process = 0;
    }
    // This core is now the idle thread. Latch its ON_CPU gate: it starts life
    // running (never switched into via commit_switch), and a stuck-clear gate
    // would otherwise let a peer resume it mid-run.
    ON_CPU[slot].store(1, Ordering::SeqCst);
    set_current_thread_register(slot);
    set_current_exception_stack(exc_stack_top);
    THREAD_STATES[slot].store(thread_state::RUNNING, Ordering::SeqCst);
    register_core_idle(core_id, slot);
    Some(slot)
}

// ============================================================================
// Thread Constants
// ============================================================================

// Assembly context switch implementation
#[cfg(target_os = "none")]
global_asm!(
    r#"
.section .text
.global thread_start
.global thread_start_closure

// Thread entry trampoline for extern "C" functions
// x19 holds the actual thread entry function
thread_start:
    // Enable IRQs for this thread
    msr daifclr, #2
    
    // Call the thread entry function (in x19)
    blr x19
    
    // Thread returned - mark as terminated and yield
    // (This shouldn't happen for -> ! functions, but just in case)
    b thread_exit_asm

// Thread entry trampoline for Rust closures
// x19 holds pointer to the closure trampoline function
// x20 holds the raw pointer to the boxed closure data
// x21 holds IRQ enable flag: 0 = enable IRQs now, non-zero = keep disabled
thread_start_closure:
    // CRITICAL: Verify x19 (trampoline) is valid before calling
    // If x19 == 0, we'd jump to address 0 and crash with EC=0x0
    cbnz x19, 2f
    // x19 is 0! Halt with marker
    mov x0, #0xBAD
    movk x0, #0x0019, lsl #16   // 0x00190BAD = "bad x19"
3:  wfi
    b 3b
2:
    // Check if we should enable IRQs (x21 == 0 means enable)
    // For process threads: x21 != 0, keep IRQs disabled until activate()
    // For system/test threads: x21 == 0, enable IRQs now
    cbnz x21, 1f           // Skip IRQ enable if x21 != 0
    msr daifclr, #2        // Enable IRQs
1:
    // Call the trampoline with closure pointer as argument
    // x19 = trampoline function pointer
    // x20 = closure data pointer (passed as x0)
    mov x0, x20
    blr x19
    
    // Thread returned - should not happen for -> ! closures
    b thread_exit_asm

thread_exit_asm:
    wfi
    b thread_exit_asm
"#
);

#[cfg(target_os = "none")]
unsafe extern "C" {
    fn thread_start() -> !;
    fn thread_start_closure() -> !;
}

#[cfg(not(target_os = "none"))]
unsafe extern "C" fn thread_start() -> ! { panic!("not on bare metal") }
#[cfg(not(target_os = "none"))]
unsafe extern "C" fn thread_start_closure() -> ! { panic!("not on bare metal") }


/// Set up a fake IRQ frame on a new thread's stack
/// 
/// This allows the simplified stack-based context switch to work for new threads.
/// When the IRQ handler restores from this stack, it will load these values.
/// 
/// UNIFIED frame layout (288 bytes) - used by both EL0 and EL1 handlers:
///   [sp+0]:   x30 + padding
///   [sp+16]:  x28, x29
///   [sp+32]:  x26, x27
///   [sp+48]:  x24, x25
///   [sp+64]:  x22, x23
///   [sp+80]:  x20, x21
///   [sp+96]:  x18, x19
///   [sp+112]: x16, x17
///   [sp+128]: x14, x15
///   [sp+144]: x12, x13
///   [sp+160]: x8, x9
///   [sp+176]: x6, x7
///   [sp+192]: x4, x5
///   [sp+208]: x2, x3
///   [sp+224]: x0, x1
///   [sp+240]: ELR, SPSR
///   [sp+256]: SP_EL0 + padding
///   [sp+272]: TPIDR_EL0 + padding
///   [sp+288..815]: NEON/FP (Q0-Q31, FPCR, FPSR) — 528 bytes, zeroed
///   [sp+816]: x10, x11
/// 
/// Returns the SP value pointing to the fake IRQ frame
pub fn setup_fake_irq_frame(
    stack_top: u64,
    entry_point: u64,
    x19: u64,  // Trampoline function pointer
    x20: u64,  // Closure data pointer
    x21: u64,  // IRQ enable flag (0 = enable)
) -> u64 {
    let frame_base = stack_top - IRQ_FRAME_SIZE as u64;
    let frame = frame_base as *mut u64;
    
    unsafe {
        // Zero the entire frame (GPR + NEON + x10/x11)
        core::ptr::write_bytes(frame as *mut u8, 0, IRQ_FRAME_SIZE);
        
        // GPR block at [sp+0..287] — offsets unchanged from before
        // [sp+0]: x30 - return address after thread_start_closure returns
        frame.add(0).write_volatile(thread_exit_stub as *const () as u64);
        
        // [sp+80]: x20, x21
        frame.add(10).write_volatile(x20);  // x20 - closure data
        frame.add(11).write_volatile(x21);  // x21 - IRQ enable flag
        
        // [sp+96]: x18, x19
        frame.add(13).write_volatile(x19);  // x19 - trampoline
        
        // [sp+240]: ELR, SPSR
        frame.add(30).write_volatile(entry_point);  // ELR - where to jump
        let spsr = if x21 != 0 {
            0x000003C5  // EL1h, IRQs DISABLED
        } else {
            0x00000345  // EL1h, IRQs ENABLED
        };
        frame.add(31).write_volatile(spsr);
        
        // NEON block at [sp+288..815] and x10/x11 at [sp+816] are all zero from write_bytes
    }
    
    frame_base
}

/// Stub for thread exit - threads should never return here
#[unsafe(no_mangle)]
extern "C" fn thread_exit_stub() -> ! {
    safe_print!(128, "[THREAD] Exit stub reached - marking terminated\n");
    mark_current_terminated();
    loop {
        yield_now();
    }
}


// ============================================================================
// Thread Contexts - Separate Static Array for Lock-Free Access
// ============================================================================
//
// Thread contexts are stored in a separate static array, NOT behind the POOL
// spinlock. This allows the scheduler to access contexts without holding the
// lock across the context switch, which would cause deadlock.
//
// Safety invariants:
// 1. Only the scheduler (with IRQs masked) modifies contexts during switch
// 2. A thread's context is only accessed when that thread is NOT running
// 3. Context must be fully initialized before state becomes READY
// 4. Context is zeroed when state becomes FREE
// ============================================================================

use core::cell::UnsafeCell;

/// Wrapper to make UnsafeCell<Context> Sync
///
/// SAFETY: THREAD_CONTEXTS\[idx\] is safe to share across cores under `smp-shared`
/// (real cross-core concurrency, not just IRQ-masked single-core mutual exclusion)
/// because every access falls into one of three cases, none of which depends on
/// the BKL:
///
/// 1. **Scheduler switch** (`SGI` context save/restore, `ThreadPool::get_context_ptrs`
///    callers): the outgoing and incoming slots are both touched only while `POOL`
///    (a real `Spinlock<ThreadPool>`) is held across the *entire* switch — decision,
///    context save, and context load — per the M5c note at the switch call site.
///    `POOL` is the actual inner lock here; the scheduler never picks a slot whose
///    state isn't READY (see `schedule_indices`), so a slot mid-setup is never a
///    switch target.
/// 2. **Spawn / reclaim setup**: a slot is only written by the thread that just won
///    the `FREE -> INITIALIZING` (or `TERMINATED -> INITIALIZING`, see
///    `reclaim_terminated_slots`) transition via `THREAD_STATES[idx].compare_exchange`
///    (SeqCst). The CAS gives the winner exclusive ownership of that index — no
///    other core can also win it — and the scheduler ignores INITIALIZING slots, so
///    nothing else touches THREAD_CONTEXTS\[idx\] until the owner publishes it.
///    Publication is `mark_thread_ready`'s plain `Ordering::SeqCst` store: a SeqCst
///    store is a release, so every write to THREAD_CONTEXTS\[idx\] that precedes it in
///    the owner's program order is guaranteed visible to any core whose SeqCst load
///    (the scheduler's `THREAD_STATES[idx].load`) observes READY — this is a real
///    memory-model guarantee, not something that happens to hold only while the BKL
///    also serializes everything. (Host test:
///    `ready_transition_publishes_context_writes_without_a_lock`.)
/// 3. **Self-read of the live thread** (`get_saved_user_context`/fork's
///    `get_saved_user_context(current_thread_id())`): a thread reading its own slot
///    cannot race itself — there is only one execution of "this thread" at a time by
///    construction.
///
/// What this does NOT cover: the debug/stat dump paths (`dump_thread_resume_points`,
/// `list_kernel_threads`) read arbitrary threads' contexts, including ones RUNNING on
/// a peer core, with no synchronization at all. That is a deliberate, accepted race —
/// display-only, single aligned `u64` reads (no torn multi-word reads, no memory
/// unsafety), tolerant of a stale value. Do not use that pattern for anything that
/// feeds a correctness decision.
///
/// See `docs/archive/BKL_PHASE7D_THREAD_CONTEXTS.md` for the full audit (including
/// the dead, latently-racy `ThreadPool::spawn`/`spawn_with_stack_size`/
/// `spawn_system_closure`/`spawn_user_closure` methods removed alongside this fix —
/// they scanned for FREE slots with a plain load instead of a CAS, which is only
/// sound because nothing else ever called them).
#[repr(transparent)]
struct SyncContext(UnsafeCell<Context>);

// SAFETY: See the invariant written out above — real cross-core exclusion via
// `POOL` (case 1) or a `THREAD_STATES` CAS (case 2), not single-core IRQ masking.
unsafe impl Sync for SyncContext {}

impl SyncContext {
    const fn new() -> Self {
        Self(UnsafeCell::new(Context::zero()))
    }
    
    #[inline]
    fn get(&self) -> *mut Context {
        self.0.get()
    }
}

/// Per-thread CPU contexts, kept out of `POOL` so the scheduler can access them
/// without holding that lock across a context switch.
/// Safety: see `SyncContext`'s SAFETY comment above — POOL / a THREAD_STATES CAS /
/// self-read, not IRQ masking alone.
static THREAD_CONTEXTS: [SyncContext; MAX_THREADS] = {
    const INIT: SyncContext = SyncContext::new();
    [INIT; MAX_THREADS]
};

/// Get a mutable pointer to a thread's context
/// SAFETY: caller must be in one of the three cases `SyncContext` documents
/// (holds `POOL` across the switch, owns the slot's INITIALIZING CAS, or is
/// reading its own live thread).
#[inline]
fn get_context_mut(idx: usize) -> *mut Context {
    THREAD_CONTEXTS[idx].get()
}

/// Get an immutable pointer to a thread's context  
/// SAFETY: Caller must ensure thread is not running
#[inline]
fn get_context(idx: usize) -> *const Context {
    THREAD_CONTEXTS[idx].get()
}


/// Fixed-size thread pool with per-thread stack sizes
pub struct ThreadPool {
    slots: [ThreadSlot; MAX_THREADS],
    stacks: [StackInfo; MAX_THREADS],
    current_idx: usize,
    initialized: bool,
    /// Counter for proportional scheduling of thread 0
    /// Thread 0 gets boosted when this reaches NETWORK_THREAD_RATIO
    network_boost_counter: u32,
    /// Global round-robin index for fair thread rotation
    /// This ensures all threads get scheduled, not just the first ready one
    /// after the current thread.
    round_robin_idx: usize,
}

impl ThreadPool {
    pub const fn new() -> Self {
        Self {
            slots: [const { ThreadSlot::empty() }; MAX_THREADS],
            stacks: [const { StackInfo::empty() }; MAX_THREADS],
            current_idx: 0,
            initialized: false,
            network_boost_counter: 0,
            round_robin_idx: 0,
        }
    }

    /// Initialize the pool - allocate stacks with sizes based on thread role
    ///
    /// Thread 0: Boot stack (1MB, fixed location) - preemptible
    /// Threads 1 to RESERVED_THREADS-1: System threads (256KB each) - preemptible
    /// Threads RESERVED_THREADS to MAX_THREADS-1: User process threads (128KB each) - preemptible
    pub fn init(&mut self) {
        // Get the STORED boot TTBR0 value - all kernel threads will use this
        // CRITICAL: Must use stored value, not current TTBR0 which could be a user process's!
        let boot_ttbr0: u64 = crate::mmu::get_boot_ttbr0();

        // Slot 0 is the idle/boot thread (uses boot stack, never terminated)
        THREAD_STATES[IDLE_THREAD_IDX].store(thread_state::RUNNING, Ordering::SeqCst);
        self.slots[IDLE_THREAD_IDX].start_time_us = (runtime().uptime_us)();
        // Real shared-kernel SMP: slot 0 is core 0's per-core idle thread. Registering it
        // makes the scheduler's idle-fallback logic uniform across cores (single-core
        // builds also go through it, with core_id 0 → slot 0 = the original behavior).
        register_core_idle(0, IDLE_THREAD_IDX);
        
        // Initialize boot thread context in THREAD_CONTEXTS (not in slot)
        unsafe {
            let boot_ctx = &mut *get_context_mut(IDLE_THREAD_IDX);
            boot_ctx.magic = CONTEXT_MAGIC;
            boot_ctx.ttbr0 = boot_ttbr0;
            // Boot thread starts with kernel mode SPSR
            boot_ctx.spsr = 0x00000005; // EL1h
            // Other fields stay zero (callee-saved regs saved on first context switch)
            boot_ctx.user_entry = 0;
            boot_ctx.user_sp = 0;
            boot_ctx.is_user_process = 0;
        }

        // Boot stack info — bounds come from the kernel via ExecConfig because they
        // are profile-dependent (boot.rs / build.rs / linker.ld place the boot stack
        // right after the reserved image region, which differs between the `size` and
        // `release` images). These MUST NOT be hardcoded: when the boot stack was
        // relocated, a stale 0x40700000 constant stamped the canary into the kernel
        // heap at low RAM. See docs/LOW_MEMORY_ENVIRONMENT.md "Known bug".
        // The boot stack was already in use before threading init; we CANNOT reserve
        // space at the top.
        let _boot_stack_top = config().boot_stack_top as u64; // STACK_TOP from boot.rs
        let boot_stack_base = config().boot_stack_base; // STACK_TOP - STACK_SIZE
        self.stacks[IDLE_THREAD_IDX] = StackInfo::new(
            boot_stack_base,
            config().kernel_stack_size,
        );
        
        // Allocate a SEPARATE exception stack for thread 0 (boot thread).
        // Unlike spawned threads which reserve space at the top of their stack,
        // the boot stack was already in use before we could reserve space.
        // Allocate from PMM to avoid using kernel heap for stacks.
        let exc_pages = (EXCEPTION_STACK_SIZE + 4095) / 4096;
        let exc_frame = akuma_pmm::alloc_pages_contiguous_zeroed(exc_pages).map(crate::PhysFrame::new)
            .expect("Failed to allocate boot exception stack from PMM");
        let boot_exception_stack_ptr = crate::mmu::phys_to_virt(exc_frame.addr);
        let boot_exception_stack_top = unsafe {
            (boot_exception_stack_ptr as *const u8).add(EXCEPTION_STACK_SIZE) as u64
        };
        // Align to 16 bytes
        self.slots[IDLE_THREAD_IDX].exception_stack_top = boot_exception_stack_top & !0xF;
        
        // CRITICAL: Update TPIDR_EL1 to point to Thread 0's new exception stack!
        // exceptions::init() set it to 0x40800000 (boot stack top) initially,
        // but we've now allocated a proper exception stack from the heap.
        // Without this, the first IRQ would use the wrong exception stack pointer.
        set_current_exception_stack(self.slots[IDLE_THREAD_IDX].exception_stack_top);
        
        // Initialize canary for boot stack
        if config().enable_stack_canaries {
            init_stack_canary(boot_stack_base);
        }

        // Stack pre-allocation.
        //
        // release: pre-allocate every slot up to thread_limit (system stacks for
        // 1..reserved, user stacks for reserved..thread_limit) — guaranteed
        // available, no per-spawn allocation.
        //
        // size profile: lazy stacks. Pre-allocate only a small WARM FLOOR of FREE
        // stacks per class; ensure_slot_stack grows the rest on demand at spawn
        // and cleanup_terminated frees back to the floor on recycle. This avoids
        // reserving ~1 MB of PMM for slots that are idle most of the time.
        #[cfg(not(kernel_profile_extreme))]
        {
            for i in 1..config().reserved_threads {
                assert!(
                    self.allocate_stack_for_slot(i, config().system_thread_stack_size),
                    "boot: failed to allocate system thread stack for slot {}", i
                );
            }
            for i in config().reserved_threads..thread_limit() {
                assert!(
                    self.allocate_stack_for_slot(i, config().user_thread_stack_size),
                    "boot: failed to allocate user thread stack for slot {}", i
                );
            }
        }
        #[cfg(kernel_profile_extreme)]
        {
            let sys_end = (1 + WARM_FREE_SYSTEM).min(config().reserved_threads);
            for i in 1..sys_end {
                assert!(
                    self.allocate_stack_for_slot(i, config().system_thread_stack_size),
                    "boot: failed to allocate warm system thread stack for slot {}", i
                );
            }
            let user_start = config().reserved_threads;
            let user_end = (user_start + WARM_FREE_USER).min(thread_limit());
            for i in user_start..user_end {
                assert!(
                    self.allocate_stack_for_slot(i, config().user_thread_stack_size),
                    "boot: failed to allocate warm user thread stack for slot {}", i
                );
            }
        }

        self.initialized = true;
    }

    /// Allocate a stack for a specific slot using PMM contiguous pages.
    ///
    /// Stack layout (stack grows downward):
    /// ```text
    /// |------------------| <- stack_top (highest address)
    /// | Exception area   |  EXCEPTION_STACK_SIZE (1KB) for trap frames
    /// |------------------|
    /// | Kernel stack     |  Rest of stack for normal kernel code
    /// |------------------| <- stack_base (lowest address)
    /// ```
    /// Allocate a PMM-backed stack for a slot. Returns `false` if the PMM has no
    /// room (lazy callers release the slot and report ENOMEM; boot callers treat
    /// it as fatal). Previously this `.expect()`-panicked — fine when only ever
    /// called at boot, but lazy allocation can hit a genuinely exhausted PMM.
    fn allocate_stack_for_slot(&mut self, slot_idx: usize, size: usize) -> bool {
        let page_size = 4096;
        let pages = (size + page_size - 1) / page_size;
        let alloc_size = pages * page_size;

        // Allocate contiguous physical pages from PMM (bypasses kernel heap)
        let frame = match akuma_pmm::alloc_pages_contiguous_zeroed(pages).map(crate::PhysFrame::new) {
            Some(f) => f,
            None => return false,
        };
        let stack_ptr = crate::mmu::phys_to_virt(frame.addr) as usize;
        let stack_info = StackInfo::new(stack_ptr, alloc_size);

        // Initialize canary at bottom of stack
        if config().enable_stack_canaries {
            init_stack_canary(stack_info.base);
        }

        // Stack high-water probe: paint the stack with a sentinel so
        // `stack_high_water` can report true peak usage (an UPPER bound, since any
        // write — even a zero — breaks the sentinel). Gated off in production.
        if STACK_USAGE_PROBE {
            fill_stack_sentinel(stack_info.base, stack_info.top);
        }

        self.stacks[slot_idx] = stack_info;

        // Set exception stack top (top of the reserved 1KB area)
        // The exception stack is at the very top of the kernel stack
        self.slots[slot_idx].exception_stack_top = (stack_info.top & !0xF) as u64;
        true
    }

    /// Free a PMM-backed stack for a specific slot.
    fn free_stack_for_slot(&mut self, slot_idx: usize) {
        let stack = &self.stacks[slot_idx];
        if stack.is_allocated() {
            // Capture the high-water before the painted stack goes away, so a
            // short-lived user thread's peak survives this free (WARM_FREE_USER=0
            // frees on every exit). Cheap scan; only when the probe is on.
            if STACK_USAGE_PROBE {
                let base = stack.base;
                let top = base + stack.size;
                let canary_bytes = config().canary_words * 8;
                let mut addr = (base + canary_bytes + 7) & !7;
                let mut first_used = top;
                unsafe {
                    while addr + 8 <= top {
                        if (addr as *const u64).read_volatile() != STACK_SENTINEL {
                            first_used = addr;
                            break;
                        }
                        addr += 8;
                    }
                }
                record_stack_peak(slot_idx, top - first_used);
            }
            // Overflow verdict, taken here because this is the last moment the
            // evidence exists — the frames go back to the PMM on the next line and
            // are handed to somebody else. `init_stack_canary` paints the canary at
            // every stack's base and nothing repaints it while the thread lives, so
            // a broken pattern is proof this thread ran off the bottom, and the
            // bytes below it belonged to whatever the PMM had placed there.
            //
            // Reported rather than merely counted because the alternative is what
            // this check was added for: a 10 KB run-off past a 64 KB user-thread
            // stack landed in a *user process's L3 page table*, zeroed three PTEs
            // mid-`sys_spawn`, and surfaced as an unrelated SIGSEGV in the process
            // whose mapping had vanished. Nothing anywhere said "stack overflow" —
            // `check_all_stack_canaries` existed but had no callers.
            // See `docs/archive/EXTREME_SSHD_KERNEL_STACK_OVERFLOW.md`.
            if config().enable_stack_canaries && !check_stack_canary(stack.base) {
                safe_print!(160,
                    "[STACK-OVERFLOW] tid={} ran off its {}KB kernel stack (base={:#x}) — \
                     kernel memory below it was corrupted\n",
                    slot_idx, stack.size / 1024, stack.base);
            }
            let page_size = 4096;
            let pages = (stack.size + page_size - 1) / page_size;
            let phys_addr = crate::mmu::virt_to_phys(stack.base);
            akuma_pmm::free_pages_contiguous(phys_addr, pages);
            self.stacks[slot_idx] = StackInfo::empty();
        }
    }

    /// Select next ready thread (round-robin) - LOCK-FREE for state transitions
    ///
    /// # Preemption rules:
    /// - `voluntary=true`: Thread yielded voluntarily (yield_now) - always switch
    /// - `voluntary=false`: Timer-triggered preemption
    ///   - If preemption is explicitly disabled: Don't switch
    ///   - Otherwise: always preemptible
    ///
    /// Pick the next thread for `core_id` to run. Returns `Some((old, new))` if a
    /// switch is needed. Under real shared-kernel SMP this runs serialized by the Big
    /// Kernel Lock (the whole IRQ/scheduler excursion holds it), so two cores never
    /// execute this concurrently and the RUNNING state alone prevents double-running a
    /// thread. Per-core idle threads (see [`IS_IDLE_THREAD`]) are skipped by the
    /// round-robin scan; a core falls back only to ITS OWN idle. `core_id` is 0 on
    /// single-core builds, so behavior there is unchanged.
    pub fn schedule_indices(&mut self, voluntary: bool, core_id: usize) -> Option<(usize, usize)> {
        // Use TPIDRRO_EL0 register for current thread ID - more reliable than atomic
        let current_idx = get_current_thread_register();

        // Wake-pass: mark READY any WAITING thread whose wake deadline has passed.
        // This MUST run on every scheduler entry — including involuntary
        // (timer-tick) entries where the current thread has preemption disabled
        // (e.g. an idle thread halted in `idle_halt`'s WFI). The preemption check
        // below only suppresses the context SWITCH; if it also suppressed the
        // wake-pass, every sleeping thread's wakeup (nanosleep, schedule_blocking,
        // condvar/futex timeouts) would be delayed by however long the idle thread
        // stays preempt-disabled, inflating sleep latency by whole ticks. The
        // wake-pass only flips state + SEVs; it never switches context, so running
        // it with preemption disabled is safe. (Reordering it to the top is also
        // what makes `idle_halt`'s preempt-guarded WFI cheap for accounting
        // without costing wakeup latency.)
        let now = (runtime().uptime_us)();
        let mut woke_any = false;
        for i in 0..MAX_THREADS {
            if THREAD_STATES[i].load(Ordering::SeqCst) == thread_state::WAITING {
                let wake_time = WAKE_TIMES[i].load(Ordering::SeqCst);
                if wake_time > 0 && now >= wake_time {
                    // Wake this thread — CAS, not store: a lock-free ThreadWaker::wake
                    // (or a cross-thread kill's TERMINATED) can land between our load
                    // and this write; an unconditional READY store would overwrite it
                    // (see ThreadWaker::wake for the corruption that enables). The
                    // WAKE_TIMES clear only follows a transition WE own; the woken
                    // thread can't re-park before it lands because parking requires
                    // running, and running requires a switch under POOL — held here.
                    if THREAD_STATES[i]
                        .compare_exchange(
                            thread_state::WAITING,
                            thread_state::READY,
                            Ordering::SeqCst,
                            Ordering::SeqCst,
                        )
                        .is_ok()
                    {
                        WAKE_TIMES[i].store(0, Ordering::SeqCst);
                        woke_any = true;
                    }
                }
            }
        }
        // Send event to wake any threads in WFI
        if woke_any {
            #[cfg(target_os = "none")]
            unsafe { core::arch::asm!("sev"); }
        }

        // For timer-triggered preemption, first check if preemption is explicitly disabled.
        // (Only gates the context switch — the wake-pass above already ran.)
        if !voluntary && is_preemption_disabled() {
            return None;
        }

        // Proportional scheduling for the network polling thread (run_async_main).
        // The network thread gets boosted every NETWORK_THREAD_RATIO scheduler ticks,
        // giving it a 1/N share of CPU time (e.g., 25% with ratio=4).
        let net_tid = NETWORK_THREAD_ID.load(Ordering::Relaxed);
        if net_tid != usize::MAX && current_idx != net_tid {
            self.network_boost_counter += 1;
            if self.network_boost_counter >= config().network_thread_ratio {
                self.network_boost_counter = 0;
                if THREAD_STATES[net_tid].load(Ordering::SeqCst) == thread_state::READY
                    && ON_CPU[net_tid].load(Ordering::SeqCst) == 0
                {
                    // Same ON_CPU latch as commit_switch (this path bypasses it).
                    ON_CPU[current_idx].store(1, Ordering::SeqCst);
                    ON_CPU[net_tid].store(1, Ordering::SeqCst);
                    if core_id < MAX_CORES {
                        PER_CORE_OFFCPU[core_id].store(current_idx, Ordering::SeqCst);
                    }
                    // Atomic re-READY of the outgoing thread — see commit_switch for
                    // why this must not be a check-then-store.
                    let _ = THREAD_STATES[current_idx].fetch_update(
                        Ordering::SeqCst,
                        Ordering::SeqCst,
                        |s| (s != thread_state::TERMINATED && s != thread_state::WAITING)
                            .then_some(thread_state::READY),
                    );
                    THREAD_STATES[net_tid].store(thread_state::RUNNING, Ordering::SeqCst);
                    self.slots[net_tid].start_time_us = now;
                    set_current_thread_register(net_tid);
                    self.current_idx = net_tid;
                    return Some((current_idx, net_tid));
                }
            }
        }
        // If current thread is the network thread, or counter hasn't reached ratio, use round-robin below

        // EXPERIMENTAL (Failure D falsification test, see `config::PRIORITIZE_NEVER_SCHEDULED`):
        // unconditionally prefer any READY thread that has never once been scheduled
        // (`LAST_CORE == 0xFF`) over the wakeup-locality hint and the round-robin scan
        // below. Not fairness-preserving by design — this is a diagnostic override, not
        // a real scheduling policy.
        let never_scheduled = if config().prioritize_never_scheduled {
            let mut found = None;
            for i in 0..MAX_THREADS {
                if i != current_idx
                    && !IS_IDLE_THREAD[i].load(Ordering::Relaxed)
                    && THREAD_STATES[i].load(Ordering::SeqCst) == thread_state::READY
                    && ON_CPU[i].load(Ordering::SeqCst) == 0
                    && LAST_CORE[i].load(Ordering::Relaxed) == 0xFF
                {
                    found = Some(i);
                    break;
                }
            }
            found
        } else {
            None
        };

        // Wakeup locality: if a thread was just woken by an explicit signal
        // (futex/cond), prefer running it NEXT (once) so a producer→consumer
        // handoff doesn't wait a full round-robin cycle behind ~20 ready threads.
        // This is the dominant per-syscall cost for the rump sysproxy. Consumed
        // (swap to MAX_THREADS) so it fires once; the preempted current thread
        // stays READY below, so round-robin fairness is preserved across ticks.
        let hinted = PREEMPT_WAKE_TID.swap(MAX_THREADS, Ordering::SeqCst);
        let next_idx = if let Some(idx) = never_scheduled {
            idx
        } else if hinted < MAX_THREADS
            && hinted != current_idx
            && !IS_IDLE_THREAD[hinted].load(Ordering::Relaxed)
            && THREAD_STATES[hinted].load(Ordering::SeqCst) == thread_state::READY
            && ON_CPU[hinted].load(Ordering::SeqCst) == 0
        {
            hinted
        } else {
            // Find next ready thread using GLOBAL round-robin index
            // This ensures fair rotation through ALL threads, not just starting from current.
            // Without this, threads 10, 11 would never run if 8, 9 are always ready and
            // the scheduler always runs from a low-numbered system thread.
            //
            // Skip per-core idle threads: they are never taken by the scan (a core only
            // runs ITS OWN idle, chosen as the fallback below), so one core can't grab
            // another core's idle under SMP.
            let mut next_idx = (self.round_robin_idx + 1) % MAX_THREADS;
            let start_idx = next_idx;

            loop {
                let state = THREAD_STATES[next_idx].load(Ordering::SeqCst);

                if state == thread_state::READY
                    && next_idx != current_idx
                    && !IS_IDLE_THREAD[next_idx].load(Ordering::Relaxed)
                    && ON_CPU[next_idx].load(Ordering::SeqCst) == 0
                {
                    // Found a ready, non-idle thread to switch TO.
                    break;
                }

                next_idx = (next_idx + 1) % MAX_THREADS;

                if next_idx == start_idx {
                    // No non-idle READY thread. Fall back to THIS core's idle thread so
                    // the core has something to run (single-core: idle is slot 0, the
                    // original behavior of dropping to idle when nothing else is ready).
                    let idle = IDLE_SLOT_FOR_CORE[core_id].load(Ordering::Relaxed);
                    if idle < 0 || idle as usize == current_idx {
                        // No idle registered yet, or we're already on our idle: stay put.
                        return None;
                    }
                    let idle = idle as usize;
                    if ON_CPU[idle].load(Ordering::SeqCst) != 0 {
                        // Should be unreachable (only THIS core runs its idle), but
                        // never switch onto a stack another core may still be on.
                        return None;
                    }
                    self.commit_switch(current_idx, idle, now);
                    return Some((current_idx, idle));
                }
            }

            // Update global round-robin index to where we found the next thread
            // This ensures the NEXT scheduling decision continues from here
            self.round_robin_idx = next_idx;
            next_idx
        };

        self.commit_switch(current_idx, next_idx, now);
        Some((current_idx, next_idx))
    }

    /// Apply a scheduler switch decision: bill the outgoing thread's CPU time, flip it
    /// back to READY (unless TERMINATED/WAITING), mark the incoming thread RUNNING, and
    /// publish it as this core's current thread (TPIDRRO_EL0). Shared by the normal
    /// round-robin path and the per-core idle fallback.
    fn commit_switch(&mut self, current_idx: usize, next_idx: usize, now: u64) {
        // ON_CPU protocol (see its doc): the incoming thread is about to run on
        // this core; the outgoing thread's gate stays set (re-asserted here for
        // robustness) until `rust_switch_finished` clears it after the vector
        // asm's `mov sp, x0` — this core keeps executing on its kernel stack
        // until then, well after POOL is released. SeqCst orders the latch
        // before the READY publication below for any SeqCst reader (pickers).
        ON_CPU[current_idx].store(1, Ordering::SeqCst);
        ON_CPU[next_idx].store(1, Ordering::SeqCst);
        let core = crate::bkl::current_core_id() as usize;
        if core < MAX_CORES {
            PER_CORE_OFFCPU[core].store(current_idx, Ordering::SeqCst);
        }

        // Accumulate CPU time for the thread being scheduled out.
        let start = self.slots[current_idx].start_time_us;
        if start > 0 {
            let elapsed = now.saturating_sub(start);
            TOTAL_CPU_TIMES[current_idx].fetch_add(elapsed, Ordering::Relaxed);
        }

        // Put the outgoing thread back to READY — unless it is TERMINATED or
        // WAITING (WAITING keeps its state so the wake-pass handles it). This
        // must be one atomic RMW, not check-then-store: `mark_thread_terminated`
        // is called cross-thread with no lock (kill_thread_group), so a
        // TERMINATED can land between a load here and a plain READY store —
        // resurrecting a killed thread whose address space teardown is already
        // under way. It would then be picked by a peer and run on freed (and
        // possibly recycled) page tables.
        let _ = THREAD_STATES[current_idx].fetch_update(
            Ordering::SeqCst,
            Ordering::SeqCst,
            |s| (s != thread_state::TERMINATED && s != thread_state::WAITING)
                .then_some(thread_state::READY),
        );
        let prev = THREAD_STATES[next_idx].swap(thread_state::RUNNING, Ordering::SeqCst);
        if prev == thread_state::TERMINATED {
            // F8 evidence tripwire: the pick scan only accepts READY, so a
            // TERMINATED here means a cross-thread kill landed between the scan
            // and this commit — the switch will now run a thread whose teardown
            // (and possibly address space) is already being reclaimed.
            safe_print!(128, "[SGI-S PICKED-TERMINATED] new_tid={} old_tid={}\n",
                next_idx, current_idx);
        }
        // Record the core the incoming thread is now running on. commit_switch always
        // runs on the core that will run next_idx (single runqueue, each core schedules
        // itself), so current_core_id() is authoritative.
        LAST_CORE[next_idx].store(crate::bkl::current_core_id() as u8, Ordering::Relaxed);

        // Update timing (still in slot, but we own it)
        self.slots[next_idx].start_time_us = now;

        // Update current thread in CPU register (authoritative, per-core source of truth)
        set_current_thread_register(next_idx);
        self.current_idx = next_idx; // Legacy mirror (only validate_current_sp reads it)
    }

    pub fn thread_stats(&self) -> (usize, usize, usize) {
        let mut ready = 0;
        let mut running = 0;
        let mut terminated = 0;
        // Use atomic THREAD_STATES array (source of truth)
        for i in 0..MAX_THREADS {
            match THREAD_STATES[i].load(Ordering::Relaxed) {
                thread_state::READY => ready += 1,
                thread_state::RUNNING => running += 1,
                thread_state::TERMINATED => terminated += 1,
                _ => {}
            }
        }
        (ready, running, terminated)
    }

    pub fn thread_count(&self) -> usize {
        // Use atomic THREAD_STATES array (source of truth)
        (0..MAX_THREADS)
            .filter(|&i| THREAD_STATES[i].load(Ordering::Relaxed) != thread_state::FREE)
            .count()
    }

    pub unsafe fn get_context_ptrs(
        &mut self,
        old_idx: usize,
        new_idx: usize,
    ) -> (*mut Context, *const Context) {
        // Contexts are now in THREAD_CONTEXTS static array, not in slots
        let old_ptr = get_context_mut(old_idx);
        let new_ptr = get_context(new_idx);
        (old_ptr, new_ptr)
    }
}

// ============================================================================
// Stack Canary Functions
// ============================================================================

/// Initialize stack canary at the bottom of a stack
fn init_stack_canary(stack_base: usize) {
    if stack_base == 0 {
        return;
    }
    unsafe {
        let ptr = stack_base as *mut u64;
        for i in 0..config().canary_words {
            ptr.add(i).write_volatile(config().stack_canary);
        }
    }
}

/// Check if stack canary is intact
fn check_stack_canary(stack_base: usize) -> bool {
    if stack_base == 0 {
        return true; // Boot stack or unallocated
    }
    unsafe {
        let ptr = stack_base as *const u64;
        for i in 0..config().canary_words {
            if ptr.add(i).read_volatile() != config().stack_canary {
                return false; // Corrupted!
            }
        }
    }
    true
}

// ============================================================================
// Global Thread Pool
// ============================================================================

static POOL: Spinlock<ThreadPool> = Spinlock::new(ThreadPool::new());
/// Per-core "the next scheduler SGI on THIS core is a voluntary switch" flags.
///
/// PER-CORE, not global (fixed 2026-07-21): the voluntary setters (`yield_now`,
/// `schedule_blocking`, `request_voluntary_reschedule`) always pair the flag with a
/// SELF-targeted scheduler SGI, so producer and consumer are the same core. With one
/// global flag, a PEER core's concurrent timer SGI would `swap` the flag away — its
/// involuntary tick ran as voluntary (bypassing that core's preemption-disabled
/// check) while the yielding core's SGI ran as INVOLUNTARY, silently eating the
/// yield. Mostly-invisible when involuntary switches could stand in for the lost
/// voluntary one, but fatal for a thread whose involuntary path is gated (a
/// `LifecycleGuard` holder in a cooperative wait loop spins forever in EL1 holding
/// the BKL — the SMP=4 fork-hammer wedge, `[WATCHDOG] disabled at lifecycle.rs`).
static VOLUNTARY_SCHEDULE: [AtomicBool; MAX_CORES] = {
    const INIT: AtomicBool = AtomicBool::new(false);
    [INIT; MAX_CORES]
};
static SGI_DEBUG_ONCE: AtomicBool = AtomicBool::new(false);

/// This core's voluntary-reschedule flag (see [`VOLUNTARY_SCHEDULE`]). Core 0 on
/// non-SMP and host builds.
#[inline]
fn voluntary_schedule_flag() -> &'static AtomicBool {
    &VOLUNTARY_SCHEDULE[crate::bkl::current_core_id() as usize % MAX_CORES]
}

/// Enable SGI debug for next yield (one-shot)
pub fn set_sgi_debug(enable: bool) {
    SGI_DEBUG_ONCE.store(enable, Ordering::SeqCst);
}

/// Mark the next scheduler SGI on THIS core as a VOLUNTARY switch, so `schedule_indices`
/// switches away even when preemption is explicitly disabled for the current thread.
/// Used by an IRQ-context cross-core wakeup that readies a thread and must preempt the
/// current one now. Pair with a self-targeted scheduler-SGI trigger.
pub fn request_voluntary_reschedule() {
    voluntary_schedule_flag().store(true, Ordering::Release);
}

/// Initialize the thread pool
pub fn init() {
    // Print stack requirements before initialization
    print_stack_requirements();
    
    // Verify the thread-stack pool fits in free PMM (stacks are allocated from
    // PMM via alloc_pages_contiguous_zeroed, NOT the kernel heap). The pool size
    // tracks thread_limit(), which is scaled to RAM before this runs.
    let (_total, _alloc, free_pages) = akuma_pmm::stats();
    let free_bytes = free_pages.saturating_mul(crate::mmu::PAGE_SIZE);
    if let Err(msg) = verify_stack_memory(free_bytes) {
        panic!("Stack allocation failed: {}", msg);
    }
    
    // Initialize ThreadPool (allocates stacks, sets up boot thread)
    {
        let mut pool = POOL.lock();
        pool.init();
    }
    
    // Initialize atomic thread states to match ThreadPool state
    // Thread 0 is RUNNING (boot thread), all others are FREE
    THREAD_STATES[0].store(thread_state::RUNNING, Ordering::SeqCst);
    // Boot thread starts life running on core 0 without ever being switched
    // into — latch its ON_CPU gate so a peer can never pick it while it runs
    // (it becomes READY-visible the moment it blocks in schedule_blocking).
    ON_CPU[0].store(1, Ordering::SeqCst);
    for i in 1..MAX_THREADS {
        THREAD_STATES[i].store(thread_state::FREE, Ordering::SeqCst);
    }
    set_current_thread_register(0);  // Initialize CPU register for boot thread
}

/// Trampoline function that calls a boxed FnOnce closure
fn closure_trampoline<F: FnOnce() -> ! + Send + 'static>(closure_ptr: *mut ()) -> ! {
    let closure = unsafe { Box::from_raw(closure_ptr as *mut F) };
    closure()
}

/// Spawn a new preemptible thread with a Rust closure and default stack.
///
/// Uses user thread slots (RESERVED_THREADS..MAX_THREADS) with fixed 128KB stacks.
pub fn spawn_fn<F>(f: F) -> Result<usize, &'static str>
where
    F: FnOnce() -> ! + Send + 'static,
{
    spawn_user_thread_fn_internal(f, false)
}


/// Counter of yield_now() calls observed with IRQs masked (DAIF.I=1).
/// SGIs are gated by DAIF.I, so a yield issued under an IrqGuard (or any
/// IRQ-disabling spinlock) is a silent no-op — the caller will busy-spin
/// instead of yielding, and on this single-core kernel that wedges the
/// timer interrupt too. See docs/STABILITY_URGENT_ISSUES.md issue #1.
pub static YIELD_WITH_IRQS_MASKED: AtomicU64 = AtomicU64::new(0);
static YIELD_MASKED_WARNED: AtomicU32 = AtomicU32::new(0);
const YIELD_MASKED_WARN_LIMIT: u32 = 8;

/// Yield to another thread
#[inline(never)]
pub fn yield_now() {
    #[cfg(target_os = "none")]
    {
        // If DAIF.I is set, the SGI we are about to trigger will not be delivered
        // to this core until IRQs are re-enabled — yield_now becomes a no-op and
        // the caller spins.
        if (akuma_primitives::irq::read_daif() & akuma_primitives::irq::DAIF_I_MASKED) != 0 {
            YIELD_WITH_IRQS_MASKED.fetch_add(1, Ordering::Relaxed);
            let warns = YIELD_MASKED_WARNED.fetch_add(1, Ordering::Relaxed);
            if warns < YIELD_MASKED_WARN_LIMIT {
                let lr: u64;
                unsafe {
                    core::arch::asm!("mov {}, x30", out(reg) lr, options(nomem, nostack));
                }
                let tid = get_current_thread_register();
                safe_print!(96, "[SCHED] WARNING: yield_now with IRQs masked tid={} lr={:#x}\n", tid, lr);
            }
        }
    }
    voluntary_schedule_flag().store(true, Ordering::Release);
    (runtime().trigger_sgi)(0);
}

/// Halt the calling (idle) thread until the next interrupt, AND keep CPU-time
/// accounting honest while doing so.
///
/// The scheduler bills a thread for its entire quantum residency
/// (`now - start_time_us` at switch-out in [`ThreadPool::schedule_indices`]).
/// A raw `wfi` in an idle loop would therefore be billed as busy CPU — inflating
/// `TIME(ms)`/`CPU%` for the idle threads even though the core is halted (the
/// issue-#11/#2 accounting symptom: `top` shows tens of seconds of `TIME(ms)`
/// on threads 0/1 while the host vCPU sits at ~1%). Here we record the entry
/// instant, WFI, then shift this thread's quantum `start_time_us` forward by
/// exactly the halt duration — so the next switch-out bills only the genuinely
/// busy time before/after the halt, never the halt itself.
///
/// IRQs MUST be enabled on entry: WFI halts until a pending IRQ (the periodic
/// timer tick ~10 ms, or a device/rump-NIC IRQ) and that IRQ's handler must run
/// before WFI returns; we mask only briefly around the post-halt bookkeeping to
/// stop a racing timer tick from billing the halt. [`yield_now`] is still the
/// cooperative give-up-the-CPU primitive; this is the complementary "there is
/// nothing to do, so genuinely stop" primitive for idle loops.
#[cfg(target_os = "none")]
pub fn idle_halt() {
    let tid = current_thread_id();
    // Disable preemption for the duration of the halt. Without this, a
    // preemptible idle thread (e.g. the network poller) would be billed and
    // switched out by the timer tick *mid-WFI* — the residency including the
    // halt lands in its CPU-time bucket before we can correct it, and the
    // correction (start_time_us bump below) is discarded at the next switch-in.
    // With preemption disabled the timer's involuntary schedule_indices returns
    // None early (`!voluntary && is_preemption_disabled()`), so the halt is
    // neither billed nor switched. WFI still wakes on the very same timer IRQ
    // (and on device IRQs), and the voluntary yield_now() that follows this in
    // the idle loop bypasses the preemption check, so readying a thread still
    // reschedules immediately. The watchdog (100 ms WARN) tolerates a ≤1-tick
    // (~10 ms) disabled window.
    disable_preemption();
    let entered = (runtime().uptime_us)();
    // Real shared-kernel SMP: an idle core must not hold the Big Kernel Lock while
    // halted, or peer cores can't enter the kernel. Drop it before WFI; the IRQ that
    // wakes us re-takes it via the IRQ-path reconcile as it returns into this thread.
    // No-op unless `cfg(kernel_smp_shared)`.
    crate::bkl::leave_kernel();
    // SAFETY: WFI halts the PE until a pending IRQ. IRQs are enabled on entry, so
    // the interrupt (timer tick / device) is taken and serviced before WFI returns.
    unsafe { core::arch::asm!("wfi", options(nomem, nostack)) };
    // Re-take the BKL for the post-halt bookkeeping below (idempotent if the waking
    // IRQ's reconcile already re-acquired it for this thread) — UNLESS this thread sits
    // inside a deliberately-dropped-BKL window (a guarded net/vfs syscall relaxing via
    // blocking_relax): re-taking here would re-hold the BKL for the window's remainder,
    // exactly the conversion the dropped-window ledger exists to prevent. The
    // bookkeeping below is safe BKL-free — it touches only POOL under its own IRQ-masked
    // lock. No-op unless smp-shared.
    if !crate::bkl::in_dropped_window() {
        crate::bkl::enter_kernel();
        // Profiler only: name this hold. An idle thread never passes a syscall/fault
        // tagging site, so without this its BKL time (this bookkeeping plus the
        // `yield_now()` that follows in the idle loop) is attributed "unknown". Gated on
        // IS_IDLE_THREAD because non-idle callers use `idle_halt()` as a plain wait and
        // must keep their own excursion's tag.
        #[cfg(kernel_smp_shared)]
        if tid < MAX_THREADS && IS_IDLE_THREAD[tid].load(Ordering::Relaxed) {
            crate::sync::set_holder_tag(crate::bkl::current_core_id(), crate::sync::HOLD_TAG_IDLE);
        }
    }
    let halted = (runtime().uptime_us)().saturating_sub(entered);
    if tid < MAX_THREADS && halted > 0 {
        let _guard = IrqGuard::new();
        if let Some(mut pool) = POOL.try_lock() {
            // Shift the quantum start forward by the halt duration so the next
            // switch-out's `now - start_time_us` excludes the time we spent halted.
            pool.slots[tid].start_time_us =
                pool.slots[tid].start_time_us.saturating_add(halted);
        }
    }
    enable_preemption();
}

#[cfg(not(target_os = "none"))]
pub fn idle_halt() {}

/// Cooperative wait for a blocking kernel loop that is polling for external
/// progress (network data, a child exit, …) while holding the Big Kernel Lock.
///
/// First [`yield_now`], so any thread already READY on this core runs. Then, under
/// shared-kernel SMP only, [`idle_halt`] — which DROPS the BKL around a WFI — so a
/// peer core can enter the kernel and produce the progress this loop is waiting on.
/// Without the drop the loop busy-spins holding the BKL (nothing else READY on the
/// core → `yield_now` returns without switching), freezing every peer core: exactly
/// the socket-recv / `exec_with_io_cwd` cross-core wedge (see
/// docs/runbooks/debug-smp.md). We wake on the next timer tick and the caller
/// re-checks its condition.
///
/// Off `cfg(kernel_smp_shared)` this is a plain `yield_now` — single-core / default
/// builds are byte-for-byte unchanged (the `idle_halt` call compiles out).
#[inline]
pub fn blocking_relax() {
    yield_now();
    #[cfg(kernel_smp_shared)]
    idle_halt();
}

/// SIMPLIFIED SGI handler for stack-based context switching
/// 
/// Takes current SP from assembly, returns new SP if switch needed (or 0).
/// The assembly does the actual SP switch AFTER this function returns.
/// This avoids the problem of switching SP in the middle of Rust code.
#[cfg(target_os = "none")]
pub fn sgi_scheduler_handler_with_sp(irq: u32, current_sp: u64) -> u64 {
    (runtime().end_of_interrupt)(irq);

    let voluntary = voluntary_schedule_flag().swap(false, Ordering::Acquire);
    let debug = SGI_DEBUG_ONCE.swap(false, Ordering::SeqCst);
    
    if debug {
        safe_print!(64, "[SGI-DBG] entry voluntary={}\n", voluntary);
    }
    
    // Get scheduling decision. Use try_lock instead of a blocking lock(): this handler
    // runs from the timer-driven SGI in IRQ context (PSTATE.I masked). If the timer
    // interrupted the current thread while it already held POOL (e.g. mid-syscall or
    // mid-fault), a blocking lock() would spin forever with IRQs masked — freezing the
    // box. Skipping one best-effort preemption tick is harmless: the next tick retries.
    // See docs/RUST_TOOLCHAIN_ISSUES.md §3.
    //
    // M5c: `pool` is held across the ENTIRE switch below (decision + context save +
    // new-thread load), not just the decision. `schedule_indices`/`commit_switch` mark
    // the outgoing thread READY (pickable by a peer core) — so the outgoing context MUST
    // be saved before POOL is released, or a peer could pick it and restore a stale SP.
    // The Big Kernel Lock provided that atomicity before; holding POOL across the whole
    // switch makes it hold on POOL alone, so the scheduler SGI no longer needs the BKL.
    if debug { (runtime().print_str)("[SGI-DBG] acquiring POOL\n"); }
    let mut pool = match POOL.try_lock() {
        Some(guard) => guard,
        None => {
            static SGI_POOL_SKIP: AtomicU64 = AtomicU64::new(0);
            let n = SGI_POOL_SKIP.fetch_add(1, Ordering::Relaxed);
            if n.is_multiple_of(1000) {
                let tid = current_thread_id();
                safe_print!(96, "[SGI] POOL contended, skipped {} ticks (tid={})\n", n, tid);
            }
            return 0;
        }
    };
    if debug { (runtime().print_str)("[SGI-DBG] got POOL\n"); }
    // Which core is scheduling — selects this core's idle fallback. 0 on single-core.
    let core_id = crate::bkl::current_core_id() as usize;
    let switch_info = pool.schedule_indices(voluntary, core_id).map(|(old_idx, new_idx)| {
        let new_tpidr = pool.slots[new_idx].exception_stack_top;
        (old_idx, new_idx, new_tpidr)
    });
    if debug {
        if switch_info.is_some() {
            (runtime().print_str)("[SGI-DBG] schedule_indices returned Some\n");
        } else {
            (runtime().print_str)("[SGI-DBG] schedule_indices returned None!\n");
        }
    }

    if let Some((old_idx, new_idx, new_tpidr)) = switch_info {
        // `pool` is still locked here and stays locked until this function returns —
        // covering the context save/load so the switch is atomic on POOL alone.
        if debug || config().enable_sgi_debug_prints {
            safe_print!(64, "[SGI-DBG] switching {} -> {}\n", old_idx, new_idx);
        }
        
        unsafe {
            // Get context pointers
            let old_ctx = get_context_mut(old_idx);
            let new_ctx = get_context(new_idx);
            
            // Save current SP (from IRQ frame) to old context
            (*old_ctx).sp = current_sp;
            
            // Save current TTBR0 to old context
            // CRITICAL: Processes set their own TTBR0 via activate(),
            // so we must save it here to restore correctly later
            let current_ttbr0: u64;
            core::arch::asm!("mrs {}, ttbr0_el1", out(reg) current_ttbr0);
            (*old_ctx).ttbr0 = current_ttbr0;

            // TTBR0 tripwire, save side (§2f AS MISMATCH hunt, see EXPECTED_L0):
            // the live tables at switch-out must be the outgoing thread's own.
            // A miss here means this thread was RUNNING under someone else's L0
            // — and the save above just made that permanent in its context.
            {
                let expected = EXPECTED_L0[old_idx].load(Ordering::Acquire);
                if expected != 0 && current_ttbr0 & TTBR0_L0_MASK != expected {
                    safe_print!(160,
                        "[TTBR SAVE-MISMATCH] core={} old_tid={} live={:#x} expected_l0={:#x} new_tid={}\n",
                        core_id, old_idx, current_ttbr0, expected, new_idx);
                }
            }

            // Load new SP from new context
            let new_sp = (*new_ctx).sp;

            // Verify new SP is valid — FULLY, because code below **dereferences** it
            // (the POISON tripwire reads `new_sp + 240` / `+ 248`) and `ldp`/`stp` on a
            // bad SP inside this handler is unrecoverable: the EL1 sync fault it raises
            // re-faults in its own prologue, so the core spins at
            // `exception_vector_table + 0x200` forever with IRQs masked — no console
            // output, no watchdog, no clock. That silent hang is what this check exists
            // to convert into the loud line below.
            //
            // The old test was `new_sp == 0 || new_sp < 0x4000_0000`, which accepted any
            // garbage at or above RAM base: a stale SP, a recycled slot's SP, or one past
            // RAM end all sailed through and hung the machine at the tripwire read
            // instead of reporting anything (`COW_PILE_AUDIT.md` §9 F8; a `new_sp=0x0`
            // sighting is the same defect with the one value the old test caught).
            //
            // Three things must hold before any dereference:
            //   * inside the live RAM window — `ram_base`/`ram_end`, not a hardcoded
            //     floor, since the window moves with `MEMORY=`;
            //   * far enough below `ram_end` that the tripwire's `+248` read and the
            //     restore's own frame stay in RAM;
            //   * 16-byte aligned, or the first `stp` raises an SP-alignment fault with
            //     exactly the same unrecoverable shape as an unmapped SP.
            let ram_base = crate::mmu::ram_base() as u64;
            let ram_end = crate::mmu::ram_end() as u64;
            // The restored IRQ frame is 256 bytes (see `setup_fake_irq_frame`); require
            // it whole rather than just the two tripwire words.
            const RESTORE_FRAME_BYTES: u64 = 256;
            let sp_ok = new_sp >= ram_base
                && ram_end.saturating_sub(new_sp) >= RESTORE_FRAME_BYTES
                && new_sp % 16 == 0;
            if !sp_ok {
                // Name the threads: the culprit is whoever left this SP in `new_idx`'s
                // context, and without both ids the next occurrence is unattributable.
                safe_print!(192,
                    "[SGI-S FATAL] new_sp={:#x} invalid! old_tid={} new_tid={} \
                     ram=[{:#x},{:#x}) aligned={}\n",
                    new_sp, old_idx, new_idx, ram_base, ram_end, new_sp % 16 == 0);
                loop { core::arch::asm!("wfi"); }
            }

            // Update exception stack for new thread
            set_current_exception_stack(new_tpidr);
            
            // Load TTBR0 for new thread
            let new_ttbr0 = (*new_ctx).ttbr0;

            // TTBR0 tripwire, restore side: the tables we are about to install
            // must be the incoming thread's own. A miss means its saved context
            // was corrupted while it was off-CPU (a wrong-old_idx save, or a
            // stale-slot revival — the READY-on-INITIALIZING race).
            SWITCH_INS[new_idx].fetch_add(1, Ordering::Relaxed);
            {
                let expected = EXPECTED_L0[new_idx].load(Ordering::Acquire);
                if expected != 0 && new_ttbr0 & TTBR0_L0_MASK != expected {
                    safe_print!(160,
                        "[TTBR LOAD-MISMATCH] core={} new_tid={} ctx={:#x} expected_l0={:#x} old_tid={}\n",
                        core_id, new_idx, new_ttbr0, expected, old_idx);
                }
            }

            // F8 tripwire: an incoming context whose TTBR0 names a FREED L0 is the
            // fault-loop wedge one instruction before it happens — installing it
            // unmaps kernel text (kernel runs in the TTBR0 low half), the next
            // fetch aborts, and the vector entry's own fetch aborts recursively
            // (PC pinned at vector+0x200, ESR=0x86000004). Print the culprit pair
            // BEFORE the install so the wedge is attributable from the log.
            if crate::mmu::l0_recently_freed(new_ttbr0 & TTBR0_L0_MASK) {
                safe_print!(160,
                    "[SGI-S FREED-L0] new_ttbr0={:#x} old_tid={} new_tid={} new_state={}\n",
                    new_ttbr0, old_idx, new_idx,
                    THREAD_STATES[new_idx].load(Ordering::SeqCst));
            }

            // Publish the transition to the per-core live-TTBR0 registry (the
            // page-table-UAF free gate, see mmu::any_core_on_l0): PREV covers the
            // outgoing table across the msr window, ACTIVE covers the incoming one
            // from before the hardware can walk it.
            let pub_core = crate::mmu::publish_l0_begin(new_ttbr0);
            core::arch::asm!(
                "dsb ish",
                "msr ttbr0_el1, {ttbr0}",
                "isb",
                "tlbi vmalle1",
                "dsb ish",
                "isb",
                ttbr0 = in(reg) new_ttbr0,
            );
            crate::mmu::publish_l0_end(pub_core);
            
            if config().enable_sgi_debug_prints {
                safe_print!(64, "[SGI-S] returning new_sp={:#x}\n", new_sp);
            }

            // Tripwire for the SMP=4 mixed-EL corruption (user PC = kernel text,
            // SPSR = EL0t): inspect the IRQ frame we are about to restore (ELR at
            // +240, SPSR at +248 — see setup_fake_irq_frame). An EL0-target frame
            // whose ELR is a kernel address would eret userspace into kernel text —
            // catch it HERE, at the restore, with both thread ids.
            {
                let elr = ((new_sp + 240) as *const u64).read_volatile();
                let spsr = ((new_sp + 248) as *const u64).read_volatile();
                let kernel_text = (0x4010_0000..0x6000_0000).contains(&elr);
                // EL0-target frames must not eret into kernel text (NOT merely
                // ≥0x4000_0000 — user mmap VAs legitimately reach 0x1_xxxx_xxxx+);
                // EL1-target frames must eret INTO kernel text (ELR=0x8 shape).
                let poison = if (spsr & 0xF) == 0 { kernel_text } else { !kernel_text };
                if poison {
                    safe_print!(128,
                        "[SGI-S POISON] eret elr={:#x} spsr={:#x} old_tid={} new_tid={} sp={:#x}\n",
                        elr, spsr, old_idx, new_idx, new_sp);
                }
            }

            // Stack-aliasing tripwire (same corruption family): the frame we are
            // about to restore MUST lie within the incoming thread's own kernel
            // stack. A miss means ctx.sp points into another thread's (or a freed
            // and reused) stack — the restore would eret with whatever kernel
            // locals live there, which is exactly the §5.1 "EL0 return with a
            // kernel register context" shape (BKL_RUSTC_SCALING_BASELINE.md).
            // Idle threads are exempt: each core's idle is seeded at bringup on
            // its per-core BOOT stack (see the "sp/ttbr0 are captured on the
            // first switch-out from idle" seeding), which is not the pool stack
            // registered for its slot — every switch into an idle thread fired
            // this line, ~20k times per SMP=4 boot, drowning the real signal.
            if !IS_IDLE_THREAD[new_idx].load(Ordering::Relaxed) {
                let si = &pool.stacks[new_idx];
                let lo = si.base as u64;
                let hi = si.top as u64;
                if si.size != 0 && (new_sp < lo || new_sp + 832 > hi) {
                    safe_print!(160,
                        "[SGI-S STACK] new_sp={:#x} outside stack [{:#x},{:#x}) old_tid={} new_tid={}\n",
                        new_sp, lo, hi, old_idx, new_idx);
                }
            }

            // Return new SP - assembly will do the switch
            return new_sp;
        }
    }
    
    0  // No switch needed
}

/// Called from the IRQ vector asm immediately after `mov sp, x0`: this core is
/// now off the outgoing thread's kernel stack, so peers may pick that thread up
/// (or recycle its slot). Clears the off-CPU gate `commit_switch` latched.
/// Runs with IRQs masked on the incoming thread's stack; takes no locks.
#[unsafe(no_mangle)]
pub extern "C" fn rust_switch_finished() {
    let core = crate::bkl::current_core_id() as usize;
    if core < MAX_CORES {
        let tid = PER_CORE_OFFCPU[core].swap(usize::MAX, Ordering::SeqCst);
        if tid < MAX_THREADS {
            ON_CPU[tid].store(0, Ordering::Release);
        }
    }
}

/// Is `l0_base` still referenced by any thread slot's SAVED context? Returns the
/// first `(tid, state)` found. The address-space free path consults this in
/// addition to the per-core gate (`mmu::any_core_on_l0`): a live core's TTBR0 is
/// not the only reference the scheduler can turn back into hardware state — the
/// SGI switch installs `ctx.ttbr0` verbatim from the incoming thread's saved
/// context, so a saved reference to a freed L0 is a machine-wedging fault loop
/// waiting for its switch-in (F8, `COW_PILE_AUDIT.md` §10: recursive instruction
/// abort at `vector+0x200`, ESR=0x86000004, kernel text unmapped by the install).
///
/// Every state is a blocker on purpose, even the "unschedulable" ones: FREE and
/// INITIALIZING contexts are overwritten by spawn before the slot can go READY,
/// and TERMINATED contexts are zeroed by the recycler — both of which UNPIN a
/// deferred free at the next `drain_pending_ttbr_frees` — so treating them as
/// blockers costs only a short deferral, while trusting the state machine costs
/// the machine if any revival route exists that the model missed.
/// Bounded loop, no heap, no locks.
pub fn any_saved_ctx_on_l0(l0_base: u64) -> Option<(usize, u8)> {
    if l0_base == 0 {
        return None;
    }
    (0..MAX_THREADS).find_map(|i| {
        let ttbr0 = unsafe { (*get_context(i)).ttbr0 };
        (ttbr0 & TTBR0_L0_MASK == l0_base)
            .then(|| (i, THREAD_STATES[i].load(Ordering::SeqCst)))
    })
}

/// Test hook: swap a slot's saved-context TTBR0, returning the previous value
/// (boot-suite self-tests only — they can't get a real thread parked with a
/// chosen stale TTBR0 in its saved context).
#[doc(hidden)]
pub fn test_swap_saved_ctx_ttbr0(slot: usize, ttbr0: u64) -> u64 {
    unsafe {
        let ctx = &mut *get_context_mut(slot % MAX_THREADS);
        core::mem::replace(&mut ctx.ttbr0, ttbr0)
    }
}

/// Test/diagnostic accessor: whether `tid`'s [`ON_CPU`] gate is set.
pub fn on_cpu_flag(tid: usize) -> bool {
    tid < MAX_THREADS && ON_CPU[tid].load(Ordering::SeqCst) != 0
}

/// Test/diagnostic accessor: how many [`ON_CPU`] gates are set. Bounded by the
/// number of cores on a healthy system (each core runs exactly one thread, plus
/// at most one not-yet-cleared outgoing gate per core mid-switch).
pub fn on_cpu_count() -> usize {
    (0..MAX_THREADS)
        .filter(|&i| ON_CPU[i].load(Ordering::SeqCst) != 0)
        .count()
}

/// Update a thread's context for a new execution (e.g., after execve or fork)
pub fn update_thread_context(thread_id: usize, user_context: &crate::process::UserContext) {
    // Tripwire for the SMP=4 mixed-EL corruption: a "user" context whose PC is a
    // kernel address is poison at CREATION time (the capture read a clobbered
    // frame/slot) — catch it here with the culprit tid before it is published.
    if user_context.pc >= 0x4000_0000 {
        safe_print!(128,
            "[CTX POISON] update_thread_context tid={} pc={:#x} spsr={:#x} (from tid={})\n",
            thread_id, user_context.pc, user_context.spsr, current_thread_id());
    }
    // Disable IRQs to safely access context
    with_irqs_disabled(|| {
        unsafe {
            let ctx = &mut *get_context_mut(thread_id);
            
            // Update context fields that are directly in Context struct
            ctx.elr = user_context.pc;
            // ctx.sp points to the kernel stack top (where the trap frame is).
            // We generally don't change ctx.sp for fork(), we want to keep the stack frame.
            // But for execve(), we might want to reset it?
            // For fork(), the thread is NEW, so ctx.sp points to the fake frame we just built.
            // We should NOT change ctx.sp to user_context.sp (which is a user stack pointer!).
            
            ctx.spsr = user_context.spsr;
            ctx.ttbr0 = user_context.ttbr0;

            // TTBR0 tripwire baseline. Only when building ANOTHER thread's context
            // (clone/fork/vfork child init — the canonical ttbr0 the parent chose).
            // For the current thread (execve rewriting itself) the expected value
            // must follow the LIVE tables, which `activate()` stamps when it
            // installs them; stamping the new L0 here would false-positive the
            // save-side check if we're preempted before that activate.
            if thread_id != get_current_thread_register() {
                note_thread_expected_l0(thread_id, user_context.ttbr0);
            }

            ctx.user_entry = user_context.pc;
            ctx.user_sp = user_context.sp;
            ctx.user_tls = user_context.tpidr;
            ctx.is_user_process = 1;
            
            // Update registers in the trap frame on the stack
            // The trap frame is at ctx.sp
            let frame_ptr = ctx.sp as *mut u64;
            
            // Frame layout from setup_fake_irq_frame / IRQ handler:
            // [sp+224]: x0, x1
            frame_ptr.add(224/8).write_volatile(user_context.x0);
            frame_ptr.add(232/8).write_volatile(user_context.x1);
            
            // We can update other registers if needed, but for fork() x0=0 is the main one.
            // The trap frame has 0 for others by default (from setup_fake_irq_frame).
            // If we want to copy all registers from parent (for full fork), we should do it here.
            // But UserContext only has x0-x30 if we added them.
            // For now, updating x0 is sufficient for vfork return value.
        }
    });
}

// ============================================================================
// Waker Integration
// ============================================================================

use core::task::{RawWaker, RawWakerVTable, Waker};

/// Waker implementation for thread-based waking. Carries a [`WakeHandle`], so
/// it wakes one *incarnation* of the slot — a waker created for a thread that
/// has since exited (and whose slot was recycled) does nothing at all.
pub struct ThreadWaker {
    handle: WakeHandle,
}

impl ThreadWaker {
    /// Waker for `thread_id`'s CURRENT incarnation — the caller must know the
    /// tid is live now (see [`wake_handle_for_thread`]).
    pub fn new(thread_id: usize) -> Self {
        Self { handle: wake_handle_for_thread(thread_id) }
    }

    /// Wake the thread associated with this waker
    pub fn wake(&self) {
        let tid = self.handle.tid();
        if tid < MAX_THREADS {
            // Generation gate FIRST, before any side effect: a stale handle's
            // sticky-flag store would spend a phantom wake on the slot's next
            // occupant (an early return from its first park), and its READY
            // CAS could fire against a new occupant that happens to be
            // legitimately WAITING — a spurious wake of the wrong thread.
            // (A recycle that lands between this check and the stores below is
            // the same benign spurious-wake, shrunk from an unbounded window
            // to a few instructions; the CAS below keeps it corruption-free.)
            if !self.handle.is_current() {
                return;
            }

            // Set sticky wake flag so schedule_blocking knows we were woken
            WOKEN_STATES[tid].store(true, Ordering::SeqCst);

            // WAITING -> READY must be a CAS, not check-then-store. This runs with no
            // lock and is preemptible between the two halves, so a check-then-store
            // waker that gets switched out after observing WAITING can resume
            // arbitrarily later — after the target woke by timeout, ran, exited, and
            // its slot was reclaimed and re-claimed by a new clone — and then stamp
            // READY onto an INITIALIZING (context half-written) or TERMINATED slot.
            // A peer scheduler picking that slot restores the PREVIOUS occupant's
            // context: its ttbr0 (a third party's page tables — the §2f
            // "AS MISMATCH" in debug-thread-spawn-segv.md) and its kernel stack,
            // possibly still in use (double-run — the [BKL] stuck storm shape).
            //
            // WAKE_TIMES is deliberately NOT cleared here: the same stale-waker
            // window let the old `store(0)` erase a FRESH deadline the slot's new
            // occupant had just published (mark_thread_waiting), leaving it parked
            // forever with no timeout. Every sleep entry rewrites WAKE_TIMES, and
            // the wake-pass only reads it for WAITING threads, so a leftover value
            // on a READY thread is inert.
            if THREAD_STATES[tid]
                .compare_exchange(
                    thread_state::WAITING,
                    thread_state::READY,
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                )
                .is_ok()
            {
                // Wakeup locality (gated off — see PREEMPT_WAKE_TID / WAKEUP_LOCALITY_HINT).
                if WAKEUP_LOCALITY_HINT {
                    PREEMPT_WAKE_TID.store(tid, Ordering::SeqCst);
                }
                // Trigger SGI to ensure scheduler runs and picks up the thread
                (runtime().trigger_sgi)(0);
                // Under real shared-kernel SMP, also nudge the woken thread's last-known
                // core directly so its scheduler picks up the READY thread within this
                // SGI's latency rather than waiting for the ~10 ms timer tick. Without
                // this, a cross-core wake relies on the target core's timer tick to
                // notice the READY thread, which under heavy POOL contention can be
                // delayed by many ticks — long enough for a barrier/condvar round to
                // stall until the futex revalidation timeout rescues it.
                let last_core = LAST_CORE[tid].load(Ordering::Relaxed);
                if last_core != 0xFF {
                    (runtime().wake_core)(last_core);
                }
            }
        }
    }
}

/// Creates a RawWaker around a packed [`WakeHandle`] — the generation rides in
/// the data pointer, so a `Waker` cloned into an executor or wait registry
/// stays incarnation-bound with no allocation.
fn waker_from_handle(handle: WakeHandle) -> RawWaker {
    let ptr = handle.0 as *const ();
    RawWaker::new(ptr, &THREAD_WAKER_VTABLE)
}

/// Creates a waker for the current thread (its current incarnation).
pub fn current_thread_waker() -> Waker {
    unsafe { Waker::from_raw(waker_from_handle(current_wake_handle())) }
}

const THREAD_WAKER_VTABLE: RawWakerVTable = RawWakerVTable::new(
    thread_waker_clone,
    thread_waker_wake,
    thread_waker_wake_by_ref,
    thread_waker_drop,
);

unsafe fn thread_waker_clone(data: *const ()) -> RawWaker {
    RawWaker::new(data, &THREAD_WAKER_VTABLE)
}

unsafe fn thread_waker_wake(data: *const ()) {
    wake_by_handle(WakeHandle(data as u64));
}

unsafe fn thread_waker_wake_by_ref(data: *const ()) {
    wake_by_handle(WakeHandle(data as u64));
}

unsafe fn thread_waker_drop(_data: *const ()) {
    // No-op, waker doesn't own any resources
}

/// Returns a Waker for `thread_id`'s CURRENT incarnation. Only meaningful
/// where the caller knows the tid is live right now — a waiter registering
/// itself, or a wake site that has just validated liveness. Calling this at
/// wake time on a tid stored long ago launders a stale tid into a
/// fresh-looking handle; store a [`WakeHandle`] at enqueue time instead and
/// wake with [`wake_by_handle`].
pub fn get_waker_for_thread(thread_id: usize) -> Waker {
    unsafe { Waker::from_raw(waker_from_handle(wake_handle_for_thread(thread_id))) }
}

/// Block the current thread until the specified wake time, then yield
/// 
/// This is safe to call from syscall handlers. The thread will be marked as
/// WAITING and will not be scheduled until:
/// 1. The wake_time_us deadline has passed, OR
/// 2. An external event wakes the thread (not yet implemented)
///
/// When the thread is woken, it resumes execution right after this function returns.
/// 
/// # TTBR0 Handling
/// 
/// When called from a syscall context, TTBR0 contains user page tables.
/// We must switch to kernel (boot) TTBR0 before yielding so that:
/// 1. the context switch saves kernel TTBR0, not user TTBR0
/// 2. When resumed, kernel code can access all kernel memory
/// 
/// After resuming, we restore the user TTBR0 before returning to syscall handler.
/// Sleep the current thread for approximately `us` microseconds by blocking until an
/// absolute wake time. Thin wrapper over [`schedule_blocking`] that computes the
/// deadline from the current uptime. Used by the real-SMP worker demo (and any kernel
/// thread that wants a timed sleep without the syscall layer).
pub fn sleep_us(us: u64) {
    let now = (runtime().uptime_us)();
    schedule_blocking(now.saturating_add(us));
}

/// Publish `WAITING` for `tid` and consume a wake that raced the publication.
/// Returns `true` if a wake was already pending, meaning the caller must **not** park.
///
/// # The lost wakeup this closes
///
/// [`ThreadWaker::wake`] is two steps: set the sticky `WOKEN_STATES` flag, then — *only
/// if the target is already `WAITING`* — flip it to `READY` and ring the scheduler. A
/// waker that arrives while the target is still `RUNNING` therefore leaves nothing but
/// the sticky flag behind, and `WOKEN_STATES` is read in exactly one place: this
/// function and [`schedule_blocking`]'s park loop. Nothing in the scheduler ever
/// reconsiders a `WAITING` thread on account of it.
///
/// So a thread that stores `WAITING` and is descheduled before it re-reads the flag is
/// stranded forever. That is not a narrow window — `schedule_blocking` *asks* to be
/// switched out immediately after publishing (`voluntary_schedule_flag` + a self-SGI),
/// so the common path is that the park loop's own re-check runs only after the thread
/// is resumed. The unguarded gap is between `schedule_blocking`'s entry check and the
/// `WAITING` store: a peer core that pops this tid out of `FUTEX_WAITERS` in that gap
/// records the wake as delivered, and the waiter never runs again.
///
/// Diagnosed from `[FUTEX-ORPHAN]` histories ending `EpW` — enqueued, parked, popped by
/// `futex_do_wake` — with no `u` (`schedule_blocking` returned) ever following, on
/// musl's `__thread_list_lock` during a `-j4` in-VM self-host build. See
/// `docs/runbooks/debug-futex-lost-wakeup.md` §4a.
///
/// # Why masking IRQs is sufficient
///
/// A context switch on this core only happens through an IRQ, so masking makes the
/// store-then-check pair atomic locally. Against a peer core the two `SeqCst` variables
/// give a total order with no losing interleaving: if the waker's `WOKEN_STATES` store
/// precedes our swap we observe it and refuse to park; if it follows our swap, then our
/// `WAITING` store precedes it too, so the waker's subsequent state load sees `WAITING`
/// and performs the `READY` transition itself.
fn publish_waiting_and_take_pending_wake(tid: usize, wake_time_us: u64) -> bool {
    let _guard = IrqGuard::new();
    mark_thread_waiting(tid, wake_time_us);
    if !WOKEN_STATES[tid].swap(false, Ordering::SeqCst) {
        return false;
    }
    // A wake landed before we were visible as WAITING, so the waker did not ready us and
    // never will. Undo the publication ourselves.
    WAKE_TIMES[tid].store(0, Ordering::SeqCst);
    // Same guard as the park loop: `kill_thread_group` may have marked us TERMINATED,
    // and resuming as RUNNING would return to user mode with freed resources. One
    // atomic RMW — a TERMINATED landing between a load and a plain store would be
    // overwritten (see commit_switch).
    resume_running_unless_terminated(tid);
    true
}

/// Flip `tid` back to RUNNING unless it has been marked TERMINATED — as one atomic
/// RMW, so a cross-thread `mark_thread_terminated` (kill_thread_group) can never be
/// overwritten by the resume. Only ever called by the thread itself as it comes out
/// of a park, where every state except TERMINATED means "we own the slot again".
fn resume_running_unless_terminated(tid: usize) {
    let _ = THREAD_STATES[tid].fetch_update(
        Ordering::SeqCst,
        Ordering::SeqCst,
        |s| (s != thread_state::TERMINATED).then_some(thread_state::RUNNING),
    );
}

pub fn schedule_blocking(wake_time_us: u64) {
    let tid = current_thread_id();

    // Check if we were already woken (sticky wake)
    if WOKEN_STATES[tid].swap(false, Ordering::SeqCst) {
        return;
    }

    let now = (runtime().uptime_us)();
    
    // Check if already past deadline - don't bother blocking
    if now >= wake_time_us {
        return;
    }

    // Save current preemption state and ensure it's enabled for the block
    let was_disabled = is_preemption_disabled();
    if was_disabled {
        // Log this as it might be a sign of a bug (blocking while holding a lock)
        // but we'll allow it by temporarily enabling preemption.
        // safe_print!(64, "[threading] schedule_blocking called with preemption disabled (tid={})\n", tid);
        
        // We MUST enable preemption here, otherwise the timer IRQ will acknowledge
        // but will NOT schedule another thread, leading to a hang in the wfi loop.
        enable_preemption();
    }
    
    // Mark thread as WAITING with wake time, and re-check the sticky wake flag
    // *atomically with respect to being descheduled*. See
    // `publish_waiting_and_take_pending_wake` — skipping this leaks a wake permanently.
    if publish_waiting_and_take_pending_wake(tid, wake_time_us) {
        if was_disabled {
            disable_preemption();
        }
        return;
    }

    // Immediately hand the CPU to a ready thread instead of `WFI`-ing until the next
    // timer tick preempts us. We're already WAITING, so the scheduler switches us out
    // and we leave the round-robin until a waker (or our wake_time) readies us — no
    // busy-spin. Without this, a block→switch waits up to one tick: fine on the BSP
    // (device IRQs drive reschedules) but on a secondary two cooperating threads (the
    // rump sysproxy client↔server pipe hop) have NO IRQ between them, so every hop was
    // tick-bound (~10 ms). A voluntary SGI bypasses the preemption-disabled guard too.
    voluntary_schedule_flag().store(true, Ordering::Release);
    (runtime().trigger_sgi)(0);

    // Wait for timer to preempt us and for scheduler to wake us
    loop {
        // Double check sticky wake flag in loop
        if WOKEN_STATES[tid].swap(false, Ordering::SeqCst) {
            WAKE_TIMES[tid].store(0, Ordering::SeqCst);
            // Don't overwrite TERMINATED — kill_thread_group may have marked
            // this thread terminated while we were waiting. Resuming as
            // RUNNING would let the thread return to user mode with freed
            // resources (lazy regions cleared, fds closed). Atomic RMW so a
            // TERMINATED landing mid-check can't be overwritten.
            resume_running_unless_terminated(tid);
            break;
        }

        let state = THREAD_STATES[tid].load(Ordering::SeqCst);
        if state != thread_state::WAITING {
            break;
        }

        if crate::process::is_current_interrupted() {
            WAKE_TIMES[tid].store(0, Ordering::SeqCst);
            // Same TERMINATED guard as above — the old unconditional RUNNING
            // store here could resurrect a thread killed while it waited.
            resume_running_unless_terminated(tid);
            break;
        }
        
        // Wait for interrupt - timer IRQ will fire within 10ms
        #[cfg(target_os = "none")]
        unsafe { core::arch::asm!("wfi"); }
    }

    // Restore preemption state
    if was_disabled {
        disable_preemption();
    }
}

/// Get thread stats (ready, running, terminated) - LOCK-FREE
pub fn thread_stats() -> (usize, usize, usize) {
    let mut ready = 0;
    let mut running = 0;
    let mut terminated = 0;
    for i in 0..MAX_THREADS {
        match THREAD_STATES[i].load(Ordering::Relaxed) {
            thread_state::READY => ready += 1,
            thread_state::RUNNING => running += 1,
            thread_state::TERMINATED => terminated += 1,
            _ => {}
        }
    }
    (ready, running, terminated)
}


/// Get counts for all thread states (lock-free)
pub fn thread_stats_full() -> ThreadStatsFull {
    let mut stats = ThreadStatsFull {
        free: 0,
        ready: 0,
        running: 0,
        terminated: 0,
        initializing: 0,
        waiting: 0,
    };
    for i in 0..MAX_THREADS {
        match THREAD_STATES[i].load(Ordering::Relaxed) {
            thread_state::FREE => stats.free += 1,
            thread_state::READY => stats.ready += 1,
            thread_state::RUNNING => stats.running += 1,
            thread_state::TERMINATED => stats.terminated += 1,
            thread_state::INITIALIZING => stats.initializing += 1,
            thread_state::WAITING => stats.waiting += 1,
            _ => {}
        }
    }
    stats
}

/// Clean up terminated threads (mark slots as free) - LOCK-FREE
pub fn cleanup_terminated() -> usize {
    cleanup_terminated_lockfree()
}

/// Get active thread count
pub fn thread_count() -> usize {
    // Lock-free: count non-free threads
    (0..MAX_THREADS)
        .filter(|&i| THREAD_STATES[i].load(Ordering::Relaxed) != thread_state::FREE)
        .count()
}

/// Get stack info for a specific thread (base, top)
/// Returns None if thread index is invalid
pub fn get_thread_stack_info(tid: usize) -> Option<(usize, usize)> {
    if tid >= MAX_THREADS {
        return None;
    }
    // Disable IRQs to prevent deadlock if timer fires while we hold the lock
    with_irqs_disabled(|| {
        let pool = POOL.lock();
        let stack = &pool.stacks[tid];
        if stack.base == 0 {
            None
        } else {
            Some((stack.base, stack.top))
        }
    })
}

/// Mark current thread as terminated (thread 0 cannot be terminated) - LOCK-FREE
#[track_caller]
pub fn mark_current_terminated() {
    let idx = get_current_thread_register();
    if idx != IDLE_THREAD_IDX {
        mark_thread_terminated(idx);
    }
}

/// Get the current thread's user copy fault handler address.
///
/// Lock-free (see `USER_COPY_FAULT_HANDLER`): called from the data-abort handler,
/// where taking `POOL.lock()` could self-deadlock a nested fault.
pub fn get_user_copy_fault_handler() -> u64 {
    let tid = get_current_thread_register();
    if tid < MAX_THREADS {
        USER_COPY_FAULT_HANDLER[tid].load(Ordering::Acquire)
    } else {
        0
    }
}

/// Set the current thread's user copy fault handler address. Lock-free.
pub fn set_user_copy_fault_handler(handler: u64) {
    let tid = get_current_thread_register();
    if tid < MAX_THREADS {
        USER_COPY_FAULT_HANDLER[tid].store(handler, Ordering::Release);
    }
}

/// Get current thread ID from TPIDR_EL0 register
/// This is more reliable than a global atomic as it's per-CPU
#[inline]
pub fn current_thread_id() -> usize {
    get_current_thread_register()
}

/// Pend a signal on the given thread slot.
/// The signal will be delivered at the next syscall return for that thread.
/// Overwrites any previously pending signal (only one pending signal supported).
/// Also wakes the thread if it is sleeping (e.g. in FUTEX_WAIT).
/// sig=0 clears all pending signals (used by tests and cleanup).
pub fn pend_signal_for_thread(tid: usize, sig: u32) {
    if tid >= MAX_THREADS { return; }
    if sig == 0 {
        PENDING_SIGNALS[tid].store(0, Ordering::Release);
        return;
    }
    if sig <= 64 {
        let bit = 1u64 << (sig - 1);
        PENDING_SIGNALS[tid].fetch_or(bit, Ordering::Release);
        get_waker_for_thread(tid).wake();
    }
}

/// Peek at pending signals for a thread slot without consuming.
/// Returns the lowest pending signal number, or 0 if none.
pub fn peek_pending_signal(slot: usize) -> u32 {
    if slot < MAX_THREADS {
        let bits = PENDING_SIGNALS[slot].load(Ordering::Acquire);
        if bits != 0 { bits.trailing_zeros() as u32 + 1 } else { 0 }
    } else {
        0
    }
}

/// Clear a specific pending signal for a thread.
/// Used to prevent SIGURG delivery to uninitialized Go M-threads.
pub fn clear_pending_signal(slot: usize, sig: u32) {
    if slot < MAX_THREADS && sig > 0 && sig <= 64 {
        let bit = 1u64 << (sig - 1);
        PENDING_SIGNALS[slot].fetch_and(!bit, Ordering::AcqRel);
    }
}

/// Get the per-thread alternate signal stack (sp, size, flags).
/// Returns (0, 0, SS_DISABLE=2) for an unset or out-of-range slot.
pub fn get_sigaltstack(slot: usize) -> (u64, u64, i32) {
    if slot < MAX_THREADS {
        (
            THREAD_SIGALTSTACK_SP[slot].load(Ordering::Acquire),
            THREAD_SIGALTSTACK_SIZE[slot].load(Ordering::Acquire),
            THREAD_SIGALTSTACK_FLAGS[slot].load(Ordering::Acquire) as i32,
        )
    } else {
        (0, 0, 2) // SS_DISABLE
    }
}

/// Set the per-thread alternate signal stack.
pub fn set_sigaltstack(slot: usize, sp: u64, size: u64, flags: i32) {
    if slot < MAX_THREADS {
        THREAD_SIGALTSTACK_SP[slot].store(sp, Ordering::Release);
        THREAD_SIGALTSTACK_SIZE[slot].store(size, Ordering::Release);
        THREAD_SIGALTSTACK_FLAGS[slot].store(flags as u32, Ordering::Release);
    }
}

/// Take the lowest-numbered pending signal for the current thread that is not
/// masked.  Returns Some(sig) and clears that signal's bit, or None.
/// SIGKILL (9) and SIGSTOP (19) bypass the mask entirely.
pub fn take_pending_signal(mask: u64) -> Option<u32> {
    let tid = get_current_thread_register();
    if tid >= MAX_THREADS { return None; }
    let pending = PENDING_SIGNALS[tid].load(Ordering::Acquire);
    if pending == 0 { return None; }
    // SIGKILL and SIGSTOP bits cannot be blocked
    let force_bits: u64 = (1u64 << 8) | (1u64 << 18); // bits for sig 9 and 19
    let deliverable = pending & (!mask | force_bits);
    if deliverable == 0 { return None; }
    let sig = deliverable.trailing_zeros() as u32 + 1;
    let bit = 1u64 << (sig - 1);
    PENDING_SIGNALS[tid].fetch_and(!bit, Ordering::AcqRel);
    Some(sig)
}

/// Get max thread count
pub fn max_threads() -> usize {
    MAX_THREADS
}

// ============================================================================
// System Thread API (for SSH sessions, etc.)
// ============================================================================

/// Spawn a thread specifically for system services (SSH sessions, etc.) - LOCK-FREE
///
/// Only spawns in slots 1..RESERVED_THREADS (system thread range).
/// These threads get larger stacks (256KB) and are preemptible.
/// Returns the thread ID or error if no system thread slots are available.
pub fn spawn_system_thread_fn<F>(f: F) -> Result<usize, &'static str>
where
    F: FnOnce() -> ! + Send + 'static,
{
    // Step 1: Atomically claim a free slot (lock-free). Same reclaim-then-retry as
    // the user-thread path — see `spawn_user_thread_fn_internal`.
    let slot_idx = match claim_free_slot(1, config().reserved_threads) {
        Some(idx) => idx,
        None => {
            if reclaim_terminated_slots() == 0 {
                return Err("No free system thread slots");
            }
            match claim_free_slot(1, config().reserved_threads) {
                Some(idx) => idx,
                None => return Err("No free system thread slots"),
            }
        }
    };

    // Lazy stacks: ensure this slot has a stack (no-op if pre-allocated). On a
    // genuinely exhausted PMM this fails — release the slot and report ENOMEM
    // rather than running on a zero stack pointer.
    if !ensure_slot_stack(slot_idx, config().system_thread_stack_size) {
        THREAD_STATES[slot_idx].store(thread_state::FREE, Ordering::SeqCst);
        return Err("Failed to allocate system thread stack from PMM");
    }

    // Step 2: Box the closure (heap allocation - no lock held!)
    let boxed: Box<F> = Box::new(f);
    let closure_ptr = Box::into_raw(boxed) as *mut ();
    let trampoline: fn(*mut ()) -> ! = closure_trampoline::<F>;

    // Step 3: Set up fake IRQ frame and context
    // This enables stack-based context switching
    with_irqs_disabled(|| {
        // Get stack info from POOL (brief lock)
        let stack_top = {
            let pool = POOL.lock();
            let stack = &pool.stacks[slot_idx];
            // Initial stack top is BELOW the exception area
            ((stack.top - EXCEPTION_STACK_SIZE) & !0xF) as u64
        };
        
        // Get STORED boot TTBR0 (not current, which could be user process's!)
        let boot_ttbr0 = crate::mmu::get_boot_ttbr0();

        // Set up fake IRQ frame for stack-based context switching
        let sp = setup_fake_irq_frame(
            stack_top,
            thread_start_closure as *const () as u64,  // ELR - where to jump
            trampoline as *const () as u64,            // x19 - trampoline
            closure_ptr as u64,                        // x20 - closure data
            0,                                         // x21 - enable IRQs
        );
        
        // Debug output
        safe_print!(128, "[spawn_system_fn SIMPLE] tid={} stack_top={:#x} irq_sp={:#x}\n",
            slot_idx, stack_top, sp);

        // Write minimal context - only SP and TTBR0 needed for simple path
        unsafe {
            let ctx = &mut *get_context_mut(slot_idx);
            ctx.magic = CONTEXT_MAGIC;
            ctx.sp = sp;
            ctx.ttbr0 = boot_ttbr0;
            // Legacy fields for compatibility
            ctx.x19 = trampoline as *const () as u64;
            ctx.x20 = closure_ptr as u64;
            ctx.x30 = thread_start_closure as *const () as u64;
            ctx.elr = thread_start_closure as *const () as u64;
            ctx.spsr = 0x00000345;
        }

        // Write slot metadata (needs POOL lock)
        {
            let mut pool = POOL.lock();
            pool.slots[slot_idx].start_time_us = 0;
        }
        
        // NOW set atomic state to READY - context is fully set up, scheduler can run it
        THREAD_STATES[slot_idx].store(thread_state::READY, Ordering::SeqCst);
    });
    
    Ok(slot_idx)
}

/// Count available system thread slots
///
/// Returns the number of free slots in the system thread range (1..RESERVED_THREADS).
pub fn system_threads_available() -> usize {
    // Lock-free: count free system thread slots
    count_free_slots(1, config().reserved_threads)
}

/// Count active system threads
///
/// Returns the number of non-free slots in the system thread range (1..RESERVED_THREADS).
pub fn system_threads_active() -> usize {
    // Lock-free: count non-free system thread slots
    (1..config().reserved_threads)
        .filter(|&i| THREAD_STATES[i].load(Ordering::Relaxed) != thread_state::FREE)
        .count()
}

// ============================================================================
// User Process Thread API
// ============================================================================

/// Spawn a thread specifically for user processes
///
/// Only spawns in slots RESERVED_THREADS..MAX_THREADS (user thread range).
/// Returns the thread ID or error if no user thread slots are available.
/// User threads are preemptive by default.
pub fn spawn_user_thread_fn<F>(f: F) -> Result<usize, &'static str>
where
    F: FnOnce() -> ! + Send + 'static,
{
    spawn_user_thread_fn_internal(f, false)
}

/// Spawn a user thread for running a user PROCESS
///
/// This variant starts with IRQs DISABLED to prevent the race condition where
/// timer fires before activate() sets the user TTBR0. The closure MUST call
/// enable_irqs() after setting up the user address space.
pub fn spawn_user_thread_fn_for_process<F>(f: F) -> Result<usize, &'static str>
where
    F: FnOnce() -> ! + Send + 'static,
{
    spawn_user_thread_fn_internal(f, true)
}

/// Initialize a freshly claimed slot's [`Context`] for a brand-new thread.
///
/// Slots are **recycled**, so this must reset every field the previous occupant
/// could have written — not merely the ones the new thread needs to start.
///
/// The published user-mode triple (`user_entry` / `user_sp` / `user_tls`) is the
/// one that bites. [`get_saved_user_context`]'s fallback path returns it verbatim
/// for any thread with no live trap frame, gated only on `is_user_process` —
/// which this function sets. Leaving the triple stale therefore hands a *new*
/// process the previous occupant's user entry point and stack pointer, with
/// every general-purpose register zeroed. When that previous occupant was a
/// dynamically linked binary, the inherited entry point is **ld-musl's
/// `_dlstart`**, so the new process re-runs musl's RELR `*slot += base` loop over
/// an address space whose interpreter data page is already relocated. Each such
/// birth adds one more `base` to the same physical word, which is precisely the
/// `N × INTERP_BASE + 0x6c964` instruction-abort class (fingerprint: constant
/// `SP_EL0`, `x19 = x20 = x29 = x30 = 0`, one shared frame PA, `N` monotone per
/// boot) — see `docs/runbooks/debug-thread-spawn-segv.md`, class 2.
///
/// Split out of `spawn_user_thread_fn_internal` only so the reset is reachable
/// from a host test; `#[inline]` keeps the spawn path's codegen as it was.
#[inline]
fn init_thread_slot_context(
    slot_idx: usize,
    sp: u64,
    boot_ttbr0: u64,
    trampoline: u64,
    closure_ptr: u64,
    x21: u64,
    start_irqs_disabled: bool,
) {
    // SAFETY: the slot was just claimed by this caller and is not READY yet, so
    // no scheduler can be reading the context concurrently.
    unsafe {
        let ctx = &mut *get_context_mut(slot_idx);
        ctx.magic = CONTEXT_MAGIC;
        ctx.sp = sp;
        ctx.ttbr0 = boot_ttbr0;
        // Legacy fields for compatibility with old scheduler path
        ctx.x19 = trampoline;
        ctx.x20 = closure_ptr;
        ctx.x21 = x21;
        ctx.x30 = thread_start_closure as *const () as u64;
        ctx.elr = thread_start_closure as *const () as u64;
        ctx.spsr = 0x00000345; // EL1h, IRQs enabled
        ctx.is_user_process = if start_irqs_disabled { 1 } else { 0 };
        // Drop the previous occupant's published user context (see fn docs).
        ctx.user_entry = 0;
        ctx.user_sp = 0;
        ctx.user_tls = 0;
    }
}

/// Internal implementation for spawning user threads
///
/// - start_irqs_disabled: if true, thread starts with IRQs disabled (for process threads)
fn spawn_user_thread_fn_internal<F>(f: F, start_irqs_disabled: bool) -> Result<usize, &'static str>
where
    F: FnOnce() -> ! + Send + 'static,
{
    // Step 1: Atomically claim a free slot (lock-free). On a miss, collect
    // cooled-down terminated slots ourselves and retry once before failing — the
    // pool is usually not exhausted, just uncollected (see
    // `reclaim_terminated_slots`). Without this, a spawn storm reports ENOMEM to
    // userspace while the slots it needs sit TERMINATED waiting for an idle loop
    // that a busy system never reaches.
    let slot_idx = match claim_free_slot(config().reserved_threads, thread_limit()) {
        Some(idx) => idx,
        None => {
            if reclaim_terminated_slots() == 0 {
                return Err("No free user thread slots");
            }
            match claim_free_slot(config().reserved_threads, thread_limit()) {
                Some(idx) => idx,
                None => return Err("No free user thread slots"),
            }
        }
    };

    // Lazy stacks: ensure this slot has a stack (no-op if pre-allocated).
    if !ensure_slot_stack(slot_idx, config().user_thread_stack_size) {
        THREAD_STATES[slot_idx].store(thread_state::FREE, Ordering::SeqCst);
        return Err("Failed to allocate user thread stack from PMM");
    }

    // Step 2: Box the closure (heap allocation - no lock held!)
    let boxed: Box<F> = Box::new(f);
    let closure_ptr = Box::into_raw(boxed) as *mut ();
    let trampoline: fn(*mut ()) -> ! = closure_trampoline::<F>;

    // Step 3: Set up fake IRQ frame and minimal context
    // This enables stack-based context switching
    with_irqs_disabled(|| {
        // Get stack info from POOL (brief lock)
        let stack_top = {
            let pool = POOL.lock();
            let stack = &pool.stacks[slot_idx];
            // Initial stack top is BELOW the exception area
            ((stack.top - EXCEPTION_STACK_SIZE) & !0xF) as u64
        };
        
        // Get STORED boot TTBR0 (not current, which could be user process's!)
        let boot_ttbr0 = crate::mmu::get_boot_ttbr0();

        // x21 = IRQ enable flag: 0 = enable, non-zero = keep disabled
        let x21 = if start_irqs_disabled { 1u64 } else { 0u64 };

        // Set up fake IRQ frame for stack-based context switching
        let sp = setup_fake_irq_frame(
            stack_top,
            thread_start_closure as *const () as u64,  // ELR - where to jump
            trampoline as *const () as u64,            // x19 - trampoline
            closure_ptr as u64,                        // x20 - closure data
            x21,                                       // x21 - IRQ enable flag
        );

        init_thread_slot_context(
            slot_idx,
            sp,
            boot_ttbr0,
            trampoline as *const () as u64,
            closure_ptr as u64,
            x21,
            start_irqs_disabled,
        );

        // Write slot metadata (needs POOL lock)
        {
            let mut pool = POOL.lock();
            pool.slots[slot_idx].start_time_us = 0;
        }
        
        // NOW set atomic state to READY - context is fully set up, scheduler can run it
        THREAD_STATES[slot_idx].store(thread_state::READY, Ordering::SeqCst);
    });
    
    Ok(slot_idx)
}

/// Count available user thread slots

/// Count available user thread slots
///
/// Returns the number of free slots in the user thread range (RESERVED_THREADS..MAX_THREADS).
pub fn user_threads_available() -> usize {
    // Lock-free: count free user thread slots
    count_free_slots(config().reserved_threads, thread_limit())
}

/// Count active user threads
///
/// Returns the number of non-free slots in the user thread range.
pub fn user_threads_active() -> usize {
    // Lock-free: count non-free user thread slots
    (config().reserved_threads..thread_limit())
        .filter(|&i| THREAD_STATES[i].load(Ordering::Relaxed) != thread_state::FREE)
        .count()
}

// Note: is_thread_terminated is defined above using lock-free atomics

/// Get the state of a specific thread (for debugging) - LOCK-FREE
pub fn get_thread_state_enum(thread_id: usize) -> Option<ThreadState> {
    if thread_id >= MAX_THREADS {
        return None;
    }
    let state = THREAD_STATES[thread_id].load(Ordering::Relaxed);
    Some(match state {
        thread_state::FREE => ThreadState::Free,
        thread_state::READY => ThreadState::Ready,
        thread_state::RUNNING => ThreadState::Running,
        thread_state::TERMINATED => ThreadState::Terminated,
        thread_state::INITIALIZING => ThreadState::Ready, // Treat as ready for display
        _ => ThreadState::Free,
    })
}

/// Save the current trap frame pointer for a thread.
/// Called at the start of EL0 sync handler so fork can read full register state.
pub fn set_current_trap_frame(frame: *const UserTrapFrame) {
    let tid = current_thread_id();
    if tid < MAX_THREADS {
        CURRENT_TRAP_FRAME[tid].store(frame as u64, Ordering::Release);
    }
}

/// Clear the current trap frame pointer for a thread.
///
/// The SVC epilogue's clear is not the only one needed: an exit path that never
/// returns to the epilogue (`return_to_kernel`, `return_to_kernel_from_fault`) and
/// an EL0 entry that bypasses it (`enter_user_mode`, i.e. execve / initial launch)
/// must clear it too, or the thread's slot keeps a pointer to a trap frame that is
/// no longer live — and, once the slot is recycled, no longer even allocated.
pub fn clear_current_trap_frame() {
    let tid = current_thread_id();
    if tid < MAX_THREADS {
        CURRENT_TRAP_FRAME[tid].store(0, Ordering::Release);
    }
}

/// Raw `CURRENT_TRAP_FRAME` entry for `tid`; 0 when no live EL0 trap frame is
/// registered. Returns the pointer *without* dereferencing it — for the boot-suite
/// self-test that asserts thread teardown drops the entry.
pub fn trap_frame_ptr_for_thread(tid: usize) -> u64 {
    if tid < MAX_THREADS {
        CURRENT_TRAP_FRAME[tid].load(Ordering::Acquire)
    } else {
        0
    }
}

/// Test-only: publish a raw trap-frame pointer on another thread's slot, modelling a
/// thread that died inside a syscall without running the SVC epilogue.
///
/// Callers must pass a readable, `UserTrapFrame`-aligned address that stays valid for
/// the length of the test: the heartbeat's `dump_thread_resume_points` will
/// dereference any non-zero entry belonging to a non-`FREE` slot.
pub fn set_trap_frame_ptr_for_tid_test(tid: usize, ptr: u64) {
    if tid < MAX_THREADS {
        CURRENT_TRAP_FRAME[tid].store(ptr, Ordering::Release);
    }
}

/// Read `ELR_EL1` from the live trap frame of the current thread, if available.
///
/// Returns `None` outside an EL0 sync exception window (when no trap frame has
/// been registered via `set_current_trap_frame`). Used by syscall errno
/// diagnostics to attach the user PC of the faulting SVC.
pub fn current_trap_frame_elr() -> Option<u64> {
    let tid = current_thread_id();
    if tid >= MAX_THREADS {
        return None;
    }
    let frame_ptr = CURRENT_TRAP_FRAME[tid].load(Ordering::Acquire);
    if frame_ptr == 0 {
        return None;
    }
    let frame = unsafe { &*(frame_ptr as *const UserTrapFrame) };
    Some(frame.elr_el1)
}

/// Get the saved user context for a thread.
/// Used by fork() to duplicate the parent's state.
/// Reads from the live trap frame on the stack when available (captures all registers).
/// Debug: dump every non-FREE thread's state to the console (`[THR-DUMP]`).
/// Used by the heartbeat to locate where parked threads are stuck during a hang,
/// without needing SSH (which can itself wedge). Compact, allocation-free.
///
/// The printed `elr=` is the live `ELR_EL1` read out of the thread's saved IRQ
/// frame at `Context.sp + 240` (see `get_saved_kernel_resume`'s doc comment and
/// docs/archive/J4_WRITE_PERM_FAULT_AND_HALF_WRITTEN_LINKER_OUTPUT.md §7.14 for
/// why `Context.elr` itself is dead after thread creation). Prints 0 if the
/// thread has never been switched out via the IRQ path yet (`Context.sp == 0`).
pub fn dump_thread_resume_points() {
    safe_print!(16, "[THR-DUMP]\n");
    for tid in 0..MAX_THREADS {
        let st = THREAD_STATES[tid].load(Ordering::SeqCst);
        if st == thread_state::FREE { continue; }
        let elr = {
            let ctx = unsafe { &*get_context(tid) };
            if ctx.sp == 0 {
                0
            } else {
                // SAFETY: Context.sp points at the 832-byte IRQ trap frame this
                // thread was last switched out from (src/exceptions.rs:266-278);
                // ELR_EL1 lives at offset +240 in that frame.
                unsafe { core::ptr::read_volatile((ctx.sp as usize + 240) as *const u64) }
            }
        };
        let stc = match st {
            x if x == thread_state::READY => 'r',
            x if x == thread_state::RUNNING => 'R',
            x if x == thread_state::TERMINATED => 'T',
            x if x == thread_state::INITIALIZING => 'I',
            _ => '?',
        };
        // Correlate to a pid + its current syscall (which subsystem it's in).
        let (pid, sc, tg, l0) = match crate::process::find_pid_by_thread(tid) {
            Some(p) => {
                match crate::process::lookup_process_shared(p) {
                    Some(pr) => (p as i64, pr.current_syscall.load(Ordering::Relaxed),
                                 pr.tgid as i64, pr.address_space.l0_phys() as u64),
                    None => (p as i64, !0, -1, 0),
                }
            }
            None => (-1, !0, -1, 0),
        };
        let _ = (tg, l0);
        let tsc = thread_current_syscall(tid) as i64; // exact per-thread syscall
        // For a thread parked in a syscall, its saved trap frame still holds the
        // syscall args — x0 (futex uaddr), x1 (futex op). Lets us correlate a
        // stuck FUTEX_WAIT to a (missing) FUTEX_WAKE on the same address.
        let (a0, a1) = {
            let fp = CURRENT_TRAP_FRAME[tid].load(Ordering::Acquire);
            if fp != 0 {
                let f = unsafe { &*(fp as *const UserTrapFrame) };
                (f.x0, f.x1)
            } else { (0, 0) }
        };
        // LAST_CORE == 0xFF means never scheduled even once, ever, since this slot's
        // last claim (Failure D falsification test, 2026-08-07 — see
        // config::PRIORITIZE_NEVER_SCHEDULED). Printed raw so a hang dump can answer
        // "are there genuinely never-run threads right now" without inference.
        let last_core = LAST_CORE[tid].load(Ordering::Relaxed);
        // Raw TOTAL_CPU_TIMES (not the live-corrected `get_thread_cpu_time`, which takes
        // POOL.lock() — avoided here to keep this dump lock-free). Undercounts a
        // currently-RUNNING thread's in-progress quantum, which doesn't matter for a
        // hang dump: a genuinely stuck thread isn't RUNNING at print time, so this is
        // exact for the case this field exists to answer — "how much real CPU time has
        // this thread ever actually gotten".
        let cpu_us = TOTAL_CPU_TIMES[tid].load(Ordering::Relaxed);
        safe_print!(255,
            "  tid={} st={} pid={} tgid={} l0={:#x} sc={} tsc={} a0={:#x} a1={:#x} elr={:#x} last_core={} cpu_us={}\n",
            tid, stc, pid, tg, l0, sc as i64, tsc, a0, a1, elr, last_core, cpu_us);
    }
    // The inverse view: processes with no thread at all. A hang shows up here and
    // nowhere else — such a process is absent from the loop above by definition.
    crate::process::dump_orphan_processes();
}

/// Debug: saved kernel resume point of a context-switched-out thread, as
/// `(x30 /* dead field, see below */, live_elr, sp)`.
///
/// **`Context.x30`/`.elr` are dead fields after thread creation**
/// (docs/archive/J4_WRITE_PERM_FAULT_AND_HALF_WRITTEN_LINKER_OUTPUT.md §7.14).
/// `sgi_scheduler_handler_with_sp` (the only switch path, voluntary or
/// involuntary) writes just `.sp`/`.ttbr0` into `Context` on every switch — never
/// `.elr`/`.x30`. For an ordinary `clone_thread` worker those two fields are set
/// once at slot-claim time (to `thread_start_closure`, so the *first* cooperative
/// switch has somewhere to land) and never touched again. The real, live register
/// state — including the actual current `ELR_EL1` — lives on the thread's own
/// kernel stack, in the IRQ frame `.sp` points to (`src/exceptions.rs:266-278`:
/// `ELR` at `sp+240`, `SPSR` at `sp+248`). The `live_elr` returned here is read
/// from that frame, not from `Context.elr`. `x30` is still the dead, creation-time
/// value — kept in the tuple for callers that want it, but don't read it as a
/// resume point. Returns `live_elr == 0` if the thread has never been switched
/// out via the IRQ path yet (`Context.sp == 0`). Returns `None` for out-of-range
/// ids.
pub fn get_saved_kernel_resume(thread_id: usize) -> Option<(u64, u64, u64)> {
    if thread_id >= MAX_THREADS {
        return None;
    }
    with_irqs_disabled(|| {
        let ctx = unsafe { &*get_context(thread_id) };
        let live_elr = if ctx.sp == 0 {
            0
        } else {
            // SAFETY: Context.sp points at the 832-byte IRQ trap frame this
            // thread was last switched out from (src/exceptions.rs:266-278);
            // ELR_EL1 lives at offset +240 in that frame.
            unsafe { core::ptr::read_volatile((ctx.sp as usize + 240) as *const u64) }
        };
        Some((ctx.x30, live_elr, ctx.sp))
    })
}

/// Count of `get_saved_user_context` calls that found no live EL0 trap frame.
/// Every one of them used to launch a child at its parent's *initial exec* entry
/// point; see the function docs.
static NO_TRAP_FRAME_CHILDREN: AtomicU64 = AtomicU64::new(0);

/// How many `get_saved_user_context` calls found no live EL0 trap frame this boot.
/// Non-zero means fork/vfork/clone syscalls are being refused — read the
/// `[NO-TRAPFRAME]` lines for the caller.
pub fn no_trap_frame_child_count() -> u64 {
    NO_TRAP_FRAME_CHILDREN.load(Ordering::Relaxed)
}

/// The user-mode context to give a **new child** of `thread_id` (fork/vfork/clone).
///
/// Only the live EL0 trap frame can answer this. It is the register state at the
/// `svc` that asked for the child, so the child resumes exactly where its parent
/// did, one instruction on.
///
/// **There is deliberately no fallback.** The slot also carries a `user_entry` /
/// `user_sp` / `user_tls` triple, and reading *that* looks like a graceful
/// degradation — it is not. Those fields record where the thread's image was
/// *first entered*, written once by [`update_thread_context`] at execve and never
/// again. Handing them to a child does not produce a slightly-stale fork return;
/// it produces a process that starts over at its parent's ELF entry point, on its
/// parent's initial stack, with every GPR zeroed. For a dynamically linked parent
/// that entry point is **ld-musl's `_dlstart`**, so the child re-runs musl's RELR
/// apply loop (`*slot += base`) over an address space whose interpreter data page
/// the parent already relocated. Each such birth adds one more `base` to the same
/// physical word and then branches through it: the `N × INTERP_BASE + 0x6c964`
/// instruction-abort class, `N` climbing by one per event for the whole boot
/// (`docs/runbooks/debug-thread-spawn-segv.md`, class 2). Returning `None` fails
/// the syscall instead, which is loud, local, and recoverable.
pub fn get_saved_user_context(thread_id: usize) -> Option<crate::process::UserContext> {
    if thread_id >= MAX_THREADS {
        return None;
    }

    // If this is the current thread and we have a live trap frame, use it for full register state
    if thread_id == current_thread_id() {
        let frame_ptr = CURRENT_TRAP_FRAME[thread_id].load(Ordering::Acquire);
        if frame_ptr != 0 {
            let frame = unsafe { &*(frame_ptr as *const UserTrapFrame) };
            let ctx = unsafe { &*get_context(thread_id) };
            let ttbr0 = if ctx.ttbr0 != 0 { ctx.ttbr0 } else { crate::mmu::get_boot_ttbr0() };

            if config().syscall_debug_info_enabled {
                safe_print!(128, "[threading] get_saved_user_context: captured from trap frame for thread {} (PC={:#x}, SP={:#x})\n",
                    thread_id, frame.elr_el1, frame.sp_el0);
            }
            return Some(crate::process::UserContext {
                x0: frame.x0, x1: frame.x1, x2: frame.x2, x3: frame.x3,
                x4: frame.x4, x5: frame.x5, x6: frame.x6, x7: frame.x7,
                x8: frame.x8, x9: frame.x9, x10: frame.x10, x11: frame.x11,
                x12: frame.x12, x13: frame.x13, x14: frame.x14, x15: frame.x15,
                x16: frame.x16, x17: frame.x17, x18: frame.x18, x19: frame.x19,
                x20: frame.x20, x21: frame.x21, x22: frame.x22, x23: frame.x23,
                x24: frame.x24, x25: frame.x25, x26: frame.x26, x27: frame.x27,
                x28: frame.x28, x29: frame.x29, x30: frame.x30,
                sp: frame.sp_el0,
                pc: frame.elr_el1,
                spsr: 0, // Always clean EL0t for child processes
                tpidr: frame.tpidr_el0,
                ttbr0,
            });
        }
    }

    // No live trap frame. There is no correct child context to build — see the
    // function docs for why the `user_entry`/`user_sp` triple is not one. Report
    // the stale values that *would* have been returned so the caller is
    // identifiable from a single log line, then fail the syscall.
    let (stale_entry, stale_sp, is_user) = with_irqs_disabled(|| {
        let ctx = unsafe { &*get_context(thread_id) };
        (ctx.user_entry, ctx.user_sp, ctx.is_user_process)
    });
    let n = NO_TRAP_FRAME_CHILDREN.fetch_add(1, Ordering::Relaxed);
    // Unbounded printing would itself wedge the box under a fork storm; the first
    // few plus a decade-spaced tail is enough to establish rate and identity.
    if n < 8 || n.is_power_of_two() {
        safe_print!(192,
            "[NO-TRAPFRAME] refusing child of tid={} (cur={}) — no live EL0 frame; \
             stale user_entry={:#x} user_sp={:#x} is_user={} count={}\n",
            thread_id, current_thread_id(), stale_entry, stale_sp, is_user, n + 1);
    }
    None
}

// ============================================================================
// Stack Protection Functions
// ============================================================================

/// Check all thread stacks for overlap (debug/diagnostic)
/// Returns list of (thread_a, thread_b) pairs that overlap
pub fn check_stack_overlaps() -> Vec<(usize, usize)> {
    // Copy stack info while holding lock (quick), process outside
    let stacks: [StackInfo; MAX_THREADS] = with_irqs_disabled(|| {
        let pool = POOL.lock();
        pool.stacks
    });

    // O(n²) check done outside critical section
    let mut overlaps = Vec::new();
    for i in 0..MAX_THREADS {
        for j in (i + 1)..MAX_THREADS {
            if stacks[i].overlaps(&stacks[j]) {
                overlaps.push((i, j));
            }
        }
    }
    overlaps
}

/// Get stack bounds for a thread
pub fn get_stack_bounds(thread_id: usize) -> Option<(usize, usize)> {
    with_irqs_disabled(|| {
        let pool = POOL.lock();
        if thread_id < MAX_THREADS && pool.stacks[thread_id].is_allocated() {
            Some((pool.stacks[thread_id].base, pool.stacks[thread_id].top))
        } else {
            None
        }
    })
}

/// Validate current stack pointer is within bounds
pub fn validate_current_sp() -> bool {
    let sp: usize;
    #[cfg(target_os = "none")]
    unsafe {
        core::arch::asm!("mov {}, sp", out(reg) sp);
    }
    #[cfg(not(target_os = "none"))]
    { sp = 0; }

    with_irqs_disabled(|| {
        let pool = POOL.lock();
        let current = pool.current_idx;
        pool.stacks[current].contains(sp)
    })
}

/// Check all thread stack canaries for corruption
/// Returns list of thread IDs with corrupted canaries
pub fn check_all_stack_canaries() -> Vec<usize> {
    if !config().enable_stack_canaries {
        return Vec::new();
    }

    // Copy stack info quickly while holding lock
    let stacks: [StackInfo; MAX_THREADS] = with_irqs_disabled(|| {
        let pool = POOL.lock();
        pool.stacks
    });

    // Check canaries outside critical section (memory reads can be slow)
    let mut bad = Vec::new();
    for i in 1..MAX_THREADS {
        if stacks[i].is_allocated() && !check_stack_canary(stacks[i].base) {
            bad.push(i);
        }
    }
    bad
}

/// Report every **live** thread whose stack canary has been overrun, once per
/// thread, and return how many new ones were found.
///
/// The teardown check in `ThreadPool::free_stack_for_slot` only fires when a
/// thread exits, which is exactly the case an overflow can prevent: if the run-off
/// corrupts something that hangs or panics the box, the thread never reaches
/// teardown and the report is never printed. This is the periodic counterpart —
/// cheap enough for the idle loop (one `canary_words`-word read per allocated
/// slot, no allocation, no `POOL` lock held across the reads) and latched per slot
/// so a broken canary is announced once, not every sweep.
///
/// The latch key is the reported stack's **base**, not just its slot: a slot whose
/// stack was freed and re-allocated (`WARM_FREE_USER == 0` does that on every user
/// process exit) comes back at a different base with a freshly painted canary, and
/// must be able to report again. Keying on the base also keeps this lock-free —
/// there is no re-arm hook to place inside `init_stack_canary`, which runs under
/// the `POOL` lock and could not take it again. See
/// `docs/archive/EXTREME_SSHD_KERNEL_STACK_OVERFLOW.md`.
pub fn report_overrun_stack_canaries() -> usize {
    if !config().enable_stack_canaries {
        return 0;
    }
    let stacks: [StackInfo; MAX_THREADS] = with_irqs_disabled(|| POOL.lock().stacks);
    let mut found = 0;
    for (i, stack) in stacks.iter().enumerate().skip(1) {
        if !stack.is_allocated() {
            continue;
        }
        if check_stack_canary(stack.base) {
            // Intact ⇒ disarm. Covers a repainted (recycled) stack, and keeps a
            // one-off corruption — including the boot suite's own deliberate one
            // in `test_stack_canary_overrun_is_reported` — from leaving this slot
            // permanently unable to report a later, real overflow.
            CANARY_REPORTED_BASE[i].store(0, Ordering::Relaxed);
            continue;
        }
        if CANARY_REPORTED_BASE[i].swap(stack.base, Ordering::Relaxed) == stack.base {
            continue;
        }
        found += 1;
        safe_print!(160,
            "[STACK-OVERFLOW] tid={} ran off its {}KB kernel stack (base={:#x}) — \
             kernel memory below it was corrupted\n",
            i, stack.size / 1024, stack.base);
    }
    found
}

/// A slot that has a stack allocated but is not running anything: `(slot, base)`.
///
/// Exists for `test_stack_canary_overrun_is_reported`, which needs a real stack
/// whose canary it can break and restore without disturbing a live thread. Skips
/// slot 0 (the boot stack, which `check_stack_canary` treats as always-intact).
pub fn first_idle_stack_base() -> Option<(usize, usize)> {
    let stacks: [StackInfo; MAX_THREADS] = with_irqs_disabled(|| POOL.lock().stacks);
    (1..MAX_THREADS).find_map(|i| {
        let free = THREAD_STATES[i].load(Ordering::SeqCst) == thread_state::FREE;
        (free && stacks[i].is_allocated()).then_some((i, stacks[i].base))
    })
}

/// Check if there are any threads ready to run
pub fn has_ready_threads() -> bool {
    // Use atomic THREAD_STATES array (lock-free)
    (0..MAX_THREADS)
        .any(|i| THREAD_STATES[i].load(Ordering::Relaxed) == thread_state::READY)
}

// ============================================================================
// Kernel Thread Info for kthreads command
// ============================================================================


/// Get list of all kernel threads with their info
pub fn list_kernel_threads() -> Vec<KernelThreadInfo> {
    // Take a quick snapshot - read atomic states (lock-free) and pool data (brief lock)
    let snapshot: ThreadPoolSnapshot = with_irqs_disabled(|| {
        let pool = POOL.lock();
        
        let mut states = [ThreadState::Free; MAX_THREADS];
        let mut sps = [0u64; MAX_THREADS];

        for i in 0..MAX_THREADS {
            // Read state from atomic array (lock-free source of truth)
            states[i] = match THREAD_STATES[i].load(Ordering::Relaxed) {
                thread_state::FREE => ThreadState::Free,
                thread_state::READY => ThreadState::Ready,
                thread_state::RUNNING => ThreadState::Running,
                thread_state::TERMINATED => ThreadState::Terminated,
                thread_state::INITIALIZING => ThreadState::Ready, // Show as ready (being set up)
                thread_state::WAITING => ThreadState::Ready, // Show waiting threads
                _ => ThreadState::Free,
            };
            // Read SP from THREAD_CONTEXTS (not from slot)
            sps[i] = unsafe { (*get_context(i)).sp };
        }

        ThreadPoolSnapshot {
            states,
            sps,
            stacks: pool.stacks,
        }
    });

    // Process snapshot outside critical section (Vec allocation, canary checks, etc.)
    let mut threads = Vec::new();

    for i in 0..MAX_THREADS {
        // Skip free slots
        if snapshot.states[i] == ThreadState::Free {
            continue;
        }

        let state_str = match snapshot.states[i] {
            ThreadState::Free => "free",
            ThreadState::Ready => "ready",
            ThreadState::Running => "running",
            ThreadState::Terminated => "zombie",
        };

        let stack = &snapshot.stacks[i];
        let sp = snapshot.sps[i];

        // Estimate stack usage from saved SP in context
        let stack_used = if stack.is_allocated() && sp != 0 {
            let sp_usize = sp as usize;
            if sp_usize >= stack.base && sp_usize <= stack.top {
                stack.top.saturating_sub(sp_usize)
            } else {
                0
            }
        } else {
            0
        };

        // Check canary status (memory read, done outside lock)
        let canary_ok = if i == 0 || !stack.is_allocated() {
            true
        } else if config().enable_stack_canaries {
            check_stack_canary(stack.base)
        } else {
            true
        };

        // Thread name based on index range and state
        let net_tid = NETWORK_THREAD_ID.load(Ordering::Relaxed);
        let name = match i {
            0 => "bootstrap",
            _ if i == net_tid => "network",
            1..=7 => "system-thread",
            _ => "user-process",
        };

        threads.push(KernelThreadInfo {
            tid: i,
            state: state_str,
            stack_base: stack.base,
            stack_size: stack.size,
            stack_used,
            canary_ok,
            name,
        });
    }

    threads
}

pub fn dump_stack_info() {
    let threads = list_kernel_threads();

    for t in threads {
        let size_kb = t.stack_size / 1024;
        let used_kb = t.stack_used / 1024;
        safe_print!(192, "Thread ID: {} State: {} Stack Size: {} KB Used: {} KB\n", t.tid, t.state, size_kb, used_kb);
    }
}

// ============================================================================
// Stack Memory Verification
// ============================================================================

/// Calculate the total stack memory required based on kernel config
pub fn calculate_stack_requirements() -> StackAllocationSummary {
    types::calculate_stack_requirements(
        config().reserved_threads,
        config().kernel_stack_size,
        config().system_thread_stack_size,
        config().user_thread_stack_size,
        thread_limit(),
    )
}

/// Verify that the thread-stack pool fits in `available_mem` (free PMM bytes —
/// stacks come from PMM, not the heap), for the current `thread_limit()`.
pub fn verify_stack_memory(available_mem: usize) -> Result<StackAllocationSummary, alloc::string::String> {
    types::verify_stack_memory_params(
        available_mem,
        config().reserved_threads,
        config().kernel_stack_size,
        config().system_thread_stack_size,
        config().user_thread_stack_size,
        thread_limit(),
    )
}

/// Print stack allocation summary to console
pub fn print_stack_requirements() {
    let summary = calculate_stack_requirements();
    let heap_required = summary.system_total + summary.user_total;
    
    (runtime().print_str)("=== Stack Memory Requirements ===\n");
    safe_print!(64, "Boot stack (fixed):     {} KB\n", summary.boot_stack / 1024);
    safe_print!(128, "System threads:         {} × {} KB = {} KB\n",
        summary.system_thread_count,
        summary.system_stack_size / 1024,
        summary.system_total / 1024);
    safe_print!(128, "User threads:           {} × {} KB = {} KB\n",
        summary.user_thread_count,
        summary.user_stack_size / 1024,
        summary.user_total / 1024);
    safe_print!(96, "Exception area/thread:  {} KB (for IRQ/syscall handlers)\n",
        summary.exception_stack_size / 1024);
    safe_print!(96, "Usable kernel stack:    {} KB (per thread, for execute() etc.)\n",
        summary.usable_kernel_stack / 1024);
    safe_print!(96, "Total from heap:        {} KB ({} MB)\n",
        heap_required / 1024,
        heap_required / (1024 * 1024));
    safe_print!(96, "Grand total:            {} KB ({} MB)\n",
        summary.total_bytes / 1024,
        summary.total_bytes / (1024 * 1024));
}

#[cfg(test)]
mod signal_mask_tests {
    use super::*;

    // Regression for the per-process→per-thread signal-mask fix
    // (docs/SIGNAL_DELIVERY_FORKTEST_EVIDENCE.md §D): each thread slot must hold an
    // INDEPENDENT blocked-signal mask. The bug was a single Process::signal_mask
    // shared by all CLONE_THREAD siblings (via the owner PID), letting one thread's
    // mask change clobber another's → mis-timed async-signal delivery → register
    // corruption. These use the explicit-tid accessors (host-safe; no asm).
    #[test]
    fn per_thread_masks_are_independent() {
        let (a, b) = (40usize, 41usize);
        seed_thread_signal_mask(a, 0xAAAA_AAAA_AAAA_AAAA);
        seed_thread_signal_mask(b, 0x5555_5555_5555_5555);
        assert_eq!(thread_signal_mask_of(a), 0xAAAA_AAAA_AAAA_AAAA);
        // The decisive check: changing slot A did not affect slot B.
        assert_eq!(thread_signal_mask_of(b), 0x5555_5555_5555_5555);
        // A fresh value on A leaves B untouched.
        seed_thread_signal_mask(a, 0);
        assert_eq!(thread_signal_mask_of(b), 0x5555_5555_5555_5555);
        seed_thread_signal_mask(b, 0); // cleanup
    }

    #[test]
    fn signal_mask_out_of_range_is_zero() {
        assert_eq!(thread_signal_mask_of(MAX_THREADS), 0);
        assert_eq!(thread_signal_mask_of(usize::MAX), 0);
    }
}

#[cfg(test)]
mod pending_kill_tests {
    use super::*;

    // The deferred-kill flag is per-thread, independent across slots, and
    // take clears it exactly once (the contract kill_thread_group's grace-wait
    // relies on). On host `current_thread_id()` is always 0 (the idle thread,
    // which `request_thread_kill` refuses to arm), so these drive the tid-
    // explicit `has_pending_kill` / `take_kill_request` core directly.

    #[test]
    fn request_sets_and_take_clears() {
        let tid = 42;
        assert!(!has_pending_kill(tid));
        // request_thread_kill arms the flag (the wake it fires is a host no-op).
        request_thread_kill(tid);
        assert!(has_pending_kill(tid));
        assert!(take_kill_request_via_tid(tid), "first take must see the pending request");
        assert!(!has_pending_kill(tid), "flag must be cleared after take");
        assert!(!take_kill_request_via_tid(tid), "second take must see nothing");
    }

    #[test]
    fn flags_are_independent_per_thread() {
        let (a, b) = (43usize, 44);
        request_thread_kill(a);
        request_thread_kill(b);
        assert!(has_pending_kill(a));
        assert!(has_pending_kill(b));
        take_kill_request_via_tid(a);
        assert!(!has_pending_kill(a), "clearing A must not affect B");
        assert!(has_pending_kill(b));
        take_kill_request_via_tid(b); // cleanup
    }

    #[test]
    fn idle_thread_and_out_of_range_are_noops() {
        request_thread_kill(IDLE_THREAD_IDX);
        assert!(!has_pending_kill(IDLE_THREAD_IDX), "idle thread is never armed");
        request_thread_kill(MAX_THREADS + 5);
        assert!(!has_pending_kill(MAX_THREADS + 5));
        assert!(!take_kill_request_via_tid(MAX_THREADS + 5));
    }
}

/// Regression coverage for docs/archive/GIT_CLONE_STALE_ITIMER_SIGALRM.md.
/// Confined to slots 20..24 — disjoint from every other module's fixed/ranged
/// tids in this file (35.., see `park_wake_race_tests`'s own inventory comment).
#[cfg(test)]
mod itimer_tests {
    use super::*;

    #[test]
    fn itimer_state_is_independent_per_thread() {
        let (a, b) = (20usize, 21usize);
        set_itimer(a, 1_000, 500);
        set_itimer(b, 2_000, 0);
        assert_eq!(get_itimer(a), (1_000, 500));
        assert_eq!(get_itimer(b), (2_000, 0));
        set_itimer(a, 0, 0); // cleanup
        set_itimer(b, 0, 0);
    }

    #[test]
    fn itimer_out_of_range_tid_is_noop() {
        assert_eq!(get_itimer(MAX_THREADS), (0, 0));
        set_itimer(MAX_THREADS, 999, 999); // must not panic or alias a real slot
        assert_eq!(get_itimer(0), (0, 0));
    }

    /// The actual regression. A slot that last held a process which armed
    /// `ITIMER_REAL` (`alarm()`/`setitimer()`) and then exited *without*
    /// disarming it — e.g. busybox `wget -T`, which implements its timeout via
    /// `alarm()` — must not leak that deadline into whatever unrelated process
    /// claims the slot next.
    ///
    /// Before `scrub_thread_slot` cleared `ITIMER_DEADLINE`/`ITIMER_INTERVAL`,
    /// the new occupant inherited an already-long-expired deadline and took a
    /// fatal SIGALRM (no handler installed yet) at its very first timer tick.
    /// This is what made `git clone`'s `git-remote-http` helper die near-instantly
    /// with "Alarm clock" — see docs/archive/GIT_CLONE_STALE_ITIMER_SIGALRM.md.
    #[test]
    fn scrub_thread_slot_clears_stale_itimer_on_slot_reuse() {
        crate::test_support::ensure_test_runtime();
        const RANGE_START: usize = 22;
        const RANGE_END: usize = 24;

        let Some(tid) = claim_free_slot(RANGE_START, RANGE_END) else {
            panic!("test slot range unexpectedly busy");
        };

        // Simulate the old occupant arming a one-shot alarm that has already
        // expired (deadline=1us since boot) and exiting without disarming it.
        set_itimer(tid, 1, 0);
        assert_eq!(get_itimer(tid), (1, 0));
        release_test_thread_slot(tid);

        // A completely unrelated process claims the same slot.
        let tid2 = claim_free_slot(RANGE_START, RANGE_END).expect("re-claim of the same slot");
        assert_eq!(tid, tid2, "the narrow range pins the same slot");

        assert_eq!(
            get_itimer(tid2),
            (0, 0),
            "a recycled thread slot must not inherit its predecessor's ITIMER_REAL \
             deadline — that deadline is already in the past, so `check_itimers` \
             would fire an immediate, fatal SIGALRM against the new occupant"
        );

        release_test_thread_slot(tid2);
    }
}

/// Real-concurrency proof for the park/wake handshake in
/// [`publish_waiting_and_take_pending_wake`]. Two `std::thread`s stand in for two cores
/// racing over one thread slot: one runs the parking side of `schedule_blocking`
/// (entry check → publish `WAITING` → decide whether to park), the other fires a
/// `ThreadWaker`. The invariant under test is the one the `-j4` self-host wedge
/// violated: **a wake is never lost** — after both sides finish, the parking side has
/// either consumed the wake itself, or the waker left the slot `READY` so the scheduler
/// will resume it. A slot left `WAITING` with the wake already spent is the bug.
///
/// Against the pre-fix code (publish `WAITING`, park, only *then* re-check the sticky
/// flag) this fails within a few hundred iterations. Confined to slots 45..50, a range
/// no other test in this file touches.
#[cfg(test)]
mod park_wake_race_tests {
    use super::*;
    use std::sync::{Arc, Barrier};
    use std::thread;

    // Slots 40/41 are signal_mask_tests, 42-44 pending_kill_tests, 50..64 the contexts
    // test. 45..50 is ours, and each test below owns a disjoint piece of it so the three
    // can run under `cargo test`'s default parallelism.
    const SLOT_START: usize = 45;
    const SLOTS: usize = 3; // 45, 46, 47 — the race test
    const SLOT_UNCONTENDED: usize = 48;
    const SLOT_PENDING: usize = 49;

    #[test]
    fn a_wake_racing_the_waiting_publication_is_never_lost() {
        const ITERS: usize = 4000;
        // `ThreadWaker::wake` calls `runtime().trigger_sgi` on the WAITING branch — the
        // branch this test is about — which panics if no runtime is registered. Test
        // order is not guaranteed, so register the stub ourselves rather than relying on
        // another test having done it.
        crate::test_support::ensure_test_runtime();

        for i in 0..ITERS {
            let tid = SLOT_START + (i % SLOTS);
            THREAD_STATES[tid].store(thread_state::RUNNING, Ordering::SeqCst);
            WOKEN_STATES[tid].store(false, Ordering::SeqCst);
            WAKE_TIMES[tid].store(0, Ordering::SeqCst);

            let barrier = Arc::new(Barrier::new(2));
            let waker_side = {
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    ThreadWaker::new(tid).wake();
                })
            };

            barrier.wait();
            // The parking side of `schedule_blocking`: the entry check on the sticky
            // flag, then the guarded publication — with the work that really sits
            // between them in place. `schedule_blocking` calls `runtime().uptime_us()`,
            // compares the deadline and runs the preemption dance there, and *that* span
            // is the window: a waker landing inside it sees us RUNNING, so it arms the
            // sticky flag and skips the READY transition, leaving nothing behind. The
            // spin count is varied so the waker lands on both sides of the gap as well
            // as inside it.
            let parked = if WOKEN_STATES[tid].swap(false, Ordering::SeqCst) {
                false
            } else {
                for _ in 0..(i * 37) % 3000 {
                    std::hint::spin_loop();
                }
                !publish_waiting_and_take_pending_wake(tid, u64::MAX)
            };

            waker_side.join().unwrap();

            if parked {
                // We committed to sleeping. The waker must have made us runnable, or
                // nothing ever will — this is the wedge.
                assert_ne!(
                    THREAD_STATES[tid].load(Ordering::SeqCst),
                    thread_state::WAITING,
                    "iteration {i}: slot {tid} parked WAITING but the wake was already \
                     consumed — the waker saw us RUNNING and skipped the READY \
                     transition, so no wake can ever reach this thread again"
                );
            }

            THREAD_STATES[tid].store(thread_state::FREE, Ordering::SeqCst);
            WOKEN_STATES[tid].store(false, Ordering::SeqCst);
        }
    }

    /// The uncontended path still behaves: no wake pending ⇒ we publish WAITING and
    /// report "go ahead and park".
    #[test]
    fn no_pending_wake_publishes_waiting_and_parks() {
        let tid = SLOT_UNCONTENDED;
        THREAD_STATES[tid].store(thread_state::RUNNING, Ordering::SeqCst);
        WOKEN_STATES[tid].store(false, Ordering::SeqCst);

        assert!(!publish_waiting_and_take_pending_wake(tid, 12_345));
        assert_eq!(THREAD_STATES[tid].load(Ordering::SeqCst), thread_state::WAITING);
        assert_eq!(WAKE_TIMES[tid].load(Ordering::SeqCst), 12_345);

        THREAD_STATES[tid].store(thread_state::FREE, Ordering::SeqCst);
        WAKE_TIMES[tid].store(0, Ordering::SeqCst);
    }

    /// A wake that landed while the thread was still RUNNING is consumed, the thread is
    /// handed back as RUNNING, and the flag is left clear so it cannot fire twice.
    #[test]
    fn a_wake_pending_from_the_running_state_is_consumed_not_parked() {
        let tid = SLOT_PENDING;
        THREAD_STATES[tid].store(thread_state::RUNNING, Ordering::SeqCst);
        WOKEN_STATES[tid].store(false, Ordering::SeqCst);

        // Exactly what `ThreadWaker::wake` does to a thread that is not yet WAITING: it
        // arms the sticky flag and skips the READY transition.
        ThreadWaker::new(tid).wake();
        assert_eq!(THREAD_STATES[tid].load(Ordering::SeqCst), thread_state::RUNNING,
            "waking a RUNNING thread must not transition it — that is the precondition \
             for this whole race");

        assert!(publish_waiting_and_take_pending_wake(tid, u64::MAX),
            "the pending wake must be reported so the caller skips the park");
        assert_eq!(THREAD_STATES[tid].load(Ordering::SeqCst), thread_state::RUNNING);
        assert!(!WOKEN_STATES[tid].load(Ordering::SeqCst), "flag must be consumed once");

        THREAD_STATES[tid].store(thread_state::FREE, Ordering::SeqCst);
    }
}

/// Regression tests for the stale-wake / cross-kill state-transition races
/// (debug-thread-spawn-segv.md §2f). The corruption they pin down: a wake or a
/// spawn publish landing as a plain READY *store* could overwrite INITIALIZING
/// (a clone child whose context is half-written — a peer then runs the slot's
/// previous occupant's ttbr0 and kernel stack: the `AS MISMATCH` / `[BKL] stuck`
/// shape) or TERMINATED (resurrecting a killed thread onto freed page tables).
/// Every transition is now a CAS/fetch_update; these tests assert the refusal
/// semantics directly. Slots 35..40 — disjoint from every other module here
/// (40/41 signal_mask, 42-44 pending_kill, 45..50 park_wake, 50..64 contexts).
#[cfg(test)]
mod state_transition_guard_tests {
    use super::*;

    #[test]
    fn a_stale_waker_cannot_revive_a_non_waiting_slot() {
        crate::test_support::ensure_test_runtime();
        let tid = 35;
        for &state in &[
            thread_state::INITIALIZING, // the §2f corruption: half-built clone child
            thread_state::TERMINATED,   // a killed thread mid-teardown
            thread_state::FREE,         // a recycled slot
            thread_state::RUNNING,      // live on another core
            thread_state::READY,        // already runnable — nothing to do
        ] {
            THREAD_STATES[tid].store(state, Ordering::SeqCst);
            WOKEN_STATES[tid].store(false, Ordering::SeqCst);

            ThreadWaker::new(tid).wake();

            assert_eq!(
                THREAD_STATES[tid].load(Ordering::SeqCst), state,
                "a waker must only ever transition WAITING -> READY; overwriting \
                 state {state} hands a peer scheduler a slot whose context does \
                 not belong to a runnable thread"
            );
            assert!(WOKEN_STATES[tid].load(Ordering::SeqCst),
                "the sticky flag is still armed so a wake-before-park is not lost");
        }
        THREAD_STATES[tid].store(thread_state::FREE, Ordering::SeqCst);
        WOKEN_STATES[tid].store(false, Ordering::SeqCst);
    }

    #[test]
    fn a_successful_wake_does_not_touch_the_deadline() {
        crate::test_support::ensure_test_runtime();
        let tid = 36;
        // The old waker cleared WAKE_TIMES *after* its WAITING check; preempted
        // between the two, the clear could land on a FRESH deadline the slot's
        // next occupant had just published — a thread parked forever. The waker
        // now never writes WAKE_TIMES (every sleep entry rewrites it).
        THREAD_STATES[tid].store(thread_state::WAITING, Ordering::SeqCst);
        WAKE_TIMES[tid].store(5_555, Ordering::SeqCst);
        WOKEN_STATES[tid].store(false, Ordering::SeqCst);

        ThreadWaker::new(tid).wake();

        assert_eq!(THREAD_STATES[tid].load(Ordering::SeqCst), thread_state::READY,
            "the legitimate WAITING -> READY transition still works");
        assert_eq!(WAKE_TIMES[tid].load(Ordering::SeqCst), 5_555,
            "the waker must not write WAKE_TIMES — a stale one erasing a fresh \
             deadline is the parked-forever wedge");

        THREAD_STATES[tid].store(thread_state::FREE, Ordering::SeqCst);
        WAKE_TIMES[tid].store(0, Ordering::SeqCst);
        WOKEN_STATES[tid].store(false, Ordering::SeqCst);
    }

    /// A thread with no live EL0 trap frame must not yield a child context — not
    /// even its own, still-current `user_entry` / `user_sp`.
    ///
    /// This is the `N × INTERP_BASE + 0x6c964` instruction-abort class in miniature
    /// (`docs/runbooks/debug-thread-spawn-segv.md`, class 2). The published triple
    /// records where the *image was first entered*, which for a dynamically linked
    /// process is ld-musl's `_dlstart` — a perfectly valid-looking address that is
    /// nonetheless never a correct fork return. A child launched there re-runs
    /// musl's RELR `*slot += base` loop over an already-relocated interpreter data
    /// page: one `+= base` per birth on one physical word, `N` climbing for the
    /// whole boot.
    ///
    /// Both halves are load-bearing:
    /// - a *live* thread (the fork's parent, still owning its slot) must refuse, and
    /// - a *recycled* slot must refuse, since the triple would then be a dead
    ///   process's.
    #[test]
    fn a_thread_with_no_trap_frame_never_yields_a_child_context() {
        crate::test_support::ensure_test_runtime();
        let tid = 40;
        const LDMUSL_DLSTART: u64 = 0x3006_9de8;

        // A live, dynamically linked process thread: `update_thread_context` published
        // this triple at its execve and nothing has overwritten it since. This is the
        // parent of a fork — not a recycled slot.
        unsafe {
            let ctx = &mut *get_context_mut(tid);
            ctx.is_user_process = 1;
            ctx.user_entry = LDMUSL_DLSTART;
            ctx.user_sp = 0x20_3fff_e1f0; // the real per-parent constant from relr_probe_run1.log
            ctx.user_tls = 0x3272_a918;
        }
        assert!(
            CURRENT_TRAP_FRAME[tid].load(Ordering::Acquire) == 0,
            "precondition: this slot has no live EL0 trap frame",
        );
        assert!(
            get_saved_user_context(tid).is_none(),
            "a live thread with no trap frame handed out its *initial exec* context — \
             a child forked from it would start at {LDMUSL_DLSTART:#x} (ld-musl's \
             _dlstart) on the parent's initial stack with every GPR zeroed",
        );

        // Same answer once the slot is recycled, where the triple is a dead
        // process's rather than merely the wrong one.
        init_thread_slot_context(tid, 0x8000, 0xbeef_0000, 0, 0, 1, true);
        assert!(
            get_saved_user_context(tid).is_none(),
            "a freshly recycled slot still reported a user context",
        );

        unsafe {
            let ctx = &mut *get_context_mut(tid);
            ctx.is_user_process = 0;
        }
        THREAD_STATES[tid].store(thread_state::FREE, Ordering::SeqCst);
    }

    #[test]
    fn spawn_publish_cannot_resurrect_a_terminated_child() {
        let tid = 37;
        // A group kill that lands on a half-built child between context setup and
        // the parent's publish must win: READY here would run a thread whose
        // process teardown is already in progress.
        THREAD_STATES[tid].store(thread_state::TERMINATED, Ordering::SeqCst);
        mark_thread_ready(tid);
        assert_eq!(THREAD_STATES[tid].load(Ordering::SeqCst), thread_state::TERMINATED,
            "mark_thread_ready must never overwrite TERMINATED");

        // The normal spawn publish still works.
        THREAD_STATES[tid].store(thread_state::INITIALIZING, Ordering::SeqCst);
        mark_thread_ready(tid);
        assert_eq!(THREAD_STATES[tid].load(Ordering::SeqCst), thread_state::READY);

        THREAD_STATES[tid].store(thread_state::FREE, Ordering::SeqCst);
    }

    #[test]
    fn park_resume_cannot_resurrect_a_terminated_thread() {
        let tid = 38;
        THREAD_STATES[tid].store(thread_state::TERMINATED, Ordering::SeqCst);
        resume_running_unless_terminated(tid);
        assert_eq!(THREAD_STATES[tid].load(Ordering::SeqCst), thread_state::TERMINATED,
            "a thread killed while parked must stay TERMINATED through its resume");

        THREAD_STATES[tid].store(thread_state::WAITING, Ordering::SeqCst);
        resume_running_unless_terminated(tid);
        assert_eq!(THREAD_STATES[tid].load(Ordering::SeqCst), thread_state::RUNNING,
            "a normal park resume still transitions to RUNNING");

        THREAD_STATES[tid].store(thread_state::FREE, Ordering::SeqCst);
    }

    /// The case the state CAS alone cannot defend: the slot's NEW occupant is
    /// legitimately WAITING, so a stale waker's WAITING→READY transition would
    /// succeed — waking the wrong thread with a wake meant for its predecessor.
    /// The generation in the handle is what refuses it.
    #[test]
    fn a_stale_handle_is_refused_even_against_a_waiting_new_occupant() {
        crate::test_support::ensure_test_runtime();
        // Slot 39: claim through the REAL claim path so scrub bumps SLOT_GEN.
        let Some(tid) = claim_free_slot(39, 40) else {
            panic!("slot 39 unexpectedly busy — this module owns 35..40");
        };
        let gen_first = thread_generation(tid);
        let stale = wake_handle_for_thread(tid);
        assert!(stale.is_current());

        // The thread dies; the slot is recycled to a new occupant.
        release_test_thread_slot(tid);
        let tid2 = claim_free_slot(39, 40).expect("re-claim of slot 39");
        assert_eq!(tid, tid2, "the range pins the same slot");
        assert!(thread_generation(tid) > gen_first, "claim-time scrub must bump the generation");
        assert!(!stale.is_current(), "the old incarnation's handle must have gone stale");

        // New occupant parks, legitimately, with a deadline.
        THREAD_STATES[tid].store(thread_state::WAITING, Ordering::SeqCst);
        WAKE_TIMES[tid].store(7_777, Ordering::SeqCst);
        WOKEN_STATES[tid].store(false, Ordering::SeqCst);

        wake_by_handle(stale);

        assert_eq!(THREAD_STATES[tid].load(Ordering::SeqCst), thread_state::WAITING,
            "a stale handle must not wake the slot's new occupant");
        assert!(!WOKEN_STATES[tid].load(Ordering::SeqCst),
            "a stale handle must not even arm the sticky flag — that spends a \
             phantom wake on the new occupant's next park");
        assert_eq!(WAKE_TIMES[tid].load(Ordering::SeqCst), 7_777);

        // A handle for the CURRENT incarnation still works.
        wake_by_handle(wake_handle_for_thread(tid));
        assert_eq!(THREAD_STATES[tid].load(Ordering::SeqCst), thread_state::READY);
        assert!(WOKEN_STATES[tid].load(Ordering::SeqCst));

        WOKEN_STATES[tid].store(false, Ordering::SeqCst);
        WAKE_TIMES[tid].store(0, Ordering::SeqCst);
        release_test_thread_slot(tid);
    }
}

/// Phase 7d: real-concurrency proof for `SyncContext`'s SAFETY comment above
/// `THREAD_CONTEXTS`. Uses real `std::thread`s (standing in for cores) against the
/// actual `THREAD_STATES` atomics and `THREAD_CONTEXTS` accessors — not a model — to
/// check the two claims that comment makes: `claim_free_slot`'s FREE -> INITIALIZING
/// CAS gives exclusive ownership of a slot's context under contention, and a plain
/// `Ordering::SeqCst` store is enough to publish a context write to a peer without any
/// coarse lock. Confined to slots 50..64 (a range no other test in this file touches)
/// so it can run under `cargo test`'s default parallelism without colliding with
/// `signal_mask_tests`/`pending_kill_tests`'s fixed tids.
#[cfg(test)]
mod thread_contexts_invariant_tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::sync::{Arc, Barrier};
    use std::thread;

    const RANGE_START: usize = 50;
    const RANGE_END: usize = 64; // exclusive; MAX_THREADS == 64

    #[test]
    fn claim_free_slot_never_double_claims_under_contention() {
        const N_THREADS: usize = 8;
        const ITERS: usize = 500;

        let barrier = Arc::new(Barrier::new(N_THREADS));
        let mismatches = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::new();
        for t in 0..N_THREADS {
            let barrier = Arc::clone(&barrier);
            let mismatches = Arc::clone(&mismatches);
            handles.push(thread::spawn(move || {
                barrier.wait();
                for _ in 0..ITERS {
                    let Some(idx) = claim_free_slot(RANGE_START, RANGE_END) else {
                        // Pool momentarily exhausted by peer threads — fine, retry.
                        continue;
                    };
                    // We just won the CAS: nobody else should be touching this slot's
                    // context. Stamp it, yield to widen the window for a would-be
                    // clobberer, then check our stamp is still intact.
                    let marker = (idx as u64) << 32 | t as u64;
                    unsafe {
                        (*get_context_mut(idx)).x19 = marker;
                    }
                    thread::yield_now();
                    let seen = unsafe { (*get_context(idx)).x19 };
                    if seen != marker {
                        mismatches.fetch_add(1, Ordering::SeqCst);
                    }
                    release_test_thread_slot(idx);
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(
            mismatches.load(Ordering::SeqCst),
            0,
            "a THREAD_CONTEXTS write was clobbered by a concurrent claimant of the same \
             slot — the FREE->INITIALIZING CAS is not actually giving exclusive ownership"
        );
    }

    /// The half of the SAFETY comment that isn't about mutual exclusion at all: once a
    /// writer publishes via `mark_thread_ready` (a plain SeqCst store), is that by
    /// itself enough for a peer core to see the context write that preceded it? This is
    /// the concern `BKL_PHASE7_AUDIT.md` §2.2 raised ("relying on the BKL for
    /// publication ordering") — prove it holds on the atomics alone.
    #[test]
    fn ready_transition_publishes_context_writes_without_a_lock() {
        const ITERS: usize = 2000;
        const MAX_SPINS: u32 = 20_000_000;

        for iter in 0..ITERS {
            let Some(idx) = claim_free_slot(RANGE_START, RANGE_END) else {
                continue;
            };
            let marker = (idx as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) | (iter as u64) << 1 | 1;
            let mismatch = Arc::new(AtomicUsize::new(0));

            let reader = {
                let mismatch = Arc::clone(&mismatch);
                thread::spawn(move || {
                    let mut spins = 0u32;
                    loop {
                        if get_thread_state(idx) == thread_state::READY {
                            let val = unsafe { (*get_context(idx)).x19 };
                            if val != marker {
                                mismatch.store(1, Ordering::SeqCst);
                            }
                            return;
                        }
                        spins += 1;
                        assert!(spins < MAX_SPINS, "writer never published READY for slot {idx}");
                        std::hint::spin_loop();
                    }
                })
            };

            unsafe {
                (*get_context_mut(idx)).x19 = marker;
            }
            mark_thread_ready(idx);

            reader.join().unwrap();
            assert_eq!(
                mismatch.load(Ordering::SeqCst),
                0,
                "reader observed THREAD_STATES==READY before the THREAD_CONTEXTS write \
                 that happened-before it in the writer's program order — publication is \
                 not actually safe on the atomics alone"
            );

            release_test_thread_slot(idx);
        }
    }
}
