//! The receive side: the RX ring's indices and what the chip writes into each
//! buffer — Linux's `rtw89_pci_rxbd_deliver_skbs` and `rtw89_core_query_rxdesc`,
//! 8852C arms.
//!
//! Every RXQ buffer the chip fills starts with a 4-byte `rtw89_pci_rxbd_info`
//! (bytes written, first/last segment), then an RX descriptor — 16 bytes
//! (`rtw89_rxdesc_short`) or 32 (`_long`) — then `drv_info_size * 8` bytes of
//! driver info, then the packet. What the packet is, `pkt_type` says: an
//! 802.11 frame (with its FCS: the MAC is set to append it), a PPDU status
//! report, a firmware event (C2H), a TX release report, ...
//!
//! The ring: the host's `wp` is the next entry to read; bits 27:16 of the
//! ring's index register are the chip's, the next entry it will fill. Entries
//! between are ready. Writing `wp` back to bits 15:0 hands them to the chip
//! again (`rtw89_pci_rxbd_deliver` ends with exactly that write).

use crate::Bus;

/// `RTW89_CORE_RX_TYPE_*`.
pub mod kind {
    pub const WIFI: u8 = 0;
    pub const PPDU_STAT: u8 = 1;
    pub const TX_REL_HOST: u8 = 7;
    pub const C2H: u8 = 10;
}

/// `R_AX_RXQ_RXBD_IDX_V1`, `R_AX_RPQ_RXBD_IDX_V1`.
pub const RXQ_IDX: u32 = 0x1218;
pub const RPQ_IDX: u32 = 0x121c;

const SHORT_LEN: usize = 16;
const LONG_LEN: usize = 32;
const INFO_LEN: usize = 4;

/// The fields of an RX descriptor this driver reads.
#[allow(clippy::struct_excessive_bools)] // the descriptor's own flag bits
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Desc {
    /// Bytes of packet after the descriptor and driver info.
    pub pkt_size: u16,
    pub pkt_type: u8,
    pub long: bool,
    pub drv_info_size: u8,
    pub mac_info_valid: bool,
    /// `AX_RXD_BW_v1_MASK`: 0 = 20 MHz.
    pub bw: u8,
    /// `AX_RXD_RX_DATARATE_MASK`: 0..=3 CCK, 4..=11 OFDM, then HT/VHT/HE.
    pub data_rate: u16,
    pub crc32_err: bool,
    pub icv_err: bool,
    pub hw_dec: bool,
    pub a1_match: bool,
    /// The chip's free-running counter at reception.
    pub free_run: u32,
}

/// One received buffer, parsed.
#[derive(Clone, Copy, Debug)]
pub struct Packet<'a> {
    pub desc: Desc,
    /// The packet (an 802.11 frame with FCS, a C2H, ...).
    pub body: &'a [u8],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bad {
    /// Fewer bytes than the info word and a short descriptor.
    Short,
    /// Not a whole packet in one buffer (first/last segment bits).
    Segmented,
    /// The descriptor's lengths run past what the chip wrote.
    Length,
}

fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

/// Parse one RX buffer.
///
/// # Errors
///
/// [`Bad`] for a buffer this driver cannot take apart.
pub fn parse(buf: &[u8]) -> Result<Packet<'_>, Bad> {
    if buf.len() < INFO_LEN + SHORT_LEN {
        return Err(Bad::Short);
    }
    let info = le32(buf, 0);
    let written = (info & 0x3fff) as usize;
    let fs = info & (1 << 15) != 0;
    let ls = info & (1 << 14) != 0;
    if !(fs && ls) {
        return Err(Bad::Segmented);
    }
    let d0 = le32(buf, INFO_LEN);
    let d1 = le32(buf, INFO_LEN + 4);
    let d2 = le32(buf, INFO_LEN + 8);
    let d3 = le32(buf, INFO_LEN + 12);
    let desc = Desc {
        pkt_size: (d0 & 0x3fff) as u16,
        mac_info_valid: d0 & (1 << 23) != 0,
        pkt_type: ((d0 >> 24) & 0xf) as u8,
        drv_info_size: ((d0 >> 28) & 0x7) as u8,
        long: d0 & (1 << 31) != 0,
        data_rate: ((d1 >> 16) & 0x1ff) as u16,
        bw: ((d1 >> 29) & 0x7) as u8,
        free_run: d2,
        a1_match: d3 & 1 != 0,
        hw_dec: d3 & (1 << 2) != 0,
        crc32_err: d3 & (1 << 9) != 0,
        icv_err: d3 & (1 << 10) != 0,
    };
    let start = INFO_LEN
        + if desc.long { LONG_LEN } else { SHORT_LEN }
        + usize::from(desc.drv_info_size) * 8;
    let end = start + usize::from(desc.pkt_size);
    // `written` counts from the start of the buffer (Linux copies
    // `len - offset` bytes, `offset` measured from there); 4 bytes of slack in
    // case this chip counts from the descriptor instead.
    if end > buf.len() || (written != 0 && end > written + INFO_LEN) {
        return Err(Bad::Length);
    }
    Ok(Packet { desc, body: &buf[start..end] })
}

/// One RX ring's host side.
#[derive(Clone, Copy, Debug)]
pub struct Ring {
    pub idx_reg: u32,
    pub len: u16,
    /// Next entry to read.
    pub wp: u16,
}

impl Ring {
    #[must_use]
    pub const fn new(idx_reg: u32, len: u16) -> Self {
        Self { idx_reg, len, wp: 0 }
    }

    /// Entries the chip has filled and the host not yet read
    /// (`rtw89_pci_rxbd_recalc`).
    pub fn ready(&self, bus: &mut impl Bus) -> u16 {
        let hw = ((bus.read32(self.idx_reg) >> 16) & 0xfff) as u16 % self.len;
        (hw + self.len - self.wp) % self.len
    }

    /// Move past `n` entries (`rtw89_pci_rxbd_increase`).
    pub fn advance(&mut self, n: u16) {
        self.wp = (self.wp + n) % self.len;
    }

    /// Hand the read entries back to the chip.
    pub fn release(&self, bus: &mut impl Bus) {
        bus.write16(self.idx_reg, self.wp);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buf_with(desc0: u32, d3: u32, body: &[u8]) -> [u8; 128] {
        let mut b = [0u8; 128];
        let written = (INFO_LEN + SHORT_LEN + body.len()) as u32;
        b[..4].copy_from_slice(&(written | (1 << 15) | (1 << 14)).to_le_bytes());
        b[4..8].copy_from_slice(&desc0.to_le_bytes());
        b[16..20].copy_from_slice(&d3.to_le_bytes());
        b[20..20 + body.len()].copy_from_slice(body);
        b
    }

    #[test]
    fn short_descriptor_wifi_frame() {
        let body = [0x80, 0, 0, 0, 0xff, 0xff];
        let b = buf_with(body.len() as u32, 1, &body);
        let p = parse(&b).unwrap();
        assert_eq!(p.desc.pkt_type, kind::WIFI);
        assert!(p.desc.a1_match);
        assert_eq!(p.body, &body);
    }

    #[test]
    fn c2h_from_the_w2_trace() {
        // A C2H from the W2 recording: REC_ACK, 12 bytes.
        let c2h = [0x01, 0x00, 0x01, 0x01, 0x0c, 0, 0, 0, 0x25, 0x14, 0, 0];
        let b = buf_with(0x0c | (u32::from(kind::C2H) << 24), 0, &c2h);
        let p = parse(&b).unwrap();
        assert_eq!(p.desc.pkt_type, kind::C2H);
        assert_eq!(p.body, &c2h);
    }

    #[test]
    fn drv_info_and_long_descriptor_shift_the_packet() {
        let mut b = [0u8; 128];
        b[..4].copy_from_slice(&(0x64_u32 | (3 << 14)).to_le_bytes());
        let d0 = 4u32 | (1 << 28) | (1 << 31); // 4 bytes, 1x8 drv info, long
        b[4..8].copy_from_slice(&d0.to_le_bytes());
        b[4 + 32 + 8..4 + 32 + 12].copy_from_slice(&[1, 2, 3, 4]);
        let p = parse(&b).unwrap();
        assert!(p.desc.long);
        assert_eq!(p.body, &[1, 2, 3, 4]);
    }

    #[test]
    fn rejects_segments_and_overruns() {
        let mut b = buf_with(4, 0, &[0; 4]);
        b[1] &= !0x80; // clear FS (bit 15)
        assert_eq!(parse(&b).unwrap_err(), Bad::Segmented);
        let b = buf_with(200, 0, &[0; 4]);
        assert_eq!(parse(&b).unwrap_err(), Bad::Length);
    }

    struct Idx(u32, u32);
    impl Bus for Idx {
        fn read8(&mut self, _: u32) -> u8 {
            0
        }
        fn read16(&mut self, _: u32) -> u16 {
            0
        }
        fn read32(&mut self, _: u32) -> u32 {
            self.0
        }
        fn write8(&mut self, _: u32, _: u8) {}
        fn write16(&mut self, _: u32, v: u16) {
            self.1 = u32::from(v);
        }
        fn write32(&mut self, _: u32, _: u32) {}
        fn delay_us(&mut self, _: u32) {}
    }

    #[test]
    fn ring_counts_and_wraps() {
        // From the W2 trace: RXQ index 0x0010_0000 = chip at 16, host at 0.
        let mut r = Ring::new(RXQ_IDX, 256);
        let mut bus = Idx(0x0010_0000, 0);
        assert_eq!(r.ready(&mut bus), 16);
        r.advance(16);
        r.release(&mut bus);
        assert_eq!(bus.1, 16);
        r.wp = 250;
        bus.0 = 4 << 16;
        assert_eq!(r.ready(&mut bus), 10);
    }
}
