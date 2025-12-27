//! Embassy time driver implementation for ARM Generic Timer
//!
//! This bridges the ARM timer hardware to Embassy's async timing primitives.
//! Uses the physical timer counter (CNTPCT_EL0) as the time source.

use core::arch::asm;
use core::cell::RefCell;
use core::task::Waker;

use critical_section::Mutex;
use embassy_time_driver::Driver;

/// Embassy tick frequency - 1MHz (microsecond precision)
/// This matches our existing timer infrastructure
const TICK_HZ: u64 = 1_000_000;

/// Maximum number of concurrent wake requests
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

/// The Embassy time driver instance
struct EmbassyTimeDriver {
    queue: Mutex<RefCell<[ScheduledWake; QUEUE_SIZE]>>,
}

impl EmbassyTimeDriver {
    const fn new() -> Self {
        const EMPTY: ScheduledWake = ScheduledWake::empty();
        Self {
            queue: Mutex::new(RefCell::new([EMPTY; QUEUE_SIZE])),
        }
    }
}

/// Global driver instance - used by the embassy_time_driver macro
embassy_time_driver::time_driver_impl!(static DRIVER: EmbassyTimeDriver = EmbassyTimeDriver::new());

/// Read the ARM physical timer counter
#[inline]
fn read_counter() -> u64 {
    let counter: u64;
    unsafe {
        asm!("mrs {}, cntpct_el0", out(reg) counter);
    }
    counter
}

/// Read the ARM timer frequency
#[inline]
fn read_frequency() -> u64 {
    let freq: u64;
    unsafe {
        asm!("mrs {}, cntfrq_el0", out(reg) freq);
    }
    freq
}

/// Convert hardware counter ticks to Embassy ticks (microseconds)
#[inline]
fn counter_to_ticks(counter: u64) -> u64 {
    let freq = read_frequency();
    if freq > 0 {
        // Use u128 to prevent overflow
        ((counter as u128 * TICK_HZ as u128) / freq as u128) as u64
    } else {
        0
    }
}

/// Convert Embassy ticks to hardware counter ticks
#[inline]
fn ticks_to_counter(ticks: u64) -> u64 {
    let freq = read_frequency();
    // Use u128 to prevent overflow
    ((ticks as u128 * freq as u128) / TICK_HZ as u128) as u64
}

impl Driver for EmbassyTimeDriver {
    fn now(&self) -> u64 {
        counter_to_ticks(read_counter())
    }

    fn schedule_wake(&self, at: u64, waker: &Waker) {
        critical_section::with(|cs| {
            let mut queue = self.queue.borrow(cs).borrow_mut();

            // Find a slot - prefer empty slots or replace the one with matching waker
            let mut found_slot = None;
            let mut earliest_idx = 0;
            let mut earliest_time = u64::MAX;

            for (i, entry) in queue.iter_mut().enumerate() {
                // Check for empty slot
                if entry.waker.is_none() {
                    found_slot = Some(i);
                    break;
                }

                // Check if same waker - update in place
                if entry.waker.as_ref().map_or(false, |w| w.will_wake(waker)) {
                    entry.at = at;
                    self.update_hardware_timer_locked(&queue);
                    return;
                }

                // Track the earliest for potential replacement
                if entry.at < earliest_time {
                    earliest_time = entry.at;
                    earliest_idx = i;
                }
            }

            let slot = found_slot.unwrap_or(earliest_idx);
            queue[slot] = ScheduledWake {
                at,
                waker: Some(waker.clone()),
            };

            self.update_hardware_timer_locked(&queue);
        });
    }
}

impl EmbassyTimeDriver {
    /// Update hardware timer to fire at the earliest scheduled wake time
    fn update_hardware_timer_locked(&self, queue: &[ScheduledWake; QUEUE_SIZE]) {
        let mut earliest = u64::MAX;

        for entry in queue.iter() {
            if entry.waker.is_some() && entry.at < earliest {
                earliest = entry.at;
            }
        }

        if earliest != u64::MAX {
            // Set hardware timer compare value
            let counter_target = ticks_to_counter(earliest);
            unsafe {
                asm!("msr cntp_cval_el0, {}", in(reg) counter_target);
                // Make sure timer is enabled
                asm!("msr cntp_ctl_el0, {}", in(reg) 1u64);
            }
        }
    }

    /// Check and fire any expired wakers - call from timer interrupt
    pub fn check_alarms(&self) {
        let now = self.now();

        critical_section::with(|cs| {
            let mut queue = self.queue.borrow(cs).borrow_mut();

            for entry in queue.iter_mut() {
                if entry.waker.is_some() && entry.at <= now {
                    if let Some(waker) = entry.waker.take() {
                        waker.wake();
                    }
                    entry.at = u64::MAX;
                }
            }

            // Reschedule hardware timer for next wake
            self.update_hardware_timer_locked(&queue);
        });
    }
}

/// Call this from your timer interrupt handler to check Embassy alarms
pub fn on_timer_interrupt() {
    DRIVER.check_alarms();
}

/// Initialize the Embassy time driver
/// Call this early in boot, before using any Embassy async functionality
pub fn init() {
    // The driver is statically initialized, but we can do any runtime setup here
    // Make sure the physical timer is disabled initially
    unsafe {
        asm!("msr cntp_ctl_el0, {}", in(reg) 0u64);
    }
}

// Implement critical-section for our bare metal environment
// Using a nesting counter approach since RawRestoreState is ()
use core::sync::atomic::{AtomicU8, AtomicU64, Ordering};

static CS_NESTING: AtomicU8 = AtomicU8::new(0);
static CS_SAVED_DAIF: AtomicU64 = AtomicU64::new(0);

struct CriticalSection;

critical_section::set_impl!(CriticalSection);

unsafe impl critical_section::Impl for CriticalSection {
    unsafe fn acquire() -> critical_section::RawRestoreState {
        let daif: u64;
        unsafe {
            asm!("mrs {}, daif", out(reg) daif);
            asm!("msr daifset, #2"); // Disable IRQs
        }

        let nesting = CS_NESTING.fetch_add(1, Ordering::Relaxed);
        if nesting == 0 {
            // First level - save the original DAIF
            CS_SAVED_DAIF.store(daif, Ordering::Relaxed);
        }
    }

    unsafe fn release(_restore_state: critical_section::RawRestoreState) {
        let nesting = CS_NESTING.fetch_sub(1, Ordering::Relaxed);
        if nesting == 1 {
            // Last level - restore the original DAIF
            let daif = CS_SAVED_DAIF.load(Ordering::Relaxed);
            unsafe {
                asm!("msr daif, {}", in(reg) daif);
            }
        }
    }
}
