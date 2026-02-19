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
use core::cell::RefCell;
use core::future::Future;
use core::pin::Pin;
use core::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use core::task::{Context, Poll, Waker};

use critical_section::Mutex;

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

    pub const fn from_micros(us: u64) -> Self {
        Self { us }
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
        ((counter as u128 * TICK_HZ as u128) / freq as u128) as u64
    } else {
        0
    }
}

/// Convert microseconds to hardware counter ticks
#[inline]
fn us_to_counter(us: u64) -> u64 {
    let freq = read_frequency();
    ((us as u128 * freq as u128) / TICK_HZ as u128) as u64
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

struct AlarmQueue {
    queue: Mutex<RefCell<[ScheduledWake; QUEUE_SIZE]>>,
}

impl AlarmQueue {
    const fn new() -> Self {
        const EMPTY: ScheduledWake = ScheduledWake::empty();
        Self {
            queue: Mutex::new(RefCell::new([EMPTY; QUEUE_SIZE])),
        }
    }
}

static ALARM_QUEUE: AlarmQueue = AlarmQueue::new();

/// Schedule a waker to fire at a given deadline (in microseconds)
pub fn schedule_wake(at_us: u64, waker: &Waker) {
    critical_section::with(|cs| {
        let mut queue = ALARM_QUEUE.queue.borrow(cs).borrow_mut();

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
            if entry.waker.as_ref().map_or(false, |w| w.will_wake(waker)) {
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
    });
}

/// Update CNTV_CVAL to the earliest pending alarm
fn update_hardware_timer(queue: &[ScheduledWake; QUEUE_SIZE]) {
    let mut earliest = u64::MAX;

    for entry in queue.iter() {
        if entry.waker.is_some() && entry.at < earliest {
            earliest = entry.at;
        }
    }

    if earliest != u64::MAX {
        let counter_target = us_to_counter(earliest);
        unsafe {
            asm!("msr cntv_cval_el0, {}", in(reg) counter_target);
            asm!("msr cntv_ctl_el0, {}", in(reg) 1u64);
        }
    } else {
        // No pending alarms -- disable virtual timer
        unsafe {
            asm!("msr cntv_ctl_el0, {}", in(reg) 0u64);
        }
    }
}

/// Check and fire expired alarms. Call from IRQ 27 handler.
///
/// Wakers are collected inside the critical section but woken OUTSIDE
/// to avoid deadlocks or increased interrupt latency.
pub fn on_timer_interrupt() {
    let now = now_us();

    let mut wakers_to_wake: [Option<Waker>; QUEUE_SIZE] = Default::default();

    critical_section::with(|cs| {
        let mut queue = ALARM_QUEUE.queue.borrow(cs).borrow_mut();

        for (i, entry) in queue.iter_mut().enumerate() {
            if entry.waker.is_some() && entry.at <= now {
                wakers_to_wake[i] = entry.waker.take();
                entry.at = u64::MAX;
            }
        }

        update_hardware_timer(&queue);
    });

    let mut any_woken = false;
    for waker in wakers_to_wake.into_iter().flatten() {
        waker.wake();
        any_woken = true;
    }

    if any_woken {
        signal_wake();
    }
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

// ============================================================================
// critical-section implementation for bare-metal aarch64
// ============================================================================

// Nesting counter approach -- disables IRQs via DAIF and tracks depth.

static CS_NESTING: AtomicU8 = AtomicU8::new(0);
static CS_SAVED_DAIF: AtomicU64 = AtomicU64::new(0);

struct CriticalSection;

critical_section::set_impl!(CriticalSection);

unsafe impl critical_section::Impl for CriticalSection {
    unsafe fn acquire() -> critical_section::RawRestoreState {
        // Read current DAIF and disable IRQs atomically, then update nesting.
        let daif: u64;
        unsafe {
            asm!(
                "mrs {0}, daif",
                "msr daifset, #2",
                "isb",
                out(reg) daif,
                options(nomem, nostack)
            );
        }

        let nesting = CS_NESTING.fetch_add(1, Ordering::SeqCst);
        if nesting == 0 {
            CS_SAVED_DAIF.store(daif, Ordering::SeqCst);
        }
    }

    unsafe fn release(_restore_state: critical_section::RawRestoreState) {
        let nesting = CS_NESTING.fetch_sub(1, Ordering::SeqCst);
        if nesting == 1 {
            let daif = CS_SAVED_DAIF.load(Ordering::SeqCst);
            unsafe {
                asm!("msr daif, {}", in(reg) daif, options(nomem, nostack));
            }
        }
    }
}
