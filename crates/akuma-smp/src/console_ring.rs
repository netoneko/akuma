//! Per-core console output ring — the async, fire-and-forget console channel
//! (docs/MULTIKERNEL.md §8.2).
//!
//! Console output is high-frequency, latency-tolerant, and returns no value, so it
//! is the wrong fit for the synchronous control inbox (a 16-byte [`crate::Msg`] per
//! write would be tiny and would flood low-rate protocol traffic). Instead each core
//! owns a byte ring here: it `write`s its console bytes (batched, never blocking) and
//! the **console-owner core** (core 0, which owns the UART) `read`s and drains them.
//! Conceptually this is still "send console as messages to the owner kernel" — the
//! contiguous byte buffer IS the batch.
//!
//! SPSC: exactly one producer (the owning core) and one consumer (the owner core).
//! A full ring **drops** rather than blocking, so a slow drainer can never stall the
//! producer — the same non-blocking discipline as [`crate::Ring`].

use core::sync::atomic::{AtomicU32, AtomicU8, Ordering};

/// Bytes buffered per core before the producer starts dropping (tunable).
///
/// Matched to a 4 KiB page so the drainer can grab a whole page per drain pass, and
/// so it holds many max-length log lines (the kernel's largest `safe_print!` is ~256
/// bytes). MUST be a power of two so the `u32` write/read indices wrap seamlessly
/// across `u32::MAX` (2^32 is a multiple of any power of two ⇒ `idx % CAP` stays
/// continuous at the wrap).
pub const CONSOLE_RING_CAP: usize = 4096;
const _: () = assert!(CONSOLE_RING_CAP.is_power_of_two());

/// A per-core SPSC byte ring living in the SHARED descriptor. Producer advances
/// `tail`, consumer advances `head`; occupancy is `tail - head` (wrapping).
#[repr(C)]
pub struct ConsoleRing {
    /// Consumer (console-owner core) read index.
    head: AtomicU32,
    /// Producer (owning core) write index.
    tail: AtomicU32,
    buf: [AtomicU8; CONSOLE_RING_CAP],
}

impl Default for ConsoleRing {
    fn default() -> Self {
        Self::new()
    }
}

impl ConsoleRing {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            head: AtomicU32::new(0),
            tail: AtomicU32::new(0),
            buf: [const { AtomicU8::new(0) }; CONSOLE_RING_CAP],
        }
    }

    /// Producer: append as many of `bytes` as fit; returns the count written. Drops
    /// the overflow (returns < `bytes.len()`) rather than blocking. Single-producer
    /// only — the owning core is the sole writer of its ring.
    pub fn write(&self, bytes: &[u8]) -> usize {
        let cap = CONSOLE_RING_CAP as u32;
        let h = self.head.load(Ordering::Acquire);
        let mut t = self.tail.load(Ordering::Relaxed);
        let mut written = 0;
        for &b in bytes {
            if t.wrapping_sub(h) >= cap {
                break; // full → drop the rest, never block
            }
            self.buf[(t % cap) as usize].store(b, Ordering::Relaxed);
            t = t.wrapping_add(1);
            written += 1;
        }
        // Publish the payload bytes before the consumer can observe the new tail.
        self.tail.store(t, Ordering::Release);
        written
    }

    /// Consumer: copy up to `out.len()` bytes into `out`; returns the count read.
    /// Single-consumer only — the console-owner core is the sole reader.
    pub fn read(&self, out: &mut [u8]) -> usize {
        let cap = CONSOLE_RING_CAP as u32;
        let mut h = self.head.load(Ordering::Relaxed);
        let t = self.tail.load(Ordering::Acquire);
        let mut n = 0;
        while n < out.len() && h != t {
            out[n] = self.buf[(h % cap) as usize].load(Ordering::Relaxed);
            h = h.wrapping_add(1);
            n += 1;
        }
        // Release so the producer sees the freed space after we've read the bytes.
        self.head.store(h, Ordering::Release);
        n
    }

    /// Whether the ring currently holds no bytes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.head.load(Ordering::Acquire) == self.tail.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_read_roundtrip() {
        let r = ConsoleRing::new();
        assert!(r.is_empty());
        assert_eq!(r.write(b"hello"), 5);
        assert!(!r.is_empty());
        let mut out = [0u8; 8];
        assert_eq!(r.read(&mut out), 5);
        assert_eq!(&out[..5], b"hello");
        assert!(r.is_empty());
    }

    #[test]
    fn partial_reads_preserve_fifo() {
        let r = ConsoleRing::new();
        assert_eq!(r.write(b"abcdef"), 6);
        let mut out = [0u8; 3];
        assert_eq!(r.read(&mut out), 3);
        assert_eq!(&out, b"abc");
        assert_eq!(r.read(&mut out), 3);
        assert_eq!(&out, b"def");
        assert_eq!(r.read(&mut out), 0);
    }

    #[test]
    fn full_ring_drops_overflow_never_blocks() {
        let r = ConsoleRing::new();
        let big = [b'x'; CONSOLE_RING_CAP + 100];
        assert_eq!(r.write(&big), CONSOLE_RING_CAP); // only CAP fit; rest dropped
        // A further write while full returns 0 (dropped), does not block.
        assert_eq!(r.write(b"y"), 0);
        let mut out = [0u8; CONSOLE_RING_CAP];
        assert_eq!(r.read(&mut out), CONSOLE_RING_CAP);
        assert!(out.iter().all(|&b| b == b'x'));
    }

    #[test]
    fn wraparound_preserves_bytes() {
        let r = ConsoleRing::new();
        // Drive head/tail far past CAP to exercise the modular wrap; each round
        // writes then fully drains, so occupancy never exceeds the chunk size.
        let mut out = [0u8; 200];
        for round in 0..(CONSOLE_RING_CAP * 5) as u32 {
            let b = (round & 0xff) as u8;
            let chunk = [b; 200];
            assert_eq!(r.write(&chunk), 200);
            assert_eq!(r.read(&mut out), 200);
            assert!(out.iter().all(|&x| x == b), "round {round}");
        }
        assert!(r.is_empty());
    }
}
