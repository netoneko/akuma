//! Frame control, the 24-byte management header, information elements.

use crate::Addr;

/// Frame types (frame control bits 3:2).
pub const TYPE_MGMT: u8 = 0;
pub const TYPE_CTRL: u8 = 1;
pub const TYPE_DATA: u8 = 2;

/// Management subtypes (frame control bits 7:4).
pub mod mgmt {
    pub const ASSOC_REQ: u8 = 0;
    pub const ASSOC_RESP: u8 = 1;
    pub const PROBE_REQ: u8 = 4;
    pub const PROBE_RESP: u8 = 5;
    pub const BEACON: u8 = 8;
    pub const DISASSOC: u8 = 10;
    pub const AUTH: u8 = 11;
    pub const DEAUTH: u8 = 12;
    pub const ACTION: u8 = 13;
}

/// Information element ids.
pub mod eid {
    pub const SSID: u8 = 0;
    pub const SUPP_RATES: u8 = 1;
    pub const DS_PARAMS: u8 = 3;
    pub const RSN: u8 = 48;
    pub const EXT_RATES: u8 = 50;
    pub const HT_CAP: u8 = 45;
    pub const HT_OP: u8 = 61;
}

/// The first two bytes of every frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fc(pub u16);

impl Fc {
    #[must_use]
    pub const fn new(ty: u8, subtype: u8) -> Self {
        Self(((ty as u16) << 2) | ((subtype as u16) << 4))
    }
    #[must_use]
    pub const fn ty(self) -> u8 {
        ((self.0 >> 2) & 3) as u8
    }
    #[must_use]
    pub const fn subtype(self) -> u8 {
        ((self.0 >> 4) & 0xf) as u8
    }
    #[must_use]
    pub const fn to_ds(self) -> bool {
        self.0 & (1 << 8) != 0
    }
    #[must_use]
    pub const fn from_ds(self) -> bool {
        self.0 & (1 << 9) != 0
    }
    #[must_use]
    pub const fn protected(self) -> bool {
        self.0 & (1 << 14) != 0
    }
    #[must_use]
    pub const fn with(self, bit: u16) -> Self {
        Self(self.0 | bit)
    }
}

pub const FC_TO_DS: u16 = 1 << 8;
pub const FC_FROM_DS: u16 = 1 << 9;
pub const FC_PROTECTED: u16 = 1 << 14;

/// Bytes of a management (or non-QoS three-address data) header.
pub const HDR_LEN: usize = 24;

/// The three-address header every management frame starts with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hdr {
    pub fc: Fc,
    pub duration: u16,
    pub addr1: Addr,
    pub addr2: Addr,
    pub addr3: Addr,
    pub seq_ctrl: u16,
}

fn addr(b: &[u8]) -> Addr {
    let mut a = [0; 6];
    a.copy_from_slice(&b[..6]);
    a
}

impl Hdr {
    /// `None` for a frame shorter than a header.
    #[must_use]
    pub fn parse(f: &[u8]) -> Option<Self> {
        if f.len() < HDR_LEN {
            return None;
        }
        Some(Self {
            fc: Fc(u16::from_le_bytes([f[0], f[1]])),
            duration: u16::from_le_bytes([f[2], f[3]]),
            addr1: addr(&f[4..]),
            addr2: addr(&f[10..]),
            addr3: addr(&f[16..]),
            seq_ctrl: u16::from_le_bytes([f[22], f[23]]),
        })
    }

    /// Write the header into `out[..24]`.
    pub fn write(&self, out: &mut [u8]) {
        out[..2].copy_from_slice(&self.fc.0.to_le_bytes());
        out[2..4].copy_from_slice(&self.duration.to_le_bytes());
        out[4..10].copy_from_slice(&self.addr1);
        out[10..16].copy_from_slice(&self.addr2);
        out[16..22].copy_from_slice(&self.addr3);
        out[22..24].copy_from_slice(&self.seq_ctrl.to_le_bytes());
    }
}

/// One information element.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ie<'a> {
    pub id: u8,
    pub data: &'a [u8],
}

/// The elements of a management body, in order. Stops at the first element
/// that runs past the end (a truncated or FCS-carrying tail).
#[derive(Clone, Debug)]
pub struct Ies<'a>(pub &'a [u8]);

impl<'a> Iterator for Ies<'a> {
    type Item = Ie<'a>;
    fn next(&mut self) -> Option<Ie<'a>> {
        let b = self.0;
        if b.len() < 2 {
            return None;
        }
        let len = usize::from(b[1]);
        if b.len() < 2 + len {
            self.0 = &[];
            return None;
        }
        self.0 = &b[2 + len..];
        Some(Ie { id: b[0], data: &b[2..2 + len] })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fc_decodes_a_beacon_and_a_protected_data_frame() {
        let b = Fc(0x0080);
        assert_eq!((b.ty(), b.subtype()), (TYPE_MGMT, mgmt::BEACON));
        let d = Fc(0x4108);
        assert_eq!(d.ty(), TYPE_DATA);
        assert!(d.to_ds() && d.protected() && !d.from_ds());
        assert_eq!(Fc::new(TYPE_MGMT, mgmt::AUTH).0, 0x00b0);
    }

    #[test]
    fn header_round_trips() {
        let h = Hdr {
            fc: Fc::new(TYPE_MGMT, mgmt::PROBE_REQ),
            duration: 0,
            addr1: [0xff; 6],
            addr2: [2, 0, 0, 0, 0, 1],
            addr3: [0xff; 6],
            seq_ctrl: 0x10,
        };
        let mut b = [0u8; HDR_LEN];
        h.write(&mut b);
        assert_eq!(Hdr::parse(&b), Some(h));
        assert_eq!(Hdr::parse(&b[..23]), None);
    }

    #[test]
    fn ies_stop_at_a_truncated_element() {
        let body = [0, 3, b'a', b'b', b'c', 3, 1, 6, 48, 20, 1];
        let v: [Option<Ie>; 3] = {
            let mut it = Ies(&body);
            [it.next(), it.next(), it.next()]
        };
        assert_eq!(v[0], Some(Ie { id: 0, data: b"abc" }));
        assert_eq!(v[1], Some(Ie { id: 3, data: &[6] }));
        assert_eq!(v[2], None);
    }
}
