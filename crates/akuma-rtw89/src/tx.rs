//! The transmit side for 802.11 frames: the WD page (`rtw89_pci_txwd_submit`
//! with `rtw89_core_fill_txdesc_v1`, the 8852C arm) and the host half of a
//! data/management TX ring.
//!
//! # The WD page
//!
//! On every TX channel but CH12, a TXBD ring entry points at a **WD page**,
//! not at the frame:
//!
//! | bytes | what | Linux |
//! |---|---|---|
//! | 32 | WD body | `rtw89_txwd_body_v1` (`rtw89_core_fill_txdesc_v1`) |
//! | 24 | WD info | `rtw89_txwd_info` (always present here: every class sets `en_wd_info`) |
//! | 8 | WP info | `rtw89_pci_tx_wp_info`: the page's sequence number, valid bit |
//! | 6 | address info | one `rtw89_pci_tx_addr_info_32_v1`: where the frame is, how long |
//!
//! — 70 bytes, which is the length the TXBD entry carries. The frame itself
//! (header first, no FCS, no MIC) sits elsewhere in DMA memory; a protected
//! frame reserves the 8 bytes of CCMP header after its 802.11 header, the chip
//! writes them from the packet number in body words 4–5, and appends the MIC.
//!
//! One address-info entry covers at most `TXADDR_INFO_LENTHG_V1_MAX` = 2044
//! bytes; a longer frame needs two, which would make the page 76 bytes and
//! the ring bookkeeping two-frame. Nothing this driver sends before the data
//! path is anywhere near that, so [`submit`] rejects longer frames rather
//! than grow the page.
//!
//! # The three frame classes
//!
//! Field values follow what the recorded join showed Linux using for each
//! kind of frame (`overlays/ryzen/w0-trace.sh JOIN=1`, the `txd` probe dumps
//! `struct rtw89_tx_desc_info`; the tests replay three of its records):
//!
//! | class | qsel / channel | sequence | notes |
//! |---|---|---|---|
//! | [`Desc::mgmt`] | `QSEL_MGMT` / `CH_MGMT` | hardware (`hw_ssn_sel` 1, `hw_seq_mode` 1) | fixed rate, no rate fallback, `hdr_llc_len` 0 |
//! | [`Desc::eapol`] | `QSEL_VO` / `CH_ACH3` | software | EAPOL is a data frame with tid 7 (`rtw89_core_get_qsel`), unencrypted |
//! | [`Desc::data`] | `QSEL_BE` / `CH_ACH0` | software | CCMP-128 via `Sec`; `agg_en` off — there is no ADDBA session to aggregate into |
//!
//! `mac_id` names an address-CAM entry: this station's own (0) for management
//! frames, the peer's (1) for frames to the access point, as Linux picks it
//! (`rtw89_core_tx_get_mac_id`) and the recording shows.

use crate::Bus;
use crate::h2c;
use core::sync::atomic::{Ordering, fence};

/// Bytes of the WD page this driver writes.
pub const WD_LEN: usize = 32 + 24 + 8 + 6;
/// Bytes of one WD page of channel memory (`RTW89_PCI_TXWD_PAGE_SIZE`): the
/// page's [`WD_LEN`] of live bytes, the rest padding.
pub const PAGE_SIZE: usize = 128;
/// WD pages per channel (`RTW89_PCI_TXWD_NUM_MAX` is 512; eight in-flight
/// frames is more than a join needs, and the chip fetches entries as they
/// come).
pub const PAGES: usize = 8;
/// Longest frame one channel accepts, and the most one address-info entry
/// carries minus what the chip wants back as slack.
pub const FRAME_MAX: usize = 1600;
const ADDR_INFO_MAX: usize = 2044;
const _: () = assert!(FRAME_MAX <= ADDR_INFO_MAX, "a frame must fit one address-info entry");
/// Bytes of one channel's TX memory, as [`Ring::submit`] expects it: the
/// TXBD ring, then [`PAGES`] WD pages, then [`PAGES`] frame buffers.
pub const CHAN_BYTES: usize = ring_bytes() + PAGES * (PAGE_SIZE + FRAME_MAX);

/// TXBD ring entries per channel (`RING_LEN`, as every ring is sized).
const RING_ENTRIES: usize = 256;
/// Bytes of one channel's TXBD ring.
const fn ring_bytes() -> usize {
    RING_ENTRIES * h2c::BD_LEN
}

/// TX DMA channels (`RTW89_TXCH_*`) and queue selectors (`RTW89_TX_QSEL_*`).
pub const CH_ACH0: u8 = 0;
pub const CH_ACH3: u8 = 3;
pub const CH_MGMT: u8 = 8;
pub const QSEL_BE: u8 = 0;
pub const QSEL_VO: u8 = 3;
pub const QSEL_MGMT: u8 = 0x12;

/// `RTW89_SEC_KEY_TYPE_CCMP128`.
pub const SEC_CCMP128: u8 = 6;

/// The index register of TX channel `ch` (0..=8): `R_AX_ACH0_TXBD_IDX` +
/// 4 × channel; CH8 (`R_AX_CH8_TXBD_IDX`, 0x1078) follows ACH7.
#[must_use]
pub const fn idx_reg(ch: u8) -> u32 {
    0x1058 + 4 * ch as u32
}

/// The TX channels the join uses and their offset in [`crate::bringup::Dma`]'s
/// `tx_phys` / the glue's channel memory: data (ACH0), EAPOL (ACH3) and
/// management (CH8), in [`crate::regs::TX_RINGS`] order.
pub const USED: [usize; 3] = [CH_ACH0 as usize, CH_ACH3 as usize, CH_MGMT as usize];

/// How long [`Ring::submit`] waits for the chip to drain a page before
/// giving up, in 1 µs steps.
const RECLAIM_STEPS: u32 = 10_000;

/// Hardware encryption of one frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sec {
    /// Security CAM entry holding the key.
    pub cam_idx: u8,
    pub keyid: u8,
    /// The CCMP packet number (48 bits); the chip transmits it big-endian in
    /// the frame's CCMP header and little-endian across body words 4–5.
    pub pn: u64,
}

/// What `rtw89_tx_desc_info` holds, for the frames this driver sends.
///
/// Every field is one the recorded join showed Linux setting for one of the
/// three classes; the constructors carry those values so a call site names
/// only what varies per frame.
#[allow(clippy::struct_excessive_bools)] // the descriptor's own flag bits
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Desc {
    /// Frame length: header, CCMP header space, body. No FCS, no MIC.
    pub pkt_size: u16,
    pub qsel: u8,
    pub ch_dma: u8,
    pub mac_id: u8,
    /// 802.11 header length (QoS field included, LLC excluded) in 2-byte
    /// units — `rtw89_core_tx_update_llc_hdr`. 0 for management frames.
    pub hdr_llc_len: u8,
    pub agg_en: bool,
    pub bk: bool,
    /// The software sequence number; ignored when the hardware counts.
    pub seq: u16,
    /// `hw_ssn_sel`: 1 = the hardware sequence counter (management frames).
    pub hw_ssn_sel: u8,
    /// `hw_seq_mode`: 1 = use `hw_ssn_sel`'s counter.
    pub hw_seq_mode: u8,
    /// Transmit at `data_rate`, do not fall back (`use_rate` + `dis_data_fb`).
    pub fixed_rate: bool,
    pub data_rate: u16,
    pub sec: Option<Sec>,
}

impl Desc {
    /// A management frame (auth, assoc, action): fixed basic rate, hardware
    /// sequence, `qsel` `QSEL_MGMT` on the management channel.
    #[must_use]
    pub const fn mgmt(pkt_size: u16, mac_id: u8) -> Self {
        Self {
            pkt_size,
            qsel: QSEL_MGMT,
            ch_dma: CH_MGMT,
            mac_id,
            hdr_llc_len: 0,
            agg_en: false,
            bk: false,
            seq: 0,
            hw_ssn_sel: 1,
            hw_seq_mode: 1,
            fixed_rate: true,
            data_rate: 0,
            sec: None,
        }
    }

    /// An EAPOL frame: a data frame of tid 7, which Linux maps to `QSEL_VO`
    /// and the ACH3 channel, unencrypted, software sequence.
    #[must_use]
    pub const fn eapol(pkt_size: u16, mac_id: u8) -> Self {
        Self {
            pkt_size,
            qsel: QSEL_VO,
            ch_dma: CH_ACH3,
            mac_id,
            hdr_llc_len: 13, // 26 bytes of QoS data header
            agg_en: false,
            bk: false,
            seq: 0,
            hw_ssn_sel: 0,
            hw_seq_mode: 0,
            fixed_rate: false,
            data_rate: 0,
            sec: None,
        }
    }

    /// An encrypted data frame: `QSEL_BE` on ACH0, software sequence.
    /// `agg_en` stays off — with no ADDBA session an aggregated frame would
    /// never be acknowledged.
    #[must_use]
    pub const fn data(pkt_size: u16, mac_id: u8, seq: u16, sec: Sec) -> Self {
        Self {
            pkt_size,
            qsel: QSEL_BE,
            ch_dma: CH_ACH0,
            mac_id,
            hdr_llc_len: 13,
            agg_en: false,
            bk: false,
            seq,
            hw_ssn_sel: 0,
            hw_seq_mode: 0,
            fixed_rate: false,
            data_rate: 0,
            sec: Some(sec),
        }
    }

    /// The 32-byte WD body (`rtw89_core_fill_txdesc_v1`, 8852C).
    #[must_use]
    fn body(&self) -> [u8; 32] {
        let mut b = [0u8; 32];
        let dw0 = (1u32 << 22) // WD_INFO_EN
            | ((u32::from(self.ch_dma) & 0xf) << 16) // CHANNEL_DMA
            | ((u32::from(self.hdr_llc_len) & 0x1f) << 11) // HDR_LLC_LEN
            | (1 << 7); // WD_PAGE
        let dw1 = (1u32 << 26) // ADDR_INFO_NUM: always one entry
            | match self.sec {
                Some(s) => ((u32::from(s.keyid) & 3) << 4) | sec_type(self),
                None => 0,
            };
        let dw2 = ((u32::from(self.mac_id) & 0xff) << 24)
            | ((u32::from(self.qsel) & 0x3f) << 17)
            | (u32::from(self.pkt_size) & 0x3fff);
        let dw3 = (u32::from(self.seq) & 0xfff)
            | (u32::from(self.agg_en) << 12)
            | (u32::from(self.bk) << 13);
        b[0..4].copy_from_slice(&dw0.to_le_bytes());
        b[4..8].copy_from_slice(&dw1.to_le_bytes());
        b[8..12].copy_from_slice(&dw2.to_le_bytes());
        b[12..16].copy_from_slice(&dw3.to_le_bytes());
        if let Some(s) = self.sec {
            // SEC_IV: the packet number, low byte first (`sec_seq[i] = pn >> (i*8)`),
            // in dword 4's bits 31:16 and dword 5 (`rtw89_build_txwd_body4/5`).
            b[18] = s.pn as u8;
            b[19] = (s.pn >> 8) as u8;
            b[20] = (s.pn >> 16) as u8;
            b[21] = (s.pn >> 24) as u8;
            b[22] = (s.pn >> 32) as u8;
            b[23] = (s.pn >> 40) as u8;
        }
        let dw7 = (1u32 << 31) // USE_RATE_V1
            | ((u32::from(self.data_rate) & 0x1ff) << 16);
        b[28..32].copy_from_slice(&dw7.to_le_bytes());
        b
    }

    /// The 24-byte WD info (`rtw89_build_txwd_info{0,1,2,4}_v1`).
    #[must_use]
    fn info(&self) -> [u8; 24] {
        let mut b = [0u8; 24];
        let dw0: u32 = u32::from(self.fixed_rate) << 10; // DISDATAFB
        let dw2: u32 = u32::from(self.sec.is_some()) << 8 // FORCE_KEY_EN
            | match self.sec {
                Some(s) => u32::from(s.cam_idx),
                None => 0,
            };
        let dw4: u32 = 1 << 27 // RTS_EN (unicast: !is_bmc)
            | 1 << 31; // HW_RTS_EN
        b[0..4].copy_from_slice(&dw0.to_le_bytes());
        b[8..12].copy_from_slice(&dw2.to_le_bytes());
        b[16..20].copy_from_slice(&dw4.to_le_bytes());
        b
    }
}

/// The `SEC_TYPE` the descriptor carries for `d`'s cipher: the key type
/// itself, CCMP128 = 6, as `rtw89_core_tx_update_sec_key` records it.
const fn sec_type(d: &Desc) -> u32 {
    match d.sec {
        Some(_) => SEC_CCMP128 as u32,
        None => 0,
    }
}

/// Builds one WD page: body, info, WP info, address info.
///
/// `wp_seq` names the page (the release report quotes it); `frame_phys` is
/// the frame's bus address — below 4 GiB, so the address-info high bits are
/// zero.
///
/// # Panics
///
/// `frame_phys` at or above 4 GiB (the address-info format is 32-bit low +
/// 4 high bits and the glue keeps DMA memory low anyway).
pub fn wd_page(d: &Desc, wp_seq: u16, frame_phys: u64, out: &mut [u8; WD_LEN]) {
    assert!(frame_phys < 1 << 32, "frame DMA address above 4 GiB");
    out[..32].copy_from_slice(&d.body());
    out[32..56].copy_from_slice(&d.info());
    // WP info: this page's sequence, valid bit set, the other three clear.
    let seq0 = wp_seq | 0x8000; // RTW89_PCI_TXWP_VALID
    out[56..58].copy_from_slice(&seq0.to_le_bytes());
    // Address info: the whole frame, last segment.
    let opt = (d.pkt_size & 0x7ff)
        | (((frame_phys >> 32) as u16 & 0xf) << 11)
        | (1 << 15); // LS
    out[64..66].copy_from_slice(&opt.to_le_bytes());
    out[66..68].copy_from_slice(&(frame_phys as u16).to_le_bytes());
    out[68..70].copy_from_slice(&((frame_phys >> 16) as u16).to_le_bytes());
}

/// Why a frame did not go out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The frame is longer than [`FRAME_MAX`].
    TooBig(usize),
    /// The chip did not drain a WD page within the budget.
    Stuck { reg: u32, last: u32 },
}

/// One TX channel's host half: the TXBD ring write pointer and the WD pages
/// it has handed out.
///
/// `mem` is the channel's [`CHAN_BYTES`] of DMA memory — ring, then
/// [`PAGES`] WD pages, then [`PAGES`] frame buffers, page *p* pairing with
/// frame *p*. A page comes back when the chip's read index moves past the
/// ring entry that named it; the chip has then fetched the WD page and the
/// frame, and DMA on x86 is coherent, so both may be overwritten. At most
/// [`PAGES`] entries are ever outstanding, which is what makes "the tail
/// entry's index is behind the chip's" a well-formed question on a ring that
/// wraps at 256.
pub struct Ring {
    /// How many frames have been submitted, ever — the ring's write pointer
    /// and entry index are both its low bits.
    head: usize,
    /// How many of those the chip has read back, ever.
    tail: usize,
}

impl Default for Ring {
    fn default() -> Self {
        Self::new()
    }
}

impl Ring {
    #[must_use]
    pub const fn new() -> Self {
        Self { head: 0, tail: 0 }
    }

    /// Copies `frame` into the channel's memory, writes the WD page and ring
    /// entry, and kicks the channel. Allocation-free: it fails rather than
    /// waits forever.
    ///
    /// # Errors
    ///
    /// [`Error::TooBig`] for a frame past [`FRAME_MAX`], [`Error::Stuck`] if
    /// all pages are out and the chip stops advancing.
    pub fn submit(
        &mut self,
        bus: &mut impl Bus,
        mem: &mut [u8],
        mem_phys: u64,
        frame: &[u8],
        d: Desc,
    ) -> Result<(), Error> {
        if frame.len() > FRAME_MAX {
            return Err(Error::TooBig(frame.len()));
        }
        let reg = idx_reg(d.ch_dma);
        let mut waited = 0;
        while self.head - self.tail >= PAGES {
            self.reclaim(bus, d.ch_dma);
            if self.head - self.tail < PAGES {
                break;
            }
            if waited >= RECLAIM_STEPS {
                return Err(Error::Stuck { reg, last: bus.read32(reg) });
            }
            bus.delay_us(1);
            waited += 1;
        }
        let page = (self.head % PAGES) as u8;
        let page_at = ring_bytes() + page as usize * PAGE_SIZE;
        let frame_at = ring_bytes() + PAGES * PAGE_SIZE + page as usize * FRAME_MAX;
        {
            let mut out = [0u8; WD_LEN];
            wd_page(&d, u16::from(page), mem_phys + page_at as u64, &mut out);
            mem[page_at..page_at + WD_LEN].copy_from_slice(&out);
        }
        mem[frame_at..frame_at + frame.len()].copy_from_slice(frame);
        let at = (self.head % RING_ENTRIES) * h2c::BD_LEN;
        mem[at..at + h2c::BD_LEN].copy_from_slice(&h2c::bd(mem_phys + page_at as u64, WD_LEN));
        // Pages, frame and ring entry in memory before the chip is told.
        fence(Ordering::Release);
        self.head += 1;
        bus.write16(reg, (self.head % RING_ENTRIES) as u16);
        Ok(())
    }

    /// Frees the pages of the outstanding entries the chip has read. The chip
    /// reports how far it is in bits 27:16 of the channel's index register
    /// (`h2c::hw_idx`); with at most [`PAGES`] entries out, the tail entry is
    /// consumed exactly when that index has moved past its position.
    fn reclaim(&mut self, bus: &mut impl Bus, ch: u8) {
        let hw = h2c::hw_idx(bus.read32(idx_reg(ch)));
        while self.tail < self.head {
            let entry = (self.tail % RING_ENTRIES) as u16;
            let dist = (entry + RING_ENTRIES as u16 - hw) % RING_ENTRIES as u16;
            // dist 0 = the chip sits on this entry; a small dist = it is
            // somewhere among the outstanding ones; past them = consumed.
            if dist <= PAGES as u16 {
                break;
            }
            self.tail += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate std;
    use std::vec::Vec;

    fn zeros(n: usize) -> Vec<u8> {
        let mut v = Vec::new();
        v.resize(n, 0);
        v
    }

    fn filled(n: usize, b: u8) -> Vec<u8> {
        let mut v = Vec::new();
        v.resize(n, b);
        v
    }

    /// Three `txd` probe records from the recorded join
    /// (`~/.akuma/w0/20261006-102808`, `struct rtw89_tx_desc_info` bytes, no
    /// addresses in them): an authentication frame, EAPOL message 2, and an
    /// encrypted data frame from after the keys went in.
    const REC_MGMT: [u8; 32] = [
        0x1e, 0x00, 0x00, 0x00, 0x12, 0x08, 0x00, 0x00, 0x01, 0x01, 0x01, 0x01, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00,
    ];
    const REC_EAPOL: [u8; 32] = [
        0x9b, 0x00, 0x00, 0x00, 0x03, 0x03, 0x0d, 0x00, 0x01, 0x01, 0x00, 0x00, 0x01, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00,
    ];
    const REC_DATA: [u8; 32] = [
        0x3e, 0x00, 0x01, 0x00, 0x00, 0x00, 0x0d, 0x00, 0x01, 0x01, 0x00, 0x00, 0x00, 0x01,
        0x00, 0x06, 0x3f, 0x01, 0x01, 0x00, 0x06, 0x00, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00,
    ];

    fn dw(b: &[u8], i: usize) -> u32 {
        u32::from_le_bytes([b[i * 4], b[i * 4 + 1], b[i * 4 + 2], b[i * 4 + 3]])
    }

    /// The mgmt class reproduces the recorded auth frame's inputs: qsel
    /// `QSEL_MGMT`, the management channel, no LLC, hardware sequence, fixed
    /// rate, one address info.
    #[test]
    fn mgmt_desc_matches_the_recording() {
        let d = Desc::mgmt(0x1e, 0);
        assert_eq!(d.qsel, REC_MGMT[4]);
        assert_eq!(d.ch_dma, REC_MGMT[5]);
        assert_eq!(d.hdr_llc_len, REC_MGMT[6]);
        assert!(!d.bk && !d.agg_en);
        let b = d.body();
        assert_eq!(dw(&b, 0) >> 22 & 1, 1); // en_wd_info
        assert_eq!((dw(&b, 1) >> 26) & 0x3f, u32::from(REC_MGMT[18])); // addr_info_nr = 1
        assert_eq!(dw(&b, 0) >> 16 & 0xf, 8);
        assert_eq!(dw(&b, 2) >> 24 & 0xff, 0); // mac_id
        assert_eq!((dw(&b, 0) >> 24) & 0x1f, 0); // wp_offset
        // Hardware sequence for management frames.
        assert_eq!(d.hw_ssn_sel, 1);
        assert_eq!(d.hw_seq_mode, 1);
        // Fixed rate: use_rate + dis_data_fb in the info word.
        let i = d.info();
        assert_eq!(dw(&i, 0) >> 10 & 1, 1); // DISDATAFB
        assert_eq!(dw(&i, 4) >> 31 & 1, 1); // HW_RTS_EN
        assert_eq!(dw(&i, 4) >> 27 & 1, 1); // RTS_EN (unicast)
    }

    /// The EAPOL class is the recorded message 2: a tid-7 data frame, VO
    /// queue on ACH3, the QoS header length, no encryption, software
    /// sequence.
    #[test]
    fn eapol_desc_matches_the_recording() {
        let d = Desc::eapol(0x9b, 1);
        assert_eq!(d.pkt_size, 155);
        assert_eq!(d.qsel, REC_EAPOL[4]);
        assert_eq!(d.ch_dma, REC_EAPOL[5]);
        assert_eq!(d.hdr_llc_len, REC_EAPOL[6]);
        assert!(d.sec.is_none());
        assert_eq!(d.hw_ssn_sel, 0);
        let b = d.body();
        assert_eq!(dw(&b, 1) & 0xf, 0); // no SEC_TYPE
        assert_eq!(dw(&b, 4), 0); // no SEC_IV
        assert_eq!(dw(&b, 3) >> 12 & 1, 0); // no AGG_EN: EAPOL never aggregates
        assert_eq!(dw(&b, 2) >> 24 & 0xff, 1); // the peer's mac id
    }

    /// The data class reproduces the recorded encrypted frame, packet number
    /// included, with `agg_en` deliberately off — no ADDBA session exists.
    #[test]
    fn data_desc_matches_the_recording() {
        let d = Desc::data(62, 1, 2, Sec { cam_idx: 0, keyid: 0, pn: 4 });
        assert_eq!(d.qsel, REC_DATA[4]);
        assert_eq!(d.ch_dma, REC_DATA[5]);
        assert_eq!(d.hdr_llc_len, REC_DATA[6]);
        let b = d.body();
        assert_eq!(dw(&b, 1) & 0xf, u32::from(REC_DATA[20])); // SEC_TYPE = CCMP128
        assert_eq!(dw(&b, 1) >> 4 & 3, u32::from(REC_DATA[19])); // SEC_KEYID
        assert_eq!(dw(&b, 3) & 0xfff, 2); // SW_SEQ
        assert_eq!(dw(&b, 3) >> 12 & 1, 0); // no AGG_EN (Linux had one; we have no ADDBA)
        assert_eq!(b[18], REC_DATA[22]); // SEC_IV low byte
        assert_eq!(b[19], 0);
        let i = d.info();
        assert_eq!(dw(&i, 2) >> 8 & 1, 1); // FORCE_KEY_EN
        assert_eq!(dw(&i, 2) & 0xff, 0); // SEC_CAM_IDX = pairwise entry 0
        // (v1's info word 2 has no SEC_TYPE — that lives in body word 1.)
    }

    /// The WD page lays Linux's four parts out at their documented offsets,
    /// with the numbers the 8852C path computes.
    #[test]
    fn wd_page_layouts_linuxs_four_parts() {
        let d = Desc::data(62, 1, 2, Sec { cam_idx: 0, keyid: 0, pn: 4 });
        let mut p = [0u8; WD_LEN];
        wd_page(&d, 5, 0x2345_6000, &mut p);
        // Body words.
        assert_eq!(dw(&p, 0), 0x0040_6880); // WD_INFO_EN | HDR_LLC_LEN 13 | WD_PAGE
        assert_eq!(dw(&p, 1), (1 << 26) | u32::from(SEC_CCMP128));
        assert_eq!(dw(&p, 2), 0x0100_003e); // MACID 1 | TXPKT_SIZE 62
        assert_eq!(dw(&p, 3), 2);
        assert_eq!(dw(&p, 4), 4 << 16);
        assert_eq!(dw(&p, 5), 0);
        assert_eq!(dw(&p, 6), 0);
        assert_eq!(dw(&p, 7), 1 << 31); // USE_RATE_V1, DATA_RATE 0
        // Info words. (Unfixed-rate data: no DISDATAFB.)
        assert_eq!(dw(&p, 8), 0);
        assert_eq!(dw(&p, 10), 1 << 8); // FORCE_KEY_EN, cam 0
        assert_eq!(dw(&p, 12), (1 << 27) | (1 << 31)); // RTS_EN | HW_RTS_EN
        // WP info: page 5, valid.
        assert_eq!(u16::from_le_bytes([p[56], p[57]]), 0x8005);
        assert_eq!(&p[58..64], &[0; 6]);
        // Address info: 62 bytes below 4 GiB (so no high bits), last segment.
        assert_eq!(u16::from_le_bytes([p[64], p[65]]), 0x803e); // 62 bytes, LS
        assert_eq!(u16::from_le_bytes([p[66], p[67]]), 0x6000);
        assert_eq!(u16::from_le_bytes([p[68], p[69]]), 0x2345);
        // Nothing above bit 35 survived: the format only has 4 high bits and
        // this driver keeps frames below 4 GiB outright.
        assert_eq!(u16::from_le_bytes([p[64], p[65]]) >> 11 & 0xf, 0);
        // A management frame has the same shape, unencrypted.
        let mut m = [0u8; WD_LEN];
        wd_page(&Desc::mgmt(30, 0), 0, 0x1000, &mut m);
        assert_eq!(dw(&m, 1), 1 << 26);
        assert_eq!(dw(&m, 4), 0);
        assert_eq!(u16::from_le_bytes([m[56], m[57]]), 0x8000);
    }

    /// A fake channel: registers that count reads of the index register, plus
    /// [`CHAN_BYTES`] of memory the test can inspect.
    struct Fake {
        hw: u16,
        kicks: Vec<u16>,
    }

    impl Bus for Fake {
        fn read8(&mut self, _: u32) -> u8 {
            0
        }
        fn read16(&mut self, _: u32) -> u16 {
            0
        }
        fn read32(&mut self, off: u32) -> u32 {
            assert_eq!(off, idx_reg(CH_ACH0));
            u32::from(self.hw) << 16
        }
        fn write8(&mut self, _: u32, _: u8) {}
        fn write16(&mut self, off: u32, v: u16) {
            assert_eq!(off, idx_reg(CH_ACH0));
            self.kicks.push(v);
        }
        fn write32(&mut self, _: u32, _: u32) {}
        fn delay_us(&mut self, _: u32) {}
    }

    fn fake() -> Fake {
        Fake { hw: 0, kicks: Vec::new() }
    }

    const MEM_PHYS: u64 = 0x4000_0000;

    fn d(len: u16, seq: u16) -> Desc {
        Desc::data(len, 1, seq, Sec { cam_idx: 0, keyid: 0, pn: u64::from(seq) })
    }

    /// A submit lands the frame in its page's buffer, the WD page next to it,
    /// a ring entry pointing at the page, and kicks the channel once.
    #[test]
    fn submit_writes_page_frame_entry_and_kicks() {
        let mut r = Ring::new();
        let mut f = fake();
        let mut mem = zeros(CHAN_BYTES);
        let frame = filled(100, 0xab);
        r.submit(&mut f, &mut mem, MEM_PHYS, &frame, d(100, 0)).unwrap();
        assert_eq!(f.kicks, [1]);
        let (addr, len) = h2c::parse_bd(&mem[0..8]);
        assert_eq!(len, WD_LEN);
        // The entry's address is page 0's, so the WD page sits there.
        assert_eq!(addr, MEM_PHYS + ring_bytes() as u64);
        let wd = &mem[ring_bytes()..ring_bytes() + WD_LEN];
        assert_eq!(dw(wd, 2) & 0x3fff, 100);
        assert_eq!(u16::from_le_bytes([wd[56], wd[57]]), 0x8000); // page 0
        // The frame went into page 0's frame buffer, whole.
        let at = ring_bytes() + PAGES * PAGE_SIZE;
        assert_eq!(&mem[at..at + 100], &frame[..]);
    }

    /// Pages come back in order as the chip's index passes their entries, and
    /// a submit with all pages out waits for that instead of failing.
    #[test]
    fn pages_recycle_as_the_chip_drains() {
        let mut r = Ring::new();
        let mut f = fake();
        let mut mem = zeros(CHAN_BYTES);
        let frame = zeros(10);
        for i in 0..PAGES {
            r.submit(&mut f, &mut mem, MEM_PHYS, &frame, d(10, i as u16)).unwrap();
        }
        // All eight pages out: page numbers 0..8 in the WP words.
        for (p, i) in (0..PAGES).enumerate() {
            let wd = &mem[ring_bytes() + i * PAGE_SIZE..][..WD_LEN];
            assert_eq!(u16::from_le_bytes([wd[56], wd[57]]) & 0x7fff, p as u16);
        }
        // The chip has read two entries: the next submit reuses page 0 after
        // reclaiming it, without waiting.
        f.hw = 2;
        r.submit(&mut f, &mut mem, MEM_PHYS, &frame, d(10, 99)).unwrap();
        let wd = &mem[ring_bytes()..ring_bytes() + WD_LEN];
        assert_eq!(u16::from_le_bytes([wd[56], wd[57]]) & 0x7fff, 0);
        assert_eq!(wd[18], 99); // the new frame's packet number
        assert_eq!(f.kicks.len(), PAGES + 1);
        assert_eq!(*f.kicks.last().unwrap(), (PAGES + 1) as u16);
    }

    /// A frame past [`FRAME_MAX`] is refused before any memory is touched.
    #[test]
    fn oversize_frames_are_refused() {
        let mut r = Ring::new();
        let mut f = fake();
        let mut mem = zeros(CHAN_BYTES);
        let frame = zeros(FRAME_MAX + 1);
        assert_eq!(r.submit(&mut f, &mut mem, MEM_PHYS, &frame, d(0, 0)), Err(Error::TooBig(FRAME_MAX + 1)));
        assert_eq!(f.kicks, [] as [u16; 0]);
    }

    /// The chip never draining gives up with what the register said, and the
    /// ring stays usable once it moves again.
    #[test]
    fn a_stuck_chip_times_out() {
        let mut r = Ring::new();
        let mut f = fake();
        let mut mem = zeros(CHAN_BYTES);
        let frame = zeros(10);
        for i in 0..PAGES {
            r.submit(&mut f, &mut mem, MEM_PHYS, &frame, d(10, i as u16)).unwrap();
        }
        f.hw = 0; // nothing reclaimed, ever
        assert_eq!(
            r.submit(&mut f, &mut mem, MEM_PHYS, &frame, d(10, 99)),
            Err(Error::Stuck { reg: idx_reg(CH_ACH0), last: 0 })
        );
        // The chip wakes up; the ring works again.
        f.hw = 4;
        r.submit(&mut f, &mut mem, MEM_PHYS, &frame, d(10, 99)).unwrap();
        assert_eq!(f.kicks.len(), PAGES + 1);
    }

}
