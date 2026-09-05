//! Which chip we are talking to.
//!
//! The family shares a register block but not its bring-up quirks, and the part
//! number is not in PCI config space — every board in this family reports the
//! same `10ec:8168`. The real identity is in read-only bits of [`TCR`], and the
//! convention for naming it is the **XID**: the hardware-revision field shifted
//! down to a three-digit number.
//!
//! # Only what has been measured is named
//!
//! [`Model::Rtl8168g`] is here because a real one was read: XID `0x4c0`, on the
//! reference board this crate was developed against. Every other XID decodes to
//! [`Model::Unknown`] carrying its raw value — not because the family has no
//! other members, but because naming a part we have never run on would be a
//! claim we cannot support, and the bring-up differences between members are
//! exactly the thing a wrong name would hide.
//!
//! Adding a model means running on one. That is the bar.

use crate::regs;

/// The hardware-revision field, shifted down: `0x4c0` for an RTL8168G.
pub type Xid = u16;

/// A chip this driver has been run against, or an unrecognised one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Model {
    /// RTL8168G / RTL8111G, XID `0x4c0`. Verified on real hardware.
    Rtl8168g,
    /// Something else in the family. The driver's default bring-up may or may
    /// not suit it; that is the caller's risk to take.
    Unknown(Xid),
}

/// XID of the one part this driver has actually run on.
pub const XID_8168G: Xid = 0x4c0;

impl Model {
    /// Decode a raw `TCR` read.
    ///
    /// Only [`regs::TCR_HWREV_MASK`] participates — the rest of `TCR` is the
    /// live transmit configuration and changes as the driver programs it, so a
    /// decode that looked at the whole word would identify the same chip
    /// differently before and after `init`.
    #[must_use]
    pub const fn from_tcr(tcr: u32) -> Self {
        Self::from_xid(((tcr & regs::TCR_HWREV_MASK) >> 20) as Xid)
    }

    /// Decode an already-extracted XID.
    #[must_use]
    pub const fn from_xid(xid: Xid) -> Self {
        match xid {
            XID_8168G => Self::Rtl8168g,
            other => Self::Unknown(other),
        }
    }

    /// The XID this model came from.
    #[must_use]
    pub const fn xid(self) -> Xid {
        match self {
            Self::Rtl8168g => XID_8168G,
            Self::Unknown(x) => x,
        }
    }

    /// Whether this driver claims to have been tested on this part.
    #[must_use]
    pub const fn is_verified(self) -> bool {
        matches!(self, Self::Rtl8168g)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact `TCR` word read off the reference chip while Linux had it
    /// running at 1 Gbps — see `tests/golden_registers.rs`.
    const REFERENCE_TCR: u32 = 0x4f00_0f80;

    #[test]
    fn reference_chip_decodes_to_8168g() {
        assert_eq!(Model::from_tcr(REFERENCE_TCR), Model::Rtl8168g);
        assert_eq!(Model::from_tcr(REFERENCE_TCR).xid(), 0x4c0);
        assert!(Model::from_tcr(REFERENCE_TCR).is_verified());
    }

    /// The live transmit configuration shares the word with the revision. A
    /// decode that let those bits leak in would misidentify the same chip
    /// before and after `init` programs `TCR` — this pins that it does not.
    #[test]
    fn transmit_config_bits_do_not_change_the_identity() {
        let configured = REFERENCE_TCR
            | regs::TCR_IFG_STANDARD
            | regs::TCR_MXDMA_UNLIMITED;
        assert_eq!(Model::from_tcr(configured), Model::from_tcr(REFERENCE_TCR));

        // ...and neither does a chip that has just come out of reset with
        // every writable bit clear.
        let bare = REFERENCE_TCR & regs::TCR_HWREV_MASK;
        assert_eq!(Model::from_tcr(bare), Model::Rtl8168g);
    }

    #[test]
    fn an_unmeasured_part_is_not_claimed() {
        let m = Model::from_xid(0x540);
        assert_eq!(m, Model::Unknown(0x540));
        assert!(!m.is_verified());
        assert_eq!(m.xid(), 0x540);
    }
}
