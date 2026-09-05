//! Ring bookkeeping: whose turn it is, and where the next entry lives.
//!
//! Both rings are power-of-two arrays of [`Desc`] that the chip walks in order,
//! wrapping at the entry marked [`desc::EOR`]. The driver walks the same order
//! independently, and the two positions are reconciled only through the
//! ownership bit — there is no head/tail register to read on this chip, which
//! is why this module holds indices rather than reading them back.
//!
//! Everything here is arithmetic over `usize` with no hardware access at all,
//! so the wrap, the full/empty distinction and the reclaim order are decided by
//! tests rather than by a boot.
//!
//! # Why the receive ring has no "full"
//!
//! The driver posts every receive entry at init and re-posts each one as soon as
//! it has copied the frame out, so the ring is always full from the chip's point
//! of view. What the driver tracks is only *where it last looked*. The transmit
//! ring is the opposite: the driver fills it and the chip drains it, so that one
//! needs a real full/empty distinction and gets one.

use crate::desc::Desc;

/// Receive ring position.
///
/// One cursor: the entry to inspect next. A ring of `len` entries the chip owns
/// is the steady state, and the cursor only moves when a frame is taken out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RxRing {
    len: usize,
    next: usize,
}

impl RxRing {
    /// A ring of `len` entries, cursor at the start.
    ///
    /// `len` must be a power of two — the chip wraps on [`desc::EOR`] and the
    /// driver wraps with a mask, and the two agree only if it is.
    #[must_use]
    pub const fn new(len: usize) -> Self {
        assert!(len.is_power_of_two(), "ring length must be a power of two");
        Self { len, next: 0 }
    }

    /// Number of entries.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Whether the ring has no entries. Always false for a constructed ring;
    /// present because clippy asks for it next to `len`.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The entry to inspect next.
    #[must_use]
    pub const fn cursor(&self) -> usize {
        self.next
    }

    /// Step past the entry just consumed.
    pub const fn advance(&mut self) {
        self.next = (self.next + 1) & (self.len - 1);
    }

    /// Whether index `i` is the last entry, and so must carry [`desc::EOR`].
    #[must_use]
    pub const fn is_last(&self, i: usize) -> bool {
        i + 1 == self.len
    }

    /// The descriptor for a freshly posted receive buffer at index `i`.
    #[must_use]
    pub const fn post(&self, i: usize, buf_phys: u64, buf_size: u16) -> Desc {
        Desc::rx_posted(buf_phys, buf_size, self.is_last(i))
    }
}

/// Transmit ring position.
///
/// Two cursors: `next` is where the driver will write, `dirty` is the oldest
/// entry the chip has not yet given back. One entry is deliberately left unused
/// so that "full" and "empty" are distinguishable without a separate count —
/// the standard cost of a two-cursor ring, and cheaper than the alternative.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TxRing {
    len: usize,
    next: usize,
    dirty: usize,
}

impl TxRing {
    /// A ring of `len` entries, both cursors at the start.
    #[must_use]
    pub const fn new(len: usize) -> Self {
        assert!(len.is_power_of_two(), "ring length must be a power of two");
        Self { len, next: 0, dirty: 0 }
    }

    /// Number of entries.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Whether nothing is in flight.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.next == self.dirty
    }

    /// Whether there is no room for another frame.
    #[must_use]
    pub const fn is_full(&self) -> bool {
        ((self.next + 1) & (self.len - 1)) == self.dirty
    }

    /// How many frames are in flight.
    #[must_use]
    pub const fn in_flight(&self) -> usize {
        (self.next.wrapping_sub(self.dirty)) & (self.len - 1)
    }

    /// The index the next frame will occupy, if there is room.
    #[must_use]
    pub const fn producer(&self) -> Option<usize> {
        if self.is_full() { None } else { Some(self.next) }
    }

    /// The oldest entry not yet reclaimed, if any.
    #[must_use]
    pub const fn consumer(&self) -> Option<usize> {
        if self.is_empty() { None } else { Some(self.dirty) }
    }

    /// Commit the frame written at [`Self::producer`].
    pub const fn produce(&mut self) {
        self.next = (self.next + 1) & (self.len - 1);
    }

    /// Reclaim the entry at [`Self::consumer`], the chip having finished it.
    pub const fn consume(&mut self) {
        self.dirty = (self.dirty + 1) & (self.len - 1);
    }

    /// Whether index `i` is the last entry, and so must carry [`desc::EOR`].
    #[must_use]
    pub const fn is_last(&self, i: usize) -> bool {
        i + 1 == self.len
    }

    /// The descriptor for a frame of `len` bytes in the buffer at index `i`.
    #[must_use]
    pub const fn post(&self, i: usize, buf_phys: u64, len: u16) -> Desc {
        Desc::tx_single(buf_phys, len, self.is_last(i))
    }
}

/// The smallest Ethernet frame that may be put on the wire, padding included.
pub const MIN_FRAME: usize = 60;

/// The largest frame this driver transmits or accepts, FCS excluded.
pub const MAX_FRAME: usize = 1514;

/// Receive buffer size posted per entry.
///
/// A whole frame must fit in one buffer: this driver refuses split frames
/// ([`Desc::rx_frame_len`]), so a buffer shorter than the largest frame the
/// receiver will accept turns a legal packet into a silent drop. Rounded up to
/// a power of two above [`MAX_FRAME`] + FCS.
pub const RX_BUF_SIZE: u16 = 2048;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::desc;

    #[test]
    fn the_receive_cursor_wraps_at_the_end() {
        let mut r = RxRing::new(4);
        assert_eq!(r.cursor(), 0);
        for expected in [1, 2, 3, 0, 1] {
            r.advance();
            assert_eq!(r.cursor(), expected);
        }
    }

    /// Exactly one entry may carry the end-of-ring marker, and it must be the
    /// last. If it is on the wrong entry the chip wraps early and silently uses
    /// a fraction of the ring; if it is on none, it walks off the array.
    #[test]
    fn exactly_the_last_entry_is_marked_end_of_ring() {
        let r = RxRing::new(8);
        let marked: usize = (0..8)
            .filter(|&i| r.post(i, 0x1000, RX_BUF_SIZE).is_end_of_ring())
            .count();
        assert_eq!(marked, 1);
        assert!(r.post(7, 0x1000, RX_BUF_SIZE).is_end_of_ring());
        assert!(!r.post(6, 0x1000, RX_BUF_SIZE).is_end_of_ring());
    }

    #[test]
    fn a_fresh_transmit_ring_is_empty_and_not_full() {
        let t = TxRing::new(8);
        assert!(t.is_empty());
        assert!(!t.is_full());
        assert_eq!(t.in_flight(), 0);
        assert_eq!(t.consumer(), None);
        assert_eq!(t.producer(), Some(0));
    }

    /// The distinguishing property of the two-cursor ring: full and empty must
    /// never look alike. One entry is sacrificed to buy that.
    #[test]
    fn full_and_empty_are_distinguishable() {
        let mut t = TxRing::new(4);
        let mut posted = 0;
        while !t.is_full() {
            t.produce();
            posted += 1;
            assert!(!t.is_empty(), "a ring with {posted} in flight is not empty");
        }
        assert_eq!(posted, 3, "one entry of four is deliberately unused");
        assert!(t.is_full());
        assert_eq!(t.producer(), None);
        assert_eq!(t.in_flight(), 3);
    }

    #[test]
    fn reclaiming_makes_room_again() {
        let mut t = TxRing::new(4);
        while !t.is_full() {
            t.produce();
        }
        assert_eq!(t.producer(), None);
        t.consume();
        assert!(!t.is_full());
        assert_eq!(t.producer(), Some(3));
        assert_eq!(t.in_flight(), 2);
    }

    /// Frames come back in the order they went out, all the way around the
    /// ring and past the wrap — that is what makes a single `dirty` cursor
    /// sufficient. Expectations are kept in a fixed array so this test needs
    /// neither `alloc` nor `std`, like the crate it tests.
    #[test]
    fn frames_are_reclaimed_in_the_order_they_were_sent() {
        let mut t = TxRing::new(4);
        let mut expect = [0usize; 16];
        let (mut head, mut tail) = (0usize, 0usize);

        for _ in 0..9 {
            if let Some(i) = t.producer() {
                expect[tail] = i;
                tail += 1;
                t.produce();
            }
            if tail - head == 3 {
                // Drain one so the ring keeps moving past its own length.
                assert_eq!(t.consumer(), Some(expect[head]));
                head += 1;
                t.consume();
            }
        }
        while let Some(i) = t.consumer() {
            assert_eq!(i, expect[head], "frames must complete in send order");
            head += 1;
            t.consume();
        }
        assert_eq!(head, tail, "every sent frame was reclaimed exactly once");
        assert!(t.is_empty());
    }

    #[test]
    fn a_receive_buffer_holds_the_largest_legal_frame() {
        assert!(usize::from(RX_BUF_SIZE) >= MAX_FRAME + usize::from(desc::FCS_LEN));
    }
}
