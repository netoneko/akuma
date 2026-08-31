//! Per-thread preemption control: the counters, the watchdog, and `PreemptGuard`.
//!
//! # Why this is in the leaf crate
//!
//! `PreemptGuard` is a ~40-line RAII guard, and it was the single reason
//! `akuma-ext2` and `akuma-net` each depended on the 23.8k-line `akuma-exec`
//! (`TRIM_FAT_EMBARASSING_DUPLICATIONS.md` §5.55 named it "the long pole for
//! the whole untangling"). §5.55 also recorded why it looked immovable: it calls
//! `threading::disable_preemption`, which is *not* a standalone counter — it
//! indexes `PREEMPTION_DISABLED[tid]` by `TPIDRRO_EL0` and maintains two
//! diagnostic arrays beside it. That is scheduler-adjacent state.
//!
//! It moves anyway, because the three things it actually needed from outside
//! `core` are now available to a leaf crate:
//!
//! 1. a console, for [`current_tid`]'s corrupt-register halt and the watchdog —
//!    [`crate::console`];
//! 2. a clock, for the diagnostic timestamp — [`crate::clock`];
//! 3. IRQ masking — [`crate::irq`].
//!
//! None of that reintroduces the callback `akuma-exec`'s `sync.rs` deliberately
//! removed. That callback dispatched *the guard's own operation*, so it had to be
//! registered before the guard was first used; these are a print sink and a clock
//! feeding diagnostics, and both already degraded when unregistered.
//!
//! # The seam: reading `TPIDRRO_EL0` moves, writing it does not
//!
//! [`current_tid`] is a bounds-checked `mrs` and lives here. The *write* stays in
//! `akuma_exec::threading::set_current_thread_register`, because it is not just a
//! register write — it also re-points the per-core BKL attribution cache
//! (`load_thread_tag_to_core`), which is genuinely the scheduler's business. So
//! this crate can ask "which thread am I?" without owning the answer to "which
//! thread is this core running?".
//!
//! `akuma_exec::threading` re-exports everything here, so no call site moved.

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use crate::clock::uptime_us;
use crate::safe_print;

/// Compile-time ceiling on thread slots, and the size of every per-slot static
/// in this module.
///
/// 256 on normal profiles: this is BSS whether used or not, and a few hundred KB
/// is free on a multi-GB box, while 64 was measurably binding (one process could
/// hold ~52 threads, and 16-way `pthread_create` load hit genuine exhaustion).
/// `size`/`extreme-size` keep 64 — they target a 4 MB RAM floor where that BSS is
/// real money and nothing there spawns hundreds of threads.
///
/// **Not the working limit.** `akuma_exec::threading::compute_thread_limit` picks
/// that at boot from actual RAM and clamps to this value.
/// `akuma_exec::threading::types::MAX_THREADS` re-exports this so the two can
/// never disagree — they were independent literals with a "must match" comment
/// once, and on 2026-08-04 raising only one silently did nothing.
#[cfg(not(kernel_profile_extreme))]
pub const MAX_THREADS: usize = 256;
#[cfg(kernel_profile_extreme)]
pub const MAX_THREADS: usize = 64;

/// Preemption disabled longer than this warrants a watchdog warning (100 ms).
pub const PREEMPTION_WATCHDOG_WARN_US: u64 = 100_000;

/// Preemption disabled longer than this is reported as critical (5 s).
pub const PREEMPTION_WATCHDOG_PANIC_US: u64 = 5_000_000;

/// A gap larger than this between watchdog checks means the host slept, not that
/// a thread stalled (100 ms).
pub const MAX_EXPECTED_CHECK_GAP_US: u64 = 100_000;

/// Current thread id, from `TPIDRRO_EL0`.
///
/// Halts the core if the register is out of range: every per-slot static in the
/// kernel is indexed by this, so a corrupt value is not something to continue
/// past — it would index arbitrary memory. Returns 0 on host builds.
#[cfg(target_os = "none")]
#[inline]
#[must_use]
pub fn current_tid() -> usize {
    let val: u64 = akuma_cpu::sysreg::tpidrro_el0();
    let tid = val as usize;
    if tid >= MAX_THREADS {
        safe_print!(
            256,
            "[FATAL] TPIDRRO_EL0 CORRUPT: tid=0x{:x} >= MAX_THREADS ({})\nSystem halted - cannot determine current thread\n",
            val,
            MAX_THREADS
        );
        loop {
            akuma_cpu::park::wfi();
        }
    }
    tid
}

#[cfg(not(target_os = "none"))]
#[inline]
#[must_use]
pub fn current_tid() -> usize {
    0
}

/// Per-thread preemption disable counters, so one thread's preemption state
/// cannot affect another's. Nesting is tracked by the count.
static PREEMPTION_DISABLED: [AtomicUsize; MAX_THREADS] = {
    const INIT: AtomicUsize = AtomicUsize::new(0);
    [INIT; MAX_THREADS]
};

/// Per-thread microsecond timestamp of the last 0→1 disable. Read by the
/// watchdog to detect stuck threads; 0 means "not disabled / no timestamp".
static PREEMPTION_DISABLED_SINCE: [AtomicU64; MAX_THREADS] = {
    const INIT: AtomicU64 = AtomicU64::new(0);
    [INIT; MAX_THREADS]
};

/// Diagnostic: `core::panic::Location` pointer of the `disable_preemption()` call
/// that took each thread's count 0→1, so a long-disabled thread's watchdog line
/// names the culprit call site instead of just the tid. A `&'static Location`, so
/// a relaxed store of the pointer is fine.
static PREEMPTION_DISABLED_AT: [AtomicU64; MAX_THREADS] = {
    const INIT: AtomicU64 = AtomicU64::new(0);
    [INIT; MAX_THREADS]
};

/// Last time the watchdog ran, for detecting host sleep/wake time jumps.
static LAST_WATCHDOG_CHECK_US: AtomicU64 = AtomicU64::new(0);

/// The `file:line` that first disabled preemption for `tid` (the 0→1
/// transition), if it is still disabled.
#[must_use]
pub fn preemption_disabled_at(tid: usize) -> Option<&'static core::panic::Location<'static>> {
    let ptr = PREEMPTION_DISABLED_AT[tid].load(Ordering::Relaxed) as usize;
    if ptr == 0 || PREEMPTION_DISABLED[tid].load(Ordering::Relaxed) == 0 {
        None
    } else {
        // SAFETY: only ever stores `Location::caller()` pointers, which are &'static.
        Some(unsafe { &*(ptr as *const core::panic::Location<'static>) })
    }
}

/// Disable preemption for the current thread.
///
/// Nestable — must be matched by an equal number of [`enable_preemption`] calls.
/// While disabled, timer interrupts will not context-switch away from THIS
/// thread, but IRQs stay enabled and `yield_now()` still works.
#[inline]
#[track_caller]
pub fn disable_preemption() {
    let tid = current_tid();
    let prev = PREEMPTION_DISABLED[tid].fetch_add(1, Ordering::SeqCst);
    // Record the timestamp and call site on the first disable (0 → 1).
    if prev == 0 {
        // `uptime_us()` yields 0 before a clock is registered, and the watchdog
        // reads 0 as "no timestamp" — so this degrades instead of failing during
        // early boot and in host tests. `PreemptGuard` documents itself as usable
        // in both, and `akuma-ext2`'s host tests reach here through its
        // `no-bkl-vfs` guard once `smp-shared` is in the default feature set.
        PREEMPTION_DISABLED_SINCE[tid].store(uptime_us(), Ordering::Release);
        PREEMPTION_DISABLED_AT[tid]
            .store(core::ptr::from_ref(core::panic::Location::caller()) as u64, Ordering::Relaxed);
    }
}

/// Re-enable preemption for the current thread. Must match a
/// [`disable_preemption`].
#[inline]
pub fn enable_preemption() {
    let tid = current_tid();
    let prev = PREEMPTION_DISABLED[tid].fetch_sub(1, Ordering::SeqCst);
    debug_assert!(prev > 0, "enable_preemption called without matching disable");
    // Clear the timestamp when fully re-enabled (1 → 0).
    if prev == 1 {
        PREEMPTION_DISABLED_SINCE[tid].store(0, Ordering::Release);
    }
}

/// Is preemption currently disabled for the current thread?
#[inline]
#[must_use]
pub fn is_preemption_disabled() -> bool {
    PREEMPTION_DISABLED[current_tid()].load(Ordering::SeqCst) > 0
}

/// The per-thread preemption-disable nesting count for `tid`. Used by
/// diagnostics (the timer-tick log) to spot a leaked `disable_preemption()` that
/// would starve the scheduler.
#[inline]
#[must_use]
pub fn preemption_disabled_count(tid: usize) -> usize {
    PREEMPTION_DISABLED[tid].load(Ordering::SeqCst)
}

/// Clear every preemption record for slot `i`.
///
/// Called from `akuma_exec::threading::scrub_thread_slot` on every FREE →
/// INITIALIZING transition and again when a slot returns to FREE, so a recycled
/// slot cannot inherit its previous occupant's disable count — which would read
/// as a thread that is silently never preempted.
#[inline]
pub fn scrub_slot(i: usize) {
    if i >= MAX_THREADS {
        return;
    }
    PREEMPTION_DISABLED[i].store(0, Ordering::Release);
    PREEMPTION_DISABLED_SINCE[i].store(0, Ordering::Release);
    PREEMPTION_DISABLED_AT[i].store(0, Ordering::Release);
}

/// Watchdog: has preemption been disabled too long? Called from the timer IRQ.
///
/// Returns `None` when preemption is not disabled or is within normal time, and
/// `Some(duration_us)` past the warn threshold.
pub fn check_preemption_watchdog() -> Option<u64> {
    let tid = current_tid();
    let now = uptime_us();

    // Detect time jumps (host sleep/wake).
    let last_check = LAST_WATCHDOG_CHECK_US.swap(now, Ordering::SeqCst);
    if last_check > 0 {
        let gap = now.saturating_sub(last_check);
        if gap > MAX_EXPECTED_CHECK_GAP_US {
            // Time jumped — the host probably slept. Log and reset this thread's
            // timestamp so it doesn't trip a false alarm.
            safe_print!(128, "[WATCHDOG] Time jump detected: {}ms (host sleep/wake)\n", gap / 1000);
            let disabled_since = PREEMPTION_DISABLED_SINCE[tid].load(Ordering::Acquire);
            if disabled_since != 0 {
                PREEMPTION_DISABLED_SINCE[tid].store(now, Ordering::Release);
            }
            return None;
        }
    }

    let disabled_since = PREEMPTION_DISABLED_SINCE[tid].load(Ordering::Acquire);
    if disabled_since == 0 {
        return None;
    }

    let duration = now.saturating_sub(disabled_since);

    if duration >= PREEMPTION_WATCHDOG_PANIC_US {
        // Critical, but do NOT panic — this runs in IRQ context.
        safe_print!(
            128,
            "[WATCHDOG] Thread {} preemption disabled {}ms (critical)\n",
            tid,
            duration / 1000
        );
        return Some(duration);
    } else if duration >= PREEMPTION_WATCHDOG_WARN_US {
        return Some(duration);
    }

    None
}

/// RAII guard that disables scheduler preemption (and, under the BKL-drop
/// features, masks local IRQs) for the lifetime of a kernel spinlock critical
/// section.
///
/// # `no-bkl-*`: local IRQs are masked for the hold too
///
/// With the Big Kernel Lock dropped around a subsystem's syscalls (network:
/// `no-bkl-network`, VFS: `no-bkl-vfs`), a core can be inside one of those
/// critical sections *without* owning the BKL. A nested IRQ then runs
/// `enter_kernel()`, which hard-spins with IRQs masked until the BKL frees —
/// while THIS core still holds the inner spinlock. If the current BKL owner is
/// meanwhile spinning on that same inner lock (the async-main poller does exactly
/// that on `NETWORK`, near-constantly), the two cores deadlock AB-BA and every
/// other core piles into the BKL wait: the SMP=4 hard wedge (`[BKL] stuck`, owner
/// frozen nonzero, guest timer starved). Masking IRQs for the (short) hold makes
/// the window nest-free, so a core can never be caught "holding an inner lock,
/// waiting for the BKL".
///
/// Plain `smp-shared` builds don't need it — there EL1 always holds the BKL, so
/// the nested `enter_kernel` is the idempotent owner fast path.
///
/// # Lift history
///
/// Originally `akuma_net::runtime::PreemptGuard`, wired through the `NetRuntime`
/// registration callbacks so the net crate could stay decoupled from
/// `akuma-exec`. Lifted to `akuma_exec::sync` so the VFS BKL-drop path could
/// reuse it, which cost `akuma-net` and `akuma-ext2` a dependency on the whole
/// execution crate. Now here, in a crate with no dependencies at all, so the
/// reuse is free — see the module header. Both
/// `akuma_exec::sync::PreemptGuard` and `akuma_net::runtime::PreemptGuard`
/// re-export it for source compatibility.
#[must_use]
pub struct PreemptGuard {
    /// Whether `new()` actually disabled preemption. Present only under
    /// `smp-shared`; other builds carry no state.
    #[cfg(kernel_smp_shared)]
    active: bool,
    /// Saved DAIF to restore on drop (`no-bkl-*` builds only — see type doc).
    #[cfg(all(kernel_smp_shared, any(kernel_no_bkl_network, kernel_no_bkl_vfs)))]
    saved_daif: u64,
}

impl PreemptGuard {
    /// Disable preemption (under `smp-shared`) until the returned guard drops.
    #[inline]
    pub fn new() -> Self {
        #[cfg(kernel_smp_shared)]
        {
            // Direct call, no runtime registration — which is the historical
            // reason the net crate used a callback pointer, and why this works
            // during early boot and in host tests alike.
            disable_preemption();
            let active = true;
            // Mask IRQs AFTER disabling preemption so drop's reverse order
            // re-enables preemption only once IRQs are live again.
            #[cfg(any(kernel_no_bkl_network, kernel_no_bkl_vfs))]
            return Self { active, saved_daif: crate::irq::irq_save_mask() };
            #[cfg(not(any(kernel_no_bkl_network, kernel_no_bkl_vfs)))]
            return Self { active };
        }
        #[cfg(not(kernel_smp_shared))]
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
        // Restore IRQs first (reverse of new's order), then re-enable preemption
        // — so a timer IRQ can't preempt us between enable_preemption and the
        // DAIF restore.
        #[cfg(all(kernel_smp_shared, any(kernel_no_bkl_network, kernel_no_bkl_vfs)))]
        crate::irq::irq_restore(self.saved_daif);
        #[cfg(kernel_smp_shared)]
        if self.active {
            enable_preemption();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_THREADS, PreemptGuard, current_tid, disable_preemption, enable_preemption,
        is_preemption_disabled, preemption_disabled_at, preemption_disabled_count, scrub_slot,
    };

    use std::sync::{Mutex, MutexGuard, PoisonError};

    /// Host builds report tid 0 for every thread (`current_tid`'s non-bare-metal
    /// arm), so every test below operates on the *same* global slot 0 — and
    /// `cargo test` runs them in parallel. Cleaning up at the top and bottom of
    /// each test was not enough: under load they interleaved, one test's
    /// `disable_preemption` bumping another's count, or its `Location::caller()`
    /// winning the 0→1 store, or its `reset()` landing mid-flight in a third.
    /// That produced three distinct flaky signatures (2 failures in 60 loaded
    /// runs) — see `docs/archive/SMP_SECONDARY_TICK_KILLED_BY_WFI_PROBE.md`
    /// § "Not part of this bug: the flaky `preempt` host tests".
    ///
    /// Slot 0 is a shared resource here, so it gets a lock.
    static SLOT0: Mutex<()> = Mutex::new(());

    /// Take exclusive use of slot 0 and hand it back scrubbed. Scrubs again on
    /// drop, so a test that leaks a disable count — or panics mid-flight —
    /// cannot hand the mess to whichever test runs next.
    fn slot0() -> Slot0 {
        // A panicking test poisons the mutex. Recover instead of cascading that
        // one failure into every other test in the module, which is exactly the
        // truncated-suite effect (`host.tests: 482` instead of 592) that made
        // the flake look like a commit had disabled tests.
        let guard = SLOT0.lock().unwrap_or_else(PoisonError::into_inner);
        scrub_slot(0);
        Slot0 { _guard: guard }
    }

    struct Slot0 {
        _guard: MutexGuard<'static, ()>,
    }

    impl Drop for Slot0 {
        fn drop(&mut self) {
            scrub_slot(0);
        }
    }

    #[test]
    fn max_threads_leaves_room_for_the_reserved_range() {
        // Derived, not literal: profile-dependent (256 normally, 64 on the size
        // profiles). Mirrors the assertion in akuma-exec's threading::types.
        assert!(MAX_THREADS >= 64);
    }

    #[test]
    fn host_tid_is_zero() {
        assert_eq!(current_tid(), 0);
    }

    #[test]
    fn disable_enable_round_trips() {
        let _slot = slot0();
        assert!(!is_preemption_disabled());
        disable_preemption();
        assert!(is_preemption_disabled());
        assert_eq!(preemption_disabled_count(0), 1);
        enable_preemption();
        assert!(!is_preemption_disabled());
    }

    #[test]
    fn nesting_is_counted_not_boolean() {
        // The whole reason this is a count: an inner guard dropping must not
        // re-enable preemption for an outer one that is still held.
        let _slot = slot0();
        disable_preemption();
        disable_preemption();
        disable_preemption();
        assert_eq!(preemption_disabled_count(0), 3);
        enable_preemption();
        assert_eq!(preemption_disabled_count(0), 2);
        assert!(is_preemption_disabled());
        enable_preemption();
        enable_preemption();
        assert!(!is_preemption_disabled());
    }

    #[test]
    fn disabled_at_names_the_zero_to_one_call_site_only() {
        let _slot = slot0();
        assert!(preemption_disabled_at(0).is_none());
        let line_of_disable = line!() + 1;
        disable_preemption();
        let loc = preemption_disabled_at(0).expect("0->1 records a location");
        assert_eq!(loc.line(), line_of_disable);
        // A nested disable must NOT overwrite it — the outer call is the one that
        // made the thread non-preemptible, so it is the one worth reporting.
        disable_preemption();
        assert_eq!(preemption_disabled_at(0).unwrap().line(), line_of_disable);
        enable_preemption();
        enable_preemption();
        // Fully re-enabled: the location is no longer live.
        assert!(preemption_disabled_at(0).is_none());
    }

    #[test]
    fn scrub_slot_clears_a_leaked_count() {
        // A recycled slot inheriting a non-zero count would be a thread the
        // scheduler silently never preempts.
        let _slot = slot0();
        disable_preemption();
        disable_preemption();
        assert_eq!(preemption_disabled_count(0), 2);
        scrub_slot(0);
        assert_eq!(preemption_disabled_count(0), 0);
        assert!(preemption_disabled_at(0).is_none());
    }

    #[test]
    fn scrub_slot_ignores_out_of_range() {
        scrub_slot(MAX_THREADS);
        scrub_slot(usize::MAX);
    }

    #[test]
    fn guard_balances_the_counter() {
        let _slot = slot0();
        let before = preemption_disabled_count(0);
        {
            let _g = PreemptGuard::new();
            #[cfg(kernel_smp_shared)]
            assert_eq!(preemption_disabled_count(0), before + 1);
        }
        assert_eq!(preemption_disabled_count(0), before);
    }

    #[test]
    fn nested_guards_balance() {
        let _slot = slot0();
        {
            let _outer = PreemptGuard::new();
            {
                let _inner = PreemptGuard::new();
                #[cfg(kernel_smp_shared)]
                assert_eq!(preemption_disabled_count(0), 2);
            }
            #[cfg(kernel_smp_shared)]
            assert!(is_preemption_disabled());
        }
        assert_eq!(preemption_disabled_count(0), 0);
    }
}
