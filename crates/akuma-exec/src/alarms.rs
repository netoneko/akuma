//! Async alarm queue for futures ("make this `Waker` runnable at time T").
//!
//! A minimal Embassy-time-driver replacement, extracted from the bin crate
//! (`src/kernel_timer.rs`, 2026-08-18): this is glue over the exec/scheduler —
//! it parks async wakers against deadlines and is serviced by the scheduler
//! tick ISR's call to [`on_timer_interrupt`]. It owns no timer hardware: the
//! deadline register (CNTV_CVAL) belongs to the scheduler tick
//! (`src/timer.rs` + `akuma-timer`), so alarm resolution equals the tick.
//! Registering a deadline does not re-arm the hardware — see
//! [`update_hardware_timer`].
//!
//! The pre-extraction split (hardware + policy in `crates/akuma-timer`,
//! scheduler ISR in `src/timer.rs`, waker queue here) and the unification
//! plan (wake-pass consumes waker deadlines directly) are recorded in
//! `docs/archive/AKUMA_TIME_EXTRACTION.md` and
//! `docs/archive/TRIMMING_FAT_SCHEDULER.md`.
//!
//! Provides `Timer::after()` (async delay) and `Duration`; `with_timeout`
//! was removed with Embassy.

use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll, Waker};

use spinning_top::Spinlock;

// ============================================================================
// Duration
// ============================================================================

/// Minimal duration type (microsecond precision)
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Duration {
    us: u64,
}

impl Duration {
    #[inline]
    pub const fn from_secs(secs: u64) -> Self {
        Self { us: secs * 1_000_000 }
    }


    #[inline]
    pub const fn as_micros(&self) -> u64 {
        self.us
    }
}

// ============================================================================
// Timeout Error
// ============================================================================



// ============================================================================
// Current time
// ============================================================================

/// Get current time in microseconds since boot.
///
/// Delegates to the registered runtime's `uptime_us` hook — same source as
/// the scheduler's wake-pass. (The pre-extraction copy carried its own CNTV
/// access; two owners of one hardware seam.)
#[inline]
pub fn now_us() -> u64 {
    (crate::runtime::runtime().uptime_us)()
}

// ============================================================================
// Alarm Queue
// ============================================================================

const QUEUE_SIZE: usize = 8;

struct ScheduledWake {
    at: u64,
    waker: Option<Waker>,
}

impl ScheduledWake {
    const fn empty() -> Self {
        Self {
            at: u64::MAX,
            waker: None,
        }
    }
}

/// A real cross-core lock, replacing the old `critical_section::Mutex<RefCell<..>>`
/// (BKL Phase 7a). The `critical_section` crate's own `Impl` for this kernel was a
/// process-global nesting counter (`CS_NESTING`/`CS_SAVED_DAIF`) that gave no
/// cross-core exclusion at all — under `smp-shared`, core A's `acquire` and core B's
/// `release` shared the same counter, so a concurrent pair could restore DAIF while a
/// critical section was still open elsewhere. The BKL hid that by serializing all of
/// EL1; giving the queue its own `Spinlock` removes the dependency on it.
static ALARM_QUEUE: Spinlock<[ScheduledWake; QUEUE_SIZE]> = Spinlock::new({
    const EMPTY: ScheduledWake = ScheduledWake::empty();
    [EMPTY; QUEUE_SIZE]
});

/// Schedule a waker to fire at a given deadline (in microseconds)
pub fn schedule_wake(at_us: u64, waker: &Waker) {
    // Callers run in ordinary EL1 code with IRQs enabled, so mask them for the hold —
    // otherwise the timer IRQ firing on this same core while we hold the lock would
    // have `on_timer_interrupt` spin forever on a `Spinlock` this core already owns.
    let _irq_guard = crate::runtime::IrqGuard::new();
    let mut queue = ALARM_QUEUE.lock();

    // Find a slot - prefer empty slots or replace matching waker
    let mut found_slot = None;
    let mut earliest_idx = 0;
    let mut earliest_time = u64::MAX;

    for (i, entry) in queue.iter_mut().enumerate() {
        if entry.waker.is_none() {
            found_slot = Some(i);
            break;
        }

        // Same waker -- update in place
        if entry.waker.as_ref().is_some_and(|w| w.will_wake(waker)) {
            entry.at = at_us;
            update_hardware_timer(&queue);
            return;
        }

        if entry.at < earliest_time {
            earliest_time = entry.at;
            earliest_idx = i;
        }
    }

    let slot = found_slot.unwrap_or(earliest_idx);
    queue[slot] = ScheduledWake {
        at: at_us,
        waker: Some(waker.clone()),
    };

    update_hardware_timer(&queue);
}

/// Alarm servicing is driven by the periodic preemption tick, which owns the
/// virtual timer hardware (CNTV_CVAL) — see `timer::timer_irq_handler`. There is
/// only one virtual timer compare register, and the scheduler must keep it armed
/// at a fixed ~10ms period; if this queue also wrote CNTV_CVAL it would push the
/// next tick out to a far-future alarm (e.g. a 5s `Timer::after`) and freeze
/// preemption. So this is intentionally a no-op: alarms are checked every tick in
/// `on_timer_interrupt`, giving them the scheduler quantum (~10ms) as resolution,
/// which is fine for SSH read timeouts and periodic monitors.
fn update_hardware_timer(_queue: &[ScheduledWake; QUEUE_SIZE]) {}

/// Check and fire expired alarms. Call from IRQ 27 handler.
///
/// Wakers are collected while the queue lock is held but woken OUTSIDE it, to avoid
/// deadlocks or increased interrupt latency. No `IrqGuard` needed here: this runs from
/// IRQ context, where the CPU has already masked IRQs on this core since exception
/// entry (BKL Phase 7a — this queue no longer needs the BKL to be safe cross-core).
#[inline]
pub fn on_timer_interrupt() {
    let now = now_us();

    let mut wakers_to_wake: [Option<Waker>; QUEUE_SIZE] = Default::default();

    {
        let mut queue = ALARM_QUEUE.lock();

        for (i, entry) in queue.iter_mut().enumerate() {
            if entry.waker.is_some() && entry.at <= now {
                wakers_to_wake[i] = entry.waker.take();
                entry.at = u64::MAX;
            }
        }

        update_hardware_timer(&queue);
    }

    let mut any_woken = false;
    for waker in wakers_to_wake.into_iter().flatten() {
        waker.wake();
        any_woken = true;
    }

    if any_woken {
        signal_wake();
    }

    // Check ITIMER_REAL / alarm() expirations and deliver SIGALRM.
    (crate::runtime::runtime().check_itimers)();
}

// ============================================================================
// ARM WFE/SEV
// ============================================================================

/// Signal that async work is ready using ARM SEV instruction
///
/// Wakes any cores waiting in WFE.
#[inline(always)]
pub fn signal_wake() {
    #[cfg(target_os = "none")]
    unsafe { core::arch::asm!("sev") }
    #[cfg(not(target_os = "none"))]
    {}
}

// ============================================================================
// Initialization
// ============================================================================

/// Initialize the alarm queue.
///
/// The pre-extraction version disabled CNTV here ("until alarms are set") —
/// but CNTV belongs to the scheduler tick, which `src/timer.rs` arms
/// separately and later; nothing here ever arms or owns it. The only state
/// is the statically initialized queue, so this is deliberately empty and
/// kept only as the boot-sequence marker.
pub fn init() {}

// ============================================================================
// with_timeout
// ============================================================================


// ============================================================================
// Timer (async delay)
// ============================================================================

/// Simple async timer for delays.
pub struct Timer {
    deadline_us: u64,
}

impl Timer {
    /// Create a future that completes after `duration`.
    pub fn after(duration: Duration) -> Self {
        Self {
            deadline_us: now_us().saturating_add(duration.as_micros()),
        }
    }
}

impl Future for Timer {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if now_us() >= self.deadline_us {
            Poll::Ready(())
        } else {
            schedule_wake(self.deadline_us, cx.waker());
            Poll::Pending
        }
    }
}

