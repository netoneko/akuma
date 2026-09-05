//! Link state, decoded from the one-byte `PHYSTATUS` register.
//!
//! This is the only PHY fact the driver needs for normal operation, and it
//! costs one byte read — no MDIO transaction, no waiting. [`crate::mdio`] is
//! for everything else.

use crate::regs;

/// `PHYSTATUS` bit 0: the link is full duplex.
pub const PHYSTATUS_FULLDUP: u8 = 0x01;
/// `PHYSTATUS` bit 1: the link is up.
pub const PHYSTATUS_LINKUP: u8 = 0x02;
/// `PHYSTATUS` bit 2: negotiated 10 Mb/s.
pub const PHYSTATUS_10M: u8 = 0x04;
/// `PHYSTATUS` bit 3: negotiated 100 Mb/s.
pub const PHYSTATUS_100M: u8 = 0x08;
/// `PHYSTATUS` bit 4: negotiated 1000 Mb/s.
pub const PHYSTATUS_1000M: u8 = 0x10;

/// The negotiated line rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Speed {
    /// 10BASE-T.
    Mb10,
    /// 100BASE-TX.
    Mb100,
    /// 1000BASE-T.
    Mb1000,
    /// The link is up but no speed bit is set — the PHY is mid-negotiation, or
    /// this is a part whose speed bits this driver does not know.
    Unknown,
}

/// What the PHY reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LinkState {
    /// Whether carrier is present. Everything else is meaningless without it.
    pub up: bool,
    /// The negotiated rate.
    pub speed: Speed,
    /// Whether the link negotiated full duplex.
    pub full_duplex: bool,
}

impl LinkState {
    /// Decode a raw `PHYSTATUS` byte.
    ///
    /// The speed bits are **not** mutually exclusive in hardware terms — they
    /// are three independent status lines — so this reads them in descending
    /// order and takes the first that is set, which is what "negotiated rate"
    /// means when more than one is asserted during a transition.
    #[must_use]
    pub const fn from_phystatus(raw: u8) -> Self {
        let up = raw & PHYSTATUS_LINKUP != 0;
        let speed = if !up {
            Speed::Unknown
        } else if raw & PHYSTATUS_1000M != 0 {
            Speed::Mb1000
        } else if raw & PHYSTATUS_100M != 0 {
            Speed::Mb100
        } else if raw & PHYSTATUS_10M != 0 {
            Speed::Mb10
        } else {
            Speed::Unknown
        };
        Self {
            up,
            speed,
            full_duplex: raw & PHYSTATUS_FULLDUP != 0,
        }
    }

    /// A link that is up at a rate we recognise — the precondition for
    /// expecting traffic to flow.
    #[must_use]
    pub const fn is_usable(&self) -> bool {
        self.up && !matches!(self.speed, Speed::Unknown)
    }
}

/// Where the byte lives, for callers assembling their own register reads.
pub const REGISTER: u16 = regs::PHYSTATUS;

#[cfg(test)]
mod tests {
    use super::*;

    /// Read off the reference chip while `dmesg` reported
    /// "Link is Up - 1Gbps/Full". If this decode disagrees with that line, the
    /// bit assignments are wrong.
    const REFERENCE_PHYSTATUS: u8 = 0x93;

    #[test]
    fn reference_chip_reads_as_1gbps_full_duplex() {
        let l = LinkState::from_phystatus(REFERENCE_PHYSTATUS);
        assert!(l.up);
        assert_eq!(l.speed, Speed::Mb1000);
        assert!(l.full_duplex);
        assert!(l.is_usable());
    }

    #[test]
    fn cable_unplugged_reads_as_down() {
        let l = LinkState::from_phystatus(0x00);
        assert!(!l.up);
        assert_eq!(l.speed, Speed::Unknown);
        assert!(!l.is_usable());
    }

    /// A down link must not report a speed even if stale speed bits linger —
    /// otherwise a caller that checks `speed` before `up` sees a working link.
    #[test]
    fn speed_bits_without_carrier_are_not_a_link() {
        let l = LinkState::from_phystatus(PHYSTATUS_1000M | PHYSTATUS_FULLDUP);
        assert!(!l.up);
        assert_eq!(l.speed, Speed::Unknown);
        assert!(!l.is_usable());
    }

    #[test]
    fn each_rate_decodes() {
        let up = PHYSTATUS_LINKUP;
        assert_eq!(LinkState::from_phystatus(up | PHYSTATUS_10M).speed, Speed::Mb10);
        assert_eq!(LinkState::from_phystatus(up | PHYSTATUS_100M).speed, Speed::Mb100);
        assert_eq!(LinkState::from_phystatus(up | PHYSTATUS_1000M).speed, Speed::Mb1000);
        // Up, but negotiating: no rate yet, and that is not a usable link.
        assert_eq!(LinkState::from_phystatus(up).speed, Speed::Unknown);
        assert!(!LinkState::from_phystatus(up).is_usable());
    }

    /// Half duplex is a real outcome on a 10/100 link and must survive the
    /// decode, since it changes what the transmitter may do.
    #[test]
    fn half_duplex_is_reported() {
        let l = LinkState::from_phystatus(PHYSTATUS_LINKUP | PHYSTATUS_100M);
        assert!(l.up);
        assert!(!l.full_duplex);
    }
}
