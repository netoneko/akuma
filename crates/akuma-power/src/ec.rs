//! The battery block of the embedded controller's memory window.
//!
//! # Layout — measured, one machine
//!
//! Lenovo IdeaPad 5 2-in-1 16AHP9. The offsets are relative to the start of the
//! `ERAM` region the DSDT declares (`0xFEEC2380` there), and were checked on
//! 2026-10-07 by reading the window from Linux through `/dev/mem` next to
//! `BAT0`'s uevent, on AC and on battery (`overlays/ryzen/ec-sample.sh`,
//! `docs/archive/AKUMA_AMD64_ON_RYZEN_LAPTOP.md` §5.8): voltage, remaining,
//! full, design and RSOC matched exactly, and `current × voltage / 1000`
//! equalled `POWER_NOW` to the milliwatt.
//!
//! The multi-byte offsets the ASL appears to give are one byte too high; these
//! are the measured ones.
//!
//! ```text
//!   +0x00 u8   status: bit0 ACIN, bit1 BTIN, bits2-5 BTST
//!   +0x04 u16  design capacity   (10 mWh)
//!   +0x06 u16  design voltage    (mV)
//!   +0x08 u16  last full capacity(10 mWh)
//!   +0x0C u16  current           (mA, magnitude — sign comes from BTST)
//!   +0x0E u16  remaining capacity(10 mWh)
//!   +0x10 u16  voltage           (mV)
//!   +0x12 u8   relative state of charge (%)
//! ```
//!
//! BTST `1` = discharging was observed. `2` = charging is **inferred** from the
//! ACPI `_BST` bit convention and not yet observed on this part, and the codes
//! for full and for no battery are open (`docs/archive/AKUMA_ACPI_POWER.md`
//! §Open). Anything else decodes as [`State::Unknown`] carrying the raw code,
//! never a guess.

/// Bytes of the window this reads.
pub const BLOCK_LEN: usize = 0x14;

const OFF_STATUS: usize = 0x00;
const OFF_DESIGN_CAP: usize = 0x04;
const OFF_DESIGN_MV: usize = 0x06;
const OFF_FULL_CAP: usize = 0x08;
const OFF_CURRENT: usize = 0x0C;
const OFF_REMAINING: usize = 0x0E;
const OFF_VOLTAGE: usize = 0x10;
const OFF_RSOC: usize = 0x12;

/// What the pack is doing.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum State {
    Discharging,
    /// Inferred from BTST = 2; see the module docs.
    Charging,
    /// On AC and BTST = 0: Linux's own word for it.
    NotCharging,
    /// A BTST code this decode has not seen; the raw value is in
    /// [`Reading::btst`].
    Unknown,
}

impl State {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Discharging => "Discharging",
            Self::Charging => "Charging",
            Self::NotCharging => "Not charging",
            Self::Unknown => "Unknown",
        }
    }
}

/// One decoded sample.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct Reading {
    pub ac: bool,
    pub battery_present: bool,
    pub btst: u8,
    pub state: State,
    pub design_mwh: u32,
    pub full_mwh: u32,
    pub remaining_mwh: u32,
    pub design_mv: u16,
    pub voltage_mv: u16,
    pub current_ma: u16,
    pub rsoc: u8,
}

fn u16_at(b: &[u8; BLOCK_LEN], off: usize) -> u16 {
    u16::from_le_bytes([b[off], b[off + 1]])
}

impl Reading {
    /// Decode a window sample. Never fails: a block of garbage decodes to a
    /// [`Reading`] that [`validate`](Self::validate) then refuses.
    #[must_use]
    pub fn decode(b: &[u8; BLOCK_LEN]) -> Self {
        let status = b[OFF_STATUS];
        let ac = status & 1 != 0;
        let battery_present = status & 2 != 0;
        let btst = (status >> 2) & 0xF;
        let state = match btst {
            1 => State::Discharging,
            2 => State::Charging,
            0 if ac => State::NotCharging,
            _ => State::Unknown,
        };
        Self {
            ac,
            battery_present,
            btst,
            state,
            design_mwh: u32::from(u16_at(b, OFF_DESIGN_CAP)) * 10,
            full_mwh: u32::from(u16_at(b, OFF_FULL_CAP)) * 10,
            remaining_mwh: u32::from(u16_at(b, OFF_REMAINING)) * 10,
            design_mv: u16_at(b, OFF_DESIGN_MV),
            voltage_mv: u16_at(b, OFF_VOLTAGE),
            current_ma: u16_at(b, OFF_CURRENT),
            rsoc: b[OFF_RSOC],
        }
    }

    /// Is this a believable battery? A window read from the wrong address — or
    /// one the EC has not filled yet — is all `0x00` or all `0xFF`, and either
    /// must not be shown as "100 %".
    #[must_use]
    pub fn validate(&self) -> bool {
        self.battery_present
            && (3_000..=30_000).contains(&self.voltage_mv)
            && self.rsoc <= 100
            && self.full_mwh > 0
            && self.remaining_mwh <= self.full_mwh + self.full_mwh / 10
            && self.design_mwh >= self.full_mwh / 2
    }

    /// Instantaneous power in mW: `mA × mV / 1000`.
    #[must_use]
    pub fn power_mw(&self) -> u32 {
        (u32::from(self.current_ma) * u32::from(self.voltage_mv)) / 1000
    }

    /// Percent charge. The EC's own RSOC when it is sane, else remaining/full.
    #[must_use]
    pub fn percent(&self) -> u8 {
        if self.rsoc <= 100 {
            return self.rsoc;
        }
        if self.full_mwh == 0 {
            return 0;
        }
        (u64::from(self.remaining_mwh) * 100 / u64::from(self.full_mwh)).min(100) as u8
    }

    /// Minutes until empty (discharging) or full (charging); `None` when the
    /// pack is neither or the rate is too small to divide by.
    #[must_use]
    pub fn minutes(&self) -> Option<u32> {
        let p = self.power_mw();
        if p < 100 {
            return None;
        }
        let energy = match self.state {
            State::Discharging => self.remaining_mwh,
            State::Charging => self.full_mwh.saturating_sub(self.remaining_mwh),
            _ => return None,
        };
        Some(((u64::from(energy) * 60) / u64::from(p)).min(u64::from(u32::MAX)) as u32)
    }
}
