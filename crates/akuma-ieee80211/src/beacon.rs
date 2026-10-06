//! What a beacon or probe response advertises.

use crate::Addr;
use crate::frame::{HDR_LEN, Hdr, Ies, TYPE_MGMT, eid, mgmt};

/// The RSN cipher/AKM suite selectors this station understands (00-0F-AC-n).
pub const SUITE_CCMP: u32 = 0x000f_ac04;
pub const SUITE_TKIP: u32 = 0x000f_ac02;
pub const AKM_PSK: u32 = 0x000f_ac02;
pub const AKM_SAE: u32 = 0x000f_ac08;
pub const AKM_PSK_SHA256: u32 = 0x000f_ac06;

/// What the RSN element says, as far as joining with WPA2-PSK goes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rsn {
    pub group: u32,
    /// Pairwise CCMP offered.
    pub pairwise_ccmp: bool,
    /// AKM PSK (SHA-1) offered.
    pub akm_psk: bool,
    /// AKM SAE (WPA3) offered.
    pub akm_sae: bool,
    /// RSN capabilities; bit 6 = MFP required, bit 7 = MFP capable.
    pub caps: u16,
}

impl Rsn {
    /// Parse an RSN element body (IEEE 802.11-2020 9.4.2.24).
    #[must_use]
    pub fn parse(d: &[u8]) -> Option<Self> {
        let suite = |b: &[u8]| u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
        if d.len() < 8 || u16::from_le_bytes([d[0], d[1]]) != 1 {
            return None;
        }
        let mut r = Self { group: suite(&d[2..6]), ..Self::default() };
        let mut at = 6;
        let n = usize::from(u16::from_le_bytes([*d.get(at)?, *d.get(at + 1)?]));
        at += 2;
        for _ in 0..n {
            if suite(d.get(at..at + 4)?) == SUITE_CCMP {
                r.pairwise_ccmp = true;
            }
            at += 4;
        }
        let n = usize::from(u16::from_le_bytes([*d.get(at)?, *d.get(at + 1)?]));
        at += 2;
        for _ in 0..n {
            match suite(d.get(at..at + 4)?) {
                AKM_PSK => r.akm_psk = true,
                AKM_SAE => r.akm_sae = true,
                _ => {}
            }
            at += 4;
        }
        if let Some(c) = d.get(at..at + 2) {
            r.caps = u16::from_le_bytes([c[0], c[1]]);
        }
        Some(r)
    }
}

/// A beacon or probe response, taken apart.
#[derive(Clone, Copy, Debug)]
pub struct Bss<'a> {
    pub bssid: Addr,
    pub ssid: &'a [u8],
    /// From the DS Parameter Set element; `None` if absent (5 GHz beacons
    /// may omit it).
    pub channel: Option<u8>,
    pub interval: u16,
    pub capability: u16,
    pub rsn: Option<Rsn>,
    /// The raw elements, for whoever needs more (rates, HT).
    pub ies: &'a [u8],
}

impl Bss<'_> {
    /// Capability bit 4: the network requires privacy (WEP/WPA/RSN).
    #[must_use]
    pub const fn privacy(&self) -> bool {
        self.capability & (1 << 4) != 0
    }
}

/// Bytes after the header before the elements: timestamp, interval, capability.
const FIXED: usize = 12;

/// Parse an 802.11 frame (`fcs` = its last 4 bytes are the FCS) as a beacon
/// or probe response; `None` if it is neither or is malformed.
#[must_use]
pub fn parse(frame: &[u8], fcs: bool) -> Option<Bss<'_>> {
    let f = if fcs { frame.get(..frame.len().checked_sub(4)?)? } else { frame };
    let h = Hdr::parse(f)?;
    if h.fc.ty() != TYPE_MGMT || !matches!(h.fc.subtype(), mgmt::BEACON | mgmt::PROBE_RESP) {
        return None;
    }
    let body = f.get(HDR_LEN..)?;
    if body.len() < FIXED {
        return None;
    }
    let ies = &body[FIXED..];
    let mut b = Bss {
        bssid: h.addr3,
        ssid: &[],
        channel: None,
        interval: u16::from_le_bytes([body[8], body[9]]),
        capability: u16::from_le_bytes([body[10], body[11]]),
        rsn: None,
        ies,
    };
    for ie in Ies(ies) {
        match ie.id {
            eid::SSID if ie.data.len() <= 32 => b.ssid = ie.data,
            eid::DS_PARAMS if ie.data.len() == 1 => b.channel = Some(ie.data[0]),
            eid::RSN => b.rsn = Rsn::parse(ie.data),
            _ => {}
        }
    }
    Some(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A beacon as an AP on channel 1 sends it: SSID "akuma-test", rates,
    /// DS = 1, RSN WPA2-PSK CCMP; then a fake FCS.
    fn beacon() -> [u8; 24 + 12 + 12 + 10 + 3 + 22 + 4] {
        let mut f = [0u8; 24 + 12 + 12 + 10 + 3 + 22 + 4];
        f[0] = 0x80;
        f[4..10].copy_from_slice(&[0xff; 6]);
        f[10..16].copy_from_slice(&[2, 0, 0, 0, 0, 9]);
        f[16..22].copy_from_slice(&[2, 0, 0, 0, 0, 9]);
        let b = &mut f[24..];
        b[8] = 100; // interval 100 TU
        b[10] = 0x11; // ESS + privacy
        let mut at = 12;
        for ie in [
            &[0u8, 10, b'a', b'k', b'u', b'm', b'a', b'-', b't', b'e', b's', b't'][..],
            &[1, 8, 0x82, 0x84, 0x8b, 0x96, 0x0c, 0x12, 0x18, 0x24],
            &[3, 1, 1],
            &[48, 20, 1, 0, 0, 0x0f, 0xac, 4, 1, 0, 0, 0x0f, 0xac, 4, 1, 0, 0, 0x0f, 0xac, 2, 0, 0],
        ] {
            b[at..at + ie.len()].copy_from_slice(ie);
            at += ie.len();
        }
        f
    }

    #[test]
    fn parses_a_wpa2_beacon() {
        let f = beacon();
        let b = parse(&f, true).unwrap();
        assert_eq!(b.ssid, b"akuma-test");
        assert_eq!(b.channel, Some(1));
        assert_eq!(b.bssid, [2, 0, 0, 0, 0, 9]);
        assert_eq!(b.interval, 100);
        assert!(b.privacy());
        let r = b.rsn.unwrap();
        assert_eq!(r.group, SUITE_CCMP);
        assert!(r.pairwise_ccmp && r.akm_psk && !r.akm_sae);
    }

    #[test]
    fn ignores_other_frames() {
        let mut f = beacon();
        f[0] = 0x40; // probe request
        assert!(parse(&f, true).is_none());
        assert!(parse(&f[..30], true).is_none());
    }
}
