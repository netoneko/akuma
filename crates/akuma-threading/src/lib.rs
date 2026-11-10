//! Preemptive threading: the fixed-size thread pool, the lock-free per-thread
//! state arrays, the context switch, park/wake, per-thread stacks and overflow
//! detection, and the `thread_start` / SGI-scheduler trampolines.
//!
//! # What this crate is for
//!
//! It is where **all** of `akuma-exec`'s scheduler `unsafe` lives — extracted
//! 2026-09-02 (`AKUMA_EXEC_AUDIT.md` §6 step C). `akuma-exec` proper (fork,
//! exec, signals, channels, fds, lifecycle, reclaim) no longer carries the
//! `THREAD_CONTEXTS` raw derefs, the `msr tpidr_el1`, the context-switch asm or
//! the trampolines. Making "is Akuma's scheduler sound?" a ~5,200-line question
//! instead of part of a 9,000-line one is the point — same trade as
//! `akuma-net-nic` and `akuma-user-access`.
//!
//! It **cannot** `#![forbid(unsafe_code)]`: it is per-slot `static` arrays,
//! `global_asm!`, and raw context-frame reads end to end.
//!
//! # It sits BELOW `akuma-exec`
//!
//! It does not name the `Process` type. `UserContext` / `UserTrapFrame` /
//! `MAX_THREADS` are in `akuma-exec-core` (the forbid-clean leaf). The seven
//! things it asks of the process layer — trace gate, tid→pid lookups, the
//! reclaim drain flag, the interrupt check, and two diagnostics — are
//! [`ProcessHooks`], registered by `akuma_exec::init`. The platform callbacks it
//! needs (uptime, SGI, wake-core, EOI, print) are [`ThreadRuntime`]; the tuning
//! knobs are [`ThreadConfig`]. `akuma-exec` re-exports this crate as
//! `akuma_exec::threading`, so every `akuma_exec::threading::…` call site
//! resolves unchanged.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]
#![allow(dead_code)]
// The lint allow-list this code was written against — ported verbatim from
// `akuma-exec/src/lib.rs`, where it lived until 2026-09-02.
#![allow(
    clippy::future_not_send,
    clippy::must_use_candidate,
    clippy::missing_const_for_fn,
    clippy::uninlined_format_args,
    clippy::cast_ptr_alignment,
    clippy::items_after_statements,
    clippy::significant_drop_in_scrutinee,
    clippy::too_many_lines,
    clippy::use_self,
    clippy::struct_field_names,
    clippy::struct_excessive_bools,
    clippy::similar_names,
    clippy::unreadable_literal,
    clippy::unnecessary_cast,
    clippy::redundant_else,
    clippy::semicolon_if_nothing_returned,
    clippy::single_match_else,
    clippy::declare_interior_mutable_const,
    clippy::borrow_as_ptr,
    clippy::ptr_as_ptr,
    clippy::unused_self,
    clippy::vec_init_then_push,
    clippy::pub_underscore_fields,
    clippy::doc_markdown,
    clippy::too_long_first_doc_paragraph,
    clippy::needless_pass_by_value,
    clippy::if_not_else,
    clippy::manual_div_ceil,
    clippy::option_if_let_else,
    clippy::match_wildcard_for_single_variants,
    clippy::cast_possible_wrap,
    clippy::redundant_closure_for_method_calls,
    clippy::iter_without_into_iter,
    clippy::collapsible_if,
    clippy::significant_drop_tightening,
    clippy::ref_as_ptr,
    clippy::needless_range_loop,
    clippy::new_without_default,
    clippy::match_same_arms,
    clippy::redundant_closure,
    clippy::manual_is_variant_and,
    clippy::missing_safety_doc,
    clippy::let_and_return,
    clippy::manual_range_contains,
    clippy::empty_line_after_doc_comments,
    clippy::inline_always,
    clippy::bool_to_int_with_if,
    clippy::manual_saturating_arithmetic,
    clippy::cast_lossless,
    clippy::option_map_or_none,
    clippy::redundant_field_names,
    clippy::let_underscore_untyped,
    unused_unsafe,
    unused_mut,
    clippy::implicit_saturating_sub,
    clippy::manual_let_else,
    clippy::verbose_bit_mask,
    clippy::ptr_cast_constness,
    clippy::derive_partial_eq_without_eq,
    clippy::or_fun_call,
    clippy::not_unsafe_ptr_arg_deref,
    clippy::identity_op,
    clippy::while_let_loop,
    clippy::collapsible_else_if,
    clippy::needless_continue,
    clippy::inherent_to_string,
    clippy::manual_find,
    clippy::manual_is_multiple_of,
    clippy::eq_op,
    clippy::doc_overindented_list_items,
    clippy::map_unwrap_or,
    clippy::used_underscore_binding,
    clippy::branches_sharing_code,
    clippy::doc_comment_double_space_linebreaks,
    clippy::no_effect_underscore_binding,
    clippy::unwrap_or_default,
    clippy::should_implement_trait,
)]

extern crate alloc;

pub mod sigframe;
#[cfg(test)]
mod test_support;
/// Trap-frame / thread-slot types and the pure stack-sizing policy — in
/// `akuma-exec-core` since 2026-09-01. Re-exported under the original path so
/// `akuma_exec::threading::types::MAX_THREADS` &c. resolve for `akuma-exceptions`
/// and `akuma-vfs-glue`.
pub use akuma_exec_core::thread as types;

pub use types::*;
use alloc::boxed::Box;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use akuma_primitives::OnceCopy;
pub use akuma_switch::rust_switch_finished;
use akuma_switch::{CtxField, ON_CPU, PER_CORE_OFFCPU, peek};
use akuma_primitives::once::Registered;
use akuma_primitives::irq::{with_irqs_disabled, IrqGuard};
use akuma_primitives::safe_print;
use spinning_top::Spinlock;

// `akuma-exec` compatibility aliases: these paths resolved to `crate::…` while
// this module lived inside that crate.
use akuma_mmu as mmu;
use akuma_bkl::bkl;
// Only the `kernel_smp_shared` thread-tag call sites use this.
#[cfg(kernel_smp_shared)]
use akuma_bkl::sync;
use akuma_exec_core::process::UserContext;
use akuma_mmap::PhysFrame;

// ── platform + config + process hooks (were `crate::runtime` / `crate::process`) ──

/// The platform callbacks the scheduler needs — the subset of `akuma-exec`'s
/// `ExecRuntime` this crate ever touched. Registered by [`register`].
#[derive(Clone, Copy)]
pub struct ThreadRuntime {
    pub uptime_us: fn() -> u64,
    pub trigger_sgi: fn(u32),
    pub wake_core: fn(u8),
    pub wake_remote_idle: fn() -> bool,
    pub end_of_interrupt: fn(u32),
    pub print_str: fn(&str),
}

/// The tuning knobs — the subset of `ExecConfig` this crate reads.
#[derive(Clone, Copy)]
pub struct ThreadConfig {
    pub reserved_threads: usize,
    pub kernel_stack_size: usize,
    pub system_thread_stack_size: usize,
    pub user_thread_stack_size: usize,
    pub boot_stack_base: usize,
    pub boot_stack_top: usize,
    pub enable_stack_canaries: bool,
    pub stack_canary: u64,
    pub canary_words: usize,
    pub network_thread_ratio: u32,
    pub prioritize_never_scheduled: bool,
    pub deferred_thread_cleanup: bool,
    pub thread_cleanup_cooldown_us: u64,
    pub syscall_debug_info_enabled: bool,
    pub enable_sgi_debug_prints: bool,
}

/// The seven things the scheduler asks of the process layer above it.
#[derive(Clone, Copy)]
pub struct ProcessHooks {
    pub clear_draining: fn(usize),
    /// Is this thread inside a retired-process drain right now? The reaper asks
    /// before freeing a terminated thread's stack — see
    /// `cleanup_terminated_internal`.
    pub drain_in_flight: fn(usize) -> bool,
    pub lifecycle_trace_on: fn() -> bool,
    pub pid_for_thread: fn(usize) -> Option<u32>,
    pub find_pid_by_thread: fn(usize) -> Option<u32>,
    pub is_current_interrupted: fn() -> bool,
    /// `(current_syscall, tgid, l0_phys)` for the `[THR-DUMP]` correlation line.
    pub proc_dump_info: fn(u32) -> Option<(u64, u32, usize)>,
    pub dump_orphan_processes: fn(),
}

static THREAD_RT: Registered<ThreadRuntime> =
    Registered::new("akuma-threading: ThreadRuntime not registered — call akuma_exec::init()");
static THREAD_CFG: Registered<ThreadConfig> =
    Registered::new("akuma-threading: ThreadConfig not registered — call akuma_exec::init()");
static PROCESS_HOOKS: Registered<ProcessHooks> =
    Registered::new("akuma-threading: ProcessHooks not registered — call akuma_exec::init()");

/// Register the platform runtime + tuning config. Called once from `akuma_exec::init`.
pub fn register(rt: ThreadRuntime, cfg: ThreadConfig) {
    THREAD_RT.register(rt);
    THREAD_CFG.register(cfg);
}

/// Register the process-layer callbacks. Called once from `akuma_exec::init`.
pub fn register_process_hooks(h: ProcessHooks) {
    PROCESS_HOOKS.register(h);
}

#[inline]
fn runtime() -> ThreadRuntime {
    THREAD_RT.require()
}
#[inline]
fn config() -> ThreadConfig {
    THREAD_CFG.require()
}
#[inline]
fn process() -> ProcessHooks {
    PROCESS_HOOKS.require()
}

/// Set the current exception stack pointer (TPIDR_EL1).
#[cfg(all(target_os = "none", target_arch = "aarch64"))]
#[inline]
pub fn set_current_exception_stack(stack_top: u64) {
    akuma_switch::set_exception_stack(stack_top);
}

#[cfg(not(all(target_os = "none", target_arch = "aarch64")))]
#[inline]
pub fn set_current_exception_stack(_stack_top: u64) {}

// `StackWriter` and this crate's `safe_print!` copy both moved to
// `akuma-primitives::console` — one writer and one macro for the whole tree
// instead of five and three. `safe_print!` still resolves: `lib.rs`
// re-exports the macro. See that module's header for the census.

// ============================================================================
// Lock-Free Thread State Management
// ============================================================================

/// Cleanup callback (e.g. for process unregistration).
///
/// `OnceCopy`, not `AtomicUsize`: this and the two hooks below were an
/// `AtomicUsize` holding `cb as usize`, transmuted back to `fn(usize)` at each
/// read, from 2026-03-19 — five months before `OnceCopy` existed. The type was
/// never actually erased (the setter takes `fn(usize)` and the transmute
/// produced `fn(usize)`), so the `usize` was only an internal representation,
/// and `transmute` is strictly more dangerous than the API needs: it will
/// convert any address to any signature, with a `!= 0` check as the only guard.
/// `OnceCopy` is the tree's one mechanism for "registered at boot, read from
/// anywhere" and costs the same relaxed load. See
/// `docs/reference/subsystems/kernel-hooks.md`.
///
/// `OnceCopy` rather than `Registered` because absence is a legitimate state
/// here — the recycler runs before the kernel registers anything — so the read
/// sites degrade rather than panicking.
static CLEANUP_CALLBACK: OnceCopy<fn(usize)> = OnceCopy::new();

/// Set a callback to be invoked when a thread is cleaned up (recycled).
/// The callback receives the thread ID (index) of the cleaned up thread.
///
/// Single-shot: a second call is ignored rather than replacing the first. Every
/// in-tree registration happens exactly once, from `akuma_exec::process::init`.
pub fn set_cleanup_callback(cb: fn(usize)) {
    CLEANUP_CALLBACK.set(cb);
}

/// Kernel-registered purge for per-tid state this crate cannot reach.
///
/// `scrub_thread_slot` covers the per-slot arrays that live in this module, but other
/// subsystems register a *thread id* and rely on that thread to deregister itself —
/// `FUTEX_WAITERS` (`src/syscall/sync.rs`) most importantly, where a tid left queued by a
/// thread that died while parked is inherited by the slot's next occupant and silently
/// absorbs that address's next wake. Those tables live in the kernel crate, so the kernel
/// registers a hook here and the recycler calls it alongside `CLEANUP_CALLBACK`.
static SLOT_PURGE_CALLBACK: OnceCopy<fn(usize)> = OnceCopy::new();

/// Register the per-tid purge invoked when a thread slot is recycled. Runs with no lock
/// of this module held (same point as `CLEANUP_CALLBACK`), so the hook may take its own.
///
/// Single-shot; registered once from `akuma-kernel-glue` (`futex_purge_tid`).
pub fn set_slot_purge_callback(cb: fn(usize)) {
    SLOT_PURGE_CALLBACK.set(cb);
}

/// Recover locks a thread died *holding* — the TERMINATED→FREE sweep.
///
/// Deliberately **not** `SLOT_PURGE_CALLBACK`, which is also called from
/// `mark_thread_terminated` the instant a peer kill lands: there the slot is merely
/// TERMINATED, `ON_CPU` may still be set, and the thread can still be executing inside
/// the very critical section whose lock this would release. Dropping a queue entry that
/// early is safe (it is of no further use to a dying thread) — releasing a lock is not.
///
/// `akuma-locks-rw`'s reap contract wants the one point where the tid is *100% dead and
/// not yet reissued*: after the `ON_CPU` gate, after the cooldown, after the
/// TERMINATED→INITIALIZING CAS has excluded every spawn, and before the FREE store. That
/// is the single call site below, and it is why this is a second hook rather than a
/// second consumer of the first. See `docs/archive/AKUMA_EXT2_CLEANUP.md` §4.5/§4.6.
static SLOT_REAP_CALLBACK: OnceCopy<fn(usize)> = OnceCopy::new();

/// Register the dead-holder lock sweep. Same lock context as the callbacks above (none of
/// this module's locks held), so the hook may take its own.
///
/// Single-shot; registered once from `akuma-vfs-glue` (`ext2::reap_dead_thread`).
pub fn set_slot_reap_callback(cb: fn(usize)) {
    SLOT_REAP_CALLBACK.set(cb);
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
/// A stack's bookkeeping region (`akuma-stack`). The one place `akuma-threading` vouches for a stack's bytes.
///
/// The callers pass either a slot's PMM-backed stack (owned by that slot, mapped for its life) or the boot
/// stack (thread 0's), or just the canary words at a stack's base; none is aliased by a Rust reference.
fn stack_region(base: usize, size: usize) -> akuma_stack::StackRegion {
    akuma_switch::vouch_stack(base, size)
}

/// Snapshot the EL0 trap frame published for `tid` in `CURRENT_TRAP_FRAME`, if any.
///
/// The entry is stored by `set_current_trap_frame` on the owning thread's syscall entry and cleared by its
/// epilogue, and the frame lives on that thread's kernel stack, so a non-zero entry names a live frame. The
/// copy is a plain read of the whole struct, as the `&*` deref it replaces was.
fn published_trap_frame(tid: usize) -> Option<UserTrapFrame> {
    akuma_switch::read_published::<UserTrapFrame>(CURRENT_TRAP_FRAME[tid].load(Ordering::Acquire) as usize)
}

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
    let sp: usize = if cfg!(target_os = "none") { akuma_cpu::reg::sp() } else { top };
    let paint_end = sp.saturating_sub(8 * 1024) & !7; // 8 KB headroom below live SP
    if paint_end <= (base + 7) & !7 {
        return;
    }
    // SAFETY (stack_region): the boot stack is thread 0's; only the part below the live SP's 8 KB
    // headroom is painted.
    stack_region(base, top - base).paint_sentinel(0, paint_end);
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
    // A fresh, unused stack: nothing is live in it yet.
    stack_region(base, top - base).paint_sentinel(config().canary_words * 8, top);
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
        let size = stack.size;
        Some((stack_region(stack.base, size).high_water(config().canary_words * 8), size))
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
        boot_used = stack_region(boot_base, boot_size).high_water(config().canary_words * 8);
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
// `ON_CPU` and `PER_CORE_OFFCPU` live in `akuma-switch`, beside `rust_switch_finished` which clears them.

/// Per-thread *expected* TTBR0 L0 base — the physical page-table root this
/// thread's user context is supposed to run under. 0 = unknown (kernel/idle
/// threads and slots between scrub and first context write): checks skip it.
///
/// Written wherever the canonical value changes hands: `update_thread_context`
/// when a parent builds a child's context, and `UserAddressSpace::
/// activate`/`deactivate` (via [`note_current_expected_l0`]) when a thread
/// installs tables itself. The ASID half of TTBR0 is deliberately masked out —
/// a cloned thread legitimately runs under the parent's L0 with a different
/// ASID (see THREAD_SPAWN_SEGV_INVESTIGATION.md §2f "Read the flag carefully").
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

/// The netpoll loop's own handle while it parks between laps (`u64::MAX`: none). A packet
/// or a loopback push rings the netpoll doorbell, which wakes it through here.
static NETPOLL_PARKER: AtomicU64 = AtomicU64::new(u64::MAX);

/// Register the calling thread as the netpoll loop that parks between laps
/// ([`wake_netpoll_parker`] wakes it). Called by that thread itself, so its handle is live.
pub fn register_netpoll_parker() {
    NETPOLL_PARKER.store(current_wake_handle().0, Ordering::Release);
}

/// Wake the parked netpoll loop, if one registered. Lock-free and allocation-free, so an
/// interrupt handler may call it; a wake that lands before the loop parks is kept by the
/// sticky woken flag [`schedule_blocking`] checks, and a stale handle does nothing.
pub fn wake_netpoll_parker() {
    let raw = NETPOLL_PARKER.load(Ordering::Acquire);
    if raw != u64::MAX {
        wake_by_handle(WakeHandle(raw));
    }
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
/// Re-tested 2026-08-19 against the llama.cpp `-t N` barrier collapse: flipping
/// it ON made decode WORSE (tg16 `-t 2`: 2.5 -> 1.15 tok/s) — the single-slot
/// hint preempts the producer thread that still has compute left. Keep it off.
static PREEMPT_WAKE_TID: AtomicUsize = AtomicUsize::new(MAX_THREADS);

/// Master switch for the [`PREEMPT_WAKE_TID`] wakeup-locality experiment. Off:
/// the hint is never set, so the scheduler is exactly baseline round-robin.
const WAKEUP_LOCALITY_HINT: bool = false;

/// Consecutive involuntary ticks each thread has declined displacement for (the
/// scheduler's bounded displacement bypass — see `schedule_indices`). Relaxed:
/// only the core currently running the slot's thread writes its counter.
static DISPLACE_DEFER: [AtomicU8; MAX_THREADS] = {
    #[allow(clippy::declare_interior_mutable_const)]
    const INIT: AtomicU8 = AtomicU8::new(0);
    [INIT; MAX_THREADS]
};

/// Wake-deadline preemption (2026-08-18, `SCHEDULING_AUDIT.md` /
/// `SCHEDULING_INVESTIGATION.md`): when the scheduler's deadline wake-pass
/// promotes a WAITING thread to READY (its sleep/timer deadline expired), arm
/// [`PREEMPT_WAKE_TID`] so it runs on the NEXT switch instead of joining the
/// back of the round-robin queue.
///
/// Before this, eligibility was not execution: a thread whose sleep deadline
/// had expired waited a full round-robin pass (one timeslice per runnable
/// thread), giving short sleeps a hard floor of ~tick × runnable-threads
/// (~35 ms measured at 3 runnable threads, 10 ms tick) and making every
/// poll-interval cap (`BLOCKING_POLL_INTERVAL_US`,
/// `RUMP_BLOCKING_POLL_INTERVAL_US`) inert below that floor.
///
/// Distinct from [`WAKEUP_LOCALITY_HINT`]: that gate arms the same hint from
/// an explicit `ThreadWaker::wake` (futex/cond) and was measured NOT to help
/// the rump sysproxy; this one arms it from the *deadline* wake-pass, where
/// the measurement says the cost lives. Keep the two experiments separate.
///
/// Fairness: the hint fires once per wake; the preempted thread stays READY
/// and keeps its rotation position, so a periodic short sleeper steals only
/// its own runtime (~µs per tick), never a peer's quantum. When several
/// sleepers expire in the same pass, the earliest deadline wins the hint and
/// the rest wait their rotation as before.
const WAKE_DEADLINE_PREEMPT: bool = true;

/// Signal-wake preemption (2026-08-28,
/// `docs/archive/PTHREAD_KILL_EINTR_DELIVERY_STARVATION.md` §"Re-verified"):
/// arm [`PREEMPT_WAKE_TID`] when [`pend_signal_for_thread`] promotes a thread
/// READY, so a thread that must report `EINTR` runs on the NEXT switch instead
/// of a full round-robin pass later.
///
/// This is the third, independently-gated arm on the same one-slot hint, and it
/// exists because the other two do not cover signals:
///
/// - [`WAKEUP_LOCALITY_HINT`] arms it for *every* explicit `wake()`
///   (futex/cond). Measured harmful and stays off — it preempts a producer that
///   still has compute left (llama decode 2.5 -> 1.15 tok/s).
/// - [`WAKE_DEADLINE_PREEMPT`] arms it from the *deadline* wake-pass only, so a
///   signal-woken thread never sees it.
///
/// The measurement that motivates this arm: `pthread_kill` a thread blocked in
/// a pipe `read`, single signal, SMP=1, 1 ms tick. The pending bit is observed
/// correctly (`DELIVERED_SIGNALS` does its job) and the `EINTR` decision ->
/// handler-frame setup takes 185 us — but the woken thread waits **4.4 ms** to
/// be picked, and needs a further round to get back out to EL0, for a
/// **11-27 ms** per-signal round trip. jobserver-rs re-signals every **10 ms**
/// and gives up after 100 attempts, so the report lands after the caller has
/// already declared the thread leaked. That is the same "eligibility is not
/// execution" floor [`WAKE_DEADLINE_PREEMPT`] was added for, reached by a
/// different path.
///
/// Why the llama regression does not apply here: a signal wake is not a
/// compute-handoff. It means "this thread must observe something now", it is
/// rare (not a per-iteration hot path), and the woken thread's next act is to
/// return to userspace rather than to consume a work item a peer is still
/// producing. Fairness is unchanged from the deadline arm: the hint fires once
/// per wake and the preempted thread stays READY, keeping its rotation slot.
const SIGNAL_WAKE_PREEMPT: bool = true;

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

/// The slot registered as `core_id`'s idle thread, or `None` if that core has not
/// come up. Diagnostics and the `core_idle_slots_survive_init` self-test.
pub fn core_idle_slot(core_id: usize) -> Option<usize> {
    if core_id >= MAX_CORES {
        return None;
    }
    let slot = IDLE_SLOT_FOR_CORE[core_id].load(Ordering::Acquire);
    (slot >= 0 && (slot as usize) < MAX_THREADS).then_some(slot as usize)
}

/// The `(base, size)` this pool has RECORDED for `slot`'s kernel stack, or `None`
/// if the slot has no stack. This is the pool's belief, which is exactly what the
/// `core_idle_slots_survive_init` self-test needs to compare against the address a
/// secondary core is actually running on.
pub fn slot_stack_bounds(slot: usize) -> Option<(usize, usize)> {
    if slot >= MAX_THREADS {
        return None;
    }
    with_irqs_disabled(|| {
        let pool = POOL.lock();
        let stack = &pool.stacks[slot];
        stack.is_allocated().then_some((stack.base, stack.size))
    })
}

/// Is `slot` a thread slot that a core has already adopted as its idle thread?
///
/// Exists for `init`, which must not trample slots that are ALREADY LIVE. On the
/// self-test image `smp_shared::bringup_secondaries()` runs BEFORE `threading::init`
/// (see `src/main.rs`), so by the time init runs, each secondary core has claimed a
/// slot via [`adopt_current_as_core_idle`] and is *executing on it right now*. Init's
/// job is to set up FREE slots; for these it must keep its hands off.
///
/// Reads [`IS_IDLE_THREAD`] rather than the pool, because that is what
/// [`register_core_idle`] sets and it needs no lock — init calls this while holding
/// the `POOL` lock in one place and not in the other.
fn is_adopted_core_idle(slot: usize) -> bool {
    slot < MAX_THREADS && IS_IDLE_THREAD[slot].load(Ordering::Acquire)
}

/// Per-thread pending signal bitmask.  Bit N set = signal (N+1) pending.
/// Multiple signals can be pending simultaneously (unlike the old single-slot).
/// Set by pend_signal_for_thread via fetch_or.
/// Cleared one-at-a-time by take_pending_signal via fetch_and.
static PENDING_SIGNALS: [AtomicU64; MAX_THREADS] = {
    const INIT: AtomicU64 = AtomicU64::new(0);
    [INIT; MAX_THREADS]
};

/// Per-thread bitmask of signals that have been **delivered** since this
/// thread's current syscall began. Bit N set = signal (N+1) was delivered.
///
/// # Why a second mask exists
///
/// `PENDING_SIGNALS` is cleared by `take_pending_signal` the moment a signal is
/// delivered, and delivery happens on the return-to-EL0 paths in
/// `src/exceptions.rs` — including the `rt_sigreturn` path, which takes the next
/// pending signal immediately after restoring the previous handler's context.
///
/// A blocking syscall reports `EINTR` by *observing* the pending bit in its wait
/// loop. So a signal source fast enough to keep the deliver → handler →
/// `rt_sigreturn` → deliver chain saturated clears every bit before the blocked
/// loop is ever scheduled to look at it: the handler runs over and over while
/// the syscall it was supposed to interrupt never returns. That is a
/// **starvation bug**, not merely a race — delivery has strict priority over
/// resuming the interrupted syscall, and nothing bounds it.
///
/// It was latent for as long as it took the slow path to be slow enough. Fixing
/// the identity cache (`docs/archive/IDENTITY_CACHE_LAZY_RESTAMP.md`) removed
/// the microseconds the loop had been winning by, and `pthread_kill_eintr`
/// hung: `PHASE1 FAIL: read() never returned; helper thread leaked (handler ran
/// 64 times)`. Adding a `safe_print!` to the wait loop made it pass again, which
/// is the signature of a timing window rather than a wrong branch.
///
/// This mask makes the observation **sticky**: a delivered signal stays visible
/// to the blocking loop until that loop consumes it, so the EINTR decision no
/// longer depends on winning a race against the delivery path.
///
/// Cleared at syscall entry (a fresh excursion starts with no record) with one
/// deliberate exception, `rt_sigreturn` — see `clear_delivered_signals`.
static DELIVERED_SIGNALS: [AtomicU64; MAX_THREADS] = {
    const INIT: AtomicU64 = AtomicU64::new(0);
    [INIT; MAX_THREADS]
};

/// Per-thread "a signal handler frame is on the stack and userspace has not run
/// since we installed it".
///
/// # Why delivery needs a bound, not just a record
///
/// [`DELIVERED_SIGNALS`] makes the *interrupt* observable. It does not make the
/// interrupted syscall's **return value** observable, and those are different
/// problems. The `EINTR` is computed by the wait loop and stashed in the trap
/// frame's `x0`, but the return-to-EL0 path then installs a handler frame on top
/// of it, and `rt_sigreturn` takes the next pending signal *immediately* after
/// restoring the handler's context. So userspace never executes the instruction
/// after its `svc` while signals keep arriving: the handler runs, and runs, and
/// the caller never sees `-1/EINTR`.
///
/// Linux has the same re-check on its exit path and does not starve, because a
/// delivery round trip there costs microseconds. Akuma's costs milliseconds —
/// dispatch is tick-quantised, and the round trip needs two scheduling rounds
/// (measured 11-27 ms at a 1 ms tick, SMP=1). Against a sender at 10 ms —
/// jobserver-rs's exact cadence — the chain refills faster than it drains, and
/// that is unbounded: nothing in it depends on how long the caller has waited.
///
/// This flag is the bound. Delivery keeps its priority over resuming the
/// interrupted syscall for exactly **one** handler per unit of userspace
/// progress. A signal that loses the race stays pending and is delivered at the
/// next kernel exit — the next syscall, or the next timer tick at the latest —
/// which is Linux's own "signals are delivered at kernel exit" model, minus the
/// unbounded same-exit re-arm.
///
/// Set in `try_deliver_signal` (the single delivery chokepoint). Cleared
/// wherever userspace demonstrably ran: syscall entry — **except
/// `rt_sigreturn`**, which is the syscall the handler returns *through*, so
/// clearing there would erase the very evidence this flag exists to carry — and
/// the timer/IRQ delivery path, which by definition interrupted running
/// userspace.
static SIGFRAME_ACTIVE: [AtomicBool; MAX_THREADS] = {
    #[allow(clippy::declare_interior_mutable_const)]
    const INIT: AtomicBool = AtomicBool::new(false);
    [INIT; MAX_THREADS]
};

/// Note that a handler frame was just installed for `slot`. See [`SIGFRAME_ACTIVE`].
pub fn note_sigframe_installed(slot: usize) {
    if slot < MAX_THREADS {
        SIGFRAME_ACTIVE[slot].store(true, Ordering::Release);
    }
}

/// Is a handler frame outstanding for `slot` with no userspace progress since?
/// `rt_sigreturn` consults this to decide whether re-delivering now would starve
/// the interrupted syscall's return.
pub fn sigframe_active(slot: usize) -> bool {
    slot < MAX_THREADS && SIGFRAME_ACTIVE[slot].load(Ordering::Acquire)
}

/// Record that userspace has run for `slot`, so the next delivery may proceed.
pub fn clear_sigframe_active(slot: usize) {
    if slot < MAX_THREADS {
        SIGFRAME_ACTIVE[slot].store(false, Ordering::Release);
    }
}

/// Record that `sig` was delivered to `slot`. Called from every delivery site in
/// `src/exceptions.rs`, right where `take_pending_signal` clears the pending bit.
pub fn note_delivered_signal(slot: usize, sig: u32) {
    if slot < MAX_THREADS && sig > 0 && sig <= 64 {
        DELIVERED_SIGNALS[slot].fetch_or(1u64 << (sig - 1), Ordering::AcqRel);
    }
}

/// Signals delivered to `slot` since its current syscall began.
pub fn delivered_signals_raw(slot: usize) -> u64 {
    if slot >= MAX_THREADS {
        return 0;
    }
    DELIVERED_SIGNALS[slot].load(Ordering::Acquire)
}

/// Consume `bits` from `slot`'s delivered set — the blocking loop calls this
/// once it has turned them into an `EINTR`, so the record cannot fire twice.
pub fn consume_delivered_signals(slot: usize, bits: u64) {
    if slot < MAX_THREADS {
        DELIVERED_SIGNALS[slot].fetch_and(!bits, Ordering::AcqRel);
    }
}

/// Drop `slot`'s delivered record. Called at syscall entry so a record cannot
/// leak into an unrelated later syscall and fabricate an `EINTR` there.
///
/// **`rt_sigreturn` must not clear it.** The handler returns through that
/// syscall, so clearing there would erase the very record the interrupted
/// blocking syscall is about to resume and read — which is the bug this whole
/// mask exists to fix.
pub fn clear_delivered_signals(slot: usize) {
    if slot < MAX_THREADS {
        DELIVERED_SIGNALS[slot].store(0, Ordering::Release);
    }
}

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

/// **"This thread's blocking syscall should return `EINTR`."** One bit per
/// thread, set by `akuma_exec::process::interrupt_thread` and consumed by
/// `is_current_interrupted`.
///
/// # Why it is an array and not the `ProcessChannel` flag it mirrors
///
/// `is_current_interrupted` runs in the syscall prologue — **every syscall, both
/// kernels** — and the flag it read lived in a channel reached through
/// `get_channel(tid)`, which is `with_irqs_disabled(|| PROCESS_CHANNELS.lock()
/// .get(&tid).cloned())`: an IRQ mask, a spinlock, a `BTreeMap` lookup and an
/// `Arc` clone-and-drop. That was affordable only while the read had a fast path
/// in front of it (`Process::channel`, one field load), and that fast path was
/// *wrong* — it reads an object a whole `fork` tree shares, where the writer
/// writes per-thread, so an interrupt aimed at one thread was invisible to it.
///
/// Fixing the reader by dropping to the registry made every syscall pay the
/// lock. Measured on the HP box, 2026-09-11: `fdprobe` stopped finishing inside
/// its 10 000-yield budget and the bare-metal suite went 665/0 -> 663/3, while
/// QEMU and Firecracker both stayed green — a cost regression that only real
/// silicon under SMP=4 was slow enough to show. This is the third shape and the
/// first correct one: per-thread like the writer, one relaxed load like
/// [`PENDING_KILL`] beside it.
///
/// Scrubbed with the rest of a slot's signal state in [`scrub_thread_slot`], so
/// a recycled tid cannot inherit an interrupt aimed at its predecessor.
static THREAD_INTERRUPTED: [AtomicBool; MAX_THREADS] = {
    const INIT: AtomicBool = AtomicBool::new(false);
    [INIT; MAX_THREADS]
};

/// The pid of the exited, **not yet reaped** process that still owns each slot
/// (0 = none). While set, the slot is not reissued — see [`hold_slot_for_zombie`].
///
/// Deliberately NOT in [`scrub_thread_slot`]: it is not occupant state, it is
/// the claim that *prevents* a new occupant, and only the reap clears it.
static SLOT_HELD_BY: [AtomicU32; MAX_THREADS] = {
    const INIT: AtomicU32 = AtomicU32::new(0);
    [INIT; MAX_THREADS]
};

/// Keep `slot` out of circulation until `pid` is reaped.
///
/// A process's thread slot is its tid. If the slot is reissued while the
/// process is a zombie, the tid names two processes at once: the zombie still
/// records it (`Process::thread_id`, its `THREAD_PID_MAP` row) and the new
/// occupant runs on it, so a `kill`, `unregister` or `exit_group` aimed at the
/// zombie can land on the newcomer, and every `thread_id` table scan finds the
/// zombie (`[TRAMP-MISMATCH] … table scan found <dead pid>`). amd64 reissued a
/// slot the moment its last thread finished (`claim_slot` takes a
/// `TERMINATED` slot directly) — 2026-10-16, ryzen,
/// `docs/archive/AKUMA_STALE_TID_ZOMBIE_SLOT.md`. Like a Linux pid, the tid is
/// now not reused until the zombie is gone.
///
/// Call it on the dying thread **before** the slot leaves `RUNNING`: a slot is
/// only claimable once `TERMINATED`, so a hold set first can never race a claim.
/// The cost is one slot per unreaped zombie, which is what an unreaped zombie
/// costs on Linux too.
pub fn hold_slot_for_zombie(slot: usize, pid: u32) {
    if slot < MAX_THREADS && pid != 0 {
        SLOT_HELD_BY[slot].store(pid, Ordering::Release);
    }
}

/// `pid` was reaped: return every slot it held to circulation. Returns how many.
///
/// By pid, not by slot, so a path that already cleared the process's recorded
/// `thread_id` (the kill paths do) cannot strand a held slot. A linear pass over
/// atomics — no lock, no allocation.
pub fn release_slots_held_by(pid: u32) -> usize {
    if pid == 0 {
        return 0;
    }
    let mut released = 0;
    for held in &SLOT_HELD_BY {
        if held.compare_exchange(pid, 0, Ordering::AcqRel, Ordering::Relaxed).is_ok() {
            released += 1;
        }
    }
    released
}

/// The unreaped pid holding `slot`, or 0.
#[must_use]
pub fn slot_held_by(slot: usize) -> u32 {
    if slot < MAX_THREADS { SLOT_HELD_BY[slot].load(Ordering::Acquire) } else { 0 }
}

/// May a slot in `state`, with `on_cpu`, held by `held_by`, be given to a new
/// thread? `FREE`, or `TERMINATED` with no core still on its stack — and in both
/// cases not held by an unreaped zombie. The one rule [`claim_slot`] and the
/// x86 reaper both apply; pure so the host tests can pin it.
#[must_use]
pub fn slot_reusable(state: u8, on_cpu: u8, held_by: u32) -> bool {
    held_by == 0
        && (state == thread_state::FREE || (state == thread_state::TERMINATED && on_cpu == 0))
}

/// Raise [`THREAD_INTERRUPTED`] for `tid`. The bit half of
/// `akuma_exec::process::interrupt_thread`, which also sets the channel's own
/// flag for the readers that hold a channel directly.
pub fn set_thread_interrupted(tid: usize) {
    if tid < MAX_THREADS {
        THREAD_INTERRUPTED[tid].store(true, Ordering::Release);
    }
}

/// Atomically take (clear) `tid`'s interrupt bit.
///
/// **Consuming, like the flag it replaces.** `ProcessChannel::is_interrupted` is
/// a `swap(false)`, and the syscall prologue calls it once per excursion: an
/// interrupt is delivered to exactly one syscall and must not make every later
/// one return `EINTR` too.
pub fn take_thread_interrupted(tid: usize) -> bool {
    tid < MAX_THREADS && THREAD_INTERRUPTED[tid].swap(false, Ordering::AcqRel)
}

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
/// Called by `akuma_exec::process::raise_sigchld_for_parent` immediately before
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


/// x86_64: the identity lives in the target's per-CPU block, and the switch
/// already publishes it through [`ArchHooks::set_current_slot`] before
/// handing the stack over. Forwarded here too so the two spellings cannot
/// disagree — every AArch64 path that re-points the current thread has an
/// x86 counterpart that must do the same, and a silent no-op is how
/// `get_current_thread_register` came to read 0 for every thread.
#[cfg(target_os = "none")]
#[inline]
fn set_current_thread_register(tid: usize) {
    if let Some(h) = ARCH_HOOKS.get() {
        (h.set_current_slot)(tid);
    }
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
    // x86_64 has no per-thread register free for this. `current_tid` reads
    // `TPIDRRO_EL0`, which `akuma-cpu` correctly stubs to answer **0** off
    // AArch64 — so every thread on this target read as "thread 0", and
    // `schedule_blocking` parked the *boot* thread instead of its caller.
    // Caught by `amd64`'s own park self-test the first boot after the fold:
    // "the worker is parked" [FAIL] while the worker ran merrily on.
    //
    // The identity lives in the target's per-CPU block instead (`%gs`), which
    // is what [`ArchHooks::current_slot`] reads. `.get()` rather than
    // `require()`: this is called from diagnostics that can run before
    // registration, and answering 0 there is the same answer the register gave.
    match ARCH_HOOKS.get() {
        Some(h) => (h.current_slot)(),
        None => IDLE_THREAD_IDX,
    }
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
    THREAD_INTERRUPTED[i].store(false, Ordering::Release);
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
    akuma_primitives::preempt::clear_user_copy_fault_handler(i);

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
    (process().clear_draining)(i);

    // Profiler attribution: without this the new thread lends the previous occupant's
    // tag to any peer sampling this slot before its first kernel entry.
    #[cfg(kernel_smp_shared)]
    sync::reset_thread_tag(i);
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

/// The top of `slot`'s pool stack, allocated now if the slot has none (16-byte aligned).
///
/// `ArchHooks::prepare_task_slot` for a hooked kernel whose kernel stacks are this pool's —
/// the AArch64 root kernel under `hooked-sched` (amd64 keeps its own per-slot pair). The whole
/// stack is the thread's: the hooked path has no separate exception area, the EL0 trap frame
/// lands at the top. `None` when the slot is out of range or the PMM cannot back the stack.
pub fn ensure_pool_stack_top(slot: usize, size: usize) -> Option<usize> {
    if slot >= MAX_THREADS || !ensure_slot_stack(slot, size) {
        return None;
    }
    slot_stack_bounds(slot).map(|(base, size)| (base + size) & !0xF)
}

/// Adopt the context this core is running — a secondary's boot stack, `stack_base` and
/// `stack_size` bytes up — as core `core_id`'s idle thread, on the hooked path: the
/// [`adopt_current_as_core_idle`] of a kernel whose switch is a call.
///
/// The stack is recorded and its canary painted for the reason that function gives (the
/// pool's bounds are what `slot_stack_bounds` and the canary sweep read; the stack is
/// static `.bss`, so nothing else paints it). The thread register names the slot before
/// it is latched running. The caller marks the slot unpickable (its `can_run`) before
/// this core can first switch away from it. Call with interrupts masked, once per core.
#[cfg(target_os = "none")]
pub fn adopt_core_idle(
    core_id: usize,
    stack_base: akuma_primitives::addr::VirtAddr,
    stack_size: usize,
) -> Option<usize> {
    let slot = claim_slot()?;
    with_irqs_disabled(|| {
        POOL.lock().stacks[slot] = StackInfo::new(stack_base.get(), stack_size);
    });
    if config().enable_stack_canaries {
        init_stack_canary(stack_base.get());
    }
    register_core_idle(core_id, slot);
    (arch().set_current_slot)(slot);
    adopt_running_thread(slot);
    Some(slot)
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

/// How many times a thread has been marked `TERMINATED` by a thread other
/// than itself, since boot. The count behind the `[kill]` tracer in
/// [`mark_thread_terminated`], exposed so a boot self-test can assert a
/// teardown ran on the dying threads themselves rather than from a peer
/// (amd64's `kill_test`: on that target a cross-thread mark skips the
/// victim's own exit path and leaks what only that path releases).
#[must_use]
pub fn cross_thread_terminations() -> usize {
    CROSS_KILLS.load(Ordering::Relaxed)
}

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
        // Gated 2026-08-30. The 4096-line budget bounds the total but not the
        // rate: a guest `cargo` build spends it inside the first minute, so the
        // trace was both a flood during the interesting window and exhausted by
        // the time anything went wrong. `lifecycle_trace_on()` first, so the
        // atomic RMW below is skipped too when tracing is off.
        if (process().lifecycle_trace_on)()
            && TERM_TRACES.fetch_add(1, Ordering::Relaxed) < 4096
        {
            let loc = core::panic::Location::caller();
            let killer = get_current_thread_register();
            safe_print!(224,
                "[TERM] tid={} pid={:?} by_tid={} state={} pending_kill={} at={}:{}\n",
                idx, (process().pid_for_thread)(idx), killer,
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
            && let Some(victim_pid) = (process().find_pid_by_thread)(idx)
        {
            safe_print!(160,
                "[kill] tid={} (pid={}) terminated by tid={} (pid={}) victim_state={}\n",
                idx, victim_pid, killer,
                (process().find_pid_by_thread)(killer).unwrap_or(0),
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
        if let Some(purge) = SLOT_PURGE_CALLBACK.get() {
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
        let _ = THREAD_STATES[idx].try_update(
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
    // The trampoline is entered by `blr x19` / `jmp rbx` with the data pointer in the first argument
    // register; an `extern "C" fn()` ignores it. Passed as an address, so no fn-pointer type is forged.
    let trampoline_addr = trampoline_fn as usize as u64;

    // Without the pool lock: see the hooked arm's doc.
    let attempt = || with_irqs_disabled(|| ThreadPool::spawn_user_closure_initializing(trampoline_addr, data_ptr));

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
    /// Claim a slot for a user thread and leave it `INITIALIZING` — the
    /// x86_64 arm, and the read of this file that matters is that it is the
    /// **same three steps** as the AArch64 one above (claim, give it a stack,
    /// seed a context that enters `trampoline_fn(closure_ptr)`), with each
    /// step spelled in this target's terms.
    ///
    /// It returned `Err` until 2026-09-11, which failed every shared
    /// `fork`/`vfork`/`clone` at its spawn step. That was honest while nothing
    /// on this target reached the shared path; it is what
    /// `akuma_exec::process::fork_process` runs into first, so it is one half
    /// of what the ring-3 entry seam's last slice had to build.
    ///
    /// # Three differences from the AArch64 arm, all of them this target's
    ///
    /// * **No `self`, and it must be called without the pool lock.** The slot
    ///   table this claims from is the crate's lock-free `THREAD_STATES`
    ///   ([`claim_slot`]), so the lock buys nothing — and the stack comes
    ///   from [`ArchHooks::prepare_task_slot`], which on the AArch64 root
    ///   kernel under `hooked-sched` *is* this pool (`ensure_pool_stack_top`)
    ///   and takes `POOL` itself. It was a `&mut self` method called under the
    ///   lock until 2026-11-10, which deadlocked that kernel's first `spawn_fn`
    ///   on the non-reentrant spinlock; amd64's stacks are its own, so it never
    ///   showed there.
    /// * **Stacks come from the target** ([`ArchHooks::prepare_task_slot`]),
    ///   because `amd64` owns them per slot and reuses a recycled slot's pair.
    /// * **Exhaustion is final, and the error string says so.**
    ///   [`spawn_user_thread_initializing`] answers `"No free user thread
    ///   slots"` with a reclaim-and-retry pass; that pass exists to move
    ///   cooled-down `TERMINATED` slots to `FREE`, and [`claim_slot`]
    ///   already takes a `TERMINATED` slot no core is executing. There is
    ///   nothing left for it to free — the stack-return half is
    ///   `kernel_profile_extreme`, i.e. AArch64's — so a distinct string opts
    ///   out of a retry that could only fail again.
    pub fn spawn_user_closure_initializing(
        trampoline_addr: u64,
        closure_ptr: *mut (),
    ) -> Result<usize, &'static str> {
        let slot = claim_slot().ok_or("No free x86 task slots")?;

        let Some(stack_top) = (arch().prepare_task_slot)(slot) else {
            // `TERMINATED`, not `FREE` — the slot may already own one of the
            // two stacks, and `abandon_slot` is what says "reusable, but not
            // pristine". Same reasoning as `amd64::sched::abandon_unpublished`.
            abandon_slot(slot);
            return Err("Failed to allocate x86 task stacks");
        };

        // The slot is INITIALIZING, so no picker can select it and no switch can be reading its
        // context concurrently - the same obligation `seed_entry` documents, which this is the
        // closure-carrying variant of.
        akuma_switch::set(slot, akuma_switch::build_closure_context(stack_top, trampoline_addr, closure_ptr as u64));

        // NOTE: deliberately NOT published. The caller must `mark_thread_ready`,
        // exactly as on AArch64.
        Ok(slot)
    }
}

/// Release a slot that was claimed [`thread_state::INITIALIZING`] and will
/// never be published — the unwind half of
/// [`spawn_user_thread_initializing`].
///
/// It exists because publishing is no longer the only fallible thing that can
/// happen between claiming a slot and making it runnable: a target may need to
/// bind per-process state to the new thread first
/// (`ExecRuntime::bind_child_task`), and that can fail. Without this the slot
/// would sit `INITIALIZING` forever — invisible to the picker *and* to every
/// claim path, i.e. leaked in the one state nothing collects.
///
/// **The two architectures answer with different states, and both are right
/// for their own claim path.** AArch64 stores `FREE`, matching what
/// `spawn_user_closure_initializing` already does when its own stack
/// allocation fails: the stack went back to the PMM, so the slot is pristine.
/// x86_64 stores `TERMINATED` ([`abandon_slot`]), because on that target the
/// slot owns two leaked 32 KiB stacks that its next occupant reuses — handing
/// it back as pristine would leak the pair — and [`claim_slot`] treats a
/// `TERMINATED` slot no core is executing as claimable anyway.
///
/// Off both kernels this is a no-op: there is no slot table on a host.
pub fn release_initializing_thread(tid: usize) {
    #[cfg(target_os = "none")]
    abandon_slot(tid);
    #[cfg(not(target_os = "none"))]
    {
        let _ = tid;
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
/// Is the thread we are running on already TERMINATED?
///
/// True at the terminal teardown sites (`return_to_kernel`,
/// `return_to_kernel_from_fault`, `exit_group`'s pre-park drain), which all call
/// `mark_current_terminated()` before they drain. Such a thread can be reaped at
/// any yield and will never resume, so it must not *perform* deferred reclamation
/// — only request it. See `akuma_mmu::drain_pending_ttbr_frees` and
/// `docs/archive/SELFHOST_KERNEL_HEAP_LEAK.md`.
pub fn current_thread_is_terminated() -> bool {
    is_thread_terminated(current_thread_id())
}

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

/// Slots the collector has carried to `FREE` since boot, on the hooked path (where every
/// core's idle thread collects, so a self-test cannot assume its own `cleanup_terminated*`
/// call is the one that recycles its thread).
static COLLECTED: AtomicU64 = AtomicU64::new(0);

/// See [`COLLECTED`].
#[must_use]
pub fn slots_collected() -> u64 {
    COLLECTED.load(Ordering::Relaxed)
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

        // A terminated thread's *last* act can be a retired-process sweep
        // (`akuma_exec::process::reclaim::drain_retired`), and that sweep runs
        // on this stack. Don't free it underneath one.
        //
        // With the sweep pinned preempt-off this cannot actually fire — a
        // drainer mid-sweep is on-CPU, which the gate above already excludes —
        // so this is the same belt the `ON_CPU` check is. It is here so the
        // invariant lives with the reaper rather than depending on every future
        // drain site remembering to pin itself.
        if (process().drain_in_flight)(i) {
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
                akuma_switch::set(i, Context::zero());
                
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
            // Same for a stale interrupt: it would make the new occupant's first
            // blocking syscall return EINTR for a signal aimed at a dead thread.
            THREAD_INTERRUPTED[i].store(false, Ordering::Release);
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
            if let Some(cb) = CLEANUP_CALLBACK.get() {
                cb(i);
            }

            // Drop per-tid registrations held by subsystems outside this crate before the
            // slot can be re-claimed — otherwise the next occupant inherits them under the
            // same tid. Same lock context as the callback above (none of this module's
            // locks held), so the hook is free to take its own.
            if let Some(purge) = SLOT_PURGE_CALLBACK.get() {
                purge(i);
            }
            
            // Clear any dropped-BKL window this slot's late occupant left open. A
            // thread killed while parked inside a converted (BKL-opted-out) syscall
            // never reaches its window close — the ledger is tid-indexed, so the next
            // occupant of this slot would inherit the depth and run its EL1 excursions
            // BKL-free until the EL0-entry tripwire healed it. Must happen before the
            // slot goes FREE, i.e. before any spawn can claim it.
            let _stale_depth = bkl::clear_dropped_windows_for_dead_thread(i);

            // Final scrub before the slot becomes claimable. The individual clears above
            // stay because some are order-sensitive (CURRENT_TRAP_FRAME must precede the
            // stack free); this is the catch-all that keeps the recycler's list from
            // drifting away from the claim paths' again.
            scrub_thread_slot(i);

            // Release anything this thread died holding. Last thing before the slot
            // becomes claimable, which is exactly the reap contract: the tid is dead,
            // and no new occupant can have taken it yet. See SLOT_REAP_CALLBACK.
            if let Some(reap) = SLOT_REAP_CALLBACK.get() {
                reap(i);
            }

            // NOW set to FREE - cleanup is complete, spawn can safely claim this slot
            THREAD_STATES[i].store(thread_state::FREE, Ordering::SeqCst);
            COLLECTED.fetch_add(1, Ordering::Relaxed);
            
            // Gated as a LIFECYCLE trace (2026-08-29). This fires once per thread
            // recycle, and a thread-heavy workload — an in-VM `-j4` build, say —
            // recycles constantly, so it was ~70 bytes of console per recycle at
            // ~2.4 us/byte (docs/archive/CONSOLE_LOG_COST.md). Exactly the class
            // `syscall_debug_info_enabled` already covers: its own doc records
            // the `[FORK-DBG]`/`[TRAMP]` traces costing ~20 serial lines per
            // `fork()` for the same reason. A recycle is a lifecycle event.
            if (process().lifecycle_trace_on)() {
                safe_print!(128, "[Cleanup] Thread {} recycled after {}us cooldown\n", i, cooldown);
            }
            
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


/// x86_64 stub — real shared-kernel SMP is out of scope for this pass (see
/// `proposals/AKUMA_THREADING_ARCH_PORTABILITY.md`); nothing calls this here.
#[cfg(target_os = "none")]
pub fn adopt_current_as_core_idle(
    _core_id: usize,
    _exc_stack_top: u64,
    _stack_base: usize,
    _stack_size: usize,
) -> Option<usize> {
    None
}

// ============================================================================
// Thread Constants
// ============================================================================

// The thread-entry assembly (`thread_start`, `thread_start_closure`) lives in `akuma-switch`.

// ============================================================================
// The context switch — a function call, on both architectures
// ============================================================================
//
// The switch saves the callee-saved register file on the outgoing thread's own
// stack and swaps stack pointers (`proposals/AKUMA_THREADING_ARCH_PORTABILITY.md`).
// Both kernels schedule through it: amd64 since the port, and AArch64 since the
// SGI/fake-IRQ-frame switch was deleted (`docs/archive/AKUMA_MULTIARCH_ATTEMPT_0.md`).
// The machine effects each target performs around it are [`ArchHooks`].
// The switch and the first-run trampoline (`akuma_threading_x86_switch_context`,
// `akuma_threading_x86_thread_entry_trampoline`) live in `akuma-switch`.

// The never-run thread's first frame (`build_closure_context`, seven words: `r15 r14 r13 r12 rbx rbp rflags` and
// the return address) is written by `akuma-switch`.

/// Simple round-robin pick of the next `READY`/`RUNNING` slot after
/// `from`, wrapping. Deliberately not [`ThreadPool::schedule_indices`] —
/// that method's SMP/priority/network-thread-boost logic is real scheduling
/// policy for the AArch64 multi-core scheduler this pass does not attempt to
/// match; this mirrors `amd64::sched::yield_now`'s own plain scan instead,
/// which is exactly what a single-core, cooperative-only switch needs.
fn pick_next(from: usize, hooks: ArchHooks) -> Option<usize> {
    for step in 1..=MAX_THREADS {
        let candidate = (from + step) % MAX_THREADS;
        let state = THREAD_STATES[candidate].load(Ordering::Acquire);
        if state != thread_state::READY && state != thread_state::RUNNING {
            continue;
        }
        // Not already executing on some other core — two cores must never
        // resume one stack — and allowed here: the boot thread is pinned to the
        // boot core, and each idle thread to its own, which `can_run` answers.
        if ON_CPU[candidate].load(Ordering::Acquire) != 0 {
            PICK_SKIP_ONCPU[candidate].fetch_add(1, Ordering::Relaxed);
            continue;
        }
        if !(hooks.can_run)(candidate) {
            PICK_SKIP_CANRUN[candidate].fetch_add(1, Ordering::Relaxed);
            continue;
        }
        // **The gate is the claim** (2026-09-20). Switches no longer hold the
        // BKL (prev-handoff above; parks release across the sleep), so two
        // cores can scan the same READY candidate concurrently — the old
        // "picks were serialized by the caller's lock" safety is gone. The
        // 0→1 CAS makes exactly one scanner the winner; the loser moves on.
        // This also closes a pre-existing race: a task readied by its waker
        // (state RUNNING, gate 0, still off-CPU) was pickable on two cores at
        // once under the old scan-then-return.
        if ON_CPU[candidate]
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Relaxed)
            .is_err()
        {
            PICK_SKIP_ONCPU[candidate].fetch_add(1, Ordering::Relaxed);
            continue;
        }
        PICK_HITS[candidate].fetch_add(1, Ordering::Relaxed);
        return Some(candidate);
    }
    None
}

/// Would [`pick_next`] find something for this core? The same three tests,
/// without the claim and without switching.
///
/// For an idle core that has just announced it is about to halt (amd64
/// `smp::set_halted`): a wake that landed between its last pick and the
/// announcement is visible here, so it does not sleep through it.
#[must_use]
pub fn runnable_exists() -> bool {
    let hooks = arch();
    (0..MAX_THREADS).any(|i| {
        let state = THREAD_STATES[i].load(Ordering::SeqCst);
        (state == thread_state::READY || state == thread_state::RUNNING)
            && ON_CPU[i].load(Ordering::Acquire) == 0
            && (hooks.can_run)(i)
    })
}

/// May the scheduler tick switch away from the kernel code it interrupted?
///
/// `akuma_exceptions::irq_hooked` asks before it yields from an interrupt that landed in
/// EL1, and amd64's `sched::ring0_preemptible` before it switches ring 0. Interrupts are
/// masked (the caller is an IRQ handler).
///
/// - **Preemption not disabled** for the running thread (`PreemptGuard`,
///   `disable_preemption`).
/// - **`POOL` is free.** It is a plain spinlock that does not mask interrupts, and the
///   switch takes it for the CPU-time bill: the interrupted code may be holding it,
///   and then the switch would spin on this core's own hold. A peer's hold skips the
///   tick too, which costs one tick.
/// - **The thread is `RUNNING`.** A thread that has just published `WAITING` or
///   `TERMINATED` is on its way into its own switch, and taking the core first would
///   skip what it does in between (`finish_current`'s futex purge, a park's
///   re-check). It gets there within a few instructions.
#[must_use]
pub fn el1_preemptible() -> bool {
    if is_preemption_disabled() || POOL.is_locked() {
        return false;
    }
    let cur = (arch().current_slot)();
    cur < MAX_THREADS && THREAD_STATES[cur].load(Ordering::Acquire) == thread_state::RUNNING
}

/// The machine effects the scheduler cannot perform itself.
///
/// # Why hooks and not code in this crate
///
/// Everything here is per-core CPU state that only the *target* knows how to
/// touch: on amd64 `CR3`, the ring-3 trap stack in the TSS, the
/// `IA32_FS_BASE`/`GS_BASE` MSRs, the `fxsave` area; on AArch64 `TTBR0_EL1`,
/// `TPIDRRO_EL0` and `TPIDR_EL0` (`akuma-kernel-glue`'s `hooked` module); on
/// both, the Big Kernel Lock's recursion depth and which slot this core is
/// running. The crate builds for both targets without naming any of it.
///
/// The split is the same one [`ThreadRuntime`] and [`ProcessHooks`] already
/// make — the scheduler decides, the caller performs — applied to the layer
/// below them rather than the layer above.
///
/// Registered once by `amd64::sched::init` via [`register_arch_hooks`].
/// **Every field is load-bearing on a switch**; a missing one is not a
/// degraded scheduler but a corrupt one, which is why this is a struct with no
/// defaults rather than a set of optional setters.
#[derive(Clone, Copy)]
pub struct ArchHooks {
    /// Install `to`'s machine state on this core, called **immediately before**
    /// the stack switch and with `from` still current.
    ///
    /// The order matters and belongs to the caller: `CR3` must be written
    /// before the switch (every address space shares the kernel's upper half,
    /// so the stack stays mapped across it), and the `fxsave` of `from` must
    /// happen before the `fxrstor` of `to`.
    pub switch_to: fn(from: usize, to: usize),
    /// The slot this core is running.
    ///
    /// Not a `static` in this crate: with SMP there is one per core, and the
    /// target already keeps a per-CPU block (`%gs` on x86_64). A single static
    /// here is what limited the previous version of this path to one core.
    pub current_slot: fn() -> usize,
    /// Record the slot this core is now running. Called by the switch, before
    /// it hands the stack over.
    pub set_current_slot: fn(usize),
    /// The slot this core runs when nothing else can run: its idle thread.
    ///
    /// Consulted only when the running thread *cannot continue* (it parked, or
    /// it finished) and the picker found nothing. A core must always have
    /// somewhere to go.
    pub idle_slot: fn() -> usize,
    /// May this core run `slot`? Pinning — the boot thread is pinned to the
    /// boot core and every idle thread to its own.
    pub can_run: fn(slot: usize) -> bool,
    /// Hand the kernel lock's recursion depth from the outgoing thread to the
    /// incoming one. The lock itself stays with the core across a switch; only
    /// the depth is per-thread.
    pub transfer_lock_depth: fn(from: usize, to: usize),
    /// Open a one-instruction interrupt window, then mask again.
    ///
    /// The clock is the timer tick, a syscall runs with `IF` clear, and the
    /// only place the scheduler re-enables it is an idle loop the picker
    /// reaches only when nothing else is runnable. So a thread parked with a
    /// deadline, on a core whose other threads are all also in the kernel,
    /// would wait on a **frozen clock** — measured on this target 2026-09-06,
    /// where a 400 ms `FUTEX_WAIT` never returned. This is the escape.
    ///
    /// It is emphatically **not** `akuma_cpu::park::wfi`, which is `hlt` on
    /// x86_64: halting with interrupts masked halts forever.
    pub allow_tick: fn(),
    /// Write a child's ring-3 register file into `slot`'s machine state — the
    /// x86_64 half of [`update_thread_context`].
    ///
    /// The AArch64 side of that function writes the fake IRQ-return frame it
    /// built on the new thread's kernel stack, which is state this crate owns.
    /// x86_64 has no such frame: its ring-3 register file lives in
    /// `amd64::usermode::UserCtx`, one per task slot, written by the
    /// `syscall_entry` assembly and read by the `sysret` on the way out. That
    /// table is in `amd64::sched`'s own per-slot side array and this crate
    /// cannot name it, so the write is an effect like `switch_to` and belongs
    /// here.
    ///
    /// `slot` is always [`thread_state::INITIALIZING`] when this is called —
    /// the child is not schedulable yet, which is what makes writing another
    /// slot's machine state safe at all.
    pub write_user_context: fn(slot: usize, ctx: &UserContext),
    /// Read `slot`'s ring-3 register file back out — the x86_64 half of
    /// [`get_saved_user_context`], and the exact mirror of
    /// [`write_user_context`] above.
    ///
    /// `None` means "there is no user context here": the slot has never
    /// trapped in from ring 3, so its `user_rip`/`user_rsp` are still zero.
    /// That is the same refusal the AArch64 side makes when a thread has no
    /// live EL0 trap frame, and for the same reason — a child built from a
    /// slot that never entered ring 3 would be launched at whatever the
    /// registers happened to hold, which is silent. Failing the syscall is
    /// loud, local and recoverable.
    ///
    /// Unlike its writer this may be called on the **running** slot: `fork`
    /// reads its own caller's capture, which the `syscall_entry` assembly
    /// wrote on the way in and nothing rewrites until the way out.
    pub read_user_context: fn(slot: usize) -> Option<UserContext>,
    /// Give a freshly [`claim_slot`]ed slot its kernel and trap stacks and
    /// its default machine state, and answer the top of the kernel stack.
    ///
    /// Stacks are the *target's* on this architecture — `amd64` leaks a pair
    /// per slot on first use and a recycled slot reuses the pair it already
    /// owns — where AArch64 takes them from this crate's own PMM-backed pool.
    /// So the crate can claim a slot and seed a context into it, but it cannot
    /// make one somewhere to stand: that is this hook.
    ///
    /// `None` means the stacks could not be allocated; the caller abandons the
    /// slot. Every other field the picker or the switch reads is reset here to
    /// its default, so a recycled slot cannot inherit the previous occupant's
    /// `space_root`, `pinned` core or saved `UserCtx`.
    pub prepare_task_slot: fn(slot: usize) -> Option<usize>,
}

static ARCH_HOOKS: Registered<ArchHooks> =
    Registered::new("akuma-threading: ArchHooks not registered — call amd64::sched::init()");

/// Register the x86_64 machine effects. Called once, early, from
/// `amd64::sched::init` — before any thread but the boot thread exists.
pub fn register_arch_hooks(h: ArchHooks) {
    ARCH_HOOKS.register(h);
}

#[inline]
fn arch() -> ArchHooks {
    ARCH_HOOKS.require()
}

// ── the spawn/adopt surface: claim, seed, publish ──
//
// `amd64`'s entries are plain `extern "C" fn() -> !` with their per-thread data
// in a side table; `spawn_fn`/`spawn_system_thread_fn` box a closure. Both end
// here: the narrow, allocation-free claim/seed/publish steps over one state
// machine, so both architectures share `THREAD_STATES`, `WAKE_TIMES`,
// `WOKEN_STATES`, `ON_CPU` and the wake/park discipline built on them.


/// Register the thread this core is already executing as `slot`.
///
/// The boot thread on the boot core, and each secondary's idle thread: contexts
/// that exist before the scheduler does, and whose [`Context`] is filled in by
/// the first switch *away* from them rather than by a seed.
///
/// `ON_CPU` is latched here, not later: the thread is running right now, and a
/// peer core that picked it up between this call and its first switch would
/// resume a stack that is in use.
pub fn adopt_running_thread(slot: usize) {
    if slot >= MAX_THREADS {
        return;
    }
    THREAD_STATES[slot].store(thread_state::RUNNING, Ordering::SeqCst);
    ON_CPU[slot].store(1, Ordering::SeqCst);
    WAKE_TIMES[slot].store(0, Ordering::SeqCst);
    WOKEN_STATES[slot].store(false, Ordering::SeqCst);
    // Give this thread a `start_time_us` from the moment it becomes the
    // running one, same reason as the stamp in `try_yield`: without it, a
    // thread that is never switched OUT before something reads its CPU time
    // (or whose first switch-out would otherwise bill from a stale/zero
    // start) undercounts its very first slice.
    POOL.lock().slots[slot].start_time_us = (runtime().uptime_us)();
    // No generation bump: an adopted thread is a boot or idle thread, and
    // [`SLOT_GEN`]'s own contract is that slots which never recycle stay at
    // generation 0 forever, so a handle minted for one always validates.
}

/// Claim a slot and leave it [`thread_state::INITIALIZING`] — visible to
/// nothing until [`publish_slot`].
///
/// The gap between claiming and publishing is not an implementation detail, it
/// is the point: `amd64` has been bitten twice by a task becoming schedulable
/// before its caller had finished describing it (a `space_root` still 0, so the
/// first `sysret` fetched an unmapped entry point; a `PROCS` index not yet
/// seeded). With a second core the window is not a tick wide but zero
/// instructions wide.
///
/// Returns `None` when every slot is taken. Slots are reused: a `TERMINATED`
/// one that no core is executing is as good as a `FREE` one, which is what
/// stops [`MAX_THREADS`] being a ceiling on the number of processes a boot may
/// ever run rather than on how many are alive at once.
pub fn claim_slot() -> Option<usize> {
    for slot in 1..MAX_THREADS {
        let state = THREAD_STATES[slot].load(Ordering::Acquire);
        if !slot_reusable(state, ON_CPU[slot].load(Ordering::Acquire), slot_held_by(slot)) {
            continue;
        }
        // `collector-recycles`: a `TERMINATED` slot still carries a dead thread whose
        // kernel-side teardown is the collector's cleanup callback (the AArch64 root
        // kernel's `on_thread_cleanup`: its tid -> pid row, its process's unregister).
        // Claimed from here, the next occupant inherits that identity — measured: a
        // kernel thread's futex keyed on a dead process's tgid, its wake lost.
        if cfg!(feature = "collector-recycles") && state != thread_state::FREE {
            continue;
        }
        if THREAD_STATES[slot]
            .compare_exchange(
                state,
                thread_state::INITIALIZING,
                Ordering::AcqRel,
                Ordering::Relaxed,
            )
            .is_ok()
        {
            // A recycled slot must not inherit the previous occupant's
            // per-slot state, and not just its wait state: `scrub_thread_slot`
            // clears the pending-signal bitmask, the blocked mask, the
            // sigaltstack and the itimer deadlines, every one of which fired
            // against the *new* occupant when left stale. The measured case
            // (2026-09-12): a group-fatal'd thread's slot — one bit of pending
            // `SIGSEGV` set — was recycled by an innocent `futextest`
            // spawn/join thread, whose first syscall return then killed it
            // with the dead process's signal. Only `ON_CPU` is kept below,
            // because this claim path owns it directly.
            scrub_thread_slot(slot);
            // Drop per-tid registrations held outside this crate — on x86_64
            // that is the futex waiter table — before the slot can be used.
            //
            // **This is the x86 half of a hook that had no x86 half at all.**
            // The other kernel reaches [`SLOT_PURGE_CALLBACK`] twice: from
            // `mark_thread_terminated`, and from the slot recycler. This target
            // uses neither — it claims a `TERMINATED` slot here, directly — so
            // until 2026-09-18 nothing ever purged, and a thread that died
            // without running `amd64::thread::teardown` (a fault kill, a
            // group-fatal signal, a deferred kill that never reached its
            // boundary) left its tid queued on a futex key forever. The next
            // occupant of the slot inherits it, and a `FUTEX_WAKE(uaddr, 1)`
            // then pops the *stale* entry, counts it toward `max_wake` and
            // leaves the real waiter parked — the lost wakeup behind the `-j4`
            // wedge (`AKUMA_AMD64_SELFHOST_BUILD_SLOWNESS.md` §13). The same
            // trap as `amd64::thread::teardown`'s `set_cleanup_callback` note,
            // which reasoned about that hook and not about this one.
            //
            // Holds none of this module's locks here, so the hook may take its
            // own — the same context the two AArch64 sites document.
            //
            // Since the reaper landed this is [`release_dead_slot`] — the
            // futex purge plus the two releases that had no x86 caller at all
            // (the BKL dropped-window ledger and the orphaned-lock reap). It
            // runs here as well as in [`reap_terminated_slots`] because the
            // sweep is a *timeliness* mechanism and this is the *correctness*
            // one: this is the instant a new occupant arrives, so whatever the
            // last one left must be gone by now whether or not any collector
            // happened to run. After a reap the slot is already `FREE` and
            // every release is a no-op.
            release_dead_slot(slot);
            WAKE_TIMES[slot].store(0, Ordering::SeqCst);
            WOKEN_STATES[slot].store(false, Ordering::SeqCst);
            ON_CPU[slot].store(0, Ordering::SeqCst);
            // The picker tallies are per *occupant*, not per slot. Without this
            // they accumulate across every rebirth, and the one question they
            // exist to answer — "was this thread ever picked?" — comes back
            // with the previous occupant's answer. Caught on the instrument's
            // first boot: three idle threads reported syscalls they had not
            // made, because low slots are recycled by the boot self-tests
            // before SMP bring-up claims them.
            PICK_HITS[slot].store(0, Ordering::Relaxed);
            PICK_SKIP_ONCPU[slot].store(0, Ordering::Relaxed);
            PICK_SKIP_CANRUN[slot].store(0, Ordering::Relaxed);
            // The generation is what makes a `WakeHandle` for the *previous*
            // occupant inert. Bumped here — under the winning CAS, before
            // anything can hold a handle to the new occupant — which is the
            // same "exactly one bumper per rebirth" rule `scrub_thread_slot`
            // follows on the AArch64 claim paths. Without it a futex or pipe
            // waiter that outlived its thread would spend its wake on whoever
            // inherits the slot.
            SLOT_GEN[slot].fetch_add(1, Ordering::AcqRel);
            return Some(slot);
        }
    }
    None
}

/// Build `slot`'s initial context so the first switch into it enters `entry`.
///
/// `stack_top` is the caller's: on this target `amd64` owns thread stacks (it
/// leaks them lazily per slot and reuses them on recycle), where AArch64 takes
/// them from the crate's own PMM-backed pool. The scheduler does not care which
/// — it only needs somewhere to push a frame.
pub fn seed_entry(slot: usize, stack_top: usize, entry: extern "C" fn() -> !) {
    if slot >= MAX_THREADS {
        return;
    }
    // The slot is INITIALIZING, so no picker can select it and no switch can be reading its context
    // concurrently. `entry` is the trampoline itself (`jmp rbx`/`br x20`), entered with nothing to free.
    akuma_switch::set(slot, akuma_switch::build_closure_context(stack_top, entry as usize as u64, 0));
}

/// Make an [`claim_slot`] slot schedulable.
///
/// Separate from claiming so that forgetting it is a thread that never runs —
/// loud — rather than one that runs too early.
pub fn publish_slot(slot: usize) {
    if slot < MAX_THREADS {
        THREAD_STATES[slot].store(thread_state::READY, Ordering::Release);
    }
}

/// Release a claimed slot that will never be published.
///
/// `TERMINATED`, not `FREE`: on this target a slot owns stacks its next
/// occupant reuses, and the difference is visible to the caller's own
/// bookkeeping. [`claim_slot`] treats the two alike.
pub fn abandon_slot(slot: usize) {
    if slot < MAX_THREADS {
        THREAD_STATES[slot].store(thread_state::TERMINATED, Ordering::Release);
    }
}

/// Reserve a slot number for an identity that is not a pool thread.
///
/// The M2 spawn crossing (taoxichuan) runs its child on the crossing's own
/// band and keys its ring records on the spawn-ring slot index — a number
/// the fork path ALSO hands out as a threading tid. Without a reservation
/// the pool's `claim_free_slot` sees the number FREE and gives it to the
/// next fork child, which then stomps the live shell's cross record, SpawnSlot
/// and fd table (boot ph-2: `bind tid=2 shell=2`, echo's EPIPE, cat parked
/// forever on a destroyed pipe). `INITIALIZING` is never picked by a scheduler
/// and never reusable; the reserve is permanent for this boot, which is the
/// point — a crossing identity is not a thread that exits.
pub fn reserve_slot(slot: usize) -> bool {
    if slot >= MAX_THREADS {
        return false;
    }
    // A TERMINATED slot is equally takeable: its thread is dead by
    // definition, and the fork path's own recycler would otherwise hand the
    // number right back out from under the crossing (boot ph-4: session 3's
    // reserve of ring slot 3 refused against session 2's TERMINATED child,
    // the fork claim reclaimed it, and the session shell collided with its
    // own first child again).
    for state in [thread_state::FREE, thread_state::TERMINATED] {
        if THREAD_STATES[slot]
            .compare_exchange(
                state,
                thread_state::INITIALIZING,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_ok()
        {
            return true;
        }
    }
    false
}

/// Mark the running thread terminated and switch away for good.
///
/// Never returns: the picker only takes `READY`/`RUNNING`, so once the state is
/// published this thread is never chosen again. The loop exists because
/// [`yield_now`] can legitimately decline to switch (nothing else runnable yet)
/// and this thread must not run on regardless.
pub fn finish_current() -> ! {
    let slot = (arch().current_slot)();
    THREAD_STATES[slot].store(thread_state::TERMINATED, Ordering::SeqCst);
    WAKE_TIMES[slot].store(0, Ordering::SeqCst);
    // Drop this tid's futex registrations **now**, not when something next
    // wants the slot. This is the x86 counterpart of the purge
    // [`mark_thread_terminated`] performs at the same moment, and it exists for
    // the same reason its comment gives: for as long as a dead tid sits in
    // `FUTEX_WAITERS` it is still a wake target, so a `FUTEX_WAKE(uaddr, 1)`
    // pops the corpse, counts it toward `max_wake` and leaves the live waiter
    // parked. Until this line the earliest purge on this target was
    // [`claim_slot`]'s, i.e. "whenever someone happens to want this slot" —
    // short under a build, unbounded on an idle box
    // (`AKUMA_AMD64_NO_SLOT_RECYCLER.md` §3.2).
    //
    // Safe this early for the reason the AArch64 site states: a queue entry is
    // of no further use to a terminated thread. Unlike its trap frame or its
    // stacks, which the `yield_now` loop below still touches and which
    // therefore stay until the slot is claimed or reaped.
    if let Some(purge) = SLOT_PURGE_CALLBACK.get() {
        purge(slot);
    }
    loop {
        yield_now();
        (arch().allow_tick)();
    }
}

/// How many slots [`reap_terminated_slots`] has carried to `FREE`.
static X86_REAPS: AtomicU64 = AtomicU64::new(0);

/// Total slots reaped since boot. Zero on a box that has run user processes
/// means no collector is calling [`reap_terminated_slots`].
#[must_use]
pub fn reaps() -> u64 {
    X86_REAPS.load(Ordering::Relaxed)
}

/// Release what a dead slot's occupant left behind, and carry the slot
/// `TERMINATED → FREE`.
///
/// # The transition this restores
///
/// On AArch64 a slot is cleaned when its thread dies; on x86_64 it was cleaned
/// when the slot was next *claimed*, and `TERMINATED → FREE` never happened at
/// all ([`claim_slot`] takes a `TERMINATED` slot directly). Everything that
/// is purely per-slot state survived that — the claim scrubs it — but the two
/// hooks that have to fire *while the tid is dead and its slot not yet
/// reissued* had no x86 caller at all, and neither did the BKL ledger reset.
/// See `docs/archive/AKUMA_AMD64_NO_SLOT_RECYCLER.md`.
///
/// What it does, in the shared recycler's order:
///
/// - **[`bkl::clear_dropped_windows_for_dead_thread`].** The ledger is
///   tid-indexed. A thread killed between a `dropped_window_open` and its close
///   leaves the depth standing and the next occupant of the slot inherits it —
///   running its kernel excursions BKL-free while believing it holds the lock.
///   `amd64` opens windows in `net.rs` and `exec_runtime.rs`, so this is not
///   hypothetical here.
/// - **`SLOT_PURGE_CALLBACK`.** Idempotent, and already fired at death by
///   [`finish_current`]; repeated here to cover a slot that reached
///   `TERMINATED` by some other route.
/// - **`SLOT_REAP_CALLBACK`.** Orphaned-lock recovery: release whatever this
///   thread died holding. Last, which is exactly the reap contract — the tid is
///   dead and no new occupant can have taken it yet.
///
/// # Why no cooldown
///
/// The shared recycler's `thread_cleanup_cooldown_us` exists to guarantee the
/// thread has left its own kernel stack. `ON_CPU` establishes that directly and
/// this target already gates on it (the same test [`claim_slot`] makes), so
/// a settling time would only add latency.
///
/// # Racing the claim
///
/// The `TERMINATED → INITIALIZING` CAS is the arbiter on both sides: a claim
/// that wins the slot skips it here, and a reap that wins it makes the claim
/// look elsewhere. `FREE` is what makes the pass idempotent — without it every
/// sweep would re-run the hooks over every dead slot until something reused it.
/// `claim_slot` treats `FREE` and `TERMINATED` alike, and `slot_can_install`'s
/// `space_root` carve-out covers both, so nothing downstream can tell.
///
/// Returns how many slots it moved.
pub fn reap_terminated_slots() -> usize {
    let mut count = 0;
    for slot in 1..MAX_THREADS {
        if THREAD_STATES[slot].load(Ordering::Acquire) != thread_state::TERMINATED {
            continue;
        }
        // The core that switched away may still be executing on this slot's
        // kernel stack. Don't reap under it; the next pass gets it. And a slot
        // an unreaped zombie still holds is not dead yet (`hold_slot_for_zombie`).
        if !slot_reusable(
            thread_state::TERMINATED,
            ON_CPU[slot].load(Ordering::Acquire),
            slot_held_by(slot),
        ) {
            continue;
        }
        if THREAD_STATES[slot]
            .compare_exchange(
                thread_state::TERMINATED,
                thread_state::INITIALIZING,
                Ordering::AcqRel,
                Ordering::Relaxed,
            )
            .is_ok()
        {
            release_dead_slot(slot);
            THREAD_STATES[slot].store(thread_state::FREE, Ordering::Release);
            X86_REAPS.fetch_add(1, Ordering::Relaxed);
            count += 1;
        }
    }
    count
}

/// The three releases [`reap_terminated_slots`] performs, factored out so
/// [`claim_slot`] can perform them too.
///
/// A collector is a timeliness mechanism, not a correctness one: whether it ran
/// depends on whether anything called it. A claim is the one moment where
/// skipping these is guaranteed to hurt, because that is when the next occupant
/// arrives — so the claim does them itself rather than trusting the sweep.
/// Running both is free: each is idempotent, and after a reap the slot is
/// `FREE` with nothing left to release.
///
/// Called with the slot `INITIALIZING` under the caller's winning CAS, so no
/// other core can be claiming or reaping it, and with none of this module's
/// locks held — the hooks may take their own.
fn release_dead_slot(slot: usize) {
    let _stale_depth = bkl::clear_dropped_windows_for_dead_thread(slot);
    if let Some(purge) = SLOT_PURGE_CALLBACK.get() {
        purge(slot);
    }
    if let Some(reap) = SLOT_REAP_CALLBACK.get() {
        reap(slot);
    }
}

/// Is `slot` occupied by a thread that could still run?
///
/// `INITIALIZING`, `READY`, `RUNNING` and `WAITING` are all alive; `FREE` and
/// `TERMINATED` are not. Asked as "alive", not "not `READY`" — a thread parked
/// on a pipe is as live as one spinning on it, and the boot's drive loop must
/// not mistake the two.
#[must_use]
pub fn slot_is_live(slot: usize) -> bool {
    slot < MAX_THREADS
        && !matches!(
            THREAD_STATES[slot].load(Ordering::Acquire),
            thread_state::FREE | thread_state::TERMINATED
        )
}

/// Is `slot` parked in [`schedule_blocking`]? For diagnostics and self-tests.
#[must_use]
pub fn slot_is_waiting(slot: usize) -> bool {
    slot < MAX_THREADS && THREAD_STATES[slot].load(Ordering::Acquire) == thread_state::WAITING
}

/// Per-slot tally of [`pick_next`] outcomes: skipped because another core
/// holds the on-CPU gate, skipped because [`ArchHooks::can_run`] said no,
/// and returned as the pick.
///
/// # Why per-slot, and why all three
///
/// A thread that is created and never scheduled is invisible from outside: it
/// looks the same whether the picker never *considered* it (its state is not
/// `READY`), considered and rejected it for a gate a peer core left set, or
/// rejected it for pinning. Those are three unrelated bugs. A global counter
/// says a skip happened; only a per-slot one says *which slot* is the one
/// nothing will ever run, which is the whole question
/// (`docs/archive/AKUMA_AMD64_SELFHOST_BUILD_SLOWNESS.md` §12).
///
/// `PICK_HITS` is the control: a slot with zero hits and a large
/// `PICK_SKIP_ONCPU` is a leaked gate; zero of both means the scan never got
/// past the state test and the fault is upstream of the picker entirely.
///
/// `u64` rather than `u32` on purpose — a wrapped counter reads as "never
/// skipped", which is exactly the answer being hunted. Relaxed: these are read
/// long after the fact by a diagnostic, never to make a decision.
static PICK_SKIP_ONCPU: [AtomicU64; MAX_THREADS] = {
    const INIT: AtomicU64 = AtomicU64::new(0);
    [INIT; MAX_THREADS]
};

/// See [`PICK_SKIP_ONCPU`].
static PICK_SKIP_CANRUN: [AtomicU64; MAX_THREADS] = {
    const INIT: AtomicU64 = AtomicU64::new(0);
    [INIT; MAX_THREADS]
};

/// See [`PICK_SKIP_ONCPU`].
static PICK_HITS: [AtomicU64; MAX_THREADS] = {
    const INIT: AtomicU64 = AtomicU64::new(0);
    [INIT; MAX_THREADS]
};

/// `(picked, skipped for the on-CPU gate, skipped for pinning)` for `slot`.
/// Diagnostics only; see [`PICK_SKIP_ONCPU`] for how to read the triple.
#[must_use]
pub fn slot_pick_counts(slot: usize) -> (u64, u64, u64) {
    if slot >= MAX_THREADS {
        return (0, 0, 0);
    }
    (
        PICK_HITS[slot].load(Ordering::Relaxed),
        PICK_SKIP_ONCPU[slot].load(Ordering::Relaxed),
        PICK_SKIP_CANRUN[slot].load(Ordering::Relaxed),
    )
}

/// `(wake deadline in us, woken flag, slot generation)` for `slot`.
///
/// The other half of [`slot_debug`]: a `WAITING` slot is a lost wakeup or a
/// dead timer depending on whether it carries a deadline, and whether that
/// deadline is in the past. A deadline of `0` means "no timeout — only a waker
/// can release this", which is the shape a lost `futex`/pipe wake leaves
/// behind; a deadline already passed means [`wake_pass`] is not running.
/// The generation distinguishes a fresh occupant from the slot's previous one.
#[must_use]
pub fn slot_wait_debug(slot: usize) -> (u64, bool, u64) {
    if slot >= MAX_THREADS {
        return (0, false, 0);
    }
    (
        WAKE_TIMES[slot].load(Ordering::Relaxed),
        WOKEN_STATES[slot].load(Ordering::Relaxed),
        SLOT_GEN[slot].load(Ordering::Relaxed),
    )
}

/// `(thread state, on-cpu gate)` for `slot`, for diagnostics only.
///
/// The pair, not either alone: [`pick_next`] skips a candidate for two
/// completely different reasons — a state that is neither `READY` nor
/// `RUNNING`, or an `ON_CPU` gate another core still holds — and a starved
/// thread looks identical from outside whichever one it is. Reporting the
/// state without the gate is how "the daemon is ready and never picked" reads
/// as a scheduler bug when it is really a second core sitting on its stack.
#[must_use]
pub fn slot_debug(slot: usize) -> (u8, u8) {
    if slot >= MAX_THREADS {
        return (thread_state::FREE, 0);
    }
    (
        THREAD_STATES[slot].load(Ordering::Acquire),
        ON_CPU[slot].load(Ordering::Acquire),
    )
}

/// Ready every `WAITING` thread whose wake deadline has passed.
///
/// The x86_64 counterpart of the wake-pass at the top of
/// [`ThreadPool::schedule_indices`], and it exists for the same reason: nothing
/// else moves a thread out of `WAITING` when its timeout — rather than a waker
/// — is what should release it. Without this, [`schedule_blocking`] with a
/// deadline parks forever on this target, and every timed `futex` wait,
/// `nanosleep` and `poll` timeout built on it hangs.
///
/// **CAS, not store**, for the reason the AArch64 pass documents at length: a
/// lock-free [`ThreadWaker::wake`], or a cross-thread kill's `TERMINATED`, can
/// land between the load and the write, and an unconditional `READY` store
/// would overwrite it — resurrecting a thread that was killed, or stamping
/// `READY` onto a half-written slot. `WAKE_TIMES` is cleared only after a
/// transition this core owns.
fn wake_pass() {
    let now = (runtime().uptime_us)();
    for i in 0..MAX_THREADS {
        if THREAD_STATES[i].load(Ordering::SeqCst) != thread_state::WAITING {
            continue;
        }
        let wake_time = WAKE_TIMES[i].load(Ordering::SeqCst);
        if wake_time == 0 || now < wake_time {
            continue;
        }
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
        }
    }
}

/// The earliest wake deadline of any `WAITING` thread, on the
/// `runtime().uptime_us` clock; `None` when no parked thread has one.
///
/// For a core about to halt: [`wake_pass`] can only ready a timed-out
/// thread when something runs it, and a halted core runs it at its next
/// interrupt. With only the periodic tick, a deadline was served up to one tick
/// late; the amd64 kernel arms a one-shot timer for this value instead
/// (`amd64/src/lapic.rs`, `arm_deadline`). Lock-free and allocation-free; a
/// racing wake or new park only makes the answer stale by one halt, and the
/// halt's own wake path re-reads it.
#[must_use]
pub fn earliest_wake_time() -> Option<u64> {
    let mut best = u64::MAX;
    for i in 0..MAX_THREADS {
        if THREAD_STATES[i].load(Ordering::Relaxed) != thread_state::WAITING {
            continue;
        }
        let t = WAKE_TIMES[i].load(Ordering::Relaxed);
        if t != 0 && t < best {
            best = t;
        }
    }
    (best != u64::MAX).then_some(best)
}

/// Is a parked thread's deadline (`earliest`, from [`earliest_wake_time`])
/// already due at `now_us`?
///
/// The question a **kernel wait loop that halts instead of yielding** must ask
/// before it halts. Only [`try_yield`] runs the wake pass, so such a loop
/// never readies the due sleeper; and because the sleeper stays `WAITING` with
/// a past deadline, the halt's one-shot timer is armed for 1 count, fires at
/// once, and the loop spins at interrupt rate while the sleeper starves
/// (`docs/archive/UDP_REPLY_WAIT_TIMER_STORM.md`). A due deadline means: yield.
#[must_use]
pub fn wake_is_due(earliest: Option<u64>, now_us: u64) -> bool {
    earliest.is_some_and(|t| t <= now_us)
}

/// Hand this core to another runnable thread; `true` if a switch happened.
///
/// The voluntary switch on both architectures: [`yield_now`] calls it and
/// discards the answer. Two callers need the answer: [`schedule_blocking`] (a
/// parked thread that could not be switched away from has to do something other
/// than call this again immediately), and a core's idle loop, which must halt
/// when the picker found nothing and must **not** when it did.
///
/// A no-op when the running thread is still runnable and there is nothing else
/// — a lone thread calling this in a loop must make progress rather than
/// deadlock against itself. When the running thread **cannot** continue (it
/// parked, or it terminated) and there is nothing else, this core's idle thread
/// takes over; a core must always have somewhere to go.
///
/// `ON_CPU` is what keeps two cores off one stack. The 2026-09-20 protocol is
/// the aarch64 one (Linux-style `prev` handoff): the outgoing thread's gate
/// **stays set through the switch** — a peer's picker skips gated threads, so
/// the outgoing slot cannot be stolen even when the switch runs with no BKL
/// held — and the gate is cleared once the outgoing stack is provably dead:
/// immediately after the switch returns on the incoming stack
/// ([`rust_switch_finished`] below), or at this function's entry otherwise
/// (the fresh-task trampoline never returns here, so its predecessor's gate
/// clears at the next scheduler entry — bounded by one tick). The old
/// protocol — clear-before-switch, hidden by the caller's BKL — is what made
/// every switch require the lock, which is what made a task parked mid-syscall
/// own the lock for its whole sleep and freeze `serving` behind it
/// (`docs/archive/AKUMA_AMD64_BKL_NETWORKING.md` 2026-09-20 night).
pub fn try_yield() -> bool {
    // The scheduler tick preempts kernel code ([`el1_preemptible`]; amd64 with
    // `ring0-preempt`), so a voluntary switch must not be interrupted half-way and
    // re-entered from the tick. Each thread restores the mask it yielded with when it
    // resumes here; one that yielded from an interrupt was masked already. On x86 this
    // is `pushfq; cli` .. `popfq`, a no-op for the ring 0 that runs with `IF` clear.
    let daif = akuma_primitives::irq::irq_save_mask();
    let switched = yield_locked();
    akuma_primitives::irq::irq_restore(daif);
    switched
}

/// [`try_yield`] with interrupts already dealt with: the lock, then the switch.
fn yield_locked() -> bool {
    #[cfg(feature = "yield-takes-bkl")]
    {
        // The AArch64 root kernel's `akuma_bkl` lets a thread run a stretch of EL1 code with the
        // lock deliberately released (the dropped-window carve-outs, and `lock_bounded`'s backoff,
        // which calls the yield hook holding nothing). The lock belongs to the *core* across a
        // switch, so a thread that yields from there would hand the next one a core that does not
        // hold what that thread was holding when it switched out. Take it for the switch and give
        // it back on resume: every thread then crosses a switch holding the lock, on whichever
        // core it resumes. amd64 needs none of this (its boot suite asserts no switch runs
        // without the lock), and the tokio kernels do not use `akuma_bkl`.
        //
        // A caller that does hold it releases and re-takes it first, so any core queued for
        // the lock gets it before this core switches. A yield already lets other kernel code
        // run between the caller's two halves, so no caller can be relying on the lock across
        // one. Without the release, a core whose threads poll by yielding — to each other, or
        // to nothing — keeps the lock for as long as they poll, and the thread they are
        // polling for, spinning for the lock on a peer core, never runs: the tick's
        // preemption of EL1 (`el1_preemptible`) lands in this same function, so it would not
        // help either. Measured, before the tick preempted EL1:
        // two `blocking_relax` waiters on core 0 switching to each other held the lock for
        // good, `[BKL] stuck` on core 1 (`smp_shared_blocking_wait_peer_progress`, SMP=2).
        // Uncontended it is one release and one acquire of a free ticket lock; the ticket
        // lock is what makes it a handoff.
        let held = bkl::held_by_current();
        if held {
            bkl::leave_kernel();
        }
        bkl::enter_kernel();
        let switched = yield_switch();
        if !held {
            bkl::leave_kernel();
        }
        switched
    }
    #[cfg(not(feature = "yield-takes-bkl"))]
    yield_switch()
}

/// The switch [`try_yield`] performs: pick, hand over, return on resume.
fn yield_switch() -> bool {
    // Prev-handoff cleanup, first thing: whatever gate this core is still
    // carrying belongs to a thread whose stack is now dead (we are running,
    // here, after its switch). No-op when there is no pending gate.
    rust_switch_finished();

    // Before the pick, not after: a thread whose deadline just passed has to be
    // a candidate for *this* scan, or a core with nothing else runnable parks
    // and the timeout is served a tick late at best.
    wake_pass();

    let hooks = arch();
    let cur = (hooks.current_slot)();
    let cur_runnable = matches!(
        THREAD_STATES[cur].load(Ordering::Acquire),
        thread_state::RUNNING | thread_state::READY
    );

    let next = match pick_next(cur, hooks) {
        Some(n) if n != cur => n,
        // Nothing to switch to and this thread can carry on.
        _ if cur_runnable => return false,
        _ => {
            // It cannot carry on and there is nothing else: go idle. If this
            // core's idle thread IS the caller, there is nowhere to go and the
            // caller must handle that — see `schedule_blocking`'s x86 arm.
            let idle = (hooks.idle_slot)();
            if idle == cur {
                return false;
            }
            idle
        }
    };

    // CPU-time accounting — this arch's arm of what `commit_switch` does for
    // the generic path (bill `cur`'s elapsed slice into `TOTAL_CPU_TIMES`,
    // stamp `next`'s `start_time_us`), under the same `POOL` lock
    // `get_thread_cpu_time`'s live-delta read already takes. Without this,
    // `try_yield` was the *only* switch path on this target and never
    // touched `start_time_us` at all: `TOTAL_CPU_TIMES` never accumulated and
    // the live-delta branch had no start to measure from, so `/proc/<pid>/
    // stat`'s `utime` (field 14 — what `ps`'s TIME column reads) read 0 for
    // every amd64 process regardless of how long it had actually run.
    {
        let now = (runtime().uptime_us)();
        let mut pool = POOL.lock();
        let start = pool.slots[cur].start_time_us;
        if start > 0 {
            TOTAL_CPU_TIMES[cur].fetch_add(now.saturating_sub(start), Ordering::Relaxed);
        }
        pool.slots[next].start_time_us = now;
    }
    // And the other half of `commit_switch`'s bookkeeping: which core `next`
    // is about to run on. Without it every slot stayed at the 0xFF "never
    // scheduled" sentinel, so `/proc/stat`'s per-core bucketing
    // (`akuma-vfs-glue`'s `cpu_time_snapshot`) billed nothing to any `cpuN`
    // row and `top`'s CORE column had nothing to show. `current_core_id` is
    // amd64's `PerCpu::index` (0 = BSP), the same index `LAST_CORE` uses on
    // AArch64 — not a LAPIC id.
    LAST_CORE[next].store(bkl::current_core_id() as u8, Ordering::Relaxed);

    // Only demote a thread that is actually running. A WAITING or TERMINATED
    // one keeps its state — that is the whole reason it is being switched out.
    let _ = THREAD_STATES[cur].compare_exchange(
        thread_state::RUNNING,
        thread_state::READY,
        Ordering::AcqRel,
        Ordering::Relaxed,
    );
    THREAD_STATES[next].store(thread_state::RUNNING, Ordering::Release);
    // Prev-handoff: the OUTGOING gate stays set across the switch — this is
    // what a peer's picker reads to skip us. Record it per-core for the
    // cleanup, which runs either right after the switch returns below or, for
    // a fresh task that never returns here, at that task's first scheduler
    // entry (the `rust_switch_finished()` at the top of this function).
    // `next`'s gate is already set — the pick's CAS claimed it.
    let core = bkl::current_core_id() as usize;
    if core < MAX_CORES {
        PER_CORE_OFFCPU[core].store(cur, Ordering::SeqCst);
    }
    (hooks.transfer_lock_depth)(cur, next);
    (hooks.set_current_slot)(next);
    // Every machine effect, with `cur` still current and before the stack moves.
    (hooks.switch_to)(cur, next);

    x86_check_incoming_frame(cur, next);

    // `cur`/`next` are both valid slot indices (`pick_next` only returns in-range candidates),
    // `ON_CPU` above is what stops another core resuming `next` concurrently, and `next`'s context was
    // built either by `akuma_switch::build_closure_context` or by a previous call to this same function.
    akuma_switch::switch_contexts(cur, next);
    // The only way here is on the INCOMING thread's stack — `cur`'s stack went
    // dead in the swap above, so its gate can drop now (a fresh task never
    // returns here; its predecessor's gate clears at the task's first
    // scheduler entry instead).
    rust_switch_finished();
    true
}

/// `RFLAGS` bits the kernel can legitimately be holding at a `pushfq` in
/// [`akuma_threading_x86_switch_context`].
///
/// Everything else — `TF`, `IOPL`, `NT`, `VM`, `AC`, `VIF`/`VIP` — is either
/// never set by this kernel or not settable from where it runs, so a saved
/// frame carrying one is a frame that is not what it claims to be.
#[cfg(target_arch = "x86_64")]
/// `CF|1|PF|AF|ZF|SF|IF|DF|OF` (0xED7) plus `RF` (0x1_0000) and `ID`
/// (0x20_0000). Written out because dropping one bit turns the check into a
/// printer: an omitted `IF` made every ordinary `0x202` frame a report.
const X86_KERNEL_RFLAGS_ALLOWED: u64 = 0x0021_0ED7;

/// Look at the frame the switch is about to restore, and say so if it cannot be
/// one.
///
/// # Why this is a check and not a repair
///
/// `akuma_threading_x86_switch_context` ends `popfq; ret`. A saved flags word
/// with `TF` set makes the `ret` the last instruction before a `#DB` the kernel
/// has no handler for — the machine dies one instruction after the switch, with
/// the evidence already overwritten: the CPU pushes its exception frame exactly
/// onto the 64 bytes the switch just popped. That is the 2026-09-12 ssh/apk
/// wedge (`docs/archive/AKUMA_AMD64_SSH_WEDGE_CONTEXT_SWITCH_PF.md`), and three
/// captures of it said only "a bad flags word arrived from somewhere".
///
/// So this reads the word **before** the `popfq`, while the frame is still
/// intact, and prints the whole of it: which slot, where its saved `rsp` points,
/// and the seven words there. Sanitising the flags instead would be worse than
/// useless — the other six registers and the return address come off the same
/// frame, so a frame whose flags are wrong is one the `ret` cannot survive
/// either; the value of stopping here is naming it.
///
/// One load and one compare on a switch that is already writing `CR3` and doing
/// an `fxsave`/`fxrstor` pair.
#[cfg(target_arch = "x86_64")]
fn x86_check_incoming_frame(cur: usize, next: usize) {
    // Layout is `switch_context`'s push order: [rsp+0] r15, +8 r14, +16 r13,
    // +24 r12, +32 rbx, +40 rbp, +48 rflags, +56 return address.
    const RFLAGS_OFF: usize = 48;
    // SAFETY: `next` is in range and not running on any core (`ON_CPU`), so its
    // context is stable here; the read is one word from a kernel stack this
    // switch is about to restore from anyway.
    let rsp = akuma_switch::with_ref(next, |c| c.rsp);
    if rsp == 0 {
        return;
    }
    // SAFETY: `rsp` is a kernel stack pointer with at least seven words of
    // frame above it — the frame the `pop`s below are about to consume.
    let flags = akuma_switch::read_stack_word(rsp as usize + RFLAGS_OFF);
    // Read it again. `next` is not on any core (`ON_CPU`) and its stack is
    // therefore nobody's to touch, so these two loads must agree — and when
    // they do not, *that* is the finding, not whatever value the first one got.
    // Measured 2026-09-12: the word this read at `+0x30` turned up at `+0x0` a
    // few microseconds later, which is a thread's saved frame moving while the
    // switch that is about to restore it looks on.
    // SAFETY: as above.
    let again = akuma_switch::read_stack_word(rsp as usize + RFLAGS_OFF);
    if again != flags {
        safe_print!(192,
            "[SWITCH FRAME MOVED] cur={} next={} state={} on_cpu={} last_core={} \
             rsp={:#x} first={:#x} second={:#x}\n",
            cur, next, THREAD_STATES[next].load(Ordering::SeqCst),
            ON_CPU[next].load(Ordering::SeqCst),
            LAST_CORE[next].load(Ordering::Relaxed),
            rsp, flags, again);
        return;
    }
    if flags & !X86_KERNEL_RFLAGS_ALLOWED == 0 && flags & 0x2 != 0 {
        return;
    }
    safe_print!(160,
        "[SWITCH BADFRAME] cur={} next={} state={} rsp={:#x} rflags={:#x}\n",
        cur, next, THREAD_STATES[next].load(Ordering::SeqCst), rsp, flags);
    for i in 0..8 {
        // SAFETY: as above — eight words of the frame being restored.
        let w = akuma_switch::read_stack_word(rsp as usize + i * 8);
        safe_print!(64, "[SWITCH BADFRAME]   +{:#x} = {:#x}\n", i * 8, w);
    }
}

// ---------------------------------------------------------------------------
// The hooked path's register-file layer on aarch64 (B21 A4).
//
// The three functions above are the x86 spelling; these are the AArch64 one, for a
// kernel that builds this crate with `hooked` (a tokio kernel) and so runs the
// cooperative hooked scheduler instead of the SGI one. Same contract: save the
// outgoing thread's callee-saved state, swap stacks, restore the incoming one's,
// and return into it - at its last switch, or (first run) at the entry trampoline.
//
// The callee-saved set is AAPCS64's: x19-x28, the frame pointer x29, the link
// register x30, sp, and the low halves of v8-v15 (d8-d15). The GPRs go into the
// `Context` fields the SGI path already defines (`x19`..`x30`, `sp`, `daif`); d8-d15
// go on the outgoing stack below `sp`, so `Context` keeps one layout for both
// paths. `DAIF` is saved and restored for the reason x86 saves `RFLAGS`: the
// interrupt mask belongs to the task, not to whoever last ran on this core.
// ---------------------------------------------------------------------------

// The aarch64 hooked path's switch, entry trampoline and first frame live in `akuma-switch`.

/// The x86 path checks the saved `RFLAGS` word before `popfq`; the aarch64
/// switch restores `DAIF` from the `Context` itself, where a corrupt word has
/// no trap of its own to hide behind, so there is nothing to look at first.
#[cfg(target_arch = "aarch64")]
fn x86_check_incoming_frame(_cur: usize, _next: usize) {}

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
#[cfg(target_arch = "aarch64")]
pub fn setup_fake_irq_frame(
    stack_top: u64,
    entry_point: u64,
    x19: u64,  // Trampoline function pointer
    x20: u64,  // Closure data pointer
    x21: u64,  // IRQ enable flag (0 = enable)
) -> u64 {
    akuma_switch::setup_fake_irq_frame(
        stack_top,
        entry_point,
        x19,
        x20,
        x21,
        thread_exit_stub as *const () as u64,
    )
}

/// Stub for thread exit - threads should never return here
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

// The per-thread `Context` table, its borrow word and `peek` live in `akuma-switch`.


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
        let boot_ttbr0: u64 = mmu::get_boot_ttbr0();

        // Slot 0 is the idle/boot thread (uses boot stack, never terminated)
        THREAD_STATES[IDLE_THREAD_IDX].store(thread_state::RUNNING, Ordering::SeqCst);
        self.slots[IDLE_THREAD_IDX].start_time_us = (runtime().uptime_us)();
        // Real shared-kernel SMP: slot 0 is core 0's per-core idle thread. Registering it
        // makes the scheduler's idle-fallback logic uniform across cores (single-core
        // builds also go through it, with core_id 0 → slot 0 = the original behavior).
        register_core_idle(0, IDLE_THREAD_IDX);
        
        // Initialize boot thread context in THREAD_CONTEXTS (not in slot)
        #[cfg(target_arch = "aarch64")]
        akuma_switch::with_mut(IDLE_THREAD_IDX, |boot_ctx| {
            boot_ctx.magic = CONTEXT_MAGIC;
            boot_ctx.ttbr0 = boot_ttbr0;
            // Boot thread starts with kernel mode SPSR
            boot_ctx.spsr = 0x00000005; // EL1h
            // Other fields stay zero (callee-saved regs saved on first context switch)
            boot_ctx.user_entry = 0;
            boot_ctx.user_sp = 0;
            boot_ctx.is_user_process = 0;
        });
        // x86_64: `Context` is just `{ magic, rsp }`, and `rsp` for the
        // already-running boot thread is meaningless until the first switch
        // *away* from it saves it (same as `amd64::sched::Context::empty`'s
        // own doc comment) — nothing to write here but the magic.
        #[cfg(target_arch = "x86_64")]
        {
            let _ = boot_ttbr0;
            akuma_switch::with_mut(IDLE_THREAD_IDX, |ctx| ctx.magic = CONTEXT_MAGIC);
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
        let exc_frame = akuma_pmm::alloc_pages_contiguous_zeroed(exc_pages).map(PhysFrame::new)
            .expect("Failed to allocate boot exception stack from PMM");
        let boot_exception_stack_ptr = mmu::phys_to_virt(exc_frame.addr);
        let boot_exception_stack_top = (boot_exception_stack_ptr as usize + EXCEPTION_STACK_SIZE) as u64;
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
        // Slots a secondary core has already adopted as its idle thread are skipped
        // by every loop below: that core is RUNNING on the stack it registered, and
        // `allocate_stack_for_slot` would overwrite both `stacks[i]` and
        // `slots[i].exception_stack_top` with a fresh PMM stack nothing is executing
        // on. The pool would then describe the wrong memory for a live core —
        // `validate_current_sp`, the canary check and the high-water probe all read
        // `stacks[i]` — while the core kept running on its `.bss` boot stack. See
        // `is_adopted_core_idle` and docs/archive/SMP_ADOPTED_IDLE_SLOT_CLOBBER.md.
        #[cfg(not(kernel_profile_extreme))]
        {
            for i in 1..config().reserved_threads {
                if is_adopted_core_idle(i) {
                    continue;
                }
                assert!(
                    self.allocate_stack_for_slot(i, config().system_thread_stack_size),
                    "boot: failed to allocate system thread stack for slot {}", i
                );
            }
            for i in config().reserved_threads..thread_limit() {
                if is_adopted_core_idle(i) {
                    continue;
                }
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
                if is_adopted_core_idle(i) {
                    continue;
                }
                assert!(
                    self.allocate_stack_for_slot(i, config().system_thread_stack_size),
                    "boot: failed to allocate warm system thread stack for slot {}", i
                );
            }
            let user_start = config().reserved_threads;
            let user_end = (user_start + WARM_FREE_USER).min(thread_limit());
            for i in user_start..user_end {
                if is_adopted_core_idle(i) {
                    continue;
                }
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
        let frame = match akuma_pmm::alloc_pages_contiguous_zeroed(pages).map(PhysFrame::new) {
            Some(f) => f,
            None => return false,
        };
        let stack_ptr = mmu::phys_to_virt(frame.addr) as usize;
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
                record_stack_peak(
                    slot_idx,
                    stack_region(stack.base, stack.size).high_water(config().canary_words * 8),
                );
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
            let phys_addr = mmu::virt_to_phys(stack.base);
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
    pub fn schedule_indices(
        &mut self,
        voluntary: bool,
        core_id: usize,
        interrupted_el0: bool,
    ) -> Option<(usize, usize)> {
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
        // Earliest-deadline woken thread, for the run-next hint below.
        let mut hint_tid = usize::MAX;
        let mut hint_deadline = u64::MAX;
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
                        if wake_time < hint_deadline {
                            hint_deadline = wake_time;
                            hint_tid = i;
                        }
                    }
                }
            }
        }
        // Wake-deadline preemption: the thread whose deadline expired earliest
        // runs NEXT, not after a full round-robin pass. Consumed by the `hinted`
        // swap below in this same call (or the next entry, if the preemption
        // gate returns early). A plain store: with WAKEUP_LOCALITY_HINT off we
        // are the only writer; if both gates are ever on, last-writer-wins is
        // benign — the consumer re-validates READY/ON_CPU before using it.
        if WAKE_DEADLINE_PREEMPT && hint_tid != usize::MAX {
            PREEMPT_WAKE_TID.store(hint_tid, Ordering::SeqCst);
        }
        // Send event to wake any threads in WFI
        if woke_any {
            akuma_cpu::park::sev();
        }

        // For timer-triggered preemption, first check if preemption is explicitly disabled.
        // (Only gates the context switch — the wake-pass above already ran.)
        if !voluntary && is_preemption_disabled() {
            return None;
        }

        // Involuntary-displacement bypass: on a timer tick that would swap out a
        // thread still doing real work (RUNNING, not idle, didn't yield or block),
        // first try handing the queued READY work to an IDLE peer core instead.
        // `wake_remote_idle` rings the lowest idle peer's scheduler SGI and reports
        // whether one existed; if so, this core keeps its thread and the idle core
        // picks the READY thread up within SGI latency. Without this, the global
        // round-robin displaced a compute thread on some core every tick whenever
        // ANY other thread was READY — even with cores sitting idle — so threads
        // of one process kept losing their barrier partners for ~a tick at a time
        // (the llama.cpp `-t N` 14-22x decode collapse).
        //
        // BOUNDED, not unconditional: an unbounded bypass let a spinning thread
        // monopolize its core for as long as any core anywhere was idle, which
        // starved sshd/netpoll onto one shared core under llama `-t 3` load
        // (973 preemption-watchdog hits, dropped ssh handshakes). A thread may
        // decline displacement at most DISPLACEMENT_IMMUNITY_TICKS-1 consecutive
        // ticks — an effective ~5 ms timeslice — then rotates exactly as before,
        // so a queued READY thread waits a bounded few ticks, never forever.
        // When every core is busy the bypass does nothing at all.
        //
        // EL0-ONLY: the bypass (and the keep-running fallback below) apply only
        // when the tick interrupted USERSPACE. Under smp-shared "the BKL is held
        // iff a core is in EL1", so a thread interrupted mid-EL1 is a BKL holder
        // — the per-tick switch (to idle, whose loop drops the BKL around WFI,
        // or to another thread via the dropped-window ledger) is the RELEASE
        // VALVE for long kernel excursions. Keeping an EL1 thread pinned wedged
        // the boot suite's FS tests in a `[BKL] stuck` storm (thread 0 held the
        // BKL through bulk ext2 I/O forever, every secondary starved).
        let displacement_bypass = |current_idx: usize| -> bool {
            const DISPLACEMENT_IMMUNITY_TICKS: u8 = 5;
            if voluntary
                || !interrupted_el0
                || IS_IDLE_THREAD[current_idx].load(Ordering::Relaxed)
                || THREAD_STATES[current_idx].load(Ordering::SeqCst) != thread_state::RUNNING
            {
                return false;
            }
            let deferred = DISPLACE_DEFER[current_idx].load(Ordering::Relaxed);
            if deferred >= DISPLACEMENT_IMMUNITY_TICKS - 1 {
                DISPLACE_DEFER[current_idx].store(0, Ordering::Relaxed);
                return false;
            }
            if (runtime().wake_remote_idle)() {
                DISPLACE_DEFER[current_idx].store(deferred + 1, Ordering::Relaxed);
                true
            } else {
                DISPLACE_DEFER[current_idx].store(0, Ordering::Relaxed);
                false
            }
        };

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
                    // Boost-path arm of the displacement bypass: if an idle peer can
                    // run the net thread, don't take this core's working thread.
                    if displacement_bypass(current_idx) {
                        return None;
                    }
                    // Same ON_CPU latch as commit_switch (this path bypasses it).
                    ON_CPU[current_idx].store(1, Ordering::SeqCst);
                    ON_CPU[net_tid].store(1, Ordering::SeqCst);
                    if core_id < MAX_CORES {
                        PER_CORE_OFFCPU[core_id].store(current_idx, Ordering::SeqCst);
                    }
                    // Atomic re-READY of the outgoing thread — see commit_switch for
                    // why this must not be a check-then-store.
                    let _ = THREAD_STATES[current_idx].try_update(
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
                    // Round-robin arm of the displacement bypass (see above): an
                    // idle peer takes the READY thread; this core keeps working.
                    if displacement_bypass(current_idx) {
                        return None;
                    }
                    // Found a ready, non-idle thread to switch TO.
                    break;
                }

                next_idx = (next_idx + 1) % MAX_THREADS;

                if next_idx == start_idx {
                    // No non-idle READY thread anywhere. If the current thread is still
                    // RUNNING (an involuntary tick preempted it, it did not block or
                    // yield) AND it was interrupted in EL0, keep it on this core:
                    // switching it out to idle here made every CPU-bound thread bounce
                    // core -> idle -> another core on every timer tick (the peer core's
                    // next tick picked it back up), costing a context switch +
                    // cache/TLB locality per tick and — for multi-threaded processes —
                    // regularly removing a barrier partner from execution for ~a tick,
                    // which is what collapsed llama.cpp `-t N` decode 14-22x. RUNNING
                    // is the safe gate against double-run: a RUNNING thread is never
                    // picked by a peer core's scan (only READY is). The EL0 gate is the
                    // safe gate against BKL starvation: an EL1-interrupted thread is a
                    // BKL holder under smp-shared, and the idle switch below is the
                    // release valve that lets peer cores enter the kernel (see the
                    // displacement_bypass comment above — omitting this wedged the
                    // boot suite's FS tests in a `[BKL] stuck` storm). A WAITING
                    // (blocked) or READY (yielded) current thread also still falls
                    // through to the idle switch, which is what takes it off-CPU.
                    if interrupted_el0
                        && THREAD_STATES[current_idx].load(Ordering::SeqCst)
                            == thread_state::RUNNING
                    {
                        return None;
                    }
                    // Fall back to THIS core's idle thread so
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
        let core = bkl::current_core_id() as usize;
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
        let _ = THREAD_STATES[current_idx].try_update(
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
        LAST_CORE[next_idx].store(bkl::current_core_id() as u8, Ordering::Relaxed);

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

}

// ============================================================================
// Stack Canary Functions
// ============================================================================

/// Initialize stack canary at the bottom of a stack
fn init_stack_canary(stack_base: usize) {
    if stack_base == 0 {
        return;
    }
    // The canary words sit at the base, below anything the thread has yet used.
    stack_region(stack_base, config().canary_words * 8)
        .write_canary(config().canary_words, config().stack_canary);
}

/// Check if stack canary is intact
fn check_stack_canary(stack_base: usize) -> bool {
    if stack_base == 0 {
        return true; // Boot stack or unallocated
    }
    stack_region(stack_base, config().canary_words * 8)
        .canary_intact(config().canary_words, config().stack_canary)
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
    &VOLUNTARY_SCHEDULE[bkl::current_core_id() as usize % MAX_CORES]
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
    let free_bytes = free_pages.saturating_mul(mmu::PAGE_SIZE);
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
    // "All others are FREE" holds only for slots nobody has taken yet. A secondary
    // core that came up BEFORE this runs (the self-test image's bringup order — see
    // `src/main.rs`) is RUNNING on the slot it adopted; storing FREE over that state
    // hands a live slot back to the allocator, and the next `claim_free_slot` — the
    // async-main thread, in practice — hands out a slot another core is executing on.
    // Two threads, one slot: the boot stall this order was long assumed to just
    // "collide" on. Leave adopted idles alone; `adopt_current_as_core_idle` already
    // stored RUNNING and latched their ON_CPU gate.
    // See docs/archive/SMP_ADOPTED_IDLE_SLOT_CLOBBER.md.
    for i in 1..MAX_THREADS {
        if is_adopted_core_idle(i) {
            continue;
        }
        THREAD_STATES[i].store(thread_state::FREE, Ordering::SeqCst);
    }
    set_current_thread_register(0);  // Initialize CPU register for boot thread
}

/// Trampoline function that calls a boxed FnOnce closure
fn closure_trampoline<F: FnOnce() -> ! + Send + 'static>(closure_ptr: *mut ()) -> ! {
    akuma_switch::call_boxed_closure::<F>(closure_ptr)
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


/// Yield to another thread
#[inline(never)]
pub fn yield_now() {
    // A function call on both architectures, so a masked interrupt flag does not stop it.
    try_yield();
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
    // Non-idle halters must stay PREEMPTIBLE (SMP=1 freeze fix, 2026-08-25).
    //
    // The preemption disable below exists so an *idle* thread's halt is
    // neither billed nor switched out mid-WFI (see the original comment).
    // Applied to a NON-idle halter — a socket waiter parked via
    // `blocking_relax_net` (`wait_until` → Promiscuous park), or the netpoll
    // thread between laps — it is fatal at SMP=1: the parked waiter stays
    // RUNNING, sole owner of the only core, inside a preempt-disabled `wfi`;
    // every timer tick runs the wake-pass but `schedule_indices` then
    // declines the switch (`!voluntary && is_preemption_disabled()`), and no
    // running thread remains to raise the voluntary SGI that could
    // un-suppress it. Whole-kernel freeze, no panic, ~3 % host CPU:
    // docs/archive/SECOND_LISTENER_SMP1_FREEZE.md §2a (GDB-proven).
    //
    // The idle loop escapes this shape only because its trailing
    // `yield_now()` is a voluntary switch; the socket variant deliberately
    // has no yield (removing it bought +27 % req/s — see
    // `blocking_relax_net`). Keeping the halt preemptible hands the tick
    // exactly the displacement power that yield provides, at zero cost in
    // the common case: if nothing else is READY, no switch happens and the
    // halt is uninterrupted as before.
    //
    // Cost accepted: a non-idle waiter's halt residency can now land in its
    // cpu_us bucket (billing-only skew — the original motivation for the
    // disable), and if it is displaced mid-halt the quantum-shift correction
    // below is clamped to `now` so it can never push a quantum start into
    // the future.
    let is_idle = tid < MAX_THREADS && IS_IDLE_THREAD[tid].load(Ordering::Relaxed);
    if is_idle {
        // Disable preemption for the duration of the halt. Without this, a
        // preemptible idle thread would be billed and switched out by the
        // timer tick *mid-WFI* — the residency including the halt lands in
        // its CPU-time bucket before we can correct it, and the correction
        // (start_time_us bump below) is discarded at the next switch-in.
        // With preemption disabled the timer's involuntary schedule_indices
        // returns None early (`!voluntary && is_preemption_disabled()`), so
        // the halt is neither billed nor switched. WFI still wakes on the
        // very same timer IRQ (and on device IRQs), and the voluntary
        // yield_now() that follows this in the idle loop bypasses the
        // preemption check, so readying a thread still reschedules
        // immediately. The watchdog (100 ms WARN) tolerates a ≤1-tick
        // (~10 ms) disabled window.
        disable_preemption();
    }
    let entered = (runtime().uptime_us)();
    // Real shared-kernel SMP: an idle core must not hold the Big Kernel Lock while
    // halted, or peer cores can't enter the kernel. Drop it before WFI; the IRQ that
    // wakes us re-takes it via the IRQ-path reconcile as it returns into this thread.
    // No-op unless `cfg(kernel_smp_shared)`.
    bkl::leave_kernel();
    // SAFETY: WFI halts the PE until a pending IRQ. IRQs are enabled on entry, so
    // the interrupt (timer tick / device) is taken and serviced before WFI returns.
    akuma_cpu::park::wfi();
    // Re-take the BKL for the post-halt bookkeeping below (idempotent if the waking
    // IRQ's reconcile already re-acquired it for this thread) — UNLESS this thread sits
    // inside a deliberately-dropped-BKL window (a guarded net/vfs syscall relaxing via
    // blocking_relax): re-taking here would re-hold the BKL for the window's remainder,
    // exactly the conversion the dropped-window ledger exists to prevent. The
    // bookkeeping below is safe BKL-free — it touches only POOL under its own IRQ-masked
    // lock. No-op unless smp-shared.
    if !bkl::in_dropped_window() {
        bkl::enter_kernel();
        // Profiler only: name this hold. An idle thread never passes a syscall/fault
        // tagging site, so without this its BKL time (this bookkeeping plus the
        // `yield_now()` that follows in the idle loop) is attributed "unknown". Gated on
        // IS_IDLE_THREAD because non-idle callers use `idle_halt()` as a plain wait and
        // must keep their own excursion's tag.
        #[cfg(kernel_smp_shared)]
        if is_idle {
            sync::set_holder_tag(bkl::current_core_id(), sync::HOLD_TAG_IDLE);
        }
    }
    let halted = (runtime().uptime_us)().saturating_sub(entered);
    if tid < MAX_THREADS && halted > 0 {
        let _guard = IrqGuard::new();
        if let Some(mut pool) = POOL.try_lock() {
            // Shift the quantum start forward by the halt duration so the next
            // switch-out's `now - start_time_us` excludes the time we spent
            // halted. Clamped to now: a non-idle halter can be descheduled
            // mid-halt now (that is the fix above), and the scheduler's
            // switch-in already reset start_time_us — an unclamped shift would
            // push the quantum start into the future and under-bill every
            // slice until it caught up.
            let shifted = pool.slots[tid].start_time_us.saturating_add(halted);
            let now_us = entered.saturating_add(halted);
            pool.slots[tid].start_time_us = shifted.min(now_us);
        }
    }
    if is_idle {
        enable_preemption();
    }
}

#[cfg(not(target_os = "none"))]
pub fn idle_halt() {}

/// Take the time the calling thread just spent halted out of its CPU bill.
///
/// For a loop that halts with its own instruction rather than [`idle_halt`]'s
/// `wfi` — amd64's `idle_loop` does `sti; hlt; cli`. The scheduler bills a thread
/// for its whole residency (`now - start_time_us` at switch-out), so without this
/// every idle thread is billed for every second it sleeps: on an idle two-core VM
/// `/proc/stat` read +1208 `user` ticks and +2 `idle` over six seconds, `top`
/// showed every core pegged, and `idle` could never be anything but ~0.
///
/// Call with the uptime sampled just before the halt. Shifts `tid`'s quantum start
/// forward by exactly the halted time, clamped to now — the same bookkeeping, and
/// the same reasoning about a tick that already switched away mid-halt, as
/// [`idle_halt`]. IRQs must be off (the halt's `cli` already made them so), and the
/// pool lock is only *tried*: a missed credit overbills one quantum, a wait here
/// would stall the idle thread.
#[cfg(target_os = "none")]
pub fn credit_halted_time(tid: usize, entered_us: u64) {
    let now = (runtime().uptime_us)();
    let halted = now.saturating_sub(entered_us);
    if tid < MAX_THREADS && halted > 0 {
        if let Some(mut pool) = POOL.try_lock() {
            let shifted = pool.slots[tid].start_time_us.saturating_add(halted);
            pool.slots[tid].start_time_us = shifted.min(now);
        }
    }
}

#[cfg(not(target_os = "none"))]
pub fn credit_halted_time(_tid: usize, _entered_us: u64) {}

/// Cooperative wait for a blocking kernel loop that is polling for external
/// progress (a child exit, a pipe, …) while holding the Big Kernel Lock.
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
/// The `yield_now` is NOT redundant with the BKL drop, and 2026-08-20 measured what
/// happens without it: at `SMP=4` the boot suite reaches 23 tests instead of 294 and
/// wedges permanently in the spawn/reap path. Keep it here. The network stack uses
/// [`blocking_relax_net`] instead, which is the variant without it — see there for
/// why the two paths differ.
///
/// Off `cfg(kernel_smp_shared)` this is a plain `yield_now` — single-core / default
/// builds are byte-for-byte unchanged (the `idle_halt` call compiles out).
#[inline]
pub fn blocking_relax() {
    yield_now();
    #[cfg(kernel_smp_shared)]
    idle_halt();
}

/// [`blocking_relax`] for the **socket** wait loop (`akuma_net`'s `wait_until`),
/// wired in through `NetRuntime::blocking_relax`.
///
/// Identical except that under `cfg(kernel_smp_shared)` it does NOT `yield_now`
/// before halting. That single difference is worth +27 % on HTTP throughput and cuts
/// p90 by half (A/B 2026-08-20, 5x2000 requests per arm, same session and same
/// `httpd`: 1,028 -> 1,307 req/s, p90 2,411 -> 1,166 us, ranges disjoint):
///
/// | per NICSTAT window, matched at ~12,170 rx packets | with yield | without |
/// |---|---:|---:|
/// | `relax` parks | 11,263-11,753 | 54,137-69,406 |
/// | us per park | 356-370 | 59-77 |
/// | us per poll | 11.1-11.9 | 6.9-7.7 |
///
/// **Why the yield costs so much here specifically.** The socket waiter is woken by
/// the NIC IRQ. With the yield, the park is a scheduler pass + SGI *before* the WFI
/// is entered, so a packet arriving during that window ends no halt — the waiter
/// reaches WFI just after its wake and sleeps to the next timer tick (~360 us
/// observed). Without it the waiter is already in WFI when the packet lands and the
/// device IRQ ends the halt directly (~70 us). More parks, each shorter, is the
/// signature of wakes landing (docs/archive/AKUMA_NET_ISSUES.md §11.7).
///
/// **Why this is not done for [`blocking_relax`].** Dropping the yield kernel-wide
/// reintroduces the cross-core wedge: at `SMP=4` the boot suite got 23 tests in and
/// froze (294 with the yield). The spawn/reap waiters genuinely depend on running a
/// READY peer on their own core; the socket waiter is woken by a device interrupt
/// and does not.
#[inline]
pub fn blocking_relax_net() {
    #[cfg(not(kernel_smp_shared))]
    yield_now();
    #[cfg(kernel_smp_shared)]
    idle_halt();
}

// `rust_switch_finished` (the vector asm's `bl` target, which clears the outgoing thread's `ON_CPU` gate) is
// defined in `akuma-switch` and re-exported above.


/// The x86_64 arm, which asks the target's scheduler rather than this file's
/// [`Context`] — and **is not a stub**, though it was one until 2026-09-12.
///
/// The stub answered `None` under a premise it stated plainly: "nothing on this
/// target saves a `ttbr0`-shaped value anywhere". That expired. `amd64`'s
/// `sched::Machine::space_root` is exactly such a value — one `u64` per task
/// slot that `hook_switch_to` writes to `CR3` verbatim on switch-in — and a
/// `None` here let the free gate release an L0 that an off-CPU slot was still
/// carrying. The failure is the x86 spelling of F8: the reissued frame stops
/// naming the kernel's PDPT in PML4 slot 511, and the first kernel access after
/// the `mov cr3` takes a not-present write fault from ring 0, inside the
/// context switch (`docs/archive/AKUMA_AMD64_SSH_WEDGE_CONTEXT_SWITCH_PF.md`).
///
/// It is a registered probe rather than a scan of `THREAD_CONTEXTS`, because on
/// this target the page-table root is **not** in the saved context: x86
/// `Context` is one `rsp` field (the callee-saved file lives on the thread's own
/// stack), and the root lives in a table this crate cannot name without
/// depending on the kernel that owns it. Unregistered it answers `None`, which
/// is the old behaviour and is correct for exactly as long as the old premise
/// held — before a scheduler exists there is no slot to be carrying a root.
static SAVED_ROOT_PROBE: akuma_primitives::Registered<fn(u64) -> Option<(usize, u8)>> =
    akuma_primitives::Registered::new(
        "akuma-threading: saved-root probe not registered (x86_64 free gate degraded)",
    );

/// Register the x86_64 saved-root probe. Called once, by the target's scheduler
/// `init`, before any address space can be built — the gate it feeds has to be
/// correct for the whole run, not from the moment the first process dies.
pub fn register_saved_root_probe(probe: fn(u64) -> Option<(usize, u8)>) {
    SAVED_ROOT_PROBE.register(probe);
}

pub fn any_saved_ctx_on_l0(l0_base: u64) -> Option<(usize, u8)> {
    if l0_base == 0 {
        return None;
    }
    SAVED_ROOT_PROBE.get().and_then(|p| p(l0_base))
}

/// Test hook: swap a slot's saved-context TTBR0, returning the previous value
/// (boot-suite self-tests only — they can't get a real thread parked with a
/// chosen stale TTBR0 in its saved context).
#[doc(hidden)]
pub fn test_swap_saved_ctx_ttbr0(_slot: usize, _ttbr0: u64) -> u64 {
    0
}

/// Test/diagnostic accessor: whether `tid`'s [`ON_CPU`] gate is set.
pub fn on_cpu_flag(tid: usize) -> bool {
    tid < MAX_THREADS && ON_CPU[tid].load(Ordering::SeqCst) != 0
}

/// Release `slot`'s [`ON_CPU`] gate after its occupant ran to completion *in
/// place* and control came back to the caller's own stack on the same core.
///
/// Taoxichuan's spawn crossing (`child_enter`) runs a child on a ring slot
/// without the scheduler ever switching away from it for good: the gate the
/// switch path raised is only lowered by the next switch's tail, which never
/// comes for a slot whose last "switch" was the return into its parent. The
/// slot then sits `TERMINATED` but `ON_CPU`, which [`slot_reusable`] refuses
/// forever - one ring slot burned per command, and `claim_slot` out of
/// slots after a dozen. **Only call this when the occupant is dead and no core
/// is executing on its stacks.**
pub fn release_on_cpu(slot: usize) {
    if slot < MAX_THREADS {
        ON_CPU[slot].store(0, Ordering::SeqCst);
    }
}

/// Hold `slot`'s [`ON_CPU`] gate from another core: `true` when it was clear, so
/// the occupant is not executing anywhere and no core can pick it until the
/// holder calls [`release_on_cpu`]. `false`: it is on a core (or being
/// switched in), and only it can stop itself.
///
/// This is the test "is that thread running?" has to use. The state word is
/// not it: a READY thread another core has just claimed in [`pick_next`] is
/// already being switched in, and a thread marked TERMINATED there runs on.
#[must_use]
pub fn hold_off_cpu(slot: usize) -> bool {
    slot < MAX_THREADS && ON_CPU[slot].compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire).is_ok()
}

/// Test/diagnostic accessor: how many [`ON_CPU`] gates are set. Bounded by the
/// number of cores on a healthy system (each core runs exactly one thread, plus
/// at most one not-yet-cleared outgoing gate per core mid-switch).
pub fn on_cpu_count() -> usize {
    (0..MAX_THREADS)
        .filter(|&i| ON_CPU[i].load(Ordering::SeqCst) != 0)
        .count()
}

/// Update a thread's context for a new execution — the x86_64 arm.
///
/// It was `unimplemented!()` until 2026-09-10, on the argument that the
/// AArch64 version writes a fake-IRQ-frame trap layout x86_64 does not have.
/// True, and beside the point: this target has an equivalent place to put a
/// ring-3 register file — `amd64::usermode::UserCtx`, the per-task-slot
/// structure the `syscall_entry` assembly writes and the `sysret` reads — and
/// `amd64::sched` has been seeding a `fork` child's copy of the parent into it
/// since that target grew `fork` at all. This forwards to that same writer, so
/// the shared `spawn_child_thread_and_publish` and the target's own `sys_fork`
/// cannot describe a child two different ways.
///
/// Off both kernels (a host `cargo test`, where `target_arch` is the host's)
/// this is a no-op rather than a panic: nothing on a host has a slot to write.
pub fn update_thread_context(thread_id: usize, user_context: &UserContext) {
    #[cfg(target_os = "none")]
    (arch().write_user_context)(thread_id, user_context);
    #[cfg(not(target_os = "none"))]
    {
        let _ = (thread_id, user_context);
    }
}


// ============================================================================
// Waker Integration
// ============================================================================

use core::task::Waker;

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
            // "AS MISMATCH" in THREAD_SPAWN_SEGV_INVESTIGATION.md) and its kernel stack,
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
fn waker_from_handle(handle: WakeHandle) -> Waker {
    akuma_switch::handle_waker::<ThreadWake>(handle.0)
}

/// The one effect a thread waker performs: wake the incarnation its packed handle names.
struct ThreadWake;

impl akuma_switch::HandleWake for ThreadWake {
    fn wake(handle: u64) {
        wake_by_handle(WakeHandle(handle));
    }
}

/// Creates a waker for the current thread (its current incarnation).
pub fn current_thread_waker() -> Waker {
    waker_from_handle(current_wake_handle())
}

// The `RawWakerVTable` (clone copies the word, drop releases nothing, wake calls `ThreadWake::wake`) is
// `akuma_switch::handle_waker`.

/// Returns a Waker for `thread_id`'s CURRENT incarnation. Only meaningful
/// where the caller knows the tid is live right now — a waiter registering
/// itself, or a wake site that has just validated liveness. Calling this at
/// wake time on a tid stored long ago launders a stale tid into a
/// fresh-looking handle; store a [`WakeHandle`] at enqueue time instead and
/// wake with [`wake_by_handle`].
pub fn get_waker_for_thread(thread_id: usize) -> Waker {
    waker_from_handle(wake_handle_for_thread(thread_id))
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
    let _ = THREAD_STATES[tid].try_update(
        Ordering::SeqCst,
        Ordering::SeqCst,
        |s| (s != thread_state::TERMINATED).then_some(thread_state::RUNNING),
    );
}

/// A backstop deadline, in microseconds, for a park that has none of its own —
/// `0` for "none", which is every AArch64 build.
///
/// See [`set_untimed_park_backstop_us`].
static UNTIMED_PARK_BACKSTOP_US: AtomicU64 = AtomicU64::new(0);

/// Untimed parks that were released by the backstop rather than by a wake.
///
/// A number that climbs on a workload that should be event-driven names a
/// missing wake path — which is the whole reason the backstop exists.
static UNTIMED_PARK_BACKSTOP_WAKES: AtomicU64 = AtomicU64::new(0);

/// Give every untimed park ([`park_indefinitely`]) a deadline `us`
/// microseconds out, instead of `u64::MAX`.
///
/// **This is a tripwire, not a design.** A correct wait has a wake path: the
/// pipe that gains a byte, the futex that is signalled, the child that exits.
/// If that path is missing, an untimed park is an unrecoverable hang with no
/// output — the failure mode that costs the most to diagnose and says the
/// least. With a backstop the same bug degrades to a poll at `1/us` Hz: the
/// machine stays usable and [`untimed_park_backstop_wakes`] says how often it
/// happened.
///
/// **Registered per target, and only one registers it.** AArch64 parks untimed
/// all the time and has an interrupt-driven scheduler to recover; amd64 reaches
/// its scheduler only by being called, so a lost wake is terminal there in a
/// way it is not here. That is why this is a knob and not a constant, and why
/// the default — `0`, meaning `u64::MAX`, meaning exactly the behaviour every
/// call site had before this existed — is the one AArch64 keeps.
///
/// It moved here from `amd64/src/sched.rs`'s `block_current`, which could only
/// cover the four parks that kernel wrote itself. Every blocking arm in
/// `akuma-syscalls-glue` parks through [`park_indefinitely`] now, so a target
/// that sets this gets the tripwire on all 28 of them — which is what let the
/// `read`/`write` arms fold (`docs/archive/AKUMA_AMD64_4B_FOLD_BATCH3.md`).
pub fn set_untimed_park_backstop_us(us: u64) {
    UNTIMED_PARK_BACKSTOP_US.store(us, Ordering::Relaxed);
}

/// How many untimed parks the backstop released. `0` is the healthy value.
#[must_use]
pub fn untimed_park_backstop_wakes() -> u64 {
    UNTIMED_PARK_BACKSTOP_WAKES.load(Ordering::Relaxed)
}

/// Park the running thread with **no deadline of its own** — until a waker
/// names it, or until the backstop [`set_untimed_park_backstop_us`] installed
/// (if any) expires.
///
/// The spelling every "wait until something happens" arm uses, in place of a
/// bare `schedule_blocking(u64::MAX)`. Unregistered it *is* that call, so a
/// target that installs no backstop is unchanged.
/// Where each thread last parked, as a `&'static Location` stored as a pointer.
/// `0` = it has never parked.
///
/// # Why a source location and not a tag
///
/// A parked thread's *state* says it is waiting and its last syscall number
/// says which call it entered, and between them those two still do not say
/// where in the kernel it stopped: a `futex` syscall parks in the wait loop, in
/// a demand-paging read of `uaddr`, and (via a wake that dequeued it) nowhere
/// at all. The `-j4` wedge turned on exactly that gap — four threads reported
/// `WAITING` with `sc=futex` while the futex table held no entry for any of
/// them (`AKUMA_AMD64_SELFHOST_BUILD_SLOWNESS.md` §13), which is a combination
/// no amount of reading either column can explain.
///
/// `#[track_caller]` on the two park entry points is what makes this free of
/// per-call-site bookkeeping: every existing caller is covered, including the
/// twenty-odd blocking arms in `akuma-syscalls-glue` that never go through a
/// target's own `sched` module.
static PARK_SITES: [AtomicUsize; MAX_THREADS] = {
    const INIT: AtomicUsize = AtomicUsize::new(0);
    [INIT; MAX_THREADS]
};

/// Record the caller of a park entry point against `tid`. See [`PARK_SITES`].
#[track_caller]
fn note_park_site(tid: usize) {
    if tid < MAX_THREADS {
        let loc: &'static core::panic::Location<'static> = core::panic::Location::caller();
        PARK_SITES[tid].store(core::ptr::from_ref(loc) as usize, Ordering::Relaxed);
    }
}

/// `(file, line)` of `tid`'s last park, or `None` if it has never parked.
#[must_use]
pub fn park_site(tid: usize) -> Option<(&'static str, u32)> {
    if tid >= MAX_THREADS {
        return None;
    }
    let loc = akuma_switch::static_location(PARK_SITES[tid].load(Ordering::Relaxed))?;
    Some((loc.file(), loc.line()))
}

#[track_caller]
pub fn park_indefinitely() {
    let backstop = UNTIMED_PARK_BACKSTOP_US.load(Ordering::Relaxed);
    if backstop == 0 {
        // The `runtime()` read is skipped, not just the arithmetic: an
        // unregistered backstop must cost the AArch64 kernel nothing beyond
        // the atomic load.
        schedule_blocking(u64::MAX);
        return;
    }
    let deadline = untimed_park_deadline(backstop, (runtime().uptime_us)());
    schedule_blocking(deadline);
    // Whether the backstop is what released us is only knowable here: the park
    // reports "runnable again" and not why. A caller that re-parks immediately
    // simply counts another.
    if (runtime().uptime_us)() >= deadline {
        UNTIMED_PARK_BACKSTOP_WAKES.fetch_add(1, Ordering::Relaxed);
    }
}

/// The deadline an untimed park gets: `u64::MAX` with no backstop installed,
/// and `now + backstop` with one.
///
/// Split out because it is the whole decision and the rest of
/// [`park_indefinitely`] is effects. Two things it must get right, and both are
/// silent when wrong: `0` means **no backstop** (not "a deadline of now", which
/// would make every untimed park return instantly and turn every wait loop in
/// the tree into a spin), and the addition **saturates** (a wrap would put the
/// deadline in the past, with the same result).
#[must_use]
const fn untimed_park_deadline(backstop_us: u64, now_us: u64) -> u64 {
    if backstop_us == 0 {
        u64::MAX
    } else {
        now_us.saturating_add(backstop_us)
    }
}

#[cfg(test)]
mod untimed_park_backstop_tests {
    use super::{UNTIMED_PARK_BACKSTOP_US, untimed_park_deadline};
    use core::sync::atomic::Ordering;

    /// The degradation contract every call site depends on: with nothing
    /// installed, `park_indefinitely` **is** `schedule_blocking(u64::MAX)`.
    /// That is what makes this change invisible to the AArch64 kernel.
    #[test]
    fn no_backstop_is_an_untimed_park() {
        assert_eq!(untimed_park_deadline(0, 0), u64::MAX);
        assert_eq!(untimed_park_deadline(0, 123_456), u64::MAX);
    }

    /// And nothing in this crate's tests installs one, so the default the
    /// AArch64 kernel runs with is the one asserted above.
    #[test]
    fn the_default_is_no_backstop() {
        assert_eq!(UNTIMED_PARK_BACKSTOP_US.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn a_backstop_is_relative_to_now() {
        assert_eq!(untimed_park_deadline(1_000_000, 5), 1_000_005);
        assert_eq!(untimed_park_deadline(1, 0), 1);
    }

    /// A wrap would land the deadline in the *past*, which
    /// `schedule_blocking` answers by not parking at all — turning every wait
    /// loop in the tree into a spin on a machine whose clock has run long
    /// enough.
    #[test]
    fn the_deadline_saturates_rather_than_wrapping() {
        assert_eq!(untimed_park_deadline(1_000_000, u64::MAX), u64::MAX);
        assert_eq!(untimed_park_deadline(u64::MAX, u64::MAX), u64::MAX);
    }
}

#[track_caller]
pub fn schedule_blocking(wake_time_us: u64) {
    let tid = current_thread_id();
    note_park_site(tid);

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

    // Wait for the scheduler to wake us.
    //
    // The two architectures reach that wait completely differently and the
    // difference is not cosmetic:
    //
    // - **AArch64** has an interrupt-driven scheduler. The SGI above asks for a
    //   switch, the timer IRQ arrives within a tick regardless, and `wfi` is
    //   the right way to spend the interval.
    // - **x86_64** has neither. There is no SGI, the switch is a plain
    //   function call, and `akuma_cpu::park::wfi` is `hlt` on this target —
    //   which, executed inside a syscall (interrupts masked by `IA32_FMASK`),
    //   halts the core forever. So the x86 arm *is* the switch: hand the CPU
    //   over, and re-test each time we are resumed.
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

        if (process().is_current_interrupted)() {
            WAKE_TIMES[tid].store(0, Ordering::SeqCst);
            // Same TERMINATED guard as above — the old unconditional RUNNING
            // store here could resurrect a thread killed while it waited.
            resume_running_unless_terminated(tid);
            break;
        }

        // Wait for interrupt - timer IRQ will fire within 10ms

        {
            // We are WAITING, so `pick_next` skips us: this hands the core
            // to someone else and returns only once a waker, the wake-pass, or
            // a kill has readied us again.
            if !try_yield() {
                // Nothing else could take the core, and this core's idle thread
                // is the caller. Not a hang and not a spin either: open one
                // interrupt window so the tick lands and the clock — and
                // therefore every deadline, including our own — keeps moving.
                // The loop then re-tests. See `ArchHooks::allow_tick`.
                (arch().allow_tick)();
            }
        }
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

/// The per-thread user-copy fault handler moved to `akuma_primitives::preempt`
/// (both its setter, `akuma-user-access`, and its reader, `akuma-exceptions`,
/// need it without depending on this crate). Re-exported so
/// `akuma_exec::threading::{get,set}_user_copy_fault_handler` call sites resolve
/// unchanged.
pub use akuma_primitives::preempt::{get_user_copy_fault_handler, set_user_copy_fault_handler};

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
        // A signal is only useful if the target gets to look at it. `wake()` has
        // made the slot READY, which is eligibility, not execution: without the
        // hint the thread joins the back of the round-robin queue and does not
        // report its `EINTR` for ~tick x runnable-threads. See
        // [`SIGNAL_WAKE_PREEMPT`] for the measurement.
        if SIGNAL_WAKE_PREEMPT
            && tid != current_thread_id()
            && THREAD_STATES[tid].load(Ordering::SeqCst) == thread_state::READY
        {
            PREEMPT_WAKE_TID.store(tid, Ordering::SeqCst);
        }
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

    // Step 3, x86_64: same shape as `spawn_fn`'s x86_64 arm — no fake IRQ
    // frame, see the "x86_64 context switch" section above.
    {
        let stack_top = {
            let pool = POOL.lock();
            pool.stacks[slot_idx].top as u64
        };
        with_irqs_disabled(|| {
            // This slot was just claimed and is not yet READY.
            akuma_switch::set(
                slot_idx,
                akuma_switch::build_closure_context(stack_top as usize, trampoline as usize as u64, closure_ptr as u64),
            );
            {
                let mut pool = POOL.lock();
                pool.slots[slot_idx].start_time_us = 0;
            }
            THREAD_STATES[slot_idx].store(thread_state::READY, Ordering::SeqCst);
        });
    }

    // Step 3, aarch64: set up fake IRQ frame and context.
    // This enables stack-based context switching

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

/// Internal implementation for spawning user threads
///
/// - start_irqs_disabled: if true, thread starts with IRQs disabled (for process threads)
#[allow(unreachable_code, unused_variables)]
fn spawn_user_thread_fn_internal<F>(f: F, start_irqs_disabled: bool) -> Result<usize, &'static str>
where
    F: FnOnce() -> ! + Send + 'static,
{
    // x86_64: take the slot and its stacks from the target, exactly as the
    // two-phase fork path does (`spawn_user_closure_initializing` ->
    // `ArchHooks::prepare_task_slot`). This function used to claim from
    // `THREAD_STATES` and take a stack from the AArch64 PMM pool below, which
    // never ran `prepare_task_slot`: the slot's `trap_stack_top` stayed 0, so
    // `TSS.rsp0` was 0 and the first interrupt or fault a `spawn_ext`-created
    // process took in ring 3 was a double fault -> triple fault. The process
    // survived only while it stayed in syscalls, which is why a short `box open`
    // worked and anything that computed, or touched an unmapped page, reset the
    // machine. Also gives the slot the canary pair the rest of the target paints.
    // `start_irqs_disabled` has no analogue on this cooperative-switch target.
    {
        let boxed: Box<F> = Box::new(f);
        let closure_ptr = Box::into_raw(boxed) as *mut ();
        let trampoline: fn(*mut ()) -> ! = closure_trampoline::<F>;
        let claimed =
            with_irqs_disabled(|| ThreadPool::spawn_user_closure_initializing(trampoline as usize as u64, closure_ptr));
        return match claimed {
            Ok(slot) => {
                mark_thread_ready(slot);
                note_user_thread_highwater();
                Ok(slot)
            }
            Err(e) => {
                // `closure_ptr` came from `Box::into_raw` above and the failed claim never handed it to a thread.
                akuma_switch::drop_boxed_closure::<F>(closure_ptr);
                Err(e)
            }
        };
    }

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

    // Step 3, x86_64: no fake IRQ frame — see the "x86_64 context switch"
    // section above. `start_irqs_disabled` (the process-thread race guard
    // documented on `spawn_user_thread_fn_for_process`) has no analogue in
    // this pass's cooperative-only x86_64 path and is ignored; nothing on
    // this target calls that variant yet.
    {
        let _ = start_irqs_disabled;
        let stack_top = {
            let pool = POOL.lock();
            pool.stacks[slot_idx].top as u64
        };
        with_irqs_disabled(|| {
            // This slot was just claimed and is not yet READY, so no switch can be reading its context
            // concurrently.
            akuma_switch::set(
                slot_idx,
                akuma_switch::build_closure_context(stack_top as usize, trampoline as usize as u64, closure_ptr as u64),
            );
            {
                let mut pool = POOL.lock();
                pool.slots[slot_idx].start_time_us = 0;
            }
            THREAD_STATES[slot_idx].store(thread_state::READY, Ordering::SeqCst);
        });
    }

    // Step 3, aarch64: set up fake IRQ frame and minimal context.
    // This enables stack-based context switching
    
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

/// The calling thread's EL0 register file, from the sync trap frame its syscall entry published
/// (`set_current_trap_frame`) — `ArchHooks::read_user_context` for the AArch64 root kernel under
/// `hooked-sched`, whose front end runs the same `akuma_exceptions::sync_el0` and so publishes the
/// same frame. What the SGI arm of [`get_saved_user_context`] builds, for the same caller: `None` for
/// any thread but the caller, or with no live frame; `spsr` a clean EL0t; `ttbr0` the live root.
#[cfg(target_arch = "aarch64")]
pub fn published_user_context(tid: usize) -> Option<UserContext> {
    if tid >= MAX_THREADS || tid != current_thread_id() {
        return None;
    }
    let f = published_trap_frame(tid)?;
    Some(UserContext {
        x0: f.x0, x1: f.x1, x2: f.x2, x3: f.x3, x4: f.x4, x5: f.x5, x6: f.x6, x7: f.x7,
        x8: f.x8, x9: f.x9, x10: f.x10, x11: f.x11, x12: f.x12, x13: f.x13, x14: f.x14, x15: f.x15,
        x16: f.x16, x17: f.x17, x18: f.x18, x19: f.x19, x20: f.x20, x21: f.x21, x22: f.x22, x23: f.x23,
        x24: f.x24, x25: f.x25, x26: f.x26, x27: f.x27, x28: f.x28, x29: f.x29, x30: f.x30,
        sp: f.sp_el0,
        pc: f.elr_el1,
        spsr: 0,
        tpidr: f.tpidr_el0,
        ttbr0: mmu::get_current_ttbr0() as u64,
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
    published_trap_frame(tid).map(|f| f.elr_el1)
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
#[cfg(target_arch = "x86_64")]
pub fn dump_thread_resume_points() {
    safe_print!(16, "[THR-DUMP] not implemented on x86_64\n");
}

#[cfg(target_arch = "aarch64")]
pub fn dump_thread_resume_points() {
    safe_print!(16, "[THR-DUMP]\n");
    for tid in 0..MAX_THREADS {
        let st = THREAD_STATES[tid].load(Ordering::SeqCst);
        if st == thread_state::FREE { continue; }
        let elr = {
            let sp = peek(tid, CtxField::Sp);
            if sp == 0 {
                0
            } else {
                // SAFETY: `sp` is a switched-out thread's saved IRQ frame on
                // its own kernel stack (guarded `!= 0`); the thread cannot free
                // that stack while it is not running.
                akuma_switch::irq_frame_at(sp).elr()
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
        let (pid, sc, tg, l0) = match (process().find_pid_by_thread)(tid) {
            Some(p) => match (process().proc_dump_info)(p) {
                Some((syscall, tgid, l0_phys)) => {
                    (p as i64, syscall, tgid as i64, l0_phys as u64)
                }
                None => (p as i64, !0, -1, 0),
            },
            None => (-1, !0, -1, 0),
        };
        let _ = (tg, l0);
        let tsc = thread_current_syscall(tid) as i64; // exact per-thread syscall
        // For a thread parked in a syscall, its saved trap frame still holds the
        // syscall args — x0 (futex uaddr), x1 (futex op). Lets us correlate a
        // stuck FUTEX_WAIT to a (missing) FUTEX_WAKE on the same address.
        let (a0, a1) = {
            published_trap_frame(tid).map_or((0, 0), |f| (f.x0, f.x1))
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
    (process().dump_orphan_processes)();
}


/// x86_64 stub — this diagnostic reads the AArch64 fake-IRQ-frame layout
/// that has no counterpart in this pass's cooperative-only switch.
pub fn get_saved_kernel_resume(_thread_id: usize) -> Option<(u64, u64, u64)> {
    None
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


/// The user-mode context to give a new child of `thread_id` — the x86_64 arm.
///
/// It returned `None` unconditionally until 2026-09-11, on the argument that
/// no fork-of-a-user-process path existed on this target. That stopped being
/// true when `update_thread_context` grew its writer (the ring-3 entry seam,
/// slice 2), and this is that writer's mirror: the register file the AArch64
/// side reads out of a live EL0 trap frame lives on this target in
/// `amd64::usermode::UserCtx`, one per task slot, written by the
/// `syscall_entry` assembly on the way in.
///
/// The refusal is preserved and is the same refusal:
/// [`ArchHooks::read_user_context`] answers `None` for a slot that has
/// never trapped in from ring 3 (`user_rip`/`user_rsp` still zero), which is
/// what "no live trap frame" means here. `amd64::usermode::sys_fork` performed
/// exactly that check by hand before this existed.
///
/// Off both kernels (a host `cargo test`, where `target_arch` is the host's)
/// there is no slot table at all, so `None` stays the only honest answer —
/// the same shape `update_thread_context`'s host arm takes.
pub fn get_saved_user_context(thread_id: usize) -> Option<UserContext> {
    #[cfg(target_os = "none")]
    {
        if thread_id >= MAX_THREADS {
            return None;
        }
        let ctx = (arch().read_user_context)(thread_id);
        if ctx.is_none() {
            // Same accounting and the same rate-limited line as the AArch64
            // side: a non-zero count means fork/vfork/clone syscalls are being
            // refused, and the caller has to be identifiable from one line.
            let n = NO_TRAP_FRAME_CHILDREN.fetch_add(1, Ordering::Relaxed);
            if n < 8 || n.is_power_of_two() {
                safe_print!(160,
                    "[NO-TRAPFRAME] refusing child of tid={} (cur={}) — slot never entered                      ring 3; count={}\n",
                    thread_id, current_thread_id(), n + 1);
            }
        }
        ctx
    }
    #[cfg(not(target_os = "none"))]
    {
        let _ = thread_id;
        None
    }
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
    let sp: usize = akuma_cpu::reg::sp();

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
            // Read SP from THREAD_CONTEXTS (not from slot). `Context`'s
            // stack-pointer field is named `sp` on AArch64 and `rsp` on
            // x86_64 — see `akuma-exec-core::thread::Context`'s two arms.
            #[cfg(target_arch = "aarch64")]
            { sps[i] = peek(i, CtxField::Sp); }
            #[cfg(target_arch = "x86_64")]
            { sps[i] = peek(i, CtxField::Rsp); }
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
/// (THREAD_SPAWN_SEGV_INVESTIGATION.md §2f). The corruption they pin down: a wake or a
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
    /// (`docs/archive/THREAD_SPAWN_SEGV_INVESTIGATION.md`, class 2). The published triple
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
    #[cfg(target_arch = "aarch64")]
    fn a_thread_with_no_trap_frame_never_yields_a_child_context() {
        crate::test_support::ensure_test_runtime();
        let tid = 40;
        const LDMUSL_DLSTART: u64 = 0x3006_9de8;

        // A live, dynamically linked process thread: `update_thread_context` published
        // this triple at its execve and nothing has overwritten it since. This is the
        // parent of a fork — not a recycled slot.
        akuma_switch::with_mut(tid, |ctx| {
            ctx.is_user_process = 1;
            ctx.user_entry = LDMUSL_DLSTART;
            ctx.user_sp = 0x20_3fff_e1f0; // the real per-parent constant from relr_probe_run1.log
            ctx.user_tls = 0x3272_a918;
        });
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
        // process's rather than merely the wrong one. The SGI path's slot
        // initialiser; the hooked path seeds a slot through `seed_entry`.

        akuma_switch::with_mut(tid, |ctx| {
            ctx.is_user_process = 0;
        });
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

    /// Canary slot for the context-stamp tests. No real switch runs in a host
    /// test, so any writable `Context` field works: `x19` (the aarch64
    /// callee-saved stamp) where that arch's wide `Context` exists, `rsp` on
    /// x86_64 whose `Context` is `{magic, rsp}`. Arch-selected so the host
    /// suite compiles on both (the 2026-10-28 pe session found the suite
    /// x86_64-broken since the Context split).
    #[cfg(test)]
    fn stamp_slot(idx: usize) -> &'static AtomicU64 {
        // `Sp`/`Rsp`: an atomic view of the word (`peek`'s door), with no borrow held - these tests race
        // claimants on purpose, and the claim (not a borrow) is what they check.
        #[cfg(target_arch = "aarch64")]
        { akuma_switch::word_cell(idx, CtxField::Sp) }
        #[cfg(target_arch = "x86_64")]
        { akuma_switch::word_cell(idx, CtxField::Rsp) }
    }

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
                    stamp_slot(idx).store(marker, Ordering::Relaxed);
                    thread::yield_now();
                    let seen = stamp_slot(idx).load(Ordering::Relaxed);
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
                            let val = stamp_slot(idx).load(Ordering::Relaxed);
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

            stamp_slot(idx).store(marker, Ordering::Relaxed);
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

/// The reuse rule `claim_slot` and the x86 reaper share. The zombie hold is
/// the 2026-10-16 ryzen fix: a tid an unreaped process still owns is never
/// handed to a new thread (`docs/archive/AKUMA_STALE_TID_ZOMBIE_SLOT.md`).
#[cfg(test)]
mod ryzen_2026_10_16_slot_hold_tests {
    use super::*;

    #[test]
    fn a_held_slot_is_never_reusable() {
        for state in [thread_state::FREE, thread_state::TERMINATED] {
            assert!(slot_reusable(state, 0, 0));
            assert!(!slot_reusable(state, 0, 1468), "state {state}: a zombie's tid was reissued");
        }
        // Unchanged: a core still on the stack, or a live slot, is not reusable.
        assert!(!slot_reusable(thread_state::TERMINATED, 1, 0));
        assert!(!slot_reusable(thread_state::RUNNING, 0, 0));
    }

    #[test]
    fn release_is_by_pid_and_leaves_other_holds_alone() {
        const A: usize = 201;
        const B: usize = 202;
        hold_slot_for_zombie(A, 0x7400_0001);
        hold_slot_for_zombie(B, 0x7400_0002);
        assert_eq!(release_slots_held_by(0x7400_0001), 1);
        assert_eq!(slot_held_by(A), 0);
        assert_eq!(slot_held_by(B), 0x7400_0002, "another zombie's hold was released");
        assert_eq!(release_slots_held_by(0x7400_0002), 1);
        assert_eq!(release_slots_held_by(0), 0, "pid 0 is 'no hold', never a match");
    }
}

#[cfg(test)]
mod wake_due_tests {
    use super::wake_is_due;

    #[test]
    fn due_only_when_a_deadline_has_passed() {
        assert!(!wake_is_due(None, 100));
        assert!(!wake_is_due(Some(101), 100));
        assert!(wake_is_due(Some(100), 100));
        assert!(wake_is_due(Some(1), 100));
    }
}
