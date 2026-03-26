use alloc::string::String;
use alloc::format;
use arm_pl031::Rtc;
use core::arch::asm;
use core::sync::atomic::{AtomicU64, Ordering};
use spinning_top::Spinlock;

// UTC offset in microseconds since Unix epoch (1970-01-01 00:00:00)
// Can be set via set_utc_offset() to sync with real time
static UTC_OFFSET_US: Spinlock<Option<u64>> = Spinlock::new(None);

// PL031 RTC instance for reading real-time clock from QEMU
// The standard PL031 address for QEMU virt machine is 0x9010000
static RTC: Spinlock<Option<Rtc>> = Spinlock::new(None);

pub fn init() {
    // Initialize the PL031 RTC
    // SAFETY: 0x9010000 is the standard PL031 RTC address on QEMU virt machine
    unsafe {
        let rtc = Rtc::new(0x9010000 as *mut _);
        *RTC.lock() = Some(rtc);
    }
}

// Enable timer interrupts for preemptive scheduling
// Store configured interval for use in handler
static TIMER_INTERVAL_US: AtomicU64 = AtomicU64::new(10_000); // Default 10ms

// interval_us: interval in microseconds between interrupts
pub fn enable_timer_interrupts(interval_us: u64) {
    TIMER_INTERVAL_US.store(interval_us, Ordering::Relaxed);

    let freq = read_frequency();
    let ticks = (freq * interval_us) / 1_000_000;
    let counter = read_counter();
    let new_cval = counter + ticks;

    unsafe {
        // Set the timer compare value
        asm!("msr cntp_cval_el0, {}", in(reg) new_cval);

        // Enable the timer (bit 0 = enable, bit 1 = !mask)
        asm!("msr cntp_ctl_el0, {}", in(reg) 1u64);
    }
}

// Timer interrupt handler - called from IRQ handler
pub fn timer_irq_handler(_irq: u32) {
    // Acknowledge interrupt by setting next compare value
    let freq = read_frequency();
    let interval_us = TIMER_INTERVAL_US.load(Ordering::Relaxed);
    let interval_ticks = (freq * interval_us) / 1_000_000;
    let counter = read_counter();
    let new_cval = counter + interval_ticks;

    unsafe {
        asm!("msr cntp_cval_el0, {}", in(reg) new_cval);
    }

    // Check preemption watchdog - detect threads that hold preemption disabled too long
    if crate::config::ENABLE_PREEMPTION_WATCHDOG {
        if let Some(duration_us) = akuma_exec::threading::check_preemption_watchdog() {
            // Log warning
            // Use AtomicU64 instead of static mut to avoid data races
            static LAST_WARN_US: AtomicU64 = AtomicU64::new(0);
            let now = uptime_us();
            let last = LAST_WARN_US.load(Ordering::Relaxed);
            // Rate-limit warnings to once per second
            if now.saturating_sub(last) > 1_000_000 {
                LAST_WARN_US.store(now, Ordering::Relaxed);
                // Get poll step to help diagnose where we're stuck
                let step = crate::GLOBAL_POLL_STEP.load(Ordering::Relaxed);
                // Use stack-only print to avoid heap allocation in IRQ context
                crate::safe_print!(96, "[WATCHDOG] Preemption disabled for {}ms at step {}\n", 
                    duration_us / 1000, step);
            }
        }
    }

    // NOTE: cleanup_terminated() is NOT called here because it allocates/deallocates
    // memory which could deadlock if main code is in the middle of an allocation.
    // Cleanup should be done from user code via threading::cleanup_terminated().

    // #region agent log
    {
        static TIMER_TICK: AtomicU64 = AtomicU64::new(0);
        let tick = TIMER_TICK.fetch_add(1, Ordering::Relaxed);
        let forking = akuma_exec::process::FORK_IN_PROGRESS.load(Ordering::Relaxed);
        let interval = if forking { 50 } else { 500 };
        if tick % interval == 0 {
            let tid = akuma_exec::threading::current_thread_id();
            crate::safe_print!(64, "[TMR] t={} T={} f={}\n", tick, tid, forking as u8);
        }
    }
    // #endregion

    // Trigger SGI for scheduling - scheduler will decide if switch is needed
    crate::gic::trigger_sgi(crate::gic::SGI_SCHEDULER);
}

// Read the ARM Generic Timer counter
pub fn read_counter() -> u64 {
    let counter: u64;
    unsafe {
        asm!("mrs {}, cntpct_el0", out(reg) counter);
    }
    counter
}

// Read the timer frequency (public for debugging)
pub fn read_frequency() -> u64 {
    let freq: u64;
    unsafe {
        asm!("mrs {}, cntfrq_el0", out(reg) freq);
    }
    freq
}

// Get time in microseconds
// Note: Uses u128 intermediate to prevent overflow during multiplication
pub fn get_time_us() -> u64 {
    let counter = read_counter();
    let freq = read_frequency();
    if freq > 0 {
        // Use u128 to prevent overflow when multiplying counter * 1_000_000
        ((counter as u128 * 1_000_000) / freq as u128) as u64
    } else {
        0
    }
}

// Get current time as u64 microseconds since boot
// Overflows after ~584 years
pub fn uptime_us() -> u64 {
    get_time_us()
}

// Read Unix timestamp from PL031 RTC (seconds since Unix epoch)
// Returns None if RTC is not initialized
pub fn read_rtc_timestamp() -> Option<u32> {
    let rtc = RTC.lock();
    rtc.as_ref().map(|r| r.get_unix_timestamp())
}

// Initialize UTC time from PL031 RTC
// Returns true if successful, false if RTC not available
pub fn init_utc_from_rtc() -> bool {
    if let Some(timestamp) = read_rtc_timestamp() {
        // Convert seconds to microseconds
        let unix_epoch_us = (timestamp as u64) * 1_000_000;
        set_utc_time_us(unix_epoch_us);
        true
    } else {
        false
    }
}

// Set UTC offset for real-world time tracking
// unix_epoch_us: microseconds since Unix epoch (1970-01-01 00:00:00 UTC)
pub fn set_utc_time_us(unix_epoch_us: u64) {
    let boot_time = uptime_us();
    let mut offset = UTC_OFFSET_US.lock();
    *offset = Some(unix_epoch_us.saturating_sub(boot_time));
}

// Get current UTC time in microseconds since Unix epoch
// Returns None if UTC time has not been set
pub fn utc_time_us() -> Option<u64> {
    let offset = UTC_OFFSET_US.lock();
    offset.map(|off| off.wrapping_add(uptime_us()))
}

// Get current UTC time in seconds since Unix epoch
// Returns None if UTC time has not been set
// Used by TLS certificate verification
pub fn utc_seconds() -> Option<u64> {
    utc_time_us().map(|us| us / 1_000_000)
}

// DateTime structure for ISO 8601 formatting
#[derive(Debug, Clone, Copy)]
pub struct DateTime {
    pub year: u32,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
    pub microsecond: u32,
}

impl DateTime {
    // Convert microseconds since Unix epoch to DateTime
    pub fn from_unix_us(us: u64) -> Self {
        let secs = us / 1_000_000;
        let micros = (us % 1_000_000) as u32;

        // Days since Unix epoch
        let mut days = secs / 86400;
        let secs_today = secs % 86400;

        // Time of day
        let hour = (secs_today / 3600) as u8;
        let minute = ((secs_today % 3600) / 60) as u8;
        let second = (secs_today % 60) as u8;

        // Calculate year (starting from 1970)
        let mut year = 1970;
        loop {
            let year_days = if is_leap_year(year) { 366 } else { 365 };
            if days < year_days {
                break;
            }
            days -= year_days;
            year += 1;
        }

        // Calculate month and day
        let months = if is_leap_year(year) {
            [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
        } else {
            [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
        };

        let mut month = 1;
        for &month_days in &months {
            if days < month_days as u64 {
                break;
            }
            days -= month_days as u64;
            month += 1;
        }

        let day = (days + 1) as u8;

        Self {
            year,
            month,
            day,
            hour,
            minute,
            second,
            microsecond: micros,
        }
    }

    // Format as ISO 8601: YYYY-MM-DDTHH:MM:SS.ssssssZ
    pub fn to_iso8601(&self) -> String {
        format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:06}Z",
            self.year,
            self.month,
            self.day,
            self.hour,
            self.minute,
            self.second,
            self.microsecond
        )
    }
}

// Check if a year is a leap year
fn is_leap_year(year: u32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0)
}

// Get current UTC time as ISO 8601 string
// Returns "NOT_SET" if UTC time hasn't been configured
pub fn utc_iso8601() -> String {
    match utc_time_us() {
        Some(us) => DateTime::from_unix_us(us).to_iso8601(),
        None => String::from("NOT_SET"),
    }
}
