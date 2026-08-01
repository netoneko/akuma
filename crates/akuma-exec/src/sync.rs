//! Synchronization primitives for akuma-exec.
//!
//! Provides `RwSpinlock<T>` — a reader-writer spinlock built on `lock_api`
//! with writer priority to prevent reader starvation — and `KernelLock`, the
//! recursive Big Kernel Lock used by real (shared-kernel) SMP.

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// Mask local IRQs (set `DAIF.I`) and return the prior `DAIF` for [`irq_restore`]. Used by
/// [`KernelLock::acquire`] to make its FIFO ticket wait atomic against local exception
/// nesting, and by [`PreemptGuard`] to make inner-spinlock critical sections nest-free
/// under a dropped BKL. Bare-metal AArch64 only; a no-op returning `0` on host builds
/// (single-threaded tests have no local IRQs).
#[cfg(target_os = "none")]
#[inline(always)]
pub fn irq_save_mask() -> u64 {
    let daif: u64;
    // SAFETY: reading DAIF and setting the IRQ mask bit have no memory effects.
    unsafe {
        core::arch::asm!("mrs {}, daif", out(reg) daif, options(nomem, nostack));
        core::arch::asm!("msr daifset, #0x2", options(nomem, nostack));
    }
    daif
}

/// Restore `DAIF` saved by [`irq_save_mask`]. Bare-metal AArch64 only; no-op on host.
#[cfg(target_os = "none")]
#[inline(always)]
pub fn irq_restore(daif: u64) {
    // SAFETY: restoring the previously-saved DAIF; no memory effects.
    unsafe { core::arch::asm!("msr daif, {}", in(reg) daif, options(nomem, nostack)) };
}

#[cfg(not(target_os = "none"))]
#[inline(always)]
pub fn irq_save_mask() -> u64 {
    0
}

#[cfg(not(target_os = "none"))]
#[inline(always)]
pub fn irq_restore(_daif: u64) {}

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

/// Snapshot the BKL ticket-recovery counter (see [`KERNEL_LOCK_RECOVERIES`]). Healthy runs
/// keep this at 0.
pub fn kernel_lock_recoveries() -> u64 {
    KERNEL_LOCK_RECOVERIES.load(Ordering::Relaxed)
}

// --- PreemptGuard ---------------------------------------------------------------

/// RAII guard that disables scheduler preemption (and, under the BKL-drop features,
/// masks local IRQs) for the lifetime of a kernel spinlock critical section.
///
/// Under real shared-kernel SMP (`smp-shared` feature), the inner spinlocks protecting
/// kernel state (`NETWORK`, `SOCKET_TABLE`, `Ext2Filesystem::state`, `BLOCK_DEVICE`,
/// `proc.fds.table`, ...) must never be held across a context switch. The Big Kernel
/// Lock is released on an EL1→EL0 return, so a thread descheduled while holding one of
/// these inner spinlocks strands the lock — and any *other* core that then spins on it
/// does so **while holding the BKL**, wedging every core (the BKL owner can never be
/// rescheduled to release the inner lock).
///
/// Disabling preemption for the hold keeps the holder on-core until it releases, so
/// under the BKL the inner lock is never cross-core contended (the BKL already provides
/// the mutual exclusion; this guard only prevents the strand). The critical sections it
/// wraps must never voluntarily yield/block — same rule the kernel already followed on
/// single-core.
///
/// A zero-cost no-op on every non-`smp-shared` build: `new`/`drop` compile to nothing,
/// so the hot path is byte-for-byte unchanged.
///
/// **`no-bkl-*` addition — local IRQs are masked for the hold too.** With the Big
/// Kernel Lock dropped around a subsystem's syscalls (network: `no-bkl-network`, VFS:
/// `no-bkl-vfs`), this core can be inside one of those critical sections *without*
/// owning the BKL. A nested IRQ then runs `enter_kernel()` (exceptions.rs), which
/// hard-spins (IRQs masked) until the BKL frees — while THIS core still holds the inner
/// spinlock. If the current BKL owner is meanwhile spinning on that same inner lock (the
/// async-main poller does exactly that on `NETWORK`, near-constantly), the two cores
/// deadlock AB-BA and every other core piles into the BKL wait: the SMP=4 hard wedge
/// (`[BKL] stuck`, owner frozen nonzero, guest timer starved). Masking IRQs for the
/// (short) hold makes the window nest-free, so a core can never be caught "holding an
/// inner lock, waiting for the BKL". Plain `smp-shared` builds don't need it — there
/// EL1 always holds the BKL, so the nested `enter_kernel` is the idempotent owner fast
/// path.
///
/// # Lift history
///
/// Originally `akuma-net::runtime::PreemptGuard` (where it was wired through the
/// `NetRuntime` registration callbacks so the net crate could stay decoupled from
/// `akuma-exec`). Lifted here so the VFS BKL-drop path (`no-bkl-vfs`) can reuse the
/// same primitive without duplicating it, and because `akuma-exec` owns both
/// `threading::disable_preemption` and `irq_save_mask` directly — no runtime callback
/// indirection needed. The net crate re-exports it from
/// `akuma_net::runtime::PreemptGuard` for source compatibility.
#[must_use]
pub struct PreemptGuard {
    /// Whether `new()` actually disabled preemption. Present only under `smp-shared`;
    /// other builds carry no state.
    #[cfg(feature = "smp-shared")]
    active: bool,
    /// Saved DAIF to restore on drop (`no-bkl-*` builds only — see type doc).
    #[cfg(all(feature = "smp-shared", any(feature = "no-bkl-network", feature = "no-bkl-vfs")))]
    saved_daif: u64,
}

impl PreemptGuard {
    /// Disable preemption (under `smp-shared`) until the returned guard drops.
    #[inline]
    pub fn new() -> Self {
        #[cfg(feature = "smp-shared")]
        {
            // Direct call: akuma-exec owns threading. No runtime registration needed
            // (the historical reason net used a callback pointer), so this works during
            // early boot and in host tests alike.
            crate::threading::disable_preemption();
            let active = true;
            // Mask IRQs AFTER disabling preemption so drop's reverse order re-enables
            // preemption only once IRQs are live again.
            #[cfg(any(feature = "no-bkl-network", feature = "no-bkl-vfs"))]
            return Self { active, saved_daif: irq_save_mask() };
            #[cfg(not(any(feature = "no-bkl-network", feature = "no-bkl-vfs")))]
            return Self { active };
        }
        #[cfg(not(feature = "smp-shared"))]
        Self {}
    }
}

impl Default for PreemptGuard {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for PreemptGuard {
    #[inline]
    fn drop(&mut self) {
        // Restore IRQs first (reverse of new's order), then re-enable preemption — so
        // a timer IRQ can't preempt us between enable_preemption and the DAIF restore.
        #[cfg(all(feature = "smp-shared", any(feature = "no-bkl-network", feature = "no-bkl-vfs")))]
        irq_restore(self.saved_daif);
        #[cfg(feature = "smp-shared")]
        if self.active {
            crate::threading::enable_preemption();
        }
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
const PROFILE_MAX_CORES: usize = 8;
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
static THREAD_TAG: ThreadTagTable<{ crate::threading::MAX_THREADS }> = ThreadTagTable::new();

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
    THREAD_TAG.set(crate::threading::current_thread_id(), tag);
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
/// (EL1).** It is *reconciled* at every EL transition rather than balanced like an
/// ordinary lock: entry from EL0 acquires it; an `eret` back to EL0 releases it; a
/// nested exception taken while already in EL1, and the `eret` back to EL1 from it,
/// leave it held (the target is still EL1). Because there is exactly one EL1→EL0
/// return per kernel excursion, there is exactly one release per excursion — no
/// per-thread depth needs to travel across context switches. This upgrades the
/// kernel's pervasive single-core `with_irqs_disabled` invariant (mutual exclusion on
/// one core only) into a genuine cross-core one, so the ~218 legacy
/// `lookup_process() -> &'static mut Process` sites become correct without per-site
/// changes (docs/archive/SMP_SHARED.md, M1). Uncontended on a single-core build and in
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
/// by the releaser immediately re-grabbing). This is what makes the M5c step-2 BKL-free
/// EL0 scheduler safe: without it, a secondary that must re-enter EL1 to un-strand a
/// BKL-free-stolen thread could lose the plain test-and-set race indefinitely (a livelock;
/// see `smp_shared::SCHED_BKLFREE_EL0_ENABLED`).
///
/// `acquire`/`release` remain **idempotent** for the owner (re-acquiring what you hold, or
/// releasing what you don't, is a no-op) — the reentrant/reconcile cases take NO ticket, so
/// the ticket counters stay balanced across the non-lexical acquire/release that context
/// switches create (one ticket per EL0→EL1 crossing, one `now_serving` advance per
/// EL1→EL0 crossing, per core).
pub struct KernelLock {
    /// `0` = free; otherwise `owner_core_aff0 + 1`. Written by the ticket winner on
    /// acquire and cleared by the owner on release; the source of truth for
    /// `held_by`/`is_held`/reconcile and the reentrant fast path.
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
    #[inline]
    pub fn acquire(&self, core_id: u32) {
        let me = core_id + 1;
        // Reentrant fast path: this core already owns it (a nested EL1 exception, or a
        // reconcile-acquire that finds we never left EL1). No ticket, no wait.
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
                // Someone owns it at our turn (recovery race): rejoin the queue.
                my_ticket = self.next_ticket.fetch_add(1, Ordering::Relaxed);
                log_kernel_lock_recovered(me, "reticket-owned");
                continue;
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
            if spins == SPIN_WARN_THRESHOLD {
                spins = 0;
                log_kernel_lock_stuck(self.owner.load(Ordering::Relaxed), me);
            }
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

    /// Reconcile the lock to the EL this core is about to run in: acquire when
    /// returning to / staying in EL1, release when returning to EL0. This is the
    /// single operation the `eret` epilogues call, keeping the invariant "held iff in
    /// EL1" true across context switches that change the target EL.
    #[inline]
    pub fn reconcile(&self, core_id: u32, target_is_el0: bool) {
        if target_is_el0 {
            self.release(core_id);
        } else {
            self.acquire(core_id);
        }
    }

    /// Ticket-free variant of reconcile for use after BKL-free scheduler paths.
    /// When we run the scheduler BKL-free (M5c step-2), we never called `enter_kernel`,
    /// so a reconcile that targets EL1 must acquire without taking a ticket — otherwise
    /// we leak a ticket (next_ticket advances with no matching now_serving advance).
    /// This variant uses `acquire_no_ticket` for the acquire case.
    #[inline]
    pub fn reconcile_no_ticket(&self, core_id: u32, target_is_el0: bool) {
        if target_is_el0 {
            self.release(core_id);
        } else {
            self.acquire_no_ticket(core_id);
        }
    }

    /// Acquire the lock without taking a FIFO ticket. Used only by the BKL-free
    /// scheduler reconcile path (M5c step-2) where we never called `enter_kernel`,
    /// so we must not disturb the ticket accounting. Still idempotent and IRQ-masked
    /// for the same migration-atomicity reasons as `acquire`.
    #[inline]
    pub fn acquire_no_ticket(&self, core_id: u32) {
        let me = core_id + 1;
        // Reentrant fast path: this core already owns it. No ticket, no wait.
        if self.owner.load(Ordering::Acquire) == me {
            return;
        }
        let daif = irq_save_mask();
        // Re-check after masking: a nested IRQ may have already acquired for this core.
        if self.owner.load(Ordering::Acquire) == me {
            irq_restore(daif);
            return;
        }
        // Spin-wait for ownership WITHOUT waiting on a ticket. This is only safe when
        // we know the lock will be released by a path that doesn't expect our ticket
        // (i.e., the BKL-free scheduler path where we're reconciling to EL1 after
        // having run BKL-free). The wait is unfair here, but the BKL-free path is
        // already a special case — the normal reconcile path uses the fair ticket lock.
        let mut spins: u32 = 0;
        loop {
            // Try to take ownership directly if the lock is free.
            if self
                .owner
                .compare_exchange(0, me, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                // Account for the serving slot this barge consumed.
                //
                // We did not WAIT on a ticket, but the hold we just took still ends in an
                // ordinary `release` — the thread we reconcile into returns to EL0 through
                // the normal epilogue (`reconcile_for_spsr` → `release`), which advances
                // `now_serving` by one. Advancing without a matching allocation drives
                // `now_serving` AHEAD of `next_ticket`, and from then on every contended
                // acquirer takes a ticket already behind `now_serving`, is told it was
                // skipped, and re-tickets — the `[BKL] RECOVERED (reticket-skipped)` bursts
                // measured at SMP=4 under fork/exec churn, which degrade the fair lock to
                // an unfair test-and-set for the length of the burst.
                //
                // Bumping here (AFTER winning ownership, so a losing CAS allocates nothing)
                // keeps the pairing exact. It is also the right FIFO semantics: if a waiter
                // was sitting at `now_serving` when we barged in, its own CAS just failed
                // and it re-ticketed to the tail ("reticket-owned"), leaving that slot
                // vacant — so our release *should* advance past it. If there was no queue,
                // we allocate and consume one slot and the counters return to equal.
                self.next_ticket.fetch_add(1, Ordering::Relaxed);
                irq_restore(daif);
                return;
            }
            // Check if we already own it (reentrant case after a successful CAS above).
            if self.owner.load(Ordering::Acquire) == me {
                irq_restore(daif);
                return;
            }
            spins = spins.wrapping_add(1);
            if spins == SPIN_WARN_THRESHOLD {
                spins = 0;
                log_kernel_lock_stuck(self.owner.load(Ordering::Relaxed), me);
            }
            core::hint::spin_loop();
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
fn log_kernel_lock_stuck(owner: u32, me: u32) {
    use core::fmt::Write;
    struct Buf([u8; 96], usize);
    impl Write for Buf {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
            let b = s.as_bytes();
            let n = b.len().min(96 - self.1);
            self.0[self.1..self.1 + n].copy_from_slice(&b[..n]);
            self.1 += n;
            Ok(())
        }
    }
    let mut buf = Buf([0u8; 96], 0);
    // Attribute the hold: what the owner core was doing when it last entered the kernel
    // (tag: syscall nr, 500=fault, 501=IRQ/scheduler, 511=unknown; see the profiler above).
    // Only meaningful while `set_profiling(true)` — otherwise reads as 511 — but always
    // printed so a stuck episode during a profiled window self-attributes.
    let tag = if owner >= 1 && ((owner - 1) as usize) < PROFILE_MAX_CORES {
        HOLDER_TAG[(owner - 1) as usize].0.load(Ordering::Relaxed)
    } else {
        HOLD_TAG_UNKNOWN
    };
    let _ = writeln!(
        buf,
        "[BKL] stuck: owner={} waiter={} tag={} (aff0+1)",
        owner, me, tag
    );
    if buf.1 > 0 {
        if let Ok(s) = core::str::from_utf8(&buf.0[..buf.1]) {
            (crate::runtime::runtime().print_str)(s);
        }
    }
}

/// Diagnostic: log when [`KernelLock::acquire`]'s self-healing fired (see the recovery
/// note there). Every line here is a live sighting of the ticket-accounting leak —
/// keep them until the leak is root-caused. Stack-buffered (IRQ-masked context).
fn log_kernel_lock_recovered(me: u32, kind: &str) {
    KERNEL_LOCK_RECOVERIES.fetch_add(1, Ordering::Relaxed);
    use core::fmt::Write;
    struct Buf([u8; 96], usize);
    impl Write for Buf {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
            let b = s.as_bytes();
            let n = b.len().min(96 - self.1);
            self.0[self.1..self.1 + n].copy_from_slice(&b[..n]);
            self.1 += n;
            Ok(())
        }
    }
    let mut buf = Buf([0u8; 96], 0);
    let _ = writeln!(buf, "[BKL] RECOVERED ({kind}) by core {me} (aff0+1)");
    if buf.1 > 0 {
        if let Ok(s) = core::str::from_utf8(&buf.0[..buf.1]) {
            (crate::runtime::runtime().print_str)(s);
        }
    }
}

/// Raw reader-writer spinlock with writer priority.
///
/// State encoding in a single `AtomicU32`:
/// - Bit 31 (`WRITER_BIT`): set when a writer is pending or active
/// - Bits 0-30: reader count (up to ~2 billion, more than enough)
///
/// Transitions:
/// - `0x0000_0000` = unlocked (no readers, no writer)
/// - `0x0000_000N` = N readers active, no writer pending
/// - `0x8000_000N` = N readers active, writer pending (draining readers)
/// - `0x8000_0000` = write-locked (writer active, no readers)
///
/// Writer priority: once `WRITER_BIT` is set, new `lock_shared` calls spin
/// until the writer finishes, preventing reader starvation of writers.
pub struct RawRwSpinlock(AtomicU32);

const WRITER_BIT: u32 = 0x8000_0000;
const READER_MASK: u32 = 0x7FFF_FFFF;
const UNLOCKED: u32 = 0;

/// Spin iteration limit before logging a diagnostic (helps debug deadlocks).
const SPIN_WARN_THRESHOLD: u32 = 10_000_000;

/// Consecutive free-lock/frozen-FIFO spins before [`KernelLock::acquire`] concludes the
/// currently-served ticket has no living waiter and advances `now_serving` itself (see
/// the self-healing note in `acquire`). ~2× the stuck-warn threshold: tens of
/// milliseconds — far beyond the ns-scale hand-off windows this state legitimately
/// occupies, but short enough that a wedged box recovers before watchdogs cascade.
const LOST_TICKET_RECOVERY_SPINS: u32 = 20_000_000;

unsafe impl lock_api::RawRwLock for RawRwSpinlock {
    #[allow(clippy::declare_interior_mutable_const)]
    const INIT: Self = Self(AtomicU32::new(UNLOCKED));

    type GuardMarker = lock_api::GuardSend;

    fn lock_shared(&self) {
        loop {
            let state = self.0.load(Ordering::Relaxed);
            // If a writer is pending/active, spin (writer priority)
            if state & WRITER_BIT != 0 {
                core::hint::spin_loop();
                continue;
            }
            // Try to increment reader count
            if self.0.compare_exchange_weak(
                state,
                state + 1,
                Ordering::Acquire,
                Ordering::Relaxed,
            ).is_ok() {
                return;
            }
            core::hint::spin_loop();
        }
    }

    fn try_lock_shared(&self) -> bool {
        let state = self.0.load(Ordering::Relaxed);
        if state & WRITER_BIT != 0 {
            return false;
        }
        self.0.compare_exchange(
            state,
            state + 1,
            Ordering::Acquire,
            Ordering::Relaxed,
        ).is_ok()
    }

    unsafe fn unlock_shared(&self) {
        self.0.fetch_sub(1, Ordering::Release);
    }

    fn lock_exclusive(&self) {
        // Phase 1: Set WRITER_BIT to block new readers.
        // fetch_or is atomic — even if readers are active, this succeeds.
        let prev = self.0.fetch_or(WRITER_BIT, Ordering::Acquire);

        // If another writer already has the bit, we must wait for it to finish
        // and then retry (only one writer at a time).
        if prev & WRITER_BIT != 0 {
            // Another writer is active/pending. Spin until state == UNLOCKED,
            // then try the whole sequence again.
            loop {
                let state = self.0.load(Ordering::Relaxed);
                if state == UNLOCKED {
                    if self.0.compare_exchange_weak(
                        UNLOCKED,
                        WRITER_BIT,
                        Ordering::Acquire,
                        Ordering::Relaxed,
                    ).is_ok() {
                        break; // We now own the writer bit
                    }
                } else if state & WRITER_BIT == 0 {
                    // Previous writer finished but readers jumped in.
                    // Set writer bit again.
                    let prev2 = self.0.fetch_or(WRITER_BIT, Ordering::Acquire);
                    if prev2 & WRITER_BIT == 0 {
                        break; // We now own the writer bit
                    }
                }
                core::hint::spin_loop();
            }
        }

        // Phase 2: Wait for existing readers to drain.
        // WRITER_BIT is set, so no new readers can enter.
        let mut spins: u32 = 0;
        while self.0.load(Ordering::Acquire) != WRITER_BIT {
            spins = spins.wrapping_add(1);
            if spins == SPIN_WARN_THRESHOLD {
                // Diagnostic: log the stuck state for debugging deadlocks
                log_write_lock_stuck(self.0.load(Ordering::Relaxed));
            }
            core::hint::spin_loop();
        }
        // State is now WRITER_BIT (= write-locked, no readers)
    }

    fn try_lock_exclusive(&self) -> bool {
        self.0.compare_exchange(
            UNLOCKED,
            WRITER_BIT,
            Ordering::Acquire,
            Ordering::Relaxed,
        ).is_ok()
    }

    unsafe fn unlock_exclusive(&self) {
        self.0.store(UNLOCKED, Ordering::Release);
    }
}

/// Diagnostic: log when write lock is stuck spinning.
fn log_write_lock_stuck(state: u32) {
    // Use a stack buffer to avoid heap allocation (might be in IRQ-disabled context).
    // Only print once per stuck episode (caller checks threshold).
    let readers = state & READER_MASK;
    let writer_bit = (state & WRITER_BIT) != 0;

    // Minimal stack-based print to avoid any lock contention
    use core::fmt::Write;
    struct Buf([u8; 96], usize);
    impl Write for Buf {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
            let b = s.as_bytes();
            let n = b.len().min(96 - self.1);
            self.0[self.1..self.1 + n].copy_from_slice(&b[..n]);
            self.1 += n;
            Ok(())
        }
    }
    let mut buf = Buf([0u8; 96], 0);
    let _ = writeln!(buf, "[RWLOCK] write lock stuck: state={:#x} readers={} writer_bit={}",
        state, readers, writer_bit);
    if buf.1 > 0 {
        if let Ok(s) = core::str::from_utf8(&buf.0[..buf.1]) {
            (crate::runtime::runtime().print_str)(s);
        }
    }
}

impl RawRwSpinlock {
    /// Read the raw lock state for diagnostics.
    pub fn raw_state(&self) -> u32 {
        self.0.load(Ordering::Relaxed)
    }
}

/// Reader-writer spinlock.
pub type RwSpinlock<T> = lock_api::RwLock<RawRwSpinlock, T>;

/// Read guard for `RwSpinlock`.
pub type RwSpinlockReadGuard<'a, T> = lock_api::RwLockReadGuard<'a, RawRwSpinlock, T>;

/// Write guard for `RwSpinlock`.
pub type RwSpinlockWriteGuard<'a, T> = lock_api::RwLockWriteGuard<'a, RawRwSpinlock, T>;

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

    #[test]
    fn rwspinlock_read_then_write() {
        let lock = RwSpinlock::new(42u32);
        {
            let r = lock.read();
            assert_eq!(*r, 42);
        }
        {
            let mut w = lock.write();
            *w = 99;
        }
        assert_eq!(*lock.read(), 99);
    }

    #[test]
    fn rwspinlock_multiple_readers() {
        let lock = RwSpinlock::new(7u32);
        let r1 = lock.read();
        let r2 = lock.read();
        let r3 = lock.read();
        assert_eq!(*r1, 7);
        assert_eq!(*r2, 7);
        assert_eq!(*r3, 7);
    }

    #[test]
    fn rwspinlock_try_write_fails_while_read_held() {
        let lock = RwSpinlock::new(0u32);
        let _r = lock.read();
        assert!(lock.try_write().is_none());
    }

    #[test]
    fn rwspinlock_try_read_fails_while_write_held() {
        let lock = RwSpinlock::new(0u32);
        let _w = lock.write();
        assert!(lock.try_read().is_none());
    }

    #[test]
    fn rwspinlock_try_write_fails_while_write_held() {
        let lock = RwSpinlock::new(0u32);
        let _w = lock.write();
        assert!(lock.try_write().is_none());
    }

    #[test]
    fn rwspinlock_write_after_readers_drop() {
        let lock = RwSpinlock::new(1u32);
        {
            let _r1 = lock.read();
            let _r2 = lock.read();
            assert!(lock.try_write().is_none());
        }
        let mut w = lock.write();
        *w = 2;
        drop(w);
        assert_eq!(*lock.read(), 2);
    }

    #[test]
    fn rwspinlock_read_after_write_drops() {
        let lock = RwSpinlock::new(10u32);
        {
            let mut w = lock.write();
            *w = 20;
            assert!(lock.try_read().is_none());
        }
        assert_eq!(*lock.read(), 20);
    }

    #[test]
    fn rwspinlock_with_btreemap() {
        use alloc::collections::BTreeMap;
        let lock = RwSpinlock::new(BTreeMap::<u32, u32>::new());
        {
            let mut w = lock.write();
            w.insert(1, 10);
            w.insert(2, 20);
        }
        {
            let r = lock.read();
            assert_eq!(r.get(&1), Some(&10));
            assert_eq!(r.get(&2), Some(&20));
            assert_eq!(r.len(), 2);
        }
    }

    #[test]
    fn rwspinlock_state_encoding_writer_priority() {
        use lock_api::RawRwLock;
        let raw = RawRwSpinlock::INIT;
        assert_eq!(raw.0.load(Ordering::Relaxed), UNLOCKED);

        // Shared locks increment reader count (bits 0-30)
        raw.lock_shared();
        assert_eq!(raw.0.load(Ordering::Relaxed), 1);
        raw.lock_shared();
        assert_eq!(raw.0.load(Ordering::Relaxed), 2);

        unsafe { raw.unlock_shared(); }
        assert_eq!(raw.0.load(Ordering::Relaxed), 1);
        unsafe { raw.unlock_shared(); }
        assert_eq!(raw.0.load(Ordering::Relaxed), UNLOCKED);

        // Exclusive lock sets WRITER_BIT
        raw.lock_exclusive();
        assert_eq!(raw.0.load(Ordering::Relaxed), WRITER_BIT);
        unsafe { raw.unlock_exclusive(); }
        assert_eq!(raw.0.load(Ordering::Relaxed), UNLOCKED);
    }

    #[test]
    fn rwspinlock_try_read_blocked_by_pending_writer() {
        use lock_api::RawRwLock;
        let raw = RawRwSpinlock::INIT;

        // Simulate a pending writer by setting WRITER_BIT with readers active
        raw.0.store(WRITER_BIT | 1, Ordering::Relaxed); // 1 reader + writer pending

        // try_lock_shared should fail (writer priority)
        assert!(!raw.try_lock_shared());

        // Clean up
        raw.0.store(UNLOCKED, Ordering::Relaxed);
    }

    #[test]
    fn rwspinlock_writer_priority_blocks_new_readers() {
        let lock = RwSpinlock::new(0u32);

        // Take a write lock
        let w = lock.write();

        // While write-locked, try_read should fail
        assert!(lock.try_read().is_none());

        drop(w);

        // After write releases, read should succeed
        assert!(lock.try_read().is_some());
    }

    // --- KernelLock (Big Kernel Lock) ---

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
    fn kernel_lock_reconcile_matches_target_el() {
        let bkl = KernelLock::new();
        // Entering / staying in EL1 acquires.
        bkl.reconcile(0, /* target_is_el0 */ false);
        assert!(bkl.held_by(0));
        // Re-entering EL1 (nested) is idempotent.
        bkl.reconcile(0, false);
        assert!(bkl.held_by(0));
        // Returning to EL0 releases.
        bkl.reconcile(0, true);
        assert!(!bkl.is_held());
        // Returning to EL0 when already free is a no-op.
        bkl.reconcile(0, true);
        assert!(!bkl.is_held());
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
    fn kernel_lock_no_ticket_acquire_release_stays_balanced() {
        // The BKL-free EL0-preempt scheduler path (`bkl::reconcile_for_spsr_no_ticket` →
        // `reconcile_no_ticket` → `acquire_no_ticket`) gains ownership WITHOUT taking a
        // ticket: it never called `enter_kernel`, so taking one would leak it. But the
        // thread it resumes into is ordinary EL1 code, and its eventual EL1→EL0 return
        // goes through the NORMAL epilogue (`reconcile_for_spsr` → `release`).
        //
        // So the pair is asymmetric by construction, and the invariant that keeps the FIFO
        // honest — `now_serving` advances exactly once per ticket handed out — has to be
        // maintained by `release` declining to advance for a hold that took no ticket.
        // Without that, `now_serving` runs AHEAD of `next_ticket`, and then every
        // contended acquirer takes a ticket already behind `now_serving`, is told it was
        // "skipped", and re-tickets — the `[BKL] RECOVERED (reticket-skipped)` bursts seen
        // at SMP=4 under fork/exec churn (46 in one 80 s workload window, 0
        // `advanced-lost`), which degrade the fair lock to an unfair test-and-set for the
        // length of the burst.
        let bkl = KernelLock::new();
        for round in 0..2000u32 {
            let core = round % 4;
            let peer = (core + 1) % 4;
            // Scheduler SGI preempted EL0, ran BKL-free, and reconciles into an EL1 thread.
            bkl.acquire_no_ticket(core);
            assert!(bkl.held_by(core));
            // That thread returns to EL0 through the normal epilogue.
            bkl.release(core);
            assert!(!bkl.is_held(), "round {round}: not free after no-ticket release");
            assert_eq!(
                bkl.next_ticket.load(Ordering::Relaxed),
                bkl.now_serving.load(Ordering::Relaxed),
                "round {round}: now_serving drifted from next_ticket across a no-ticket hold"
            );
            // A normal ticketed excursion in between must still be served on its own
            // ticket — no drift means no re-ticket.
            bkl.acquire(peer);
            assert!(bkl.held_by(peer));
            bkl.release(peer);
            assert_eq!(
                bkl.next_ticket.load(Ordering::Relaxed),
                bkl.now_serving.load(Ordering::Relaxed),
                "round {round}: ticketed excursion left the counters skewed"
            );
        }
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
