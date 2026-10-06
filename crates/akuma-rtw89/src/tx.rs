//! The transmit side for 802.11 frames: the WD page (`rtw89_pci_txwd_submit`
//! with `rtw89_core_fill_txdesc_v1`, the 8852C arm) and the host half of a
//! data/management TX ring.
//!
//! On every TX channel but CH12, a ring entry points at a **WD page**, not at
//! the frame:
//!
//! | bytes | what | Linux |
//! |---|---|---|
//! | 32 | WD body | `rtw89_txwd_body_v1` |
//! | 24 | WD info | `rtw89_txwd_info` (always present here: Linux sets `en_wd_info` for every frame it sends) |
//! | 8 | WP info | `rtw89_pci_tx_wp_info`: the page's sequence number, valid bit |
//! | 6 | address info | `rtw89_pci_tx_addr_info_32_v1`: where the frame is, how long |
//!
//! — 70 bytes, which is the length the ring entry carries. The frame itself
//! (header first, no FCS, no MIC) sits elsewhere in DMA memory. A protected
//! frame reserves the 8 bytes of CCMP header after its 802.11 header; the
//! chip (`hw_sec_hdr`) writes it from the packet number in body words 4–5, and
//! appends the MIC.
//!
//! Field values follow what the recorded join showed Linux using for each
//! kind of frame (`overlays/ryzen/w0-trace.sh JOIN=1`, the `txd` probe):
//! [`Desc::mgmt`], [`Desc::eapol`], [`Desc::data`].

use crate::Bus;
use crate::h2c::BD_LEN;
use core::sync::atomic::{Ordering, fence};

/// Bytes of the WD page this driver writes.
pub const WD_LEN: usize = 32 + 24 + 8 + 6;

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

/// Hardware encryption of one frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sec {
    /// Security CAM entry holding the key.
    pub cam_idx: u8,
    pub keyid: u8,
    /// The CCMP packet number (48 bits).
    pub pn: u64,
}

/// What `rtw89_tx_desc_info` holds, for the frames this driver sends.
#[allow(clippy::struct_excessive_bools)] // the descriptor's own flag bits
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Desc {
    /// Frame length: header, CCMP header space, body. No FCS, no MIC.
    pub pkt_size: u16,
    pub qsel: u8,
    pub ch_dma: u8,
    pub mac_id: u8,
    /// 802.11 header length / 2 (`hdr_llc_len`, 0 for management frames).
    pub hdr_llc_len: u8,
    ///