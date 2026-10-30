//! The eventfd(2) state machine, pure: counter semantics, flag parsing,
//! readiness. Extracted (composition step 3b) from
//! `crates/akuma-syscalls-glue/src/eventfd.rs`, whose table/poller/refcount
//! plumbing is welded to `akuma-exec`'s `Process` and thread handles — the
//! decisions below are the part both that kernel and the taoxichuan
//! composition root share. No unsafe, no dependencies, host-tested.
#![forbid(unsafe_code)]
#![no_std]

/// Linux's eventfd2 flag bits (`include/uapi/linux/eventfd.h`).
pub const EFD_SEMAPHORE: u32 = 1;
pub const EFD_NONBLOCK: u32 = 0x800;
pub const EFD_CLOEXEC: u32 = 0x80000;

/// The errno a read of an empty counter answers (EAGAIN/EWOULDBLOCK).
pub const EAGAIN: i32 = 11;
/// The errno eventfd2 answers for flags outside the three bits above.
pub const EINVAL: i32 = 22;

/// Linux's write(2) rejects a value that would push the counter past
/// `u64::MAX - 1` with EAGAIN (a fresh read must always be able to clear it).
const COUNTER_MAX: u64 = u64::MAX - 1;

/// One eventfd: an 8-byte counter with read/write/semaphore semantics.
///
/// It also carries the flags `eventfd2` was created with. The owning kernel keeps this in
/// its descriptor table; `dup`'d names share one of these by construction
/// there (an index or a shared cell), never by copying — a copied counter
/// would fork the state, which Linux never does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventFd {
    counter: u64,
    flags: u32,
}

impl EventFd {
    /// `eventfd2(initval, flags)`: the counter starts at `initval`. Linux
    /// takes `initval` as u32, so it cannot exceed `COUNTER_MAX`.
    pub fn new(initval: u32, flags: u32) -> Result<Self, i32> {
        if flags & !(EFD_SEMAPHORE | EFD_NONBLOCK | EFD_CLOEXEC) != 0 {
            return Err(EINVAL);
        }
        Ok(Self { counter: u64::from(initval), flags })
    }

    #[must_use]
    pub fn flags(&self) -> u32 {
        self.flags
    }

    #[must_use]
    pub fn is_nonblock(&self) -> bool {
        self.flags & EFD_NONBLOCK != 0
    }

    /// A read is ready exactly while the counter is non-zero.
    #[must_use]
    pub fn can_read(&self) -> bool {
        self.counter > 0
    }

    /// A write is always ready short of the counter ceiling.
    #[must_use]
    pub fn can_write(&self) -> bool {
        self.counter < COUNTER_MAX
    }

    /// `read(2)`: with `EFD_SEMAPHORE` the counter decrements by one and the
    /// read yields 1; otherwise it yields the whole counter and clears it.
    /// An empty counter answers EAGAIN — the caller translates "would block"
    /// (a kernel that can wait may wait instead; a run-to-completion guest
    /// has no second task that could ever wake it, so EAGAIN is the truth
    /// there too).
    pub fn read(&mut self) -> Result<u64, i32> {
        if self.counter == 0 {
            return Err(EAGAIN);
        }
        if self.flags & EFD_SEMAPHORE != 0 {
            self.counter -= 1;
            Ok(1)
        } else {
            Ok(core::mem::take(&mut self.counter))
        }
    }

    /// `write(2)`: adds the 8-byte value; a value that would overflow the
    /// ceiling answers EAGAIN (Linux's rule, not a saturating add).
    pub fn write(&mut self, val: u64) -> Result<(), i32> {
        if val > COUNTER_MAX.saturating_sub(self.counter) {
            return Err(EAGAIN);
        }
        self.counter += val;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_yields_and_clears() {
        let mut e = EventFd::new(5, 0).unwrap();
        assert!(e.can_read());
        assert_eq!(e.read(), Ok(5));
        assert_eq!(e.counter, 0);
        assert!(!e.can_read());
        assert_eq!(e.read(), Err(EAGAIN));
    }

    #[test]
    fn semaphore_counts_down_by_one() {
        let mut e = EventFd::new(2, EFD_SEMAPHORE).unwrap();
        assert_eq!(e.read(), Ok(1));
        assert_eq!(e.read(), Ok(1));
        assert_eq!(e.read(), Err(EAGAIN));
    }

    #[test]
    fn write_adds() {
        let mut e = EventFd::new(0, 0).unwrap();
        e.write(3).unwrap();
        e.write(4).unwrap();
        assert_eq!(e.read(), Ok(7));
    }

    #[test]
    fn write_overflow_is_eagain_not_saturate() {
        let mut e = EventFd::new(0, 0).unwrap();
        e.write(COUNTER_MAX).unwrap();
        assert!(!e.can_write());
        assert_eq!(e.write(1), Err(EAGAIN));
        // the counter is untouched by the rejected write
        assert_eq!(e.read(), Ok(COUNTER_MAX));
    }

    #[test]
    fn flags_round_trip_and_mask() {
        assert!(EventFd::new(0, EFD_NONBLOCK | EFD_CLOEXEC).unwrap().is_nonblock());
        assert_eq!(EventFd::new(0, 0x8000_0000).unwrap_err(), EINVAL);
        assert_eq!(EventFd::new(0, 0).unwrap().flags(), 0);
    }

    #[test]
    fn initval_lands_in_counter() {
        assert_eq!(EventFd::new(u32::MAX, 0).unwrap().counter, u64::from(u32::MAX));
    }
}
