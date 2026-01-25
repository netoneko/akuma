//! Time handling for akuma

use core::ops::{Add, Sub, AddAssign, SubAssign};

/// A measurement of monotonically nondecreasing clock
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Instant {
    micros: u64,
}

impl Instant {
    /// Returns an instant corresponding to "now"
    pub fn now() -> Instant {
        Instant {
            micros: libakuma::uptime(),
        }
    }

    /// Returns the amount of time elapsed since this instant
    pub fn elapsed(&self) -> Duration {
        Instant::now().duration_since(*self)
    }

    /// Returns the amount of time elapsed from another instant
    pub fn duration_since(&self, earlier: Instant) -> Duration {
        Duration::from_micros(self.micros.saturating_sub(earlier.micros))
    }

    /// Returns the amount of time elapsed from another instant, or None if earlier is later
    pub fn checked_duration_since(&self, earlier: Instant) -> Option<Duration> {
        if self.micros >= earlier.micros {
            Some(Duration::from_micros(self.micros - earlier.micros))
        } else {
            None
        }
    }

    /// Returns self + duration, or None if overflow
    pub fn checked_add(&self, duration: Duration) -> Option<Instant> {
        self.micros.checked_add(duration.as_micros() as u64).map(|m| Instant { micros: m })
    }

    /// Returns self - duration, or None if underflow
    pub fn checked_sub(&self, duration: Duration) -> Option<Instant> {
        self.micros.checked_sub(duration.as_micros() as u64).map(|m| Instant { micros: m })
    }

    /// Saturating add
    pub fn saturating_duration_since(&self, earlier: Instant) -> Duration {
        self.checked_duration_since(earlier).unwrap_or(Duration::ZERO)
    }
}

impl Add<Duration> for Instant {
    type Output = Instant;

    fn add(self, other: Duration) -> Instant {
        self.checked_add(other).expect("overflow when adding duration to instant")
    }
}

impl Sub<Duration> for Instant {
    type Output = Instant;

    fn sub(self, other: Duration) -> Instant {
        self.checked_sub(other).expect("overflow when subtracting duration from instant")
    }
}

impl Sub<Instant> for Instant {
    type Output = Duration;

    fn sub(self, other: Instant) -> Duration {
        self.duration_since(other)
    }
}

impl AddAssign<Duration> for Instant {
    fn add_assign(&mut self, other: Duration) {
        *self = *self + other;
    }
}

impl SubAssign<Duration> for Instant {
    fn sub_assign(&mut self, other: Duration) {
        *self = *self - other;
    }
}

impl core::fmt::Debug for Instant {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Instant {{ micros: {} }}", self.micros)
    }
}

/// A span of time
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Duration {
    secs: u64,
    nanos: u32, // 0..999_999_999
}

impl Duration {
    /// Duration of zero time
    pub const ZERO: Duration = Duration { secs: 0, nanos: 0 };
    
    /// Maximum duration
    pub const MAX: Duration = Duration { secs: u64::MAX, nanos: 999_999_999 };
    
    /// One second
    pub const SECOND: Duration = Duration { secs: 1, nanos: 0 };
    
    /// One millisecond
    pub const MILLISECOND: Duration = Duration { secs: 0, nanos: 1_000_000 };
    
    /// One microsecond
    pub const MICROSECOND: Duration = Duration { secs: 0, nanos: 1_000 };
    
    /// One nanosecond
    pub const NANOSECOND: Duration = Duration { secs: 0, nanos: 1 };

    /// Create a new Duration from seconds and nanoseconds
    pub const fn new(secs: u64, nanos: u32) -> Duration {
        let extra_secs = (nanos / 1_000_000_000) as u64;
        let nanos = nanos % 1_000_000_000;
        Duration {
            secs: secs + extra_secs,
            nanos,
        }
    }

    /// Create from seconds
    pub const fn from_secs(secs: u64) -> Duration {
        Duration { secs, nanos: 0 }
    }

    /// Create from milliseconds
    pub const fn from_millis(millis: u64) -> Duration {
        Duration {
            secs: millis / 1000,
            nanos: ((millis % 1000) * 1_000_000) as u32,
        }
    }

    /// Create from microseconds
    pub const fn from_micros(micros: u64) -> Duration {
        Duration {
            secs: micros / 1_000_000,
            nanos: ((micros % 1_000_000) * 1000) as u32,
        }
    }

    /// Create from nanoseconds
    pub const fn from_nanos(nanos: u64) -> Duration {
        Duration {
            secs: nanos / 1_000_000_000,
            nanos: (nanos % 1_000_000_000) as u32,
        }
    }

    /// Create from floating-point seconds
    pub fn from_secs_f64(secs: f64) -> Duration {
        let whole_secs = secs as u64;
        let nanos = ((secs - whole_secs as f64) * 1_000_000_000.0) as u32;
        Duration { secs: whole_secs, nanos }
    }

    /// Create from floating-point seconds (32-bit)
    pub fn from_secs_f32(secs: f32) -> Duration {
        Duration::from_secs_f64(secs as f64)
    }

    /// Returns true if this Duration is zero
    pub const fn is_zero(&self) -> bool {
        self.secs == 0 && self.nanos == 0
    }

    /// Returns the number of whole seconds
    pub const fn as_secs(&self) -> u64 {
        self.secs
    }

    /// Returns the fractional part in milliseconds
    pub const fn subsec_millis(&self) -> u32 {
        self.nanos / 1_000_000
    }

    /// Returns the fractional part in microseconds
    pub const fn subsec_micros(&self) -> u32 {
        self.nanos / 1_000
    }

    /// Returns the fractional part in nanoseconds
    pub const fn subsec_nanos(&self) -> u32 {
        self.nanos
    }

    /// Total milliseconds
    pub const fn as_millis(&self) -> u128 {
        self.secs as u128 * 1000 + self.nanos as u128 / 1_000_000
    }

    /// Total microseconds
    pub const fn as_micros(&self) -> u128 {
        self.secs as u128 * 1_000_000 + self.nanos as u128 / 1_000
    }

    /// Total nanoseconds
    pub const fn as_nanos(&self) -> u128 {
        self.secs as u128 * 1_000_000_000 + self.nanos as u128
    }

    /// As floating-point seconds
    pub fn as_secs_f64(&self) -> f64 {
        self.secs as f64 + self.nanos as f64 / 1_000_000_000.0
    }

    /// As floating-point seconds (32-bit)
    pub fn as_secs_f32(&self) -> f32 {
        self.as_secs_f64() as f32
    }

    /// Checked addition
    pub fn checked_add(self, rhs: Duration) -> Option<Duration> {
        let mut secs = self.secs.checked_add(rhs.secs)?;
        let mut nanos = self.nanos + rhs.nanos;
        if nanos >= 1_000_000_000 {
            nanos -= 1_000_000_000;
            secs = secs.checked_add(1)?;
        }
        Some(Duration { secs, nanos })
    }

    /// Saturating addition
    pub fn saturating_add(self, rhs: Duration) -> Duration {
        self.checked_add(rhs).unwrap_or(Duration::MAX)
    }

    /// Checked subtraction
    pub fn checked_sub(self, rhs: Duration) -> Option<Duration> {
        let mut secs = self.secs.checked_sub(rhs.secs)?;
        let nanos = if self.nanos >= rhs.nanos {
            self.nanos - rhs.nanos
        } else {
            secs = secs.checked_sub(1)?;
            self.nanos + 1_000_000_000 - rhs.nanos
        };
        Some(Duration { secs, nanos })
    }

    /// Saturating subtraction
    pub fn saturating_sub(self, rhs: Duration) -> Duration {
        self.checked_sub(rhs).unwrap_or(Duration::ZERO)
    }

    /// Checked multiplication
    pub fn checked_mul(self, rhs: u32) -> Option<Duration> {
        let total_nanos = self.nanos as u64 * rhs as u64;
        let extra_secs = total_nanos / 1_000_000_000;
        let nanos = (total_nanos % 1_000_000_000) as u32;
        let secs = self.secs.checked_mul(rhs as u64)?.checked_add(extra_secs)?;
        Some(Duration { secs, nanos })
    }

    /// Saturating multiplication
    pub fn saturating_mul(self, rhs: u32) -> Duration {
        self.checked_mul(rhs).unwrap_or(Duration::MAX)
    }

    /// Checked division
    pub fn checked_div(self, rhs: u32) -> Option<Duration> {
        if rhs == 0 {
            return None;
        }
        let secs = self.secs / rhs as u64;
        let carry = self.secs % rhs as u64;
        let extra_nanos = carry * 1_000_000_000 / rhs as u64;
        let nanos = (self.nanos as u64 / rhs as u64 + extra_nanos) as u32;
        Some(Duration { secs, nanos })
    }
}

impl Add for Duration {
    type Output = Duration;

    fn add(self, rhs: Duration) -> Duration {
        self.checked_add(rhs).expect("overflow when adding durations")
    }
}

impl Sub for Duration {
    type Output = Duration;

    fn sub(self, rhs: Duration) -> Duration {
        self.checked_sub(rhs).expect("overflow when subtracting durations")
    }
}

impl AddAssign for Duration {
    fn add_assign(&mut self, rhs: Duration) {
        *self = *self + rhs;
    }
}

impl SubAssign for Duration {
    fn sub_assign(&mut self, rhs: Duration) {
        *self = *self - rhs;
    }
}

impl core::fmt::Debug for Duration {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        if self.nanos == 0 {
            write!(f, "{}s", self.secs)
        } else if self.nanos % 1_000_000 == 0 {
            write!(f, "{}.{:03}s", self.secs, self.nanos / 1_000_000)
        } else if self.nanos % 1_000 == 0 {
            write!(f, "{}.{:06}s", self.secs, self.nanos / 1_000)
        } else {
            write!(f, "{}.{:09}s", self.secs, self.nanos)
        }
    }
}

/// System time (stub - akuma doesn't have RTC access in userspace yet)
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct SystemTime {
    /// Duration since UNIX epoch
    dur: Duration,
}

impl SystemTime {
    /// The Unix epoch (1970-01-01 00:00:00 UTC)
    pub const UNIX_EPOCH: SystemTime = SystemTime { dur: Duration::ZERO };

    /// Returns the current system time
    pub fn now() -> SystemTime {
        // Use uptime as a proxy since we don't have RTC
        SystemTime {
            dur: Duration::from_micros(libakuma::uptime()),
        }
    }

    /// Returns the duration since an earlier time
    pub fn duration_since(&self, earlier: SystemTime) -> Result<Duration, SystemTimeError> {
        if self.dur >= earlier.dur {
            Ok(Duration::from_nanos((self.dur.as_nanos() - earlier.dur.as_nanos()) as u64))
        } else {
            Err(SystemTimeError(Duration::from_nanos((earlier.dur.as_nanos() - self.dur.as_nanos()) as u64)))
        }
    }

    /// Returns elapsed time since this instant
    pub fn elapsed(&self) -> Result<Duration, SystemTimeError> {
        SystemTime::now().duration_since(*self)
    }
}

impl core::fmt::Debug for SystemTime {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "SystemTime {{ secs: {} }}", self.dur.as_secs())
    }
}

/// Error type for system time operations
#[derive(Clone, Debug)]
pub struct SystemTimeError(Duration);

impl SystemTimeError {
    /// Returns the positive duration between the two times
    pub fn duration(&self) -> Duration {
        self.0
    }
}

impl core::fmt::Display for SystemTimeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "second time provided was later than self")
    }
}

/// Unix epoch constant
pub const UNIX_EPOCH: SystemTime = SystemTime::UNIX_EPOCH;
