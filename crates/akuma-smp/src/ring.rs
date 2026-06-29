//! Lock-free MPSC inbox ring — the only cross-core data path in the multikernel.
//!
//! Many peer cores `push`; the owning core `pop`s. Producers claim a slot by CAS on
//! `tail`; a full ring **drops** rather than blocking, so a wedged/slow consumer can
//! never stall a producer (the property the whole message plane depends on).

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

// Message kinds for the debt-based memory-reclaim protocol (§9). The pressure
// signal is the trigger; a repayment carries the physical range being returned.
/// "I'm under pressure — debtors, repay your creditors." Payload unused.
pub const MSG_PRESSURE: u32 = 1;
/// A repayment addressed to a creditor: `v0` = range base, `v1` = range length.
pub const MSG_REPAID: u32 = 2;

// Cross-core syscall-forwarding transport (docs/MULTIKERNEL.md §8.1/§10). R4a proves
// the round-trip: a forwarding core writes a payload into its `fwd_bounce` slot
// (§descriptor) and sends a request; the owner core reads it, produces a result back
// into the same slot, and replies. The ring's `ready` Release/Acquire is the publish
// edge that also orders the bounce-region bytes written before the push.
/// Forward request: `v0` = payload byte length in `fwd_bounce[from]`, `v1` = nonce.
pub const MSG_FWD_ECHO_REQ: u32 = 3;
/// Forward reply: `v0` = byte length written back to `fwd_bounce[from]`, `v1` = nonce.
pub const MSG_FWD_ECHO_REPLY: u32 = 4;

/// Inbox capacity. Small — the coordination message rate is low; protocol traffic
/// is a handful of messages per event, drained every loop pass.
pub const RING_CAP: usize = 8;

/// One message slot. `ready` gates the producer's payload writes from the consumer
/// (publish with `Release`, observe with `Acquire`). `word0` packs `(kind << 32) |
/// from`; `v0`/`v1` are two payload words (enough for a `(base, len)` range).
#[repr(C)]
pub struct MsgSlot {
    ready: AtomicU32,
    _pad: u32,
    word0: AtomicU64,
    v0: AtomicU64,
    v1: AtomicU64,
}

impl MsgSlot {
    const fn new() -> Self {
        Self {
            ready: AtomicU32::new(0),
            _pad: 0,
            word0: AtomicU64::new(0),
            v0: AtomicU64::new(0),
            v1: AtomicU64::new(0),
        }
    }
}

/// A decoded inbox message: `kind`, sender `from`, and two payload words.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Msg {
    pub kind: u32,
    pub from: u32,
    pub v0: u64,
    pub v1: u64,
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
    pub fn push(&self, kind: u32, from: u32, v0: u64, v1: u64) -> bool {
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
                slot.v0.store(v0, Ordering::Relaxed);
                slot.v1.store(v1, Ordering::Relaxed);
                slot.ready.store(1, Ordering::Release);
                return true;
            }
        }
    }

    /// Whether the ring currently has no messages. Used by a consumer for a
    /// race-free "check then sleep": a producer that pushes after this returns
    /// `true` also rings a doorbell SGI, which leaves the consumer's `wfi` wake
    /// pending — so masking IRQs across `is_empty()` + `wfi` loses no wakeups.
    /// (A producer mid-`push` that has claimed `tail` but not set `ready` shows as
    /// non-empty here; the consumer simply loops and `pop` returns `None` until the
    /// slot is published — a brief spin, never a hang.)
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.head.load(Ordering::Relaxed) == self.tail.load(Ordering::Acquire)
    }

    /// Pop the next message (owner core only), or `None` if empty.
    pub fn pop(&self) -> Option<Msg> {
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
        let v0 = slot.v0.load(Ordering::Relaxed);
        let v1 = slot.v1.load(Ordering::Relaxed);
        slot.ready.store(0, Ordering::Release);
        self.head.store(h.wrapping_add(1), Ordering::Release);
        Some(Msg { kind: (w0 >> 32) as u32, from: w0 as u32, v0, v1 })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fifo_order() {
        let r = Ring::new();
        assert!(r.push(MSG_PRESSURE, 1, 0, 0));
        assert!(r.push(MSG_REPAID, 2, 0x1000, 400));
        assert_eq!(r.pop(), Some(Msg { kind: MSG_PRESSURE, from: 1, v0: 0, v1: 0 }));
        assert_eq!(r.pop(), Some(Msg { kind: MSG_REPAID, from: 2, v0: 0x1000, v1: 400 }));
        assert_eq!(r.pop(), None);
    }

    #[test]
    fn is_empty_tracks_pending() {
        let r = Ring::new();
        assert!(r.is_empty());
        assert!(r.push(MSG_PRESSURE, 1, 0, 0));
        assert!(!r.is_empty());
        assert!(r.pop().is_some());
        assert!(r.is_empty());
    }

    #[test]
    fn full_ring_drops_never_blocks() {
        let r = Ring::new();
        for i in 0..RING_CAP {
            assert!(r.push(MSG_REPAID, 0, i as u64, 1), "slot {i} should fit");
        }
        assert!(!r.push(MSG_REPAID, 0, 999, 1)); // past capacity → dropped, not blocked
        assert_eq!(r.pop(), Some(Msg { kind: MSG_REPAID, from: 0, v0: 0, v1: 1 }));
        assert!(r.push(MSG_REPAID, 0, 1000, 1)); // one drained → one fits
        assert!(!r.push(MSG_REPAID, 0, 1001, 1));
    }

    #[test]
    fn wraparound_preserves_order() {
        let r = Ring::new();
        for i in 0..(RING_CAP * 13) as u64 {
            assert!(r.push(MSG_REPAID, 7, i, i * 2));
            assert_eq!(r.pop(), Some(Msg { kind: MSG_REPAID, from: 7, v0: i, v1: i * 2 }));
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
                    while !r.push(MSG_REPAID, p, seq, 0) {
                        std::hint::spin_loop();
                    }
                }
            }));
        }

        let total = u64::from(PRODUCERS) * PER;
        let mut seen: HashSet<(u32, u64)> = HashSet::new();
        while (seen.len() as u64) < total {
            if let Some(m) = ring.pop() {
                assert!(m.from < PRODUCERS);
                assert!(seen.insert((m.from, m.v0)), "duplicate ({}, {})", m.from, m.v0);
            } else {
                std::hint::spin_loop();
            }
        }
        for h in producers {
            h.join().unwrap();
        }
        assert_eq!(ring.pop(), None);
        assert_eq!(seen.len() as u64, total);
    }
}
