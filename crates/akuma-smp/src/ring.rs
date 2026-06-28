//! Lock-free MPSC inbox ring — the only cross-core data path in the multikernel.
//!
//! Many peer cores `push`; the owning core `pop`s. Producers claim a slot by CAS on
//! `tail`; a full ring **drops** rather than blocking, so a wedged/slow consumer can
//! never stall a producer (the property the whole message plane depends on).

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

// Message kinds for the (stubbed → debt-based) memory-reclaim protocol (§9).
/// "I'm at `value`% used — debtors, repay your creditors." (The pressure signal.)
pub const MSG_PRESSURE_REPORT: u32 = 1;
/// "I can offer `value` MiB." Stub stage only (logged, not enforced); the real
/// protocol sheds physical ranges directly to creditors instead.
pub const MSG_MEMORY_OFFER: u32 = 2;

/// Inbox capacity. Small — the coordination message rate is low; demo/protocol
/// traffic is a handful of messages per event, drained every loop pass.
pub const RING_CAP: usize = 8;

/// One message slot. `ready` gates the producer's two-word write from the consumer
/// (publish with `Release`, observe with `Acquire`). `word0` packs `(kind << 32) |
/// from`; `word1` is the payload value.
#[repr(C)]
pub struct MsgSlot {
    ready: AtomicU32,
    _pad: u32,
    word0: AtomicU64,
    word1: AtomicU64,
}

impl MsgSlot {
    const fn new() -> Self {
        Self {
            ready: AtomicU32::new(0),
            _pad: 0,
            word0: AtomicU64::new(0),
            word1: AtomicU64::new(0),
        }
    }
}

/// Bounded MPSC inbox ring. Lives in the SHARED descriptor region; private per-core
/// state is never reachable through it.
#[repr(C)]
pub struct Ring {
    head: AtomicU32, // consumer index (owner core only)
    tail: AtomicU32, // producer index (claimed via CAS by any core)
    slots: [MsgSlot; RING_CAP],
}

impl Default for Ring {
    fn default() -> Self {
        Self::new()
    }
}

impl Ring {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            head: AtomicU32::new(0),
            tail: AtomicU32::new(0),
            slots: [const { MsgSlot::new() }; RING_CAP],
        }
    }

    /// Push a message. Returns `false` (dropped) if the ring is full — never blocks.
    /// The boolean lets a caller observe drops; ignore it for fire-and-forget sends.
    pub fn push(&self, kind: u32, from: u32, value: u64) -> bool {
        loop {
            let t = self.tail.load(Ordering::Acquire);
            let h = self.head.load(Ordering::Acquire);
            if t.wrapping_sub(h) >= RING_CAP as u32 {
                return false; // full → drop, do not block
            }
            if self
                .tail
                .compare_exchange(t, t.wrapping_add(1), Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                let slot = &self.slots[(t as usize) % RING_CAP];
                slot.word0
                    .store((u64::from(kind) << 32) | u64::from(from), Ordering::Relaxed);
                slot.word1.store(value, Ordering::Relaxed);
                slot.ready.store(1, Ordering::Release);
                return true;
            }
        }
    }

    /// Pop the next message (owner core only): `(kind, from, value)` or `None`.
    pub fn pop(&self) -> Option<(u32, u32, u64)> {
        let h = self.head.load(Ordering::Relaxed);
        let t = self.tail.load(Ordering::Acquire);
        if h == t {
            return None;
        }
        let slot = &self.slots[(h as usize) % RING_CAP];
        if slot.ready.load(Ordering::Acquire) == 0 {
            return None; // producer claimed the slot but hasn't finished writing
        }
        let w0 = slot.word0.load(Ordering::Relaxed);
        let value = slot.word1.load(Ordering::Relaxed);
        slot.ready.store(0, Ordering::Release);
        self.head.store(h.wrapping_add(1), Ordering::Release);
        Some(((w0 >> 32) as u32, w0 as u32, value))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fifo_order() {
        let r = Ring::new();
        assert!(r.push(MSG_PRESSURE_REPORT, 1, 10));
        assert!(r.push(MSG_PRESSURE_REPORT, 2, 20));
        assert!(r.push(MSG_MEMORY_OFFER, 3, 30));
        assert_eq!(r.pop(), Some((MSG_PRESSURE_REPORT, 1, 10)));
        assert_eq!(r.pop(), Some((MSG_PRESSURE_REPORT, 2, 20)));
        assert_eq!(r.pop(), Some((MSG_MEMORY_OFFER, 3, 30)));
        assert_eq!(r.pop(), None);
    }

    #[test]
    fn full_ring_drops_never_blocks() {
        let r = Ring::new();
        for i in 0..RING_CAP {
            assert!(r.push(MSG_MEMORY_OFFER, 0, i as u64), "slot {i} should fit");
        }
        // One past capacity is dropped, not blocked.
        assert!(!r.push(MSG_MEMORY_OFFER, 0, 999));
        // Draining one frees exactly one slot.
        assert_eq!(r.pop(), Some((MSG_MEMORY_OFFER, 0, 0)));
        assert!(r.push(MSG_MEMORY_OFFER, 0, 1000));
        assert!(!r.push(MSG_MEMORY_OFFER, 0, 1001));
    }

    #[test]
    fn wraparound_preserves_order() {
        let r = Ring::new();
        // Push/pop far more than RING_CAP to force the indices to wrap.
        for i in 0..(RING_CAP * 13) as u64 {
            assert!(r.push(MSG_MEMORY_OFFER, 7, i));
            assert_eq!(r.pop(), Some((MSG_MEMORY_OFFER, 7, i)));
        }
        assert_eq!(r.pop(), None);
    }

    #[test]
    fn mpsc_concurrent_no_loss() {
        use std::collections::HashSet;
        use std::sync::Arc;
        use std::thread;

        const PRODUCERS: u32 = 4;
        const PER: u64 = 5000;

        let ring = Arc::new(Ring::new());
        let mut producers = std::vec::Vec::new();
        for p in 0..PRODUCERS {
            let r = Arc::clone(&ring);
            producers.push(thread::spawn(move || {
                for seq in 0..PER {
                    // Retry on full (backpressure) so nothing is lost in the test.
                    while !r.push(MSG_MEMORY_OFFER, p, seq) {
                        std::hint::spin_loop();
                    }
                }
            }));
        }

        // Single consumer drains until it has every (producer, seq) pair.
        let total = (PRODUCERS as u64) * PER;
        let mut seen: HashSet<(u32, u64)> = HashSet::new();
        while (seen.len() as u64) < total {
            if let Some((_kind, from, seq)) = ring.pop() {
                assert!(from < PRODUCERS);
                assert!(seen.insert((from, seq)), "duplicate ({from}, {seq})");
            } else {
                std::hint::spin_loop();
            }
        }
        for h in producers {
            h.join().unwrap();
        }
        // No stragglers left, and we saw exactly the full cross-product.
        assert_eq!(ring.pop(), None);
        assert_eq!(seen.len() as u64, total);
    }
}
