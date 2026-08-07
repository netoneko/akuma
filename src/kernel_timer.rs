//! Kernel async timer primitives for bare-metal aarch64
//!
//! Replaces Embassy's time driver with a minimal implementation built directly
//! on the ARM Virtual Timer (CNTV). Provides:
//! - `with_timeout()` -- wrap a future with a deadline
//! - `Timer::after()` -- async delay
//! - `Duration` -- minimal duration type
//!
//! Uses the VIRTUAL timer (CNTV, IRQ 27) to avoid conflict with the scheduler
//! which uses the physical timer (CNTP) for preemptive scheduling.

use core::arch::asm;
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
    pub const fn from_secs(secs: u64) -> Self {
        Self { us: secs * 1_000_000 }
    }

    pub const fn from_millis(ms: u64) -> Self {
        Self { us: ms * 1_000 }
    }

    pub const fn as_micros(&self) -> u64 {
        self.us
    }
}

// ============================================================================
// Timeout Error
// ============================================================================

/// Error returned when a future times out
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeoutError;

impl core::fmt::Display for TimeoutError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "timeout")
    }
}

// ============================================================================
// ARM Timer Hardware Access
// ============================================================================

/// Tick frequency -- 1MHz (microsecond precision)
const TICK_HZ: u64 = 1_000_000;

/// Read the ARM virtual timer counter (CNTVCT)
#[inline]
fn read_counter() -> u64 {
    let counter: u64;
    unsafe {
        asm!("mrs {}, cntvct_el0", out(reg) counter);
    }
    counter
}

/// Read the ARM timer frequency (CNTFRQ)
#[inline]
fn read_frequency() -> u64 {
    let freq: u64;
    unsafe {
        asm!("mrs {}, cntfrq_el0", out(reg) freq);
    }
    freq
}

/// Convert hardware counter ticks to microseconds
#[inline]
fn counter_to_us(counter: u64) -> u64 {
    let freq = read_frequency();
    if freq > 0 {
        ((u128::from(counter) * u128::from(TICK_HZ)) / u128::from(freq)) as u64
    } else {
        0
    }
}

/// Get current time in microseconds (from virtual counter)
#[inline]
pub fn now_us() -> u64 {
    counter_to_us(read_counter())
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
    let _irq_guard = crate::irq::IrqGuard::new();
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
    crate::syscall::check_itimers();
}

// ============================================================================
// ARM WFE/SEV
// ============================================================================

/// Signal that async work is ready using ARM SEV instruction
///
/// Wakes any cores waiting in WFE.
#[inline(always)]
pub fn signal_wake() {
    unsafe { asm!("sev") }
}

// ============================================================================
// Initialization
// ============================================================================

/// Initialize the kernel timer subsystem.
/// Call early in boot, before using any async timer functionality.
pub fn init() {
    // Disable the virtual timer until alarms are set.
    // We use CNTV to avoid conflict with CNTP (scheduler).
    unsafe {
        asm!("msr cntv_ctl_el0, {}", in(reg) 0u64);
    }
    crate::console::print("[KernelTimer] Initialized (CNTV)\n");
}

// ============================================================================
// with_timeout
// ============================================================================

/// Wrap a future with a timeout. Returns `Err(TimeoutError)` if the deadline
/// elapses before the inner future completes.
pub async fn with_timeout<F: Future>(
    duration: Duration,
    future: F,
) -> Result<F::Output, TimeoutError> {
    let deadline_us = now_us().saturating_add(duration.as_micros());
    let mut future = core::pin::pin!(future);

    core::future::poll_fn(move |cx| {
        // Poll the inner future first
        if let Poll::Ready(val) = future.as_mut().poll(cx) {
            return Poll::Ready(Ok(val));
        }

        // Check deadline
        if now_us() >= deadline_us {
            return Poll::Ready(Err(TimeoutError));
        }

        // Schedule a wakeup at the deadline so we don't miss it
        schedule_wake(deadline_us, cx.waker());
        Poll::Pending
    })
    .await
}

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

