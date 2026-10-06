//! Pure bring-up logic for ryzen's wifi card, the Realtek RTL8852CE
//! (`10ec:c852`, Linux's `rtw89_8852ce`) — everything the driver needs that is
//! not an MMIO access or a DMA buffer.
//!
//! # Why this exists
//!
//! Stage W1 of the wifi plan (`docs/archive/AKUMA_AMD64_ON_RYZEN_LAPTOP.md`
//! § 5): power the card on, push its firmware, and see the firmware report
//! itself ready. The split is the one `akuma-nvme` and `akuma-xhci` made: the
//! `unsafe` register file and DMA memory stay in `amd64/src/rtw89.rs`, and the
//! sequence itself lives here, under `forbid(unsafe_code)`, behind the [`Bus`]
//! trait, with host tests.
//!
//! | module | decides |
//! |---|---|
//! | [`regs`] | register offsets and bits, named as Linux names them |
//! | [`fw`] | the multi-firmware container, the firmware header, the packet plan |
//! | [`h2c`] | the firmware-command ring: buffer descriptors, the packet descriptor, the H2C header |
//! | [`bringup`] | power-on, the DMA engine's download-mode setup, the CPU, the download itself |
//!
//! # Where the sequence comes from
//!
//! Two sources, checked against each other. Linux's `rtw89` driver (v6.17,
//! `drivers/net/wireless/realtek/rtw89/`, dual-licensed `GPL-2.0 OR
//! BSD-3-Clause`; this crate uses it under the BSD-3-Clause terms, notice
//! below) says what each step means. A register trace of that driver bringing
//! up **this** card on ryzen (`overlays/ryzen/w0-trace.sh`, stage W0) says
//! what it actually wrote, read and polled, in order. `tests/golden/` holds the
//! part of that trace from power-on to the firmware CPU starting, and
//! `tests/golden_trace.rs` replays [`bringup`] against it: every write must
//! match the trace's register, width and value, except the ring base addresses,
//! which are DMA addresses and differ by construction. The firmware download
//! itself, whose payload travels by DMA where no trace can see it, is checked
//! against a simulated chip that reassembles what it was sent.
//!
//! Only the 8852C path is here. Linux's driver serves a dozen chips through
//! per-chip tables and branches; each function in [`bringup`] is the 8852C arm
//! of its Linux namesake, with the dead arms left out.
//!
//! # Linux's notice (BSD-3-Clause)
//!
//! Copyright(c) 2019-2022 Realtek Corporation. Redistribution and use in source
//! and binary forms, with or without modification, are permitted provided that
//! the following conditions are met: (1) redistributions of source code must
//! retain the above copyright notice, this list of conditions and the following
//! disclaimer; (2) redistributions in binary form must reproduce the above
//! copyright notice, this list of conditions and the following disclaimer in
//! the documentation and/or other materials provided with the distribution;
//! (3) neither the name of the copyright holder nor the names of its
//! contributors may be used to endorse or promote products derived from this
//! software without specific prior written permission. THIS SOFTWARE IS
//! PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS" AND ANY EXPRESS
//! OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE IMPLIED WARRANTIES
//! OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE DISCLAIMED. IN
//! NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE LIABLE FOR ANY
//! DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES
//! ARISING IN ANY WAY OUT OF THE USE OF THIS SOFTWARE, EVEN IF ADVISED OF THE
//! POSSIBILITY OF SUCH DAMAGE.

#![no_std]
#![forbid(unsafe_code)]

pub mod bringup;
pub mod fw;
pub mod h2c;
pub mod regs;

/// The card's register file, as the driver sees it: byte, half-word and word
/// accesses at offsets into BAR2, plus a way to wait.
///
/// The glue implements it over the mapped BAR with volatile accesses; the
/// tests implement it over a recorded trace or a simulated chip. Every access
/// the bring-up makes goes through here, which is what lets a test see all of
/// them.
pub trait Bus {
    fn read8(&mut self, off: u32) -> u8;
    fn read16(&mut self, off: u32) -> u16;
    fn read32(&mut self, off: u32) -> u32;
    fn write8(&mut self, off: u32, v: u8);
    fn write16(&mut self, off: u32, v: u16);
    fn write32(&mut self, off: u32, v: u32);
    /// Wait at least `us` microseconds.
    fn delay_us(&mut self, us: u32);
}

/// Linux's `rtw89_write32_set`: read, OR, write — always both accesses, even
/// when the bits are already set, because that is what the trace shows.
pub fn set32(bus: &mut impl Bus, off: u32, bits: u32) {
    let v = bus.read32(off);
    bus.write32(off, v | bits);
}

/// Linux's `rtw89_write32_clr`.
pub fn clr32(bus: &mut impl Bus, off: u32, bits: u32) {
    let v = bus.read32(off);
    bus.write32(off, v & !bits);
}

/// Linux's `rtw89_write32_mask`: replace the field `mask` with `val` (given
/// unshifted, as a field value).
pub fn mask32(bus: &mut impl Bus, off: u32, mask: u32, val: u32) {
    let v = bus.read32(off);
    bus.write32(off, (v & !mask) | ((val << mask.trailing_zeros()) & mask));
}

/// Linux's `rtw89_write8_set`.
pub fn set8(bus: &mut impl Bus, off: u32, bits: u8) {
    let v = bus.read8(off);
    bus.write8(off, v | bits);
}

/// Linux's `rtw89_write8_clr`.
pub fn clr8(bus: &mut impl Bus, off: u32, bits: u8) {
    let v = bus.read8(off);
    bus.write8(off, v & !bits);
}

/// Linux's `rtw89_write16_clr`.
pub fn clr16(bus: &mut impl Bus, off: u32, bits: u16) {
    let v = bus.read16(off);
    bus.write16(off, v & !bits);
}

/// Linux's `rtw89_write16_mask`.
pub fn mask16(bus: &mut impl Bus, off: u32, mask: u16, val: u16) {
    let v = bus.read16(off);
    bus.write16(off, (v & !mask) | ((val << mask.trailing_zeros()) & mask));
}

/// Linux's `read_poll_timeout`: read, test, wait `step_us`, again.
///
/// Gives up once `budget_us` has been waited. Returns the last value
/// read either way, so a caller can report what the register said.
///
/// # Errors
///
/// `Err(last value)` when the budget ran out with the condition still false.
pub fn poll32(
    bus: &mut impl Bus,
    off: u32,
    step_us: u32,
    budget_us: u32,
    mut done: impl FnMut(u32) -> bool,
) -> Result<u32, u32> {
    let mut waited = 0;
    loop {
        let v = bus.read32(off);
        if done(v) {
            return Ok(v);
        }
        if waited >= budget_us {
            return Err(v);
        }
        bus.delay_us(step_us);
        waited += step_us.max(1);
    }
}

/// [`poll32`] on a byte register.
///
/// # Errors
///
/// As [`poll32`].
pub fn poll8(
    bus: &mut impl Bus,
    off: u32,
    step_us: u32,
    budget_us: u32,
    mut done: impl FnMut(u8) -> bool,
) -> Result<u8, u8> {
    let mut waited = 0;
    loop {
        let v = bus.read8(off);
        if done(v) {
            return Ok(v);
        }
        if waited >= budget_us {
            return Err(v);
        }
        bus.delay_us(step_us);
        waited += step_us.max(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sixteen words of register file that record nothing — enough to check
    /// the read-modify-write helpers' arithmetic.
    struct Regs([u32; 16]);

    impl Bus for Regs {
        fn read8(&mut self, off: u32) -> u8 {
            (self.0[(off / 4) as usize] >> ((off % 4) * 8)) as u8
        }
        fn read16(&mut self, off: u32) -> u16 {
            (self.0[(off / 4) as usize] >> ((off % 4) * 8)) as u16
        }
        fn read32(&mut self, off: u32) -> u32 {
            self.0[(off / 4) as usize]
        }
        fn write8(&mut self, off: u32, v: u8) {
            let w = &mut self.0[(off / 4) as usize];
            let sh = (off % 4) * 8;
            *w = (*w & !(0xff << sh)) | (u32::from(v) << sh);
        }
        fn write16(&mut self, off: u32, v: u16) {
            let w = &mut self.0[(off / 4) as usize];
            let sh = (off % 4) * 8;
            *w = (*w & !(0xffff << sh)) | (u32::from(v) << sh);
        }
        fn write32(&mut self, off: u32, v: u32) {
            self.0[(off / 4) as usize] = v;
        }
        fn delay_us(&mut self, _: u32) {}
    }

    #[test]
    fn mask32_replaces_only_the_field() {
        let mut r = Regs([0; 16]);
        r.0[1] = 0xffff_ffff;
        mask32(&mut r, 4, 0x0000_e000, 0x7);
        assert_eq!(r.0[1], 0xffff_ffff);
        mask32(&mut r, 4, 0x0000_e000, 0x2);
        assert_eq!(r.0[1], 0xffff_5fff);
    }

    #[test]
    fn mask16_replaces_only_the_field() {
        let mut r = Regs([0; 16]);
        r.0[0] = 0xabcd_0000;
        // The half-word at offset 2 is 0xabcd; its low three bits become 0b010.
        mask16(&mut r, 2, 0x0007, 0x2);
        assert_eq!(r.0[0], 0xabca_0000);
    }

    #[test]
    fn poll_gives_up_with_the_last_value() {
        let mut r = Regs([0; 16]);
        r.0[2] = 0x10;
        assert_eq!(poll32(&mut r, 8, 10, 100, |v| v & 1 != 0), Err(0x10));
        assert_eq!(poll32(&mut r, 8, 10, 100, |v| v & 0x10 != 0), Ok(0x10));
        assert_eq!(poll8(&mut r, 8, 1, 0, |v| v == 0x10), Ok(0x10));
    }
}
