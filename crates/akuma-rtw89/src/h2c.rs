//! The firmware-command channel, CH12: what goes in its ring and its packets.
//!
//! A packet on CH12 is, in DMA memory, a 16-byte descriptor (Linux sizes it
//! `sizeof(struct rtw89_rxdesc_short)` and fills only dword 0:
//! `rtw89_build_txwd_fwcmd0_v1`) followed by the payload. The firmware-header
//! packet's payload starts with an 8-byte H2C header (`fwcmd_hdr`); section
//! data packets have none. The ring entry pointing at a packet is an 8-byte
//! buffer descriptor (`struct rtw89_pci_tx_bd_32`). The host hands the chip
//! entries by writing its write index to `CH12_TXBD_IDX[11:0]`; the chip
//! reports how far it has read in bits 27:16 of the same register.

/// Size of the descriptor in front of every CH12 packet.
pub const DESC_LEN: usize = 16;
/// Size of an H2C header (`H2C_HEADER_LEN`).
pub const HDR_LEN: usize = 8;
/// Size of one ring entry.
pub const BD_LEN: usize = 8;

/// `RTW89_CORE_RX_TYPE_H2C`: the descriptor type of an H2C command.
pub const TYPE_H2C: u32 = 13;
/// `RTW89_CORE_RX_TYPE_FWDL`: the descriptor type of a firmware data packet.
pub const TYPE_FWDL: u32 = 14;

/// `H2C_CAT_MAC`, `H2C_CL_MAC_FWDL`, `H2C_FUNC_MAC_FWHDR_DL`.
pub const CAT_MAC: u32 = 1;
pub const CL_MAC_FWDL: u32 = 3;
pub const FUNC_MAC_FWHDR_DL: u32 = 0;

/// Dword 0 of the packet descriptor: `RPKT_LEN[13:0]` is the bytes after the
/// descriptor, `RPKT_TYPE[27:24]` the packet type.
#[must_use]
pub fn desc(payload_len: usize, ty: u32) -> [u8; DESC_LEN] {
    let mut d = [0u8; DESC_LEN];
    let w = (payload_len as u32 & 0x3fff) | (ty << 24);
    d[..4].copy_from_slice(&w.to_le_bytes());
    d
}

/// An H2C header (`rtw89_h2c_pkt_set_hdr_fwdl`), for `body_len` bytes of body.
/// `DEL_TYPE` is `FWCMD_TYPE_H2C` (0) and no acknowledgement is asked for.
#[must_use]
pub fn h2c_header(cat: u32, class: u32, func: u32, seq: u8, body_len: usize) -> [u8; HDR_LEN] {
    let mut h = [0u8; HDR_LEN];
    let hdr0 = (cat & 0x3) | ((class & 0x3f) << 2) | ((func & 0xff) << 8) | (u32::from(seq) << 24);
    let hdr1 = (body_len + HDR_LEN) as u32 & 0x3fff;
    h[..4].copy_from_slice(&hdr0.to_le_bytes());
    h[4..].copy_from_slice(&hdr1.to_le_bytes());
    h
}

/// A ring entry: `length`, then `opt` = last-segment (bit 14) plus address
/// bits 39:32 in bits 13:6, then the low address word.
#[must_use]
pub fn bd(dma: u64, len: usize) -> [u8; BD_LEN] {
    let mut b = [0u8; BD_LEN];
    let opt: u16 = (1 << 14) | ((((dma >> 32) & 0xff) as u16) << 6);
    b[..2].copy_from_slice(&(len as u16).to_le_bytes());
    b[2..4].copy_from_slice(&opt.to_le_bytes());
    b[4..].copy_from_slice(&(dma as u32).to_le_bytes());
    b
}

/// Decode a ring entry back to `(dma, len)`: the simulated chip's side of [`bd`].
#[must_use]
pub fn parse_bd(b: &[u8]) -> (u64, usize) {
    let len = u16::from_le_bytes([b[0], b[1]]);
    let opt = u16::from_le_bytes([b[2], b[3]]);
    let lo = u32::from_le_bytes([b[4], b[5], b[6], b[7]]);
    (u64::from(lo) | (u64::from((opt >> 6) & 0xff) << 32), usize::from(len))
}

/// Bytes in each RX buffer (`RTW89_PCI_RX_BUF_SIZE`).
pub const RX_BUF_SIZE: u16 = 11454 + 40 + 4;

/// An RX ring entry (`struct rtw89_pci_rx_bd_32`, `rtw89_pci_init_rx_bd`):
/// the buffer's size, address bits 39:32 in `opt[13:6]`, the low address word.
///
/// Linux stocks every entry of both RX rings with a buffer before the
/// download, and the chip treats a ring whose two indices are equal as all
/// free. A ring of zeroed entries — size 0, address 0 — is what the first
/// metal runs (2026-10-06) gave it, and they stalled at the firmware header.
#[must_use]
pub fn rx_bd(dma: u64, size: u16) -> [u8; BD_LEN] {
    let mut b = [0u8; BD_LEN];
    let opt: u16 = (((dma >> 32) & 0xff) as u16) << 6;
    b[..2].copy_from_slice(&size.to_le_bytes());
    b[2..4].copy_from_slice(&opt.to_le_bytes());
    b[4..].copy_from_slice(&(dma as u32).to_le_bytes());
    b
}

/// The chip's read index, from a `CH12_TXBD_IDX` read.
#[must_use]
pub const fn hw_idx(reg: u32) -> u16 {
    ((reg & crate::regs::TXBD_HW_IDX_MASK) >> crate::regs::TXBD_HW_IDX_SHIFT) as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_packs_length_and_type() {
        let d = desc(2020, TYPE_FWDL);
        assert_eq!(u32::from_le_bytes([d[0], d[1], d[2], d[3]]), 0x7e4 | (14 << 24));
        assert!(d[4..].iter().all(|&b| b == 0));
    }

    #[test]
    fn header_matches_linux_field_layout() {
        // FWHDR_DL with an 80-byte body: CAT 1, CLASS 3 at bit 2, FUNC 0, seq 0.
        let h = h2c_header(CAT_MAC, CL_MAC_FWDL, FUNC_MAC_FWHDR_DL, 0, 80);
        assert_eq!(u32::from_le_bytes([h[0], h[1], h[2], h[3]]), 0x0000_000d);
        assert_eq!(u32::from_le_bytes([h[4], h[5], h[6], h[7]]), 88);
    }

    #[test]
    fn bd_round_trips_a_40_bit_address() {
        let b = bd(0x12_3456_7000, 2036);
        assert_eq!(parse_bd(&b), (0x12_3456_7000, 2036));
        assert_eq!(u16::from_le_bytes([b[2], b[3]]) & (1 << 14), 1 << 14);
    }

    #[test]
    fn rx_bd_carries_size_and_address() {
        let b = rx_bd(0x3_0012_3000, RX_BUF_SIZE);
        assert_eq!(u16::from_le_bytes([b[0], b[1]]), 11498);
        assert_eq!(u16::from_le_bytes([b[2], b[3]]), 3 << 6);
        assert_eq!(u32::from_le_bytes([b[4], b[5], b[6], b[7]]), 0x0012_3000);
    }

    #[test]
    fn hw_index_is_bits_27_to_16() {
        // From the W0 trace: host index 2, chip index 1.
        assert_eq!(hw_idx(0x0001_0002), 1);
        assert_eq!(hw_idx(0x00a6_00a7), 0xa6);
    }
}
