//! Shared socket wait hooks. Machines supply scheduler and network-pump effects.

/// The deadline strategy used by a machine's scheduler.
#[derive(Clone, Copy)]
pub enum Deadline {
    /// The scheduler supports a wakeable, deadline-bound sleep.
    Park,
    /// Poll with scheduler yields; stop even if the clock stalls.
    Yield { max_laps: u32 },
    /// Pump and perform bounded wait laps until the deadline.
    Pump,
}

/// Effects and capabilities needed by the socket wait hooks.
pub trait Machine {
    fn now() -> u64;
    fn yield_thread();
    fn park(deadline_us: u64);
    fn halt();
    fn deadline() -> Deadline;
    fn pump() {}
    #[must_use]
    fn relax_laps() -> usize {
        1
    }
    #[must_use]
    fn can_park() -> bool {
        false
    }
    #[must_use]
    fn wake_due() -> bool {
        false
    }
    #[must_use]
    fn halt_available() -> bool {
        true
    }
    #[must_use]
    fn yield_before_relax() -> bool {
        false
    }
}

/// A socket-layer yield, including any required network pump.
pub fn yield_now<M: Machine>() {
    M::pump();
    if M::can_park() {
        M::park(M::now().saturating_add(1_000));
    } else {
        M::yield_thread();
    }
}

/// Give the network and other tasks a chance to progress.
pub fn blocking_relax<M: Machine>() {
    for _ in 0..M::relax_laps() {
        M::pump();
        if M::yield_before_relax() {
            M::yield_thread();
        }
        if M::can_park() {
            M::park(M::now().saturating_add(1_000));
        } else if M::wake_due() || !M::halt_available() {
            M::yield_thread();
        } else {
            M::halt();
        }
    }
}

/// Wait until a deadline, according to the machine's scheduler capabilities.
pub fn park_until<M: Machine>(deadline_us: u64) {
    match M::deadline() {
        Deadline::Park => M::park(deadline_us),
        Deadline::Yield { max_laps } => {
            for _ in 0..max_laps {
                if M::now() >= deadline_us {
                    break;
                }
                M::yield_thread();
            }
        }
        Deadline::Pump => {
            while M::now() < deadline_us {
                M::pump();
                if M::can_park() {
                    M::park(M::now().saturating_add(1_000));
                } else {
                    M::halt();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::SeqCst};

    static NOW: AtomicU64 = AtomicU64::new(0);
    static YIELDS: AtomicUsize = AtomicUsize::new(0);
    static HALTS: AtomicUsize = AtomicUsize::new(0);
    static PARKED: AtomicU64 = AtomicU64::new(0);
    static DUE: AtomicBool = AtomicBool::new(false);
    struct Test;
    impl Machine for Test {
        fn now() -> u64 {
            NOW.load(SeqCst)
        }
        fn yield_thread() {
            YIELDS.fetch_add(1, SeqCst);
            NOW.fetch_add(1, SeqCst);
        }
        fn park(deadline: u64) {
            PARKED.store(deadline, SeqCst);
        }
        fn halt() {
            HALTS.fetch_add(1, SeqCst);
        }
        fn deadline() -> Deadline {
            Deadline::Yield { max_laps: 3 }
        }
        fn wake_due() -> bool {
            DUE.load(SeqCst)
        }
    }
    #[test]
    fn due_sleepers_yield_and_deadline_poll_is_bounded() {
        blocking_relax::<Test>();
        assert_eq!(HALTS.load(SeqCst), 1);
        DUE.store(true, SeqCst);
        blocking_relax::<Test>();
        assert_eq!(YIELDS.load(SeqCst), 1);
        park_until::<Test>(100);
        assert_eq!(YIELDS.load(SeqCst), 4);
        assert_eq!(PARKED.load(SeqCst), 0);
    }
}
