//! A battery that does not exist, for machines that have none.
//!
//! QEMU has no battery device and the trashcan is a desktop, so the whole path
//! from a `/proc` read through the renderer needs something that *produces an EC
//! block*. This produces one — raw bytes in the real layout, not a decoded
//! struct — so the simulator exercises [`crate::ec::Reading::decode`] exactly as
//! hardware does. Selected by `powersim` on the kernel command line, the way
//! `wifisim` selects the wifi simulator.
//!
//! Deterministic in `seconds` so a test can predict it: a 120 s cycle that
//! discharges 100 % → 40 % over the first 60 s on battery, then charges back on
//! AC over the next 60 s.

use crate::ec::BLOCK_LEN;

/// Seconds per half-cycle.
const HALF: u64 = 60;
const FULL_10MWH: u16 = 5442;
const DESIGN_10MWH: u16 = 5700;

/// The simulated window at `seconds` of uptime.
#[must_use]
pub fn block(seconds: u64) -> [u8; BLOCK_LEN] {
    let phase = seconds % (2 * HALF);
    let discharging = phase < HALF;
    // Percent: 100 down to 40 while discharging, 40 back up to 100 charging.
    let step = (phase % HALF) as u16; // 0..59
    let pct: u8 = if discharging { 100 - step as u8 } else { 40 + step as u8 };
    let remaining = (u32::from(FULL_10MWH) * u32::from(pct) / 100) as u16;
    // ACIN | BTIN | BTST (1 = discharging, 2 = charging) in bits 2-5.
    let status: u8 = if discharging { 0b10 | (1 << 2) } else { 0b11 | (2 << 2) };
    let mut b = [0u8; BLOCK_LEN];
    b[0x00] = status;
    b[0x04..0x06].copy_from_slice(&DESIGN_10MWH.to_le_bytes());
    b[0x06..0x08].copy_from_slice(&11_310u16.to_le_bytes());
    b[0x08..0x0A].copy_from_slice(&FULL_10MWH.to_le_bytes());
    b[0x0C..0x0E].copy_from_slice(&if discharging { 540u16 } else { 2400 }.to_le_bytes());
    b[0x0E..0x10].copy_from_slice(&remaining.to_le_bytes());
    b[0x10..0x12].copy_from_slice(&12_900u16.to_le_bytes());
    b[0x12] = pct;
    b
}
