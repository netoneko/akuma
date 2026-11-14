//! The unserved-syscall note: each number named on the console once.

use core::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// One bit per syscall number below 512 already named.
static SEEN: [AtomicU64; 8] = [const { AtomicU64::new(0) }; 8];

/// Whether `nr` is being reported for the first time (numbers from 512 up always are).
#[must_use]
pub fn first_time(nr: u64) -> bool {
    if nr >= 512 {
        return true;
    }
    let bit = 1u64 << (nr & 63);
    SEEN[(nr >> 6) as usize].fetch_or(bit, Relaxed) & bit == 0
}

/// Name an unserved syscall number on the console, once per number.
pub fn note(nr: u64) {
    if first_time(nr) {
        akuma_primitives::safe_print!(64, "[nosys] nr={}\n", nr);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn once_per_number() {
        assert!(super::first_time(301));
        assert!(!super::first_time(301));
        assert!(super::first_time(600));
        assert!(super::first_time(600));
    }
}
