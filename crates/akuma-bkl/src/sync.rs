//! Synchronization primitives for akuma-exec.
//!
//! Provides `KernelLock`, the recursive Big Kernel Lock used by real
//! (shared-kernel) SMP, plus the hold-tag attribution tables and
//! `lock_bounded`/`PreemptBoundedGuard`.
//!
//! # The hand-rolled `RwSpinlock` was deleted, not moved (2026-08-30)
//!
//! This module also carried `RawRwSpinlock` — an `AtomicU32` reader-writer lock
//! with writer priority, a stuck-spin diagnostic and an `unsafe impl
//! lock_api::RawRwLock` — and those three `unsafe` sites were most of the reason
//! this crate could not `forbid`. It had **zero consumers**: every `RwSpinlock`
//! in the tree (`akuma-ext2`'s `Ext2State`, `crates/akuma-kernel-tests-aarch64/src/tests.rs`) is
//! `spinning_top::RwSpinlock`, a *different type with the same name*, which is
//! exactly why nobody noticed. Only its own 11 tests used it.
//!
//! `docs/archive/TRIM_FAT_DEAD_CODE.md` had looked at this and classed
//! `RawRwLock` a **false positive** — correctly, for the question it asked: trait
//! methods are invisible to grep, so "no textual call site" does not prove a
//! trait impl is dead. But the type is only reachable through
//! `lock_api::RwLock<RawRwSpinlock, T>`, and nothing outside this crate ever
//! names `RawRwSpinlock` to instantiate one. A `#[deprecated]` probe confirmed it
//! against a full `--release` build: 19 warnings, every one inside this file.
//!
//! The lesson for the next dead-code scan: when grep can't see a trait's call
//! sites, ask instead whether anything *names the type* — and settle it with a
//! `#[deprecated]` probe rather than a grep.
//!
//! `SPIN_WARN_THRESHOLD` and `LOST_TICKET_RECOVERY_SPINS` stayed: both are read
//! by `KernelLock::acquire`, which is what they were really tuned for.

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// Bare DAIF save/mask and restore, re-exported from `akuma_primitives::irq`.
///
/// Used by [`KernelLock::acquire`] to make its FIFO ticket wait atomic against
/// local exception nesting, and by [`PreemptGuard`] to make inner-spinlock
/// critical sections nest-free under a dropped BKL.
///
/// These carry **no `isb`**, while `IrqGuard` — the same operation, and until
/// now a separate implementation — does. That difference is deliberately
/// preserved by the merge; `akuma_primitives::irq`'s header explains the cost on
/// each side of resolving it.
pub use akuma_primitives::irq::{irq_restore, irq_save_mask};

/// Total Big-Kernel-Lock spin iterations across all contended [`KernelLock::acquire`]
/// calls — a cross-core BKL-wait-time proxy for A/B measurement (e.g. does dropping the
/// BKL around a fault's block I/O reduce peer wait). Accumulated once per acquire, so the
/// uncontended fast path is unaffected. Read/reset via [`contention_spins`] /
/// [`reset_contention_spins`].
static CONTENTION_SPINS: AtomicU64 = AtomicU64::new(0);

/// Snapshot the total BKL contention-spin counter (see [`CONTENTION_SPINS`]).
pub fn contention_spins() -> u64 {
    CONTENTION_SPINS.load(Ordering::Relaxed)
}

/// Reset the total BKL contention-spin counter to zero (for A/B measurement windows).
pub fn reset_contention_spins() {
    CONTENTION_SPINS.store(0, Ordering::Relaxed);
}

/// Times a [`KernelLock`] wait had to self-heal its FIFO ticket accounting (the
/// `[BKL] RECOVERED` log lines: `reticket-owned`, `reticket-skipped`, `advanced-lost`).
///
/// Every one of these is a *symptom*, never a healthy event: the fair ticket lock cannot
/// lose or overshoot a ticket unless the acquire/`now_serving`-advance pairing has been
/// broken somewhere. The self-heal exists because the alternative is a hard wedge, but the
/// count belongs in a test assertion so a regression shows up as a failure rather than as
/// log lines nobody greps. Read via [`kernel_lock_recoveries`].
static KERNEL_LOCK_RECOVERIES: AtomicU64 = AtomicU64::new(0);

/// The longest wait any core has spun in [`KernelLock::acquire`] this boot, in
/// spin iterations. A tripwire, not a statistic: the fair ticket lock should
/// serve a waiter in microseconds, so anything past ~a million spins means
/// some holder kept the lock across a kernel stretch measured in *seconds* —
/// the shape that starves freshly-woken userspace threads (the litter raft
/// child, `docs/archive/AKUMA_AMD64_BKL_NETWORKING.md` 2026-09-20 §). Read
/// via [`max_wait_spins`] and noted at the end of the boot self-test suite.
static MAX_WAIT_SPINS: AtomicU64 = AtomicU64::new(0);

/// The longest `acquire` wait this boot (see [`MAX_WAIT_SPINS`]). The boot
/// suite notes it; a jump order-of-magnitude over the previous boot's value
/// is the early warning that a new long-hold path landed.
pub fn max_wait_spins() -> u64 {
    MAX_WAIT_SPINS.load(Ordering::Relaxed)
}

/// Snapshot the BKL ticket-recovery counter (see [`KERNEL_LOCK_RECOVERIES`]). Healthy runs
/// keep this at 0.
pub fn kernel_lock_recoveries() -> u64 {
    KERNEL_LOCK_RECOVERIES.load(Ordering::Relaxed)
}

/// Times the **lost-ticket** self-heal specifically (`advanced-lost`) had to force
/// `now_serving` forward — the subset of [`KERNEL_LOCK_RECOVERIES`] that means the FIFO
/// genuinely wedged: a ticket was handed out that nothing will ever consume, so every
/// contended acquirer spins `LOST_TICKET_RECOVERY_SPINS` before one of them breaks the
/// deadlock by hand.
///
/// Split out from the aggregate because the three recovery kinds are not equally bad.
/// `reticket-skipped` is *ordinary* rebalancing — benign and self-correcting within one
/// acquire. `advanced-lost` is the wedge, and it costs 20M spins per occurrence plus the
/// `[BKL] stuck owner=0` bursts those spins print. Assertions that want "the accounting is
/// sound" must watch this one, not the aggregate, or a benign skip masks a real leak.
/// Read via [`kernel_lock_lost_ticket_recoveries`].
static KERNEL_LOCK_LOST_TICKET_RECOVERIES: AtomicU64 = AtomicU64::new(0);

/// Snapshot the lost-ticket (`advanced-lost`) recovery counter. Must stay 0: see
/// [`KERNEL_LOCK_LOST_TICKET_RECOVERIES`].
pub fn kernel_lock_lost_ticket_recoveries() -> u64 {
    KERNEL_LOCK_LOST_TICKET_RECOVERIES.load(Ordering::Relaxed)
}

/// Which stuck episode last printed a `[BKL] stuck` line: `(owner << 32) | now_serving`,
/// `u64::MAX` before the first. One long hold with three cores waiting on it used to
/// print a line per waiter every [`SPIN_WARN_THRESHOLD`] spins, plus each waiter's
/// `[bkls>]` samples, interleaved byte-by-byte on the console: on the trashcan
/// (2026-09-25) that filled the 64 KiB `dmesg` ring in ~16 s, so a kot death a minute
/// earlier had already scrolled out. An episode is a hold, and a hold is one
/// `(owner, now_serving)` pair, so the first waiter to hit the threshold reports it and
/// the rest are counted in [`STUCK_SUPPRESSED`], printed with the next line that does go
/// out. Racing reporters are harmless: `swap` lets exactly one see a new key.
static STUCK_REPORTED: AtomicU64 = AtomicU64::new(u64::MAX);
/// `[BKL] stuck` lines folded into an episode already reported (see [`STUCK_REPORTED`]).
static STUCK_SUPPRESSED: AtomicU64 = AtomicU64::new(0);

/// Total `[BKL] stuck` lines suppressed as repeats of a reported episode this boot.
pub fn kernel_lock_stuck_suppressed() -> u64 {
    STUCK_SUPPRESSED.load(Ordering::Relaxed)
}

fn stuck_episode(owner: u32, serving: u32) -> u64 {
    (u64::from(owner) << 32) | u64::from(serving)
}

/// `Some(folded)` if this waiter should print the line for `episode`: it's the first to
/// report it. `folded` is how many were suppressed since the last line that printed.
/// `None` when the episode is already reported (and counts this one as suppressed).
///
/// An episode that goes on still prints every [`STUCK_REPRINT_EVERY`] folds: a permanent
/// wedge is one endless episode, and `scripts/j4_selfhost_campaign.py` tells a
/// `BKL_STORM` from a `SILENT_WEDGE` by counting these lines (> 20). Deduplicated to
/// the letter, a storm would read as silent.
fn claim_stuck_report(episode: u64) -> Option<u64> {
    if STUCK_REPORTED.swap(episode, Ordering::Relaxed) == episode {
        let folded = STUCK_FOLDED.fetch_add(1, Ordering::Relaxed) + 1;
        if folded >= STUCK_REPRINT_EVERY {
            STUCK_FOLDED.store(0, Ordering::Relaxed);
            return Some(folded);
        }
        STUCK_SUPPRESSED.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    Some(STUCK_FOLDED.swap(0, Ordering::Relaxed))
}

/// See [`claim_stuck_report`]: one line in this many, for an episode that doesn't end.
const STUCK_REPRINT_EVERY: u64 = 8;
/// Suppressed since the last `[BKL] stuck` line that printed; reset by each one.
static STUCK_FOLDED: AtomicU64 = AtomicU64::new(0);

// --- PreemptGuard ---------------------------------------------------------------

/// RAII guard that disables scheduler preemption (and, under the BKL-drop
/// features, masks local IRQs) for the lifetime of a kernel spinlock critical
/// section.
///
/// **Moved to `akuma_primitives::preempt`** — see that type's docs for the
/// `no-bkl-*` AB-BA reasoning and the full lift history. It lived here because
/// this crate owned both `threading::disable_preemption` and `irq_save_mask`;
/// the leaf crate owns both now, which is what let `akuma-ext2` and
/// `akuma-net` stop depending on this one for a ~40-line guard.
///
/// Kept as a re-export so `akuma_exec::sync::PreemptGuard` — and
/// `akuma_net::runtime::PreemptGuard`, which re-exports it in turn — keep
/// resolving.
pub use akuma_primitives::preempt::PreemptGuard;

/// Acquire a [`spinning_top::Spinlock`], disabling this thread's preemption for one
/// non-blocking attempt at a time — never across the whole wait.
///
/// `disable_preemption()` immediately before a *blocking* `.lock()` (the naive shape)
/// keeps preemption disabled for as long as the lock stays contended, not just for the
/// brief hold that follows. If the current holder itself needs to enter the kernel (the
/// BKL) to finish and release, and this thread's disabled preemption is exactly what
/// stops the scheduler from ever giving up this core to let that happen, the wait
/// becomes unbounded — the mechanism behind the `poll_input_event`/`term_state_lock`
/// wedge (`docs/archive/TERM_POLL_INPUT_PREEMPTION_FIX.md` §9). `docs/reference/
/// subsystems/locking.md`'s rule is "mask IRQs/preemption per *attempt*, never across an
/// unbounded wait"; `Ext2Filesystem::read_state`/`write_state`
/// (`crates/akuma-ext2/src/ext2.rs`) already implement it correctly for a different
/// lock — this mirrors that shape.
///
/// The backoff between failed attempts calls `yield_now()` — a *voluntary* handoff —
/// rather than only `core::hint::spin_loop()`. Re-enabling preemption alone is not
/// enough to guarantee this core is ever actually given up: between re-enabling
/// preemption and the next `try_lock`, nothing forces an *involuntary* handoff to
/// whoever holds the lock until the next timer tick. A voluntary yield hands off
/// immediately instead of waiting on the tick.
#[inline]
pub fn lock_bounded<T>(lock: &spinning_top::Spinlock<T>) -> PreemptBoundedGuard<'_, T> {
    loop {
        akuma_primitives::preempt::disable_preemption();
        if let Some(inner) = lock.try_lock() {
            return PreemptBoundedGuard { inner, _preempt: PreemptDisabledOnDrop };
        }
        akuma_primitives::preempt::enable_preemption();
        crate::yield_now();
    }
}

/// Re-enables preemption on drop; pairs with the `disable_preemption()` call in
/// [`lock_bounded`]'s successful attempt.
struct PreemptDisabledOnDrop;

impl Drop for PreemptDisabledOnDrop {
    #[inline]
    fn drop(&mut self) {
        akuma_primitives::preempt::enable_preemption();
    }
}

/// The guard returned by [`lock_bounded`].
///
/// Field order is drop order: `inner` (the spinlock guard) must release before
/// `_preempt` re-enables preemption, or there would be an instant where the lock is
/// still held with preemption already back on.
pub struct PreemptBoundedGuard<'a, T> {
    inner: spinning_top::guard::SpinlockGuard<'a, T>,
    _preempt: PreemptDisabledOnDrop,
}

impl<T> core::ops::Deref for PreemptBoundedGuard<'_, T> {
    type Target = T;
    #[inline]
    fn deref(&self) -> &T {
        &self.inner
    }
}

impl<T> core::ops::DerefMut for PreemptBoundedGuard<'_, T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut T {
        &mut self.inner
    }
}

// --- BKL-hold profiler ---------------------------------------------------------
// Attributes cross-core BKL wait to WHAT the holding core was doing, so we can see
// which excursions (which syscall, faults, or the IRQ/scheduler path) cause peers to
// wait — the lever for fine-graining past the coarse BKL. A waiter samples the current
// owner core's "tag" once when it first observes contention and, on finally acquiring,
// adds its accumulated spin count to that tag's bucket. Cheap: only the contended path
// touches this; the uncontended fast path is unchanged.

/// Max cores tracked by the profiler (matches the kernel's `MAX_CORES`).
const PROFILE_MAX_CORES: usize = 16;
/// Tag buckets: 0..=499 are syscall numbers (capped), 500 fault, 501 IRQ/scheduler,
/// 502 idle loop, 503 network poll, 511 unknown. Sized to 512.
const PROFILE_BUCKETS: usize = 512;
/// Reserved tag values.
pub const HOLD_TAG_FAULT: u64 = 500;
pub const HOLD_TAG_IRQ: u64 = 501;
/// A core's idle loop holding the BKL: the post-WFI bookkeeping plus the `yield_now()`
/// that runs the scheduler. Named because an idle thread never enters the kernel through
/// a syscall or fault, so it would otherwise be attributed [`HOLD_TAG_UNKNOWN`] —
/// honest but useless. See docs/archive/BKL_VFS_CARVE_OUT.md §18.
pub const HOLD_TAG_IDLE: u64 = 502;
/// The async-main smoltcp poll loop holding the BKL across a drain. Same reasoning as
/// [`HOLD_TAG_IDLE`]: it is a long-lived kernel thread with no tagged entry point.
///
/// Generic/fallback member of the `netpoll` family — used only for the sliver of the
/// loop body not covered by the more specific `HOLD_TAG_NETPOLL_*` sub-tags below (the
/// gap between re-acquiring the BKL post-WFI and the top-of-loop tag call). See
/// docs/archive/BKL_VFS_CARVE_OUT.md §19 for why the family was split.
pub const HOLD_TAG_NETPOLL: u64 = 503;
/// Async-main loop, top-of-iteration housekeeping: heartbeat/pstats logging,
/// `reclaim_terminated_slots`, and (measurement builds) `bkl_profile::maybe_dump`.
/// Runs every iteration before the smoltcp drain begins.
pub const HOLD_TAG_NETPOLL_MAINT: u64 = 504;
/// Async-main loop, the `while smoltcp_net::poll() {}` burst-drain itself — the part of
/// the iteration [`HOLD_TAG_NETPOLL`]'s doc calls "the drain" and §18.4 flagged as the
/// plausible bulk of the 59.7% figure.
pub const HOLD_TAG_NETPOLL_DRAIN: u64 = 505;
/// Async-main loop, the memory-monitor future's poll. Zero-cost when
/// `config::MEM_MONITOR_ENABLED` is false (the default): the poll call itself is
/// skipped, so this tag is never installed on that build.
pub const HOLD_TAG_NETPOLL_MEMMON: u64 = 506;
/// Async-main loop, herd supervisor output/exit-code polling.
pub const HOLD_TAG_NETPOLL_HERD: u64 = 507;
/// No attribution available: the holder is a thread that has never passed a tagging site.
/// With thread-scoped attribution this is a real answer ("an untagged in-kernel thread"),
/// not the "profiler is off" placeholder it also serves as.
pub const HOLD_TAG_UNKNOWN: u64 = 511;

/// Master switch for the BKL-hold profiler. **Default off** so normal boot pays nothing:
/// when on, `set_holder_tag` writes the shared `HOLDER_TAG` line on every kernel entry
/// (cross-core false sharing) and waiters do an extra `WAIT_BY_HOLDER` add — enough to
/// perturb timing-sensitive tests. A measurement enables it only around its window.
static PROFILE_ENABLED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Enable/disable the BKL-hold profiler (measurement windows only; default off).
pub fn set_profiling(on: bool) {
    PROFILE_ENABLED.store(on, Ordering::Relaxed);
}

/// A per-core tag padded to its own cache line, so `set_holder_tag` on every kernel
/// entry never false-shares across cores (that traffic alone perturbs timing tests).
#[repr(align(64))]
struct CoreTag(AtomicU64);

/// Per-core tag: what the thread **currently running on this core** is doing in the
/// kernel. This is the waiter's sampling point (`acquire` reads `HOLDER_TAG[owner_core]`)
/// and stays core-indexed and cache-line padded for exactly that reason — the waiter
/// knows only the owner's *core*, never its thread.
///
/// It is a **cache**, not the source of truth: [`THREAD_TAG`] is. See that type's docs for
/// why the distinction exists.
static HOLDER_TAG: [CoreTag; PROFILE_MAX_CORES] =
    [const { CoreTag(AtomicU64::new(HOLD_TAG_UNKNOWN)) }; PROFILE_MAX_CORES];
/// Per-tag accumulated peer wait (spin iterations attributed to a holder doing `tag`).
static WAIT_BY_HOLDER: [AtomicU64; PROFILE_BUCKETS] =
    [const { AtomicU64::new(0) }; PROFILE_BUCKETS];

/// Per-thread "what is this thread doing inside the kernel" tag — the authoritative half
/// of the attribution model.
///
/// **Why thread-scoped.** A kernel excursion belongs to a *thread*, not a core: it survives
/// preemption and can resume on a different core. When attribution lived only in the
/// per-core [`HOLDER_TAG`], a timer tick that context-switched mid-syscall left the
/// incoming thread wearing `irq/sched`, and a thread preempted inside a long BKL-held
/// syscall never re-entered the kernel to correct itself — it ran the whole remainder of
/// that syscall labelled `irq/sched`. Precisely the long excursions worth finding are the
/// ones that get preempted, so the artifact concentrated in the one bucket
/// (docs/archive/BKL_VFS_CARVE_OUT.md §16.2, §18).
///
/// This is the same reason [`crate::bkl::DroppedWindowLedger`] is thread-scoped, and the
/// storage mirrors it: a plain array of relaxed atomics, pure (no target dependencies) so
/// the contract is host-testable, out-of-range `tid`s inert.
///
/// **The contract**, maintained by three operations:
/// - a kernel *entry* ([`set_holder_tag`]) publishes the thread's tag to both tables;
/// - a *transient* nested excursion ([`set_core_tag_transient`]) touches only the core
///   cache, so the interrupted thread's own tag is never clobbered;
/// - a *thread change* ([`load_thread_tag_to_core`]) reloads the cache from the table.
///
/// Together those give the invariant `HOLDER_TAG[c] == THREAD_TAG[thread running on c]`,
/// except inside a transient excursion where the core cache deliberately reads
/// [`HOLD_TAG_IRQ`].
pub struct ThreadTagTable<const N: usize> {
    tag: [AtomicU64; N],
}

impl<const N: usize> ThreadTagTable<N> {
    /// All threads start unattributed.
    pub const fn new() -> Self {
        Self {
            tag: [const { AtomicU64::new(HOLD_TAG_UNKNOWN) }; N],
        }
    }

    /// Record what `tid` is doing in the kernel. Tags are clamped into the bucket range so
    /// the histogram index is always valid; out-of-range `tid`s are ignored.
    pub fn set(&self, tid: usize, tag: u64) {
        if let Some(t) = self.tag.get(tid) {
            t.store(tag.min((PROFILE_BUCKETS - 1) as u64), Ordering::Relaxed);
        }
    }

    /// What `tid` is doing, or [`HOLD_TAG_UNKNOWN`] for an unknown/out-of-range thread.
    pub fn get(&self, tid: usize) -> u64 {
        self.tag.get(tid).map_or(HOLD_TAG_UNKNOWN, |t| t.load(Ordering::Relaxed))
    }

    /// Drop `tid`'s attribution back to unknown. For recycled thread slots — a fresh thread
    /// must not inherit the tag of the one that previously owned the slot.
    pub fn reset(&self, tid: usize) {
        if let Some(t) = self.tag.get(tid) {
            t.store(HOLD_TAG_UNKNOWN, Ordering::Relaxed);
        }
    }
}

impl<const N: usize> Default for ThreadTagTable<N> {
    fn default() -> Self {
        Self::new()
    }
}

/// The kernel's per-thread tag table, indexed by thread id.
static THREAD_TAG: ThreadTagTable<{ akuma_primitives::MAX_THREADS }> = ThreadTagTable::new();

/// Record what the **current thread** is doing on entering kernel code, and mirror it to
/// `core_id`'s sampling cache. Called by the exception entry paths (syscall number at
/// syscall entry, `HOLD_TAG_FAULT` at fault entry) so waiters can attribute blame.
///
/// Writing both tables is what makes the attribution survive a context switch: the core
/// cache serves the waiter now, the thread entry serves this same excursion after it is
/// preempted and later resumed (possibly on another core).
#[inline]
pub fn set_holder_tag(core_id: u32, tag: u64) {
    if !PROFILE_ENABLED.load(Ordering::Relaxed) {
        return;
    }
    let tag = tag.min((PROFILE_BUCKETS - 1) as u64);
    THREAD_TAG.set(akuma_primitives::preempt::current_tid(), tag);
    let c = core_id as usize;
    if c < PROFILE_MAX_CORES {
        HOLDER_TAG[c].0.store(tag, Ordering::Relaxed);
    }
}

/// Stamp `core_id`'s sampling cache for a **transient** nested excursion that does not
/// belong to the interrupted thread — the IRQ/scheduler dispatch. Deliberately does NOT
/// touch [`THREAD_TAG`]: the interrupted thread is still mid-syscall and must keep its own
/// tag, so that whichever thread the core ends up running afterwards,
/// [`load_thread_tag_to_core`] can restore the truth.
#[inline]
pub fn set_core_tag_transient(core_id: u32, tag: u64) {
    if !PROFILE_ENABLED.load(Ordering::Relaxed) {
        return;
    }
    let c = core_id as usize;
    if c < PROFILE_MAX_CORES {
        HOLDER_TAG[c].0.store(tag.min((PROFILE_BUCKETS - 1) as u64), Ordering::Relaxed);
    }
}

/// Point `core_id`'s sampling cache at `tid`'s tag — i.e. "this core is now running this
/// thread, so waiters blocked on it should be told what *it* is doing."
///
/// Called wherever the current thread changes (`set_current_thread_register`, which every
/// context-switch path funnels through) and at the end of the IRQ dispatch. Those two
/// cover both shapes of the old bug with one rule: after a switch it installs the incoming
/// thread's tag, and after an IRQ that did *not* switch it reinstalls the interrupted
/// thread's own tag — which is why no save/restore of the pre-IRQ tag is needed.
#[inline]
pub fn load_thread_tag_to_core(core_id: u32, tid: usize) {
    if !PROFILE_ENABLED.load(Ordering::Relaxed) {
        return;
    }
    let c = core_id as usize;
    if c < PROFILE_MAX_CORES {
        HOLDER_TAG[c].0.store(THREAD_TAG.get(tid), Ordering::Relaxed);
    }
}

/// Read `tid`'s tag. For assertions and the boot self-test.
#[inline]
pub fn thread_tag(tid: usize) -> u64 {
    THREAD_TAG.get(tid)
}

/// Read `core_id`'s sampling cache — what a waiter blocked on that core would attribute
/// its wait to right now. For assertions and the boot self-test.
#[inline]
pub fn core_tag(core_id: u32) -> u64 {
    let c = core_id as usize;
    if c < PROFILE_MAX_CORES {
        HOLDER_TAG[c].0.load(Ordering::Relaxed)
    } else {
        HOLD_TAG_UNKNOWN
    }
}

/// Clear a thread slot's attribution. Called when a slot is claimed for a new thread, so a
/// recycled slot cannot lend its predecessor's tag to a waiter.
#[inline]
pub fn reset_thread_tag(tid: usize) {
    THREAD_TAG.reset(tid);
}

/// Read a tag bucket's accumulated peer-wait spins (for the profiler dump).
pub fn wait_by_holder(tag: usize) -> u64 {
    WAIT_BY_HOLDER.get(tag).map_or(0, |a| a.load(Ordering::Relaxed))
}

/// Reset the per-tag wait histogram (for a measurement window).
pub fn reset_wait_by_holder() {
    for a in &WAIT_BY_HOLDER {
        a.store(0, Ordering::Relaxed);
    }
}

/// The Big Kernel Lock (BKL) for real (shared-kernel) SMP — an **owner-tracked,
/// idempotent** spinlock that serializes kernel execution across cores.
///
/// **Invariant:** the lock is held by a core **iff that core is executing kernel code
/// (EL1).** It follows EL transitions rather than being balanced like an
/// ordinary lock: entry from EL0 acquires it; an `eret` back to EL0 releases it; a
/// nested exception taken while already in EL1, and the `eret` back to EL1 from it,
/// leave it held (the target is still EL1). Because there is exactly one EL1→EL0
/// return per kernel excursion, there is exactly one release per excursion — no
/// per-thread depth needs to travel across context switches. This upgrades the
/// kernel's pervasive single-core `with_irqs_disabled` invariant (mutual exclusion on
/// one core only) into a genuine cross-core one — historically this is what made the
/// ~218 legacy `lookup_process() -> &'static mut Process` sites correct without
/// per-site changes (docs/archive/SMP_SHARED.md, M1); Phase 7e's "Access" half has
/// since deleted that API in favor of `lookup_process_shared`/`with_process`, but
/// same-core field races on a live ACTIVE process still lean on the BKL exactly the
/// same way. Uncontended on a single-core build and in
/// M0/M1 (secondaries parked).
///
/// **IRQ state:** the IRQ/eret paths call in with local IRQs masked (hardware masks on
/// exception entry), but the **syscall path enters with IRQs *enabled*** — the EL0-sync
/// trampoline does `msr daifclr, #2` before `rust_sync_el0_handler → enter_kernel` so a
/// long syscall stays preemptible. `acquire` therefore masks IRQs itself for the duration
/// of its (fair) wait and restores them once it owns the lock; see `acquire`.
///
/// **Fairness:** contended acquisition is a **FIFO ticket lock** (`next_ticket` /
/// `now_serving`) layered over the binary `owner`, so no core can be starved by peers (or
/// by the releaser immediately re-grabbing).
///
/// `acquire`/`release` remain **idempotent** for the owner (re-acquiring what you hold, or
/// releasing what you don't, is a no-op) — the reentrant cases take NO ticket, so
/// the ticket counters stay balanced across the non-lexical acquire/release that context
/// switches create (one ticket per EL0→EL1 crossing, one `now_serving` advance per
/// EL1→EL0 crossing, per core).
pub struct KernelLock {
    /// `0` = free; otherwise `owner_core_aff0 + 1`. Written by the ticket winner on
    /// acquire and cleared by the owner on release; the source of truth for
    /// `held_by`/`is_held` and the reentrant fast path.
    owner: AtomicU32,
    /// Next FIFO ticket to hand out (monotonic, wraps). A contended acquirer takes one.
    next_ticket: AtomicU32,
    /// The ticket currently permitted to take ownership. The owner's release advances it
    /// by one, handing the lock to the next waiter in arrival order.
    now_serving: AtomicU32,
}

impl Default for KernelLock {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// The x86 TLB-shootdown spin assist.
//
// A core spinning IRQ-masked in the ticket wait below cannot take a TLB
// shootdown IPI — and on amd64 the sender of that IPI is holding the very BKL
// this core waits for, so "wait for the interrupt" never terminates. The
// sender therefore publishes the request in a per-CPU mailbox *before* sending,
// and this wait loop services it inline: flush, acknowledge, keep spinning.
// The hook is registered by the kernel (`shootdown::bkl_spin_assist`); when it
// is absent — AArch64, which broadcasts in the `tlbi` instruction; host tests;
// early boot — the call is compiled out and this loop is exactly what it was.
// See `akuma_mmu::set_shootdown_hooks` for the full deadlock argument.
// ---------------------------------------------------------------------------

/// The registered assist, if any. `set_spin_assist` stores it once, at boot.
#[cfg(all(target_os = "none", target_arch = "x86_64"))]
static SPIN_ASSIST: akuma_primitives::OnceCopy<fn()> = akuma_primitives::OnceCopy::new();

/// Register the IRQ-masked-spin assist. x86 bare-metal only; called once,
/// before any AP exists.
#[cfg(all(target_os = "none", target_arch = "x86_64"))]
pub fn set_spin_assist(f: fn()) {
    SPIN_ASSIST.set(f);
}

/// Run the assist if one is armed. Inline in the wait loop on purpose: the
/// common case is "nothing pending", one load.
#[inline]
#[cfg(all(target_os = "none", target_arch = "x86_64"))]
fn spin_assist() {
    if let Some(f) = SPIN_ASSIST.get() {
        f();
    }
}

/// The same assist for IRQ-masked spins on locks that are **not** the BKL's.
///
/// `akuma_mmu::set_shootdown_hooks` argues the shootdown wait is bounded
/// because the sender holds the BKL, the outermost lock. The BKL carve-outs
/// (`no-bkl-process`, `-mm`, `-vfs`, `-drivers`) make that false: a core can be
/// IRQ-masked on an inner lock — the address-space lock above all — while the
/// sender holds that lock waiting for this core's acknowledgement. Any such
/// spin calls this each iteration. A no-op off x86 bare metal and before the
/// kernel registers the hook.
#[inline]
pub fn masked_spin_assist() {
    #[cfg(all(target_os = "none", target_arch = "x86_64"))]
    spin_assist();
}

impl KernelLock {
    /// A free lock.
    pub const fn new() -> Self {
        Self {
            owner: AtomicU32::new(0),
            next_ticket: AtomicU32::new(0),
            now_serving: AtomicU32::new(0),
        }
    }

    /// Ensure `core_id` (an MPIDR aff0) owns the lock, waiting (FIFO) until it is our turn
    /// if another core holds it. Idempotent: a no-op if this core already owns it.
    ///
    /// Masks local IRQs for the duration of the ticket wait. The syscall path enters with
    /// IRQs enabled (see the type doc), so without masking a timer/device IRQ could nest
    /// mid-wait and *its* `enter_kernel` would take a SECOND ticket for this core — a ticket
    /// that can never be served, because the outer (lower) ticket is stalled underneath the
    /// nested exception frame → self-deadlock. Masking makes the per-core wait atomic; IRQs
    /// are restored to their prior state once we own the lock. On the IRQ/eret paths IRQs
    /// are already masked, so the mask/restore is a no-op there.
    ///
    /// While masked this core also cannot take a TLB shootdown IPI — the x86
    /// wait loop services those through `spin_assist` above, which is what
    /// keeps the shootdown sender's acknowledgement wait (see `akuma-mmu`)
    /// bounded while this core spins here.
    #[inline]
    pub fn acquire(&self, core_id: u32) {
        let me = core_id + 1;
        // Reentrant fast path: this core already owns it (a nested EL1 exception).
        // No ticket, no wait.
        if self.owner.load(Ordering::Acquire) == me {
            return;
        }
        let daif = irq_save_mask();
        // Re-check after masking: a nested IRQ in the tiny window before the mask may have
        // already acquired the lock for this core (taking the ticket itself). If so we must
        // NOT take a second ticket.
        if self.owner.load(Ordering::Acquire) == me {
            irq_restore(daif);
            return;
        }
        // Take a FIFO ticket and wait for our turn. The holder's release advances
        // `now_serving` by exactly one, so waiters are served in arrival order and none can
        // be starved by a peer or by the releaser re-grabbing.
        //
        // ## Self-healing (empirical, 2026-07-21)
        //
        // The ticket accounting has a rare leak under SMP=4 fork-hammer load: lldb on a
        // hard-wedged instance showed `owner == 0`, `next_ticket == now_serving + 5`, and
        // only FOUR cores spinning — one handed-out ticket had no living waiter, so
        // `now_serving` could never advance and every core spun forever (all four
        // backtraces in the BKL wait; the whole box dead). Root cause not yet pinned
        // (same family as the M5c step-2 `sched_bklfree_el0` ticket leak, but observed
        // with that flag OFF). Until it is, the wait loop self-heals both wedge shapes,
        // loudly:
        //
        // - **Lost ticket ahead** (the observed wedge): the lock stays FREE (`owner == 0`)
        //   while `now_serving` sits frozen short of our ticket for
        //   `LOST_TICKET_RECOVERY_SPINS` consecutive spins. A live served waiter stores
        //   `owner` within its masked spin (ns), and a releasing holder advances
        //   `now_serving` right after clearing `owner` — so free+frozen for that long
        //   means the served ticket's core is gone. CAS `now_serving` one step forward
        //   (CAS: racing recoverers can't double-advance) and keep waiting.
        // - **Skipped** (the recovery's dual): `now_serving` moved PAST our ticket — a
        //   recovery advanced over us while our vCPU was stalled (host descheduling), or
        //   the underlying leak double-advanced. Waiting for an exact match would spin
        //   forever; take a fresh ticket and rejoin the queue.
        //
        // Ownership take is a CAS (not a blind store): if a recovery advanced onto our
        // ticket in the same instant another core still owns the lock, the failed CAS
        // sends us back to the wait loop instead of minting two owners.
        let mut my_ticket = self.next_ticket.fetch_add(1, Ordering::Relaxed);
        let mut spins: u32 = 0;
        let mut total_spins: u64 = 0;
        // Profiler: the tag of the core that was blocking us, sampled once (u64::MAX = not
        // yet sampled).
        let mut wait_tag: u64 = u64::MAX;
        // Lost-ticket detector: consecutive spins with the lock free and `now_serving`
        // unchanged (reset whenever either moves).
        let mut frozen_serving = self.now_serving.load(Ordering::Acquire);
        let mut frozen_spins: u32 = 0;
        loop {
            let serving = self.now_serving.load(Ordering::Acquire);
            if serving == my_ticket {
                // Our turn: the previous holder cleared `owner` to 0 before advancing
                // `now_serving` to us. CAS instead of store — see the recovery note above.
                if self
                    .owner
                    .compare_exchange(0, me, Ordering::AcqRel, Ordering::Relaxed)
                    .is_ok()
                {
                    break;
                }
                // Someone owns it at our turn: a recovery advanced onto our ticket while a
                // peer still held it. (Until 2026-11-11 an out-of-band acquire without a
                // ticket could also land here; it went with the SGI scheduler.)
                //
                // Do NOT re-ticket here. Abandoning `my_ticket` at the exact moment
                // `now_serving` sits on it LEAKS the slot: nothing else consumes that
                // allocation, so `now_serving` ends one short of `next_ticket`
                // and the next contended acquirer freezes until `LOST_TICKET_RECOVERY_SPINS`
                // forces an `advanced-lost`. Measured 1:1 — 25 `reticket-owned` produced
                // exactly 25 `advanced-lost` over 30 `bssfork 20 3 1` runs, with every
                // intervening `[BKL] stuck` line reading `owner=0` (lock free, queue frozen).
                //
                // Instead keep our place and spin. The holder's release advances
                // `now_serving` past us, landing us in the `skipped` path below — which is
                // balanced, because that advance is what consumed our slot. Falling through
                // (rather than `continue`) keeps the stuck detector and spin accounting live.
            }
            if (serving.wrapping_sub(my_ticket) as i32) > 0 {
                // `now_serving` passed us (skipped shape above): rejoin the queue.
                my_ticket = self.next_ticket.fetch_add(1, Ordering::Relaxed);
                log_kernel_lock_recovered(me, "reticket-skipped");
                continue;
            }
            let cur_owner = self.owner.load(Ordering::Relaxed);
            if cur_owner == 0 && serving == frozen_serving {
                frozen_spins = frozen_spins.wrapping_add(1);
                if frozen_spins >= LOST_TICKET_RECOVERY_SPINS {
                    frozen_spins = 0;
                    if self
                        .now_serving
                        .compare_exchange(
                            serving,
                            serving.wrapping_add(1),
                            Ordering::Release,
                            Ordering::Relaxed,
                        )
                        .is_ok()
                    {
                        KERNEL_LOCK_LOST_TICKET_RECOVERIES.fetch_add(1, Ordering::Relaxed);
                        log_kernel_lock_recovered(me, "advanced-lost");
                    }
                    continue;
                }
            } else {
                frozen_serving = serving;
                frozen_spins = 0;
            }
            if wait_tag == u64::MAX && PROFILE_ENABLED.load(Ordering::Relaxed) && cur_owner != 0 {
                // Sample the blocking owner core's tag (owner encodes core `cur - 1`).
                let owner_core = (cur_owner - 1) as usize;
                if owner_core < PROFILE_MAX_CORES {
                    wait_tag = HOLDER_TAG[owner_core].0.load(Ordering::Relaxed);
                }
            }
            spins = spins.wrapping_add(1);
            total_spins = total_spins.wrapping_add(1);
            // WAIT-STATE SAMPLE: only after a FULL SECOND of spinning (2^20
            // spins), power-of-two steps — a waiter that is merely contended
            // never prints, so the console bandwidth survives. The 2026-09-20
            // lesson: an unconditional print here flood-wedged the box (every
            // uncontended acquire passes the wait loop's first iteration).
            if total_spins > MAX_WAIT_SPINS.load(Ordering::Relaxed) {
                MAX_WAIT_SPINS.store(total_spins, Ordering::Relaxed);
            }
            // Only the waiter next in line samples: the others are waiting on the
            // same hold one place further back, and saying so again from each of them
            // is what flooded the console (see `STUCK_REPORTED`).
            if total_spins >= 1_048_576
                && total_spins.is_power_of_two()
                && my_ticket == self.now_serving.load(Ordering::Relaxed).wrapping_add(1)
            {
                akuma_primitives::console::print_args_if_registered::<128>(format_args!(
                    "[bkls>] core={} ticket={} serving={} owner={} spins={}\n",
                    me, my_ticket,
                    self.now_serving.load(Ordering::Acquire),
                    self.owner.load(Ordering::Relaxed),
                    total_spins
                ));
            }
            if spins == SPIN_WARN_THRESHOLD {
                spins = 0;
                log_kernel_lock_stuck(self.owner.load(Ordering::Relaxed), me, self.now_serving.load(Ordering::Relaxed));
            }
            #[cfg(all(target_os = "none", target_arch = "x86_64"))]
            spin_assist();
            core::hint::spin_loop();
        }
        // Accumulate this acquire's spin count once (a cross-core BKL-wait-time proxy; see
        // `contention_spins`). The uncontended fast path (our turn immediately) adds nothing.
        if total_spins > 0 {
            CONTENTION_SPINS.fetch_add(total_spins, Ordering::Relaxed);
            if PROFILE_ENABLED.load(Ordering::Relaxed) {
                let bucket = if wait_tag == u64::MAX {
                    HOLD_TAG_UNKNOWN as usize
                } else {
                    (wait_tag as usize).min(PROFILE_BUCKETS - 1)
                };
                WAIT_BY_HOLDER[bucket].fetch_add(total_spins, Ordering::Relaxed);
            }
        }
        irq_restore(daif);
    }

    /// Ensure `core_id` does not own the lock, freeing it for the next waiter in FIFO
    /// order. Idempotent: a no-op if this core does not own it. Must run with local IRQs
    /// masked by the current owner.
    #[inline]
    pub fn release(&self, core_id: u32) {
        let me = core_id + 1;
        // Only the owner may free it; releasing what you don't hold is a no-op (the
        // reconciliation path can legitimately call this after a sibling core's
        // excursion already moved the lock). Clear `owner` to 0 BEFORE advancing
        // `now_serving` so the next ticket winner sees a free lock the instant it is
        // handed the turn (the `owner.store(me)` in `acquire` relies on this ordering).
        if self
            .owner
            .compare_exchange(me, 0, Ordering::Release, Ordering::Relaxed)
            .is_ok()
        {
            // Hand the lock to the next waiter in arrival order (exactly one advance per
            // owning release — reentrant/idempotent releases don't reach here, keeping
            // `now_serving` balanced against the tickets `acquire` handed out).
            self.now_serving.fetch_add(1, Ordering::Release);
        }
    }

    /// `true` if `core_id` currently owns the lock.
    #[inline]
    pub fn held_by(&self, core_id: u32) -> bool {
        self.owner.load(Ordering::Relaxed) == core_id + 1
    }

    /// `true` if any core owns the lock.
    #[inline]
    pub fn is_held(&self) -> bool {
        self.owner.load(Ordering::Relaxed) != 0
    }
}

/// Diagnostic: log when the Big Kernel Lock is stuck spinning (a cross-core deadlock
/// canary). Stack-buffered to avoid heap use in an IRQ-masked context.
fn log_kernel_lock_stuck(owner: u32, me: u32, serving: u32) {
    let Some(folded) = claim_stuck_report(stuck_episode(owner, serving)) else {
        return;
    };
    // Attribute the hold: what the owner core was doing when it last entered the kernel
    // (tag: syscall nr, 500=fault, 501=IRQ/scheduler, 511=unknown; see the profiler above).
    // Only meaningful while `set_profiling(true)` — otherwise reads as 511 — but always
    // printed so a stuck episode during a profiled window self-attributes.
    let tag = if owner >= 1 && ((owner - 1) as usize) < PROFILE_MAX_CORES {
        HOLDER_TAG[(owner - 1) as usize].0.load(Ordering::Relaxed)
    } else {
        HOLD_TAG_UNKNOWN
    };
    // `print_args_if_registered` first: a contended acquire can run before/without a
    // registered runtime (host unit tests drive `KernelLock` directly), and a diagnostic
    // must never be the thing that panics.
    // `folded`: lines from other waiters (or later rounds of this one) on episodes
    // already reported, since the last line. `stuck: owner=` stays the grep key.
    akuma_primitives::console::print_args_if_registered::<112>(format_args!(
        "[BKL] stuck: owner={owner} waiter={me} tag={tag} serving={serving} folded={folded} (aff0+1)\n"
    ));
}

/// Diagnostic: log when [`KernelLock::acquire`]'s self-healing fired (see the recovery
/// note there). Every line here is a live sighting of the ticket-accounting leak —
/// keep them until the leak is root-caused. Stack-buffered (IRQ-masked context).
fn log_kernel_lock_recovered(me: u32, kind: &str) {
    KERNEL_LOCK_RECOVERIES.fetch_add(1, Ordering::Relaxed);
    // See `log_kernel_lock_stuck`: `print_args_if_registered` skips the print (and the
    // formatting) so a host unit test driving `KernelLock` without a registered runtime
    // records the counter above and stays quiet.
    akuma_primitives::console::print_args_if_registered::<96>(format_args!(
        "[BKL] RECOVERED ({kind}) by core {me} (aff0+1)\n"
    ));
}


/// Spin iteration limit before logging a diagnostic (helps debug deadlocks).
const SPIN_WARN_THRESHOLD: u32 = 10_000_000;

/// Consecutive free-lock/frozen-FIFO spins before [`KernelLock::acquire`] concludes the
/// currently-served ticket has no living waiter and advances `now_serving` itself (see
/// the self-healing note in `acquire`). ~2× the stuck-warn threshold: tens of
/// milliseconds — far beyond the ns-scale hand-off windows this state legitimately
/// occupies, but short enough that a wedged box recovers before watchdogs cascade.
const LOST_TICKET_RECOVERY_SPINS: u32 = 20_000_000;


#[cfg(test)]
mod thread_tag_tests {
    use super::*;

    /// The three operations the kernel performs on the attribution tables, replayed
    /// against a private core cache so the tests are deterministic and can't collide with
    /// the process-global `HOLDER_TAG` other tests touch. Mirrors `set_holder_tag`,
    /// `set_core_tag_transient` and `load_thread_tag_to_core` exactly.
    struct Model<const NT: usize, const NC: usize> {
        threads: ThreadTagTable<NT>,
        cores: [u64; NC],
        /// Which thread each core is currently running (the kernel's TPIDRRO_EL0).
        running: [usize; NC],
    }

    impl<const NT: usize, const NC: usize> Model<NT, NC> {
        fn new() -> Self {
            Self {
                threads: ThreadTagTable::new(),
                cores: [HOLD_TAG_UNKNOWN; NC],
                running: [0; NC],
            }
        }
        /// Kernel entry (syscall / fault): publish to BOTH tables.
        fn enter_kernel(&mut self, core: usize, tag: u64) {
            self.threads.set(self.running[core], tag);
            self.cores[core] = tag;
        }
        /// Transient nested excursion (IRQ dispatch): core cache only.
        fn irq_stamp(&mut self, core: usize) {
            self.cores[core] = HOLD_TAG_IRQ;
        }
        /// The current thread changed (`set_current_thread_register`).
        fn switch_to(&mut self, core: usize, tid: usize) {
            self.running[core] = tid;
            self.cores[core] = self.threads.get(tid);
        }
        /// End of IRQ dispatch: re-point the cache at whoever runs now.
        fn irq_epilogue(&mut self, core: usize) {
            self.cores[core] = self.threads.get(self.running[core]);
        }
        /// What a peer waiting on `core`'s BKL hold would credit its spins to.
        fn sampled_by_waiter(&self, core: usize) -> u64 {
            self.cores[core]
        }
    }

    const SYS_WRITE: u64 = 64;
    const SYS_CLONE: u64 = 220;

    /// The bug this whole change exists to kill (docs/archive/BKL_VFS_CARVE_OUT.md §16.2):
    /// a thread preempted mid-syscall used to spend the ENTIRE remainder of that syscall
    /// labelled `irq/sched`, because the tag lived only on the core. Attribution must
    /// follow the thread back.
    #[test]
    fn tag_survives_preemption_and_resume() {
        let mut m: Model<8, 2> = Model::new();
        m.switch_to(0, 3);
        m.enter_kernel(0, SYS_WRITE); // thread 3 starts a long write
        assert_eq!(m.sampled_by_waiter(0), SYS_WRITE);

        // Timer tick lands mid-write; the scheduler picks thread 5.
        m.irq_stamp(0);
        m.switch_to(0, 5);
        m.irq_epilogue(0);
        assert_eq!(
            m.threads.get(3),
            SYS_WRITE,
            "the interrupted thread's own tag must not be clobbered by the transient IRQ"
        );

        // Thread 3 resumes — still inside the same write, and must be attributed as such.
        m.switch_to(0, 3);
        assert_eq!(
            m.sampled_by_waiter(0),
            SYS_WRITE,
            "resuming a preempted syscall must NOT read as irq/sched"
        );
    }

    /// An IRQ that switches to a thread which was itself preempted mid-syscall must
    /// install THAT thread's tag, not `irq/sched`. This is the half §16.2.1 deliberately
    /// left unfixed ("honest, not guessed") — it is no longer a guess, it is a lookup.
    #[test]
    fn switch_installs_incoming_threads_own_tag() {
        let mut m: Model<8, 2> = Model::new();
        // Thread 5 got as far as a clone on core 1, then was preempted away.
        m.switch_to(1, 5);
        m.enter_kernel(1, SYS_CLONE);
        m.switch_to(1, 2);

        // Core 0 now takes an IRQ and the scheduler hands it thread 5.
        m.switch_to(0, 4);
        m.enter_kernel(0, SYS_WRITE);
        m.irq_stamp(0);
        m.switch_to(0, 5);
        m.irq_epilogue(0);
        assert_eq!(
            m.sampled_by_waiter(0),
            SYS_CLONE,
            "the incoming thread carries its own excursion's tag across cores"
        );
    }

    /// With no context switch, the epilogue must restore the interrupted thread's tag —
    /// the case §16.2.1 fixed with an explicit save/restore, now falling out of the same
    /// single rule.
    #[test]
    fn irq_without_switch_restores_interrupted_tag() {
        let mut m: Model<8, 1> = Model::new();
        m.switch_to(0, 1);
        m.enter_kernel(0, SYS_WRITE);
        m.irq_stamp(0);
        assert_eq!(m.sampled_by_waiter(0), HOLD_TAG_IRQ, "IRQ dispatch is honestly IRQ");
        m.irq_epilogue(0);
        assert_eq!(m.sampled_by_waiter(0), SYS_WRITE);
    }

    /// Two cores running two threads must not read each other's attribution.
    #[test]
    fn cores_and_threads_stay_isolated() {
        let mut m: Model<8, 2> = Model::new();
        m.switch_to(0, 1);
        m.switch_to(1, 2);
        m.enter_kernel(0, SYS_WRITE);
        m.enter_kernel(1, SYS_CLONE);
        assert_eq!(m.sampled_by_waiter(0), SYS_WRITE);
        assert_eq!(m.sampled_by_waiter(1), SYS_CLONE);
        assert_eq!(m.threads.get(1), SYS_WRITE);
        assert_eq!(m.threads.get(2), SYS_CLONE);
    }

    /// Storage contract: unknown by default, clamped into the histogram's bucket range,
    /// resettable for recycled slots, inert for out-of-range tids.
    #[test]
    fn thread_tag_table_defaults_clamp_reset_and_bounds() {
        let t: ThreadTagTable<4> = ThreadTagTable::new();
        assert_eq!(t.get(0), HOLD_TAG_UNKNOWN, "a thread starts unattributed");

        t.set(0, SYS_WRITE);
        assert_eq!(t.get(0), SYS_WRITE);
        assert_eq!(t.get(1), HOLD_TAG_UNKNOWN, "tags are per-thread");

        // A syscall number past the bucket array must clamp, never index out of bounds.
        t.set(1, 100_000);
        assert_eq!(t.get(1), (PROFILE_BUCKETS - 1) as u64);

        // Recycled slot.
        t.reset(0);
        assert_eq!(t.get(0), HOLD_TAG_UNKNOWN);

        // Out-of-range tid: no panic, reads as unknown.
        t.set(99, SYS_WRITE);
        assert_eq!(t.get(99), HOLD_TAG_UNKNOWN);
        t.reset(99);
    }

    /// The kernel's real accessors must be no-ops while the profiler is off (the default),
    /// so a non-measurement build pays nothing and reads as unknown.
    #[test]
    fn accessors_are_inert_while_profiling_is_off() {
        assert!(
            !PROFILE_ENABLED.load(Ordering::Relaxed),
            "profiler must default off"
        );
        set_holder_tag(0, SYS_WRITE);
        set_core_tag_transient(0, HOLD_TAG_IRQ);
        load_thread_tag_to_core(0, 0);
        assert_eq!(thread_tag(0), HOLD_TAG_UNKNOWN);
        assert_eq!(core_tag(0), HOLD_TAG_UNKNOWN);
        // Out-of-range core is inert too.
        assert_eq!(core_tag(999), HOLD_TAG_UNKNOWN);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One stuck hold is one line, not one per waiter per round, but a hold that never
    /// ends keeps a heartbeat, so a wedge still counts as a storm (`STUCK_REPORTED`).
    /// The only test touching these statics, so the sequence below is its own.
    #[test]
    fn a_stuck_episode_is_reported_once_then_every_eighth_fold() {
        let a = stuck_episode(3, 0xdead_0001);
        let b = stuck_episode(3, 0xdead_0002);
        assert!(claim_stuck_report(a).is_some(), "the first waiter on a new episode reports it");
        let before = kernel_lock_stuck_suppressed();
        for i in 1..STUCK_REPRINT_EVERY {
            assert_eq!(claim_stuck_report(a), None, "fold {i} of the same hold is quiet");
        }
        assert_eq!(kernel_lock_stuck_suppressed() - before, STUCK_REPRINT_EVERY - 1);
        assert_eq!(claim_stuck_report(a), Some(STUCK_REPRINT_EVERY), "a hold that goes on prints again, with the count");
        assert_eq!(claim_stuck_report(a), None);
        assert_eq!(claim_stuck_report(b), Some(1), "a new hold prints at once, carrying what was folded");
        assert_ne!(stuck_episode(1, 7), stuck_episode(2, 7), "owner is part of the key");
    }

    #[test]
    fn kernel_lock_acquire_release_single_core() {
        let bkl = KernelLock::new();
        assert!(!bkl.is_held());
        assert!(!bkl.held_by(0));
        bkl.acquire(0);
        assert!(bkl.is_held());
        assert!(bkl.held_by(0));
        bkl.release(0);
        assert!(!bkl.is_held());
        assert!(!bkl.held_by(0));
    }

    #[test]
    fn kernel_lock_acquire_is_idempotent_for_owner() {
        let bkl = KernelLock::new();
        bkl.acquire(2);
        bkl.acquire(2); // nested (e.g. IRQ/fault while already in a syscall)
        bkl.acquire(2);
        assert!(bkl.held_by(2));
        // A single release frees it — there is one EL1→EL0 return per excursion.
        bkl.release(2);
        assert!(!bkl.is_held());
    }

    #[test]
    fn kernel_lock_release_by_non_owner_is_noop() {
        let bkl = KernelLock::new();
        bkl.acquire(1);
        bkl.release(0); // core 0 doesn't own it
        assert!(bkl.held_by(1), "non-owner release must not free the lock");
        bkl.release(1);
        assert!(!bkl.is_held());
    }

    #[test]
    fn kernel_lock_ownership_transfers_between_cores() {
        let bkl = KernelLock::new();
        bkl.acquire(0);
        assert!(bkl.held_by(0));
        assert!(!bkl.held_by(1));
        bkl.release(0);
        // Now a different core can take it.
        bkl.acquire(1);
        assert!(bkl.held_by(1));
        assert!(!bkl.held_by(0));
        bkl.release(1);
        assert!(!bkl.held_by(1));
    }

    #[test]
    fn kernel_lock_held_by_only_owner() {
        let bkl = KernelLock::new();
        bkl.acquire(3);
        for other in [0u32, 1, 2, 4, 5] {
            assert!(!bkl.held_by(other));
        }
        assert!(bkl.held_by(3));
        bkl.release(3);
    }

    #[test]
    fn kernel_lock_ticket_counters_stay_balanced() {
        // Every EL0→EL1 crossing takes one FIFO ticket and every EL1→EL0 crossing advances
        // `now_serving` once; reentrant acquires and idempotent (non-owner / double)
        // releases must take NO ticket and NOT advance, or the counters drift and a later
        // acquire would wait forever for a `now_serving` value that never arrives. Exercise
        // exactly those cases across cores; a drift would hang this test.
        let bkl = KernelLock::new();
        for round in 0..2000u32 {
            let core = round % 4;
            bkl.acquire(core);
            bkl.acquire(core); // nested — takes no ticket
            assert!(bkl.held_by(core));
            bkl.release(core);
            bkl.release(core); // idempotent — must NOT advance now_serving
            assert!(!bkl.is_held(), "round {round}: lock not free after release");
        }
        // Still acquirable after all that (counters didn't drift).
        bkl.acquire(1);
        assert!(bkl.held_by(1));
        bkl.release(1);
        assert!(!bkl.is_held());
    }

    #[test]
    fn kernel_lock_midexcursion_drop_reacquire_stays_balanced() {
        // Models the `no-bkl-network` net-syscall guard (Phase 2 of the BKL
        // fine-graining plan, src/syscall/net.rs `NetBklGuard`): a syscall wrapper
        // holds the BKL, then a subsystem DROPS it mid-excursion to run BKL-free and
        // RE-ACQUIRES it before the wrapper's single release. So per syscall the lock
        // sees enter → (drop) release → (re-acquire) acquire → release: two acquires,
        // two releases, each acquire taking a fresh ticket. If the extra
        // release/acquire pair drifted the ticket counters, a later core's acquire
        // would wait forever for a `now_serving` that never arrives — this test would
        // hang. Also exercise a peer stealing the lock in the BKL-free window (the
        // whole point of dropping it), which must not break FIFO balance.
        let bkl = KernelLock::new();
        for round in 0..2000u32 {
            let core = round % 4;
            let peer = (core + 1) % 4;
            // Wrapper entry (EL0→EL1).
            bkl.acquire(core);
            assert!(bkl.held_by(core));
            // Guard drop: run the syscall body BKL-free.
            bkl.release(core);
            assert!(!bkl.is_held(), "round {round}: not free in BKL-free window");
            // A peer core enters the kernel while we're BKL-free, then leaves.
            bkl.acquire(peer);
            assert!(bkl.held_by(peer));
            bkl.release(peer);
            // Guard re-acquire for the return path, then wrapper release (EL1→EL0).
            bkl.acquire(core);
            assert!(bkl.held_by(core));
            bkl.release(core);
            assert!(!bkl.is_held(), "round {round}: not free after wrapper release");
        }
        // Counters didn't drift: still cleanly acquirable.
        bkl.acquire(2);
        assert!(bkl.held_by(2));
        bkl.release(2);
        assert!(!bkl.is_held());
    }

    #[test]
    fn preempt_guard_constructs_and_nests() {
        // The lifted `PreemptGuard` (moved here from akuma-net so the `no-bkl-vfs` VFS path
        // could share it) is taken *nested* in the fs path: an ext2 `state` hold can sit
        // inside another guarded region, and `threading::disable_preemption` is a per-thread
        // COUNTER, so the inner drop must not re-enable preemption for the outer holder.
        // On host builds (no `smp-shared`) both are no-ops; this pins the API + drop order
        // so the kernel-only nesting can't be broken by a refactor here.
        let outer = PreemptGuard::new();
        {
            let inner = PreemptGuard::default();
            drop(inner);
        }
        drop(outer);
    }

    #[test]
    fn vfs_bkl_guard_latched_arm_stays_balanced_across_toggle_flip() {
        // Models `src/syscall/fs.rs`'s `VfsBklGuard`, which differs from `NetBklGuard` in
        // one dangerous way: it consults a RUNTIME toggle (`vfs_bkl_drop_enabled`) rather
        // than only a cfg. The toggle is flipped while guards can be live — the A/B boot
        // self-tests flip it between phases, and it doubles as a kill switch.
        //
        // The guard therefore latches its decision at construction. Here we prove why: a
        // guard that re-read the toggle on drop would, on an ON→OFF flip mid-syscall, skip
        // its re-acquire, and the syscall wrapper's single release would then advance
        // `now_serving` for a ticket this core does not own — corrupting the FIFO for every
        // other core. Replay both a latched guard and (as the counter-example) an
        // unlatched one, and assert only the latched one keeps the lock balanced.
        let bkl = KernelLock::new();

        // Latched: `armed` is decided at `new()` and honored on `drop()`, whatever the
        // toggle says by then.
        for round in 0..500u32 {
            let core = round % 4;
            let toggle_at_new = round % 2 == 0;
            bkl.acquire(core); // wrapper entry (EL0->EL1)

            let armed = toggle_at_new; // <- latched
            if armed {
                bkl.release(core);
            }
            // Toggle flips mid-syscall, in BOTH directions across rounds.
            let _toggle_now = !toggle_at_new;
            if armed {
                bkl.acquire(core);
            }

            assert!(
                bkl.held_by(core),
                "round {round}: latched guard left the BKL unheld before wrapper release"
            );
            bkl.release(core); // wrapper exit (EL1->EL0)
            assert!(!bkl.is_held(), "round {round}: BKL not free after wrapper release");
        }

        // Counter-example: re-reading the toggle on drop. Take the ON->OFF case, where the
        // guard drops the lock and then declines to re-acquire it.
        bkl.acquire(0);
        let armed_at_new = true;
        if armed_at_new {
            bkl.release(0);
        }
        let armed_at_drop = false; // toggle flipped to OFF mid-syscall
        if armed_at_drop {
            bkl.acquire(0);
        }
        assert!(
            !bkl.held_by(0),
            "the unlatched counter-example is supposed to lose the lock — if it holds it, \
             this test no longer demonstrates the hazard the latch exists to prevent"
        );

        // Balanced again for the next user.
        bkl.acquire(0);
        assert!(bkl.held_by(0));
        bkl.release(0);
        assert!(!bkl.is_held());
    }
}
