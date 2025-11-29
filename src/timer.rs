use alloc::string::String;
use arm_pl031::Rtc;
use core::arch::asm;
use spinning_top::Spinlock;

// Manual tick counter (u64)
// Overflow times at different frequencies:
// - 1 kHz (ms): ~584 million years
// - 1 MHz (μs): ~584 thousand years
// For most embedded systems, u64 is sufficient. Use wrapping arithmetic if needed.
static TICK_COUNT: Spinlock<u64> = Spinlock::new(0);

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
static mut TIMER_INTERVAL_US: u64 = 10_000; // Default 10ms

// interval_us: interval in microseconds between interrupts
pub fn enable_timer_interrupts(interval_us: u64) {
    unsafe { TIMER_INTERVAL_US = interval_us; }
    
    let freq = read_frequency();
    let ticks = (freq * interval_us) / 1_000_000;

    unsafe {
        // Set the timer compare value
        asm!("msr cntp_cval_el0, {}", in(reg) read_counter() + ticks);

        // Enable the timer (bit 0 = enable, bit 1 = !mask)
        asm!("msr cntp_ctl_el0, {}", in(reg) 1u64);
    }
}

/// How often to run thread cleanup (in timer ticks)
/// Set to 1 to clean up every timer tick (every ~100ms with current config)
const CLEANUP_INTERVAL_TICKS: u32 = 1;

// Timer interrupt handler - called from IRQ handler  
pub fn timer_irq_handler(_irq: u32) {
    static mut TICK_COUNTER: u32 = 0;
    
    // Acknowledge interrupt by setting next compare value
    let freq = read_frequency();
    let interval_us = unsafe { TIMER_INTERVAL_US };
    let interval_ticks = (freq * interval_us) / 1_000_000;
    
    unsafe {
        asm!("msr cntp_cval_el0, {}", in(reg) read_counter() + interval_ticks);
    }
    
    // Periodic cleanup of terminated threads
    unsafe {
        TICK_COUNTER += 1;
        if TICK_COUNTER >= CLEANUP_INTERVAL_TICKS {
            TICK_COUNTER = 0;
            let _ = crate::threading::cleanup_terminated();
        }
    }
    
    // Trigger SGI for scheduling - scheduler will decide if switch is needed
    crate::gic::trigger_sgi(crate::gic::SGI_SCHEDULER);
}

pub fn tick() {
    let mut count = TICK_COUNT.lock();
    *count = count.wrapping_add(1);
}

pub fn get_ticks() -> u64 {
    *TICK_COUNT.lock()
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

// Simple delay using the timer counter
// Uses wrapping subtraction to handle counter overflow correctly
pub fn delay_us(us: u64) {
    let start = get_time_us();
    while get_time_us().wrapping_sub(start) < us {
        core::hint::spin_loop();
    }
}

pub fn delay_ms(ms: u64) {
    delay_us(ms * 1000);
}

// Get time in nanoseconds using u128 to avoid overflow
// u128 can represent ~5.8 billion years at nanosecond precision
pub fn get_time_ns() -> u128 {
    let counter = read_counter();
    let freq = read_frequency();
    if freq > 0 {
        // Use u128 to prevent overflow
        (counter as u128 * 1_000_000_000) / freq as u128
    } else {
        0
    }
}

// Monotonic time structure - never overflows in practice
// Stores seconds and nanoseconds separately like POSIX timespec
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Timespec {
    pub sec: u64,  // Seconds since boot (overflows in 584 billion years)
    pub nsec: u32, // Nanoseconds (0-999,999,999)
}

impl Timespec {
    pub const fn zero() -> Self {
        Self { sec: 0, nsec: 0 }
    }

    pub fn now() -> Self {
        let counter = read_counter();
        let freq = read_frequency();
        if freq > 0 {
            let sec = counter / freq;
            let remainder = counter % freq;
            let nsec = ((remainder as u128 * 1_000_000_000) / freq as u128) as u32;
            Self { sec, nsec }
        } else {
            Self::zero()
        }
    }

    pub fn elapsed(&self) -> Self {
        let now = Self::now();
        now.sub(*self)
    }

    pub fn sub(&self, other: Self) -> Self {
        let mut sec = self.sec.wrapping_sub(other.sec);
        let mut nsec = self.nsec as i64 - other.nsec as i64;

        if nsec < 0 {
            sec = sec.wrapping_sub(1);
            nsec += 1_000_000_000;
        }

        Self {
            sec,
            nsec: nsec as u32,
        }
    }

    pub fn as_nanos(&self) -> u128 {
        (self.sec as u128) * 1_000_000_000 + (self.nsec as u128)
    }

    pub fn as_micros(&self) -> u128 {
        (self.sec as u128) * 1_000_000 + (self.nsec as u128) / 1000
    }

    pub fn as_millis(&self) -> u128 {
        (self.sec as u128) * 1000 + (self.nsec as u128) / 1_000_000
    }
}

// Nanosecond delay using hardware counter directly
// More accurate than microsecond delay for sub-microsecond delays
pub fn delay_ns(ns: u64) {
    let freq = read_frequency();
    if freq == 0 {
        return;
    }

    let start = read_counter();
    // Convert nanoseconds to counter ticks
    let ticks = ((ns as u128 * freq as u128) / 1_000_000_000) as u64;

    while read_counter().wrapping_sub(start) < ticks {
        core::hint::spin_loop();
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
        alloc::format!(
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

    // Format as ISO 8601 without microseconds: YYYY-MM-DDTHH:MM:SSZ
    pub fn to_iso8601_simple(&self) -> String {
        alloc::format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
            self.year,
            self.month,
            self.day,
            self.hour,
            self.minute,
            self.second
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

// Get current UTC time as simple ISO 8601 string (no microseconds)
pub fn utc_iso8601_simple() -> String {
    match utc_time_us() {
        Some(us) => DateTime::from_unix_us(us).to_iso8601_simple(),
        None => String::from("NOT_SET"),
    }
}
