//! Synchronization primitives for akuma-exec.
//!
//! Provides `RwSpinlock<T>` — a reader-writer spinlock built on `lock_api`
//! with writer priority to prevent reader starvation — and `KernelLock`, the
//! recursive Big Kernel Lock used by real (shared-kernel) SMP.

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// Mask local IRQs (set `DAIF.I`) and return the prior `DAIF` for [`irq_restore`]. Used by
/// [`KernelLock::acquire`] to make its FIFO ticket wait atomic against local exception
/// nesting. Bare-metal AArch64 only; a no-op returning `0` on host builds (single-threaded
/// tests have no local IRQs).
#[cfg(target_os = "none")]
#[inline(always)]
fn irq_save_mask() -> u64 {
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
fn irq_restore(daif: u64) {
    // SAFETY: restoring the previously-saved DAIF; no memory effects.
    unsafe { core::arch::asm!("msr daif, {}", in(reg) daif, options(nomem, nostack)) };
}

#[cfg(not(target_os = "none"))]
#[inline(always)]
fn irq_save_mask() -> u64 {
    0
}

#[cfg(not(target_os = "none"))]
#[inline(always)]
fn irq_restore(_daif: u64) {}

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
/// 511 unknown/idle. Sized to 512.
const PROFILE_BUCKETS: usize = 512;
/// Reserved tag values.
pub const HOLD_TAG_FAULT: u64 = 500;
pub const HOLD_TAG_IRQ: u64 = 501;
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

/// Per-core tag: what the core is doing while it holds (or last held) the BKL.
static HOLDER_TAG: [CoreTag; PROFILE_MAX_CORES] =
    [const { CoreTag(AtomicU64::new(HOLD_TAG_UNKNOWN)) }; PROFILE_MAX_CORES];
/// Per-tag accumulated peer wait (spin iterations attributed to a holder doing `tag`).
static WAIT_BY_HOLDER: [AtomicU64; PROFILE_BUCKETS] =
    [const { AtomicU64::new(0) }; PROFILE_BUCKETS];

/// Record what `core_id` is doing while in the kernel (holding the BKL). Called by the
/// exception entry paths (syscall number / fault / IRQ) so waiters can attribute blame.
#[inline]
pub fn set_holder_tag(core_id: u32, tag: u64) {
    if !PROFILE_ENABLED.load(Ordering::Relaxed) {
        return;
    }
    let c = core_id as usize;
    if c < PROFILE_MAX_CORES {
        HOLDER_TAG[c].0.store(tag.min((PROFILE_BUCKETS - 1) as u64), Ordering::Relaxed);
    }
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
    let _ = writeln!(
        buf,
        "[BKL] stuck: owner={} waiter={} (core ids are aff0+1)",
        owner, me
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
}
