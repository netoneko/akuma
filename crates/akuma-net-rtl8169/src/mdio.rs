//! Talking to the PHY through the one-word `PHYAR` window.
//!
//! The chip has no MDIO pins the driver can drive directly. Instead `PHYAR`
//! packs a register number, a data halfword and a busy flag into one 32-bit
//! word: write the request, then poll the same address until the chip says it
//! is done.
//!
//! # The busy bit means opposite things in the two directions
//!
//! This is the whole reason this module exists, and it is the trap:
//!
//! * **Write**: set [`BUSY`] in the request. The chip **clears** it when the
//!   write has gone out. Poll for clear.
//! * **Read**: leave [`BUSY`] clear in the request. The chip **sets** it when
//!   the result is ready. Poll for set.
//!
//! So the same bit is "operation in progress" on one path and "result
//! available" on the other. A driver that polls for the same edge in both cases
//! works in exactly one direction, and the other direction returns whatever was
//! in the register — usually zero, which reads as a plausible PHY value. There
//! is no error, which is what makes it expensive to find.

/// Set by the requester on a write; set by the chip on a completed read.
pub const BUSY: u32 = 0x8000_0000;
/// Register number field, bits 16..21.
pub const REG_SHIFT: u32 = 16;
/// Register numbers are five bits — the MDIO address space is 32 registers.
pub const REG_MASK: u32 = 0x001F_0000;
/// The data halfword, in the low bits.
pub const DATA_MASK: u32 = 0x0000_FFFF;

/// The word to write to `PHYAR` to start a read of `reg`.
///
/// [`BUSY`] is deliberately absent: on this path the chip sets it to announce
/// the answer, so a request that already had it set is indistinguishable from a
/// completed one and the first poll returns garbage immediately.
#[must_use]
pub const fn read_request(reg: u8) -> u32 {
    ((reg as u32) << REG_SHIFT) & REG_MASK
}

/// Interpret a `PHYAR` read while a read request is outstanding.
///
/// `None` means not finished yet.
#[must_use]
pub const fn read_result(raw: u32) -> Option<u16> {
    if raw & BUSY == 0 {
        None
    } else {
        Some((raw & DATA_MASK) as u16)
    }
}

/// The word to write to `PHYAR` to start a write of `data` to `reg`.
#[must_use]
pub const fn write_request(reg: u8, data: u16) -> u32 {
    BUSY | (((reg as u32) << REG_SHIFT) & REG_MASK) | (data as u32 & DATA_MASK)
}

/// Whether a `PHYAR` read shows an outstanding write has completed.
#[must_use]
pub const fn write_done(raw: u32) -> bool {
    raw & BUSY == 0
}

/// Standard MDIO register 0: basic mode control.
pub const REG_BMCR: u8 = 0x00;
/// Standard MDIO register 1: basic mode status.
pub const REG_BMSR: u8 = 0x01;
/// `BMCR` bit: restart autonegotiation.
pub const BMCR_ANEG_RESTART: u16 = 0x0200;
/// `BMCR` bit: enable autonegotiation.
pub const BMCR_ANEG_ENABLE: u16 = 0x1000;
/// `BMSR` bit: link is up. Latching-low — it reports a link that has dropped
/// since the previous read, so two reads are needed to see the current state.
pub const BMSR_LINK: u16 = 0x0004;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_read_request_does_not_set_busy() {
        let req = read_request(REG_BMSR);
        assert_eq!(req & BUSY, 0, "the chip sets BUSY to answer a read");
        assert_eq!(req >> REG_SHIFT, u32::from(REG_BMSR));
    }

    #[test]
    fn a_write_request_sets_busy_and_carries_the_data() {
        let req = write_request(REG_BMCR, BMCR_ANEG_ENABLE | BMCR_ANEG_RESTART);
        assert_ne!(req & BUSY, 0);
        assert_eq!(req & DATA_MASK, u32::from(BMCR_ANEG_ENABLE | BMCR_ANEG_RESTART));
        assert_eq!((req & REG_MASK) >> REG_SHIFT, u32::from(REG_BMCR));
    }

    /// The asymmetry, pinned in both directions at once. If either polarity is
    /// ever "tidied" to match the other, one of these two fails.
    #[test]
    fn the_busy_bit_means_opposite_things_in_the_two_directions() {
        // A read is finished when BUSY appears...
        assert_eq!(read_result(read_request(REG_BMSR)), None);
        assert_eq!(read_result(BUSY | 0x1234), Some(0x1234));

        // ...a write is finished when BUSY disappears.
        assert!(!write_done(write_request(REG_BMCR, 0x1234)));
        assert!(write_done(0x0000_1234));
    }

    /// Register numbers are five bits. A caller passing 32 must not have it
    /// silently land on register 0 of the next field over — the mask keeps the
    /// damage inside the register field either way.
    #[test]
    fn register_numbers_stay_inside_their_field() {
        for reg in 0u8..=255 {
            let req = read_request(reg);
            assert_eq!(req & !REG_MASK, 0, "reg {reg} escaped its field");
        }
        let wr = write_request(0xFF, 0xFFFF);
        assert_eq!(wr & !(REG_MASK | DATA_MASK | BUSY), 0);
    }

    /// A zero result is a legal PHY value, not a "not ready" — it must only be
    /// reported once the chip has actually flagged completion.
    #[test]
    fn zero_is_a_legal_result_but_only_once_flagged() {
        assert_eq!(read_result(0), None);
        assert_eq!(read_result(BUSY), Some(0));
    }
}
