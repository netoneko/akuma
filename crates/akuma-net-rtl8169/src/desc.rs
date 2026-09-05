//! The descriptor the chip and the driver pass frames through.
//!
//! One shape serves both directions: four little-endian words, sixteen bytes,
//! and the ring is a flat array of them at a 256-byte-aligned physical address.
//! The high word carries the ownership and framing flags; the low two words are
//! the buffer's physical address, split.
//!
//! # Ownership is the whole protocol
//!
//! [`OWN`] set means **the chip owns this descriptor** and the driver must not
//! touch it or the buffer behind it. Clearing it hands ownership back. There is
//! no lock and no other signal: every rule about when a buffer may be read or
//! written follows from that one bit, and every serious bug in a driver of this
//! shape is a violation of it.
//!
//! The direction changes what the flags mean, not where they are: on transmit
//! the driver writes [`FS`]/[`LS`] to frame a packet across descriptors, and on
//! receive the chip writes them, along with the length, into the same field it
//! read the buffer size from.

/// The chip owns this descriptor.
pub const OWN: u32 = 0x8000_0000;
/// This is the last descriptor in the ring; the next index is 0.
pub const EOR: u32 = 0x4000_0000;
/// First segment of a frame.
pub const FS: u32 = 0x2000_0000;
/// Last segment of a frame.
pub const LS: u32 = 0x1000_0000;

/// Receive: the chip found something wrong with this frame. When set, the other
/// error bits say what, and the frame must be dropped rather than delivered.
pub const RX_RES: u32 = 0x0020_0000;

/// Receive: length of the frame written into the buffer, in the low bits.
///
/// Fourteen bits on gigabit parts. The chip includes the 4-byte Ethernet FCS in
/// this count and the driver has to subtract it — a frame delivered with its
/// CRC still attached is the classic "everything works but every packet is four
/// bytes too long" bug.
pub const RX_LEN_MASK: u32 = 0x0000_3FFF;

/// Transmit: length of this fragment, in the low bits.
pub const TX_LEN_MASK: u32 = 0x0000_FFFF;

/// Bytes of Ethernet frame check sequence the receiver appends to the length.
pub const FCS_LEN: u16 = 4;

/// One ring entry, in memory layout order.
///
/// `#[repr(C)]` and four `u32`s: this is the layout the chip DMAs, so the field
/// order is a hardware fact rather than a style choice. Consumers that map the
/// ring into DMA memory must write the words little-endian.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Desc {
    /// Ownership, framing flags, and length (transmit) or status (receive).
    pub cmdstat: u32,
    /// VLAN tag control. Zero when hardware VLAN handling is off.
    pub vlan: u32,
    /// Low 32 bits of the buffer's physical address.
    pub buf_lo: u32,
    /// High 32 bits of the buffer's physical address.
    pub buf_hi: u32,
}

impl Desc {
    /// An entry the chip must not touch: every bit clear.
    pub const ZERO: Self = Self { cmdstat: 0, vlan: 0, buf_lo: 0, buf_hi: 0 };

    /// A receive entry handed to the chip: buffer posted, chip owns it.
    ///
    /// `end_of_ring` must be true for exactly the last index, or the chip walks
    /// off the end of the array.
    #[must_use]
    pub const fn rx_posted(buf_phys: u64, buf_size: u16, end_of_ring: bool) -> Self {
        let mut cmdstat = OWN | (buf_size as u32 & RX_LEN_MASK);
        if end_of_ring {
            cmdstat |= EOR;
        }
        Self {
            cmdstat,
            vlan: 0,
            buf_lo: buf_phys as u32,
            buf_hi: (buf_phys >> 32) as u32,
        }
    }

    /// A transmit entry for a whole frame in one buffer.
    #[must_use]
    pub const fn tx_single(buf_phys: u64, len: u16, end_of_ring: bool) -> Self {
        let mut cmdstat = OWN | FS | LS | (len as u32 & TX_LEN_MASK);
        if end_of_ring {
            cmdstat |= EOR;
        }
        Self {
            cmdstat,
            vlan: 0,
            buf_lo: buf_phys as u32,
            buf_hi: (buf_phys >> 32) as u32,
        }
    }

    /// Whether the chip currently owns this entry.
    #[must_use]
    pub const fn owned_by_chip(&self) -> bool {
        self.cmdstat & OWN != 0
    }

    /// Whether this is the ring's last entry.
    #[must_use]
    pub const fn is_end_of_ring(&self) -> bool {
        self.cmdstat & EOR != 0
    }

    /// The buffer address, reassembled.
    #[must_use]
    pub const fn buf_phys(&self) -> u64 {
        ((self.buf_hi as u64) << 32) | self.buf_lo as u64
    }

    /// Decode a completed receive entry.
    ///
    /// Returns `None` when the frame is unusable — an error, or a fragment that
    /// is not a whole frame on its own. A short buffer can split a frame across
    /// entries, and this driver posts buffers large enough that it never
    /// happens; if it ever does, refusing the fragment is correct and silently
    /// delivering it is not.
    #[must_use]
    pub const fn rx_frame_len(&self) -> Option<u16> {
        if self.cmdstat & RX_RES != 0 {
            return None;
        }
        if self.cmdstat & (FS | LS) != (FS | LS) {
            return None;
        }
        let with_fcs = (self.cmdstat & RX_LEN_MASK) as u16;
        if with_fcs <= FCS_LEN {
            return None;
        }
        Some(with_fcs - FCS_LEN)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_posted_receive_entry_is_owned_by_the_chip() {
        let d = Desc::rx_posted(0x1234_5678_9abc_d000, 2048, false);
        assert!(d.owned_by_chip());
        assert!(!d.is_end_of_ring());
        assert_eq!(d.buf_phys(), 0x1234_5678_9abc_d000);
        assert_eq!(d.cmdstat & RX_LEN_MASK, 2048);
    }

    #[test]
    fn the_last_entry_carries_end_of_ring() {
        assert!(Desc::rx_posted(0x1000, 2048, true).is_end_of_ring());
        assert!(Desc::tx_single(0x1000, 60, true).is_end_of_ring());
    }

    /// A 64-bit buffer address must survive the split into two words. Getting
    /// this wrong is invisible below 4 GiB and corrupts memory above it.
    #[test]
    fn addresses_above_four_gib_survive_the_split() {
        let phys = 0x0000_0007_dead_b000u64;
        let d = Desc::rx_posted(phys, 2048, false);
        assert_eq!(d.buf_lo, 0xdead_b000);
        assert_eq!(d.buf_hi, 0x0000_0007);
        assert_eq!(d.buf_phys(), phys);
    }

    /// The FCS is in the chip's length and must not be in ours.
    #[test]
    fn the_frame_check_sequence_is_stripped_from_the_length() {
        // A maximum-size frame as the chip counts it: 1514 bytes of Ethernet
        // plus the 4-byte FCS the receiver leaves attached.
        let reported_by_chip: u32 = 1518;
        let mut d = Desc::ZERO;
        d.cmdstat = FS | LS | reported_by_chip;
        assert_eq!(d.rx_frame_len(), Some(1514));
    }

    #[test]
    fn an_errored_frame_is_refused() {
        let mut d = Desc::ZERO;
        d.cmdstat = FS | LS | RX_RES | 64;
        assert_eq!(d.rx_frame_len(), None);
    }

    /// Half a frame is not a frame. Delivering a fragment as if it were whole
    /// is worse than dropping it: the stack above sees a truncated packet with
    /// no indication anything was lost.
    #[test]
    fn a_fragment_is_not_delivered() {
        let mut first = Desc::ZERO;
        first.cmdstat = FS | 512;
        assert_eq!(first.rx_frame_len(), None);

        let mut last = Desc::ZERO;
        last.cmdstat = LS | 512;
        assert_eq!(last.rx_frame_len(), None);
    }

    /// A length at or below the FCS size cannot be a real frame, and the
    /// subtraction that would follow underflows. Both are refusals.
    #[test]
    fn a_length_smaller_than_the_checksum_is_refused() {
        for len in 0..=u32::from(FCS_LEN) {
            let mut d = Desc::ZERO;
            d.cmdstat = FS | LS | len;
            assert_eq!(d.rx_frame_len(), None, "len {len} should be refused");
        }
    }

    #[test]
    fn a_transmit_entry_frames_a_whole_packet() {
        let d = Desc::tx_single(0x2000, 60, false);
        assert_eq!(d.cmdstat & (FS | LS), FS | LS);
        assert!(d.owned_by_chip());
        assert_eq!(d.cmdstat & TX_LEN_MASK, 60);
    }
}
