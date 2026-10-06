//! The frames a station sends and answers while joining and afterwards:
//! open-system authentication, association, and data frames with LLC/SNAP
//! (and room for a CCMP header the card fills in).

use crate::Addr;
use crate::frame::{FC_PROTECTED, FC_TO_DS, Fc, HDR_LEN, Hdr, TYPE_DATA, TYPE_MGMT, mgmt};

/// The RSN element this station sends.
///
/// Version 1, group CCMP, pairwise CCMP, AKM PSK, capabilities 0 (no PMF: the network this was built for offers
/// PMF but does not require it, and without it no management frame needs a
/// key). Message 2 of the 4-way handshake must repeat these exact bytes.
pub const RSN_IE: [u8; 22] = [
    48, 20, 1, 0, 0x00, 0x0f, 0xac, 4, 1, 0, 0x00, 0x0f, 0xac, 4, 1, 0, 0x00, 0x0f, 0xac, 2, 0, 0,
];

/// Every element of the association request after the SSID.
///
/// What Linux's rtw89 + mac80211 sent from this very card (RTL8852CE) when it joined a
/// 2.4 GHz WPA2 network, recorded on ryzen 2026-10-06 (`w0-trace.sh JOIN=1`),
/// with the RSN capabilities cleared as [`RSN_IE`] says. In order: supported
/// rates (1, 2, 5.5, 11, 6, 9, 12, 18), extended rates (24, 36, 48, 54),
/// power capability, RSN, HT capabilities, extended capabilities, RM enabled
/// capabilities, supported operating classes, WMM information. These are the
/// chip's capabilities, not anything about a network.
#[rustfmt::skip]
pub const ASSOC_TAIL_2G: [u8; 122] = [
    0x01, 0x08, 0x02, 0x04, 0x0b, 0x16, 0x0c, 0x12, 0x18, 0x24,
    0x32, 0x04, 0x30, 0x48, 0x60, 0x6c,
    0x21, 0x02, 0x00, 0x14,
    0x30, 0x14, 0x01, 0x00, 0x00, 0x0f, 0xac, 0x04, 0x01, 0x00, 0x00, 0x0f, 0xac, 0x04, 0x01, 0x00, 0x00, 0x0f, 0xac, 0x02, 0x00, 0x00,
    0x2d, 0x1a, 0xef, 0x19, 0x03, 0xff, 0xff, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x2c, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x7f, 0x0b, 0x04, 0x00, 0x4a, 0x02, 0x01, 0x40, 0x40, 0x40, 0x00, 0x01, 0x20,
    0x46, 0x05, 0x70, 0x00, 0x00, 0x00, 0x00,
    0x3b, 0x15, 0x51, 0x51, 0x52, 0x53, 0x54, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7a, 0x7b, 0x7c, 0x7d, 0x7e, 0x7f, 0x80, 0x81, 0x82,
    0xdd, 0x07, 0x00, 0x50, 0xf2, 0x02, 0x00, 0x01, 0x00,
];

/// The capability field Linux sent with [`ASSOC_TAIL_2G`]: ESS, privacy,
/// short preamble, short slot time, radio measurement.
pub const ASSOC_CAPABILITY: u16 = 0x1431;
/// Beacon intervals between wake-ups Linux announced.
pub const LISTEN_INTERVAL: u16 = 5;

fn mgmt_hdr(out: &mut [u8], subtype: u8, bssid: &Addr, sta: &Addr) {
    Hdr { fc: Fc::new(TYPE_MGMT, subtype), duration: 0, addr1: *bssid, addr2: *sta, addr3: *bssid, seq_ctrl: 0 }
        .write(out);
}

/// Open-system authentication, transaction 1. `None` if `out` is too small.
pub fn auth_request(out: &mut [u8], bssid: &Addr, sta: &Addr) -> Option<usize> {
    let f = out.get_mut(..HDR_LEN + 6)?;
    mgmt_hdr(f, mgmt::AUTH, bssid, sta);
    f[HDR_LEN..].copy_from_slice(&[0, 0, 1, 0, 0, 0]);
    Some(f.len())
}

/// An association request: capability, listen interval, the SSID, then
/// `tail` (normally [`ASSOC_TAIL_2G`]).
pub fn assoc_request(out: &mut [u8], bssid: &Addr, sta: &Addr, ssid: &[u8], tail: &[u8]) -> Option<usize> {
    if ssid.len() > 32 {
        return None;
    }
    let len = HDR_LEN + 4 + 2 + ssid.len() + tail.len();
    let f = out.get_mut(..len)?;
    mgmt_hdr(f, mgmt::ASSOC_REQ, bssid, sta);
    f[24..26].copy_from_slice(&ASSOC_CAPABILITY.to_le_bytes());
    f[26..28].copy_from_slice(&LISTEN_INTERVAL.to_le_bytes());
    f[28] = 0;
    f[29] = ssid.len() as u8;
    f[30..30 + ssid.len()].copy_from_slice(ssid);
    f[30 + ssid.len()..].copy_from_slice(tail);
    Some(len)
}

/// What an authentication frame says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Auth {
    pub from: Addr,
    pub to: Addr,
    pub algorithm: u16,
    pub transaction: u16,
    pub status: u16,
}

impl Auth {
    #[must_use]
    pub fn parse(f: &[u8]) -> Option<Self> {
        let h = Hdr::parse(f)?;
        if h.fc.ty() != TYPE_MGMT || h.fc.subtype() != mgmt::AUTH || f.len() < HDR_LEN + 6 {
            return None;
        }
        let w = |at: usize| u16::from_le_bytes([f[at], f[at + 1]]);
        Some(Self { from: h.addr2, to: h.addr1, algorithm: w(24), transaction: w(26), status: w(28) })
    }
}

/// What an association response says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AssocResp {
    pub from: Addr,
    pub to: Addr,
    pub capability: u16,
    pub status: u16,
    /// The association id, bits 15:14 cleared.
    pub aid: u16,
}

impl AssocResp {
    #[must_use]
    pub fn parse(f: &[u8]) -> Option<Self> {
        let h = Hdr::parse(f)?;
        if h.fc.ty() != TYPE_MGMT || !matches!(h.fc.subtype(), mgmt::ASSOC_RESP | 3) || f.len() < HDR_LEN + 6 {
            return None;
        }
        let w = |at: usize| u16::from_le_bytes([f[at], f[at + 1]]);
        Some(Self { from: h.addr2, to: h.addr1, capability: w(24), status: w(26), aid: w(28) & 0x3fff })
    }
}

/// A deauthentication or disassociation: who sent it and why.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Goodbye {
    pub from: Addr,
    pub to: Addr,
    pub deauth: bool,
    pub reason: u16,
}

impl Goodbye {
    #[must_use]
    pub fn parse(f: &[u8]) -> Option<Self> {
        let h = Hdr::parse(f)?;
        let deauth = match h.fc.subtype() {
            mgmt::DEAUTH => true,
            mgmt::DISASSOC => false,
            _ => return None,
        };
        if h.fc.ty() != TYPE_MGMT || f.len() < HDR_LEN + 2 {
            return None;
        }
        Some(Self { from: h.addr2, to: h.addr1, deauth, reason: u16::from_le_bytes([f[24], f[25]]) })
    }
}

/// LLC/SNAP before an Ethernet type (RFC 1042).
pub const SNAP: [u8; 6] = [0xaa, 0xaa, 0x03, 0x00, 0x00, 0x00];
pub const ETHERTYPE_EAPOL: u16 = 0x888e;
pub const ETHERTYPE_IPV4: u16 = 0x0800;
pub const ETHERTYPE_ARP: u16 = 0x0806;
/// Bytes of a QoS data header (the 24-byte header plus QoS control).
pub const QOS_HDR_LEN: usize = 26;
/// Bytes of a CCMP header.
pub const CCMP_HDR_LEN: usize = 8;
/// Bytes of a CCMP MIC.
pub const CCMP_MIC_LEN: usize = 8;

/// How a data frame is to be protected: `None` in the clear (EAPOL before
/// the keys), or CCMP with this packet number and key id. The card computes
/// the MIC and encrypts; the frame carries the CCMP header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ccmp {
    pub pn: u64,
    pub keyid: u8,
}

/// A to-DS QoS data frame from `sta` to `da` through the access point
/// `bssid`: header, CCMP header if protected, LLC/SNAP, `ethertype`,
/// `payload`. Returns the length (without the MIC, which the card appends).
#[allow(clippy::too_many_arguments)]
pub fn data_frame(
    out: &mut [u8],
    bssid: &Addr,
    sta: &Addr,
    da: &Addr,
    tid: u8,
    seq: u16,
    ccmp: Option<Ccmp>,
    ethertype: u16,
    payload: &[u8],
) -> Option<usize> {
    let sec = if ccmp.is_some() { CCMP_HDR_LEN } else { 0 };
    let len = QOS_HDR_LEN + sec + 8 + payload.len();
    let f = out.get_mut(..len)?;
    let mut fc = Fc::new(TYPE_DATA, 8).with(FC_TO_DS);
    if ccmp.is_some() {
        fc = fc.with(FC_PROTECTED);
    }
    Hdr { fc, duration: 0, addr1: *bssid, addr2: *sta, addr3: *da, seq_ctrl: (seq & 0xfff) << 4 }.write(f);
    f[24] = tid & 7;
    f[25] = 0;
    let mut at = QOS_HDR_LEN;
    if let Some(c) = ccmp {
        let pn = c.pn.to_le_bytes();
        f[at..at + 8].copy_from_slice(&[pn[0], pn[1], 0, 0x20 | ((c.keyid & 3) << 6), pn[2], pn[3], pn[4], pn[5]]);
        at += CCMP_HDR_LEN;
    }
    f[at..at + 6].copy_from_slice(&SNAP);
    f[at + 6..at + 8].copy_from_slice(&ethertype.to_be_bytes());
    f[at + 8..].copy_from_slice(payload);
    Some(len)
}

/// A received data frame, as Ethernet would see it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Data<'a> {
    pub da: Addr,
    pub sa: Addr,
    pub bssid: Addr,
    pub protected: bool,
    pub ethertype: u16,
    pub payload: &'a [u8],
}

impl<'a> Data<'a> {
    /// A from-DS data frame (access point to station), with or without QoS
    /// and HT control. `fcs`: the frame ends in its 4-byte FCS. A protected
    /// frame is taken as already decrypted by the card, with its CCMP header
    /// and MIC still in place (what rtw89 delivers). `None` for null frames,
    /// A-MSDUs, frames without LLC/SNAP, and anything not from-DS.
    #[must_use]
    pub fn parse(f: &'a [u8], fcs: bool) -> Option<Self> {
        let f = if fcs { f.get(..f.len().checked_sub(4)?)? } else { f };
        let h = Hdr::parse(f)?;
        if h.fc.ty() != TYPE_DATA || h.fc.to_ds() || !h.fc.from_ds() {
            return None;
        }
        let st = h.fc.subtype();
        if st & 4 != 0 {
            return None; // no data (null, QoS null)
        }
        let mut at = HDR_LEN;
        if st & 8 != 0 {
            if *f.get(24)? & 0x80 != 0 {
                return None; // A-MSDU
            }
            at += 2;
            if h.fc.0 & (1 << 15) != 0 {
                at += 4; // HT control
            }
        }
        let protected = h.fc.protected();
        let mut end = f.len();
        if protected {
            at += CCMP_HDR_LEN;
            end = end.checked_sub(CCMP_MIC_LEN)?;
        }
        let body = f.get(at..end)?;
        if body.len() < 8 || body[..6] != SNAP {
            return None;
        }
        Some(Self {
            da: h.addr1,
            sa: h.addr3,
            bssid: h.addr2,
            protected,
            ethertype: u16::from_be_bytes([body[6], body[7]]),
            payload: &body[8..],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::Ies;

    const AP: Addr = [0x02, 1, 2, 3, 4, 5];
    const ME: Addr = [0x02, 9, 8, 7, 6, 5];

    #[test]
    fn auth_request_is_linuxs_30_bytes() {
        let mut b = [0u8; 64];
        let n = auth_request(&mut b, &AP, &ME).unwrap();
        assert_eq!(n, 30);
        assert_eq!(&b[..2], &[0xb0, 0]);
        assert_eq!(&b[24..30], &[0, 0, 1, 0, 0, 0]);
        let a = Auth::parse(&b[..n]).unwrap();
        assert_eq!((a.from, a.to, a.transaction), (ME, AP, 1));
    }

    #[test]
    fn assoc_request_carries_ssid_then_the_tail() {
        let mut b = [0u8; 256];
        let n = assoc_request(&mut b, &AP, &ME, b"home", &ASSOC_TAIL_2G).unwrap();
        // Same length as Linux's request for a 6-byte SSID (158) less 2.
        assert_eq!(n, 24 + 4 + 2 + 4 + 122);
        let ids: [u8; 10] = {
            let mut it = Ies(&b[28..n]);
            core::array::from_fn(|_| it.next().map_or(0xff, |e| e.id))
        };
        assert_eq!(ids, [0, 1, 50, 33, 48, 45, 127, 70, 59, 221]);
        let rsn = Ies(&b[28..n]).find(|e| e.id == 48).unwrap();
        assert_eq!(rsn.data, &RSN_IE[2..]);
        assert!(assoc_request(&mut b, &AP, &ME, &[0; 33], &ASSOC_TAIL_2G).is_none());
    }

    #[test]
    fn assoc_response_aid_drops_the_top_bits() {
        let mut f = [0u8; 40];
        f[0] = 0x10;
        f[4..10].copy_from_slice(&ME);
        f[10..16].copy_from_slice(&AP);
        f[24..26].copy_from_slice(&0x1411u16.to_le_bytes());
        f[28..30].copy_from_slice(&(0xc000u16 | 7).to_le_bytes());
        let r = AssocResp::parse(&f).unwrap();
        assert_eq!((r.from, r.status, r.aid), (AP, 0, 7));
    }

    #[test]
    fn eapol_data_frame_matches_linuxs_layout() {
        // Message 2 of the recorded join: 155 bytes = 26 + 8 + 121.
        let mut b = [0u8; 256];
        let n = data_frame(&mut b, &AP, &ME, &AP, 7, 0, None, ETHERTYPE_EAPOL, &[0; 121]).unwrap();
        assert_eq!(n, 155);
        assert_eq!(&b[..2], &[0x88, 0x01]);
        assert_eq!(&b[26..34], &[0xaa, 0xaa, 3, 0, 0, 0, 0x88, 0x8e]);
    }

    #[test]
    fn protected_frame_carries_the_ccmp_header() {
        let mut b = [0u8; 128];
        let n = data_frame(&mut b, &AP, &ME, &AP, 0, 5, Some(Ccmp { pn: 0x0102_0304_0506, keyid: 0 }), ETHERTYPE_IPV4, &[1; 10])
            .unwrap();
        assert_eq!(n, 26 + 8 + 8 + 10);
        assert_eq!(b[1] & 0x41, 0x41, "to-DS and protected");
        assert_eq!(&b[26..34], &[0x06, 0x05, 0, 0x20, 0x04, 0x03, 0x02, 0x01]);
        assert_eq!(u16::from_le_bytes([b[22], b[23]]), 5 << 4);
    }

    #[test]
    fn received_frames_parse_with_and_without_protection() {
        // From-DS QoS data in the clear, with FCS: EAPOL message 1.
        let mut f = [0u8; 160];
        f[0] = 0x88;
        f[1] = 0x02;
        f[4..10].copy_from_slice(&ME);
        f[10..16].copy_from_slice(&AP);
        f[16..22].copy_from_slice(&AP);
        f[26..32].copy_from_slice(&SNAP);
        f[32..34].copy_from_slice(&[0x88, 0x8e]);
        let d = Data::parse(&f[..34 + 99 + 4], true).unwrap();
        assert_eq!((d.ethertype, d.payload.len(), d.protected, d.sa), (ETHERTYPE_EAPOL, 99, false, AP));
        // Protected: CCMP header after the QoS header, MIC before the FCS.
        f[1] = 0x42;
        f[26..34].fill(0);
        f[34..40].copy_from_slice(&SNAP);
        f[40..42].copy_from_slice(&[0x08, 0x06]);
        let d = Data::parse(&f[..42 + 28 + 8 + 4], true).unwrap();
        assert_eq!((d.ethertype, d.payload.len(), d.protected), (ETHERTYPE_ARP, 28, true));
        // To-DS (our own frame reflected) is not ours to take.
        f[1] = 0x41;
        assert!(Data::parse(&f[..80], true).is_none());
        // QoS null.
        f[0] = 0xc8;
        f[1] = 0x02;
        assert!(Data::parse(&f[..30], true).is_none());
    }

    #[test]
    fn deauth_reason() {
        let mut f = [0u8; 26];
        f[0] = 0xc0;
        f[10..16].copy_from_slice(&AP);
        f[24] = 15;
        let g = Goodbye::parse(&f).unwrap();
        assert!(g.deauth);
        assert_eq!((g.from, g.reason), (AP, 15));
    }
}
