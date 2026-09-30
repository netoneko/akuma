#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]
//! Intel HD Audio controller — the pure register half.
//!
//! Split the same way `akuma-pci` splits from the target's `pci.rs`: this
//! crate decodes registers and knows nothing about how bytes reach it; the
//! target's `hda.rs` owns the PCI walk, the BAR0 map and the console line.
//! Host tests cover the decode, because the only machine with the controller
//! is the one the driver boots on, and a reboot is an expensive unit test.
//!
//! M1 scope (runbook `add-intel-hda-audio.md`, bring-up step 1): GCAP and the
//! version registers, nothing else. Reset (CRST), codecs and streams join in
//! later steps, each behind the same `Regs16` seam.

pub mod codec;
pub mod stream;
pub mod verb;

/// The byte offset of a register, from the HDA specification's controller
/// register map (Intel HDA 1.0a §3.3). BAR0 points at the start of it.
pub mod reg {
    /// Global capabilities — stream counts, serial data lines, 64-bit support.
    pub const GCAP: usize = 0x00;
    /// Minor version (8-bit). With [`VMAJ`] this is the "is the BAR mapped"
    /// gate: unmapped MMIO reads as `0xff`.
    pub const VMIN: usize = 0x02;
    /// Major version (8-bit).
    pub const VMAJ: usize = 0x03;
    /// GCTL - the reset-and-status word; CRST (bit 0) is the
    /// controller reset; software clears it to enter reset and sets it to exit (§4.3).
    pub const GCTL: usize = 0x08;
    /// STATESTS — one bit per codec address that answered the link wake-up
    /// (write-1-to-clear).
    pub const STATESTS: usize = 0x0E;
    /// INTCTL — global interrupt enable; left zero (this driver polls).
    pub const INTCTL: usize = 0x20;

    /// CORB lower base address (32-bit, 128-byte aligned).
    pub const CORBLBASE: usize = 0x40;
    /// CORB upper base address (32-bit).
    pub const CORBUBASE: usize = 0x44;
    /// CORB write pointer (16-bit, entry index in `[7:0]`).
    pub const CORBWP: usize = 0x48;
    /// CORB read pointer (16-bit; bit 15 is the reset strobe).
    pub const CORBRP: usize = 0x4A;
    /// CORB control (8-bit; bit 1 = RUN).
    pub const CORBCTL: usize = 0x4C;
    /// CORB status (8-bit).
    pub const CORBSTS: usize = 0x4D;
    /// CORB size (8-bit; `[1:0]` = 0/1/2 for 2/16/256 entries, `[7:4]` = caps).
    pub const CORBSIZE: usize = 0x4E;
    /// RIRB lower base address (32-bit, 128-byte aligned).
    pub const RIRBLBASE: usize = 0x50;
    /// RIRB upper base address (32-bit).
    pub const RIRBUBASE: usize = 0x54;
    /// RIRB write pointer (16-bit; bit 15 is the reset strobe).
    pub const RIRBWP: usize = 0x58;
    /// Response interrupt count (16-bit; 0 means 256).
    pub const RINTCNT: usize = 0x5A;
    /// RIRB control (8-bit; bit 0 = response IRQ enable, bit 1 = DMA enable).
    pub const RIRBCTL: usize = 0x5C;
    /// RIRB status (8-bit; bit 0 = RINTFL, bit 2 = RIRBOIS; write 1 to clear).
    pub const RIRBSTS: usize = 0x5D;
    /// RIRB size (8-bit).
    pub const RIRBSIZE: usize = 0x5E;
    /// Immediate command output (32-bit).
    pub const ICW: usize = 0x60;
    /// Immediate response input (32-bit).
    pub const IRR: usize = 0x64;
    /// Immediate command status (16-bit; bit 0 = ICB busy, bit 1 = IRV valid).
    pub const IRS: usize = 0x68;
    /// DMA position buffer lower base (32-bit, 128-byte aligned; bit 0 enables).
    pub const DPLBASE: usize = 0x70;
    /// DMA position buffer upper base (32-bit).
    pub const DPUBASE: usize = 0x74;

    /// First stream descriptor. Input descriptors come first (`GCAP.ISS` of
    /// them), then output; each is [`SD_STRIDE`] bytes.
    pub const SD_BASE: usize = 0x80;
    /// Bytes between stream descriptors.
    pub const SD_STRIDE: usize = 0x20;
    /// Stream descriptor: control, 3 bytes (`[1]` RUN, `[0]` SRST, `[23:20]` tag).
    pub const SD_CTL: usize = 0x00;
    /// Stream descriptor: status, 1 byte at +3 (`[2]` BCIS, `[3]` FIFOE, `[4]` DESE).
    pub const SD_STS: usize = 0x03;
    /// Stream descriptor: link position in buffer, 32-bit.
    pub const SD_LPIB: usize = 0x04;
    /// Stream descriptor: cyclic buffer length in bytes, 32-bit.
    pub const SD_CBL: usize = 0x08;
    /// Stream descriptor: last valid BDL index, 16-bit.
    pub const SD_LVI: usize = 0x0C;
    /// Stream descriptor: FIFO watermark, 16-bit.
    pub const SD_FIFOW: usize = 0x0E;
    /// Stream descriptor: FIFO size, 16-bit, read-only.
    pub const SD_FIFOS: usize = 0x10;
    /// Stream descriptor: stream format, 16-bit.
    pub const SD_FMT: usize = 0x12;
    /// Stream descriptor: BDL base address, lower 32 bits (128-byte aligned).
    pub const SD_BDPL: usize = 0x18;
    /// Stream descriptor: BDL base address, upper 32 bits.
    pub const SD_BDPU: usize = 0x1C;

    /// Byte offset of output stream descriptor `n`, given `GCAP.ISS`.
    #[must_use]
    pub const fn output_sd(iss: u8, n: u8) -> usize {
        SD_BASE + SD_STRIDE * (iss as usize + n as usize)
    }
}

/// How the pure half reads a 16-bit register. Implemented over mapped MMIO by
/// the target; over a plain array by the tests below.
pub trait Regs16 {
    /// Read the 16-bit register at byte `offset`.
    fn r16(&self, offset: usize) -> u16;
}

/// How the pure half writes a 16-bit register. Separate from [`Regs16`]
/// so read-only fakes stay one-method, and so the reset path states
/// its needs in the type it takes.
pub trait RegsW16: Regs16 {
    /// Write the 16-bit register at byte `offset`.
    fn w16(&self, offset: usize, value: u16);
    /// Write a 32-bit register (CORBUBASE/RIRBUBASE need this: the
    /// controller ignores 16-bit half-writes to the upper-base regs).
    fn w32(&self, offset: usize, value: u32);
}

/// GCAP, decoded. Field order in raw (bit 0 first): 64OK, NS, BSS, ISS,
/// OSS - the spec packing, not the print order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GCap {
    /// The raw 16-bit register.
    pub raw: u16,
}

impl GCap {
    /// Output streams supported (`raw[15:12]`).
    #[must_use]
    pub fn output_streams(&self) -> u8 {
        (self.raw >> 12) as u8
    }
    /// Input streams supported (`raw[11:8]`).
    #[must_use]
    pub fn input_streams(&self) -> u8 {
        ((self.raw >> 8) & 0xf) as u8
    }
    /// Bidirectional streams supported (`raw[7:3]`, five bits).
    #[must_use]
    pub fn bidirectional_streams(&self) -> u8 {
        ((self.raw >> 3) & 0x1f) as u8
    }
    /// Serial data out signals, `NSDO` (`raw[2:1]`): 0, 1, 2 = one, two, four.
    #[must_use]
    pub fn serial_data_signals(&self) -> u8 {
        ((self.raw >> 1) & 0x3) as u8
    }
    /// Whether the controller takes 64-bit DMA addresses (`raw[0]`).
    #[must_use]
    pub fn supports_64bit(&self) -> bool {
        self.raw & 1 != 0
    }
}

/// What discovery learned about the controller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Info {
    /// The decoded `GCAP`.
    pub gcap: GCap,
    /// Major version (`VMAJ`).
    pub vmaj: u8,
    /// Minor version (`VMIN`).
    pub vmin: u8,
}

/// `GCTL.CRST` — software-driven controller reset bit: clear to enter,
/// set to exit (HDA 1.0a §4.3).
pub const CRST: u16 = 1;

/// The runbook's discovery gate: a version of `0xffff` means the BAR is not
/// mapped, and "everything after this is noise".
#[must_use]
pub fn version_is_noise(vmaj: u8, vmin: u8) -> bool {
    vmaj == 0xff && vmin == 0xff
}

/// M1 discovery: read `GCAP` + version, and gate.
///
/// Returns `None` when the version reads `0xffff` — the caller prints that as
/// a mapping failure, not as a controller property.
#[must_use]
pub fn discover<R: Regs16 + ?Sized>(regs: &R) -> Option<Info> {
    let gcap = GCap { raw: regs.r16(reg::GCAP) };
    // One aligned halfword at 0x02: VMIN (byte 2) | VMAJ<<8 (byte 3).
    // Reading r16(0x03) instead is unaligned - it crosses the dword
    // boundary and real MMIO decoders answer 0xff, which is where
    // three boots of version=255.0 came from.
    let vv = regs.r16(reg::VMIN);
    let vmin = (vv & 0xff) as u8;
    let vmaj = (vv >> 8) as u8;
    if version_is_noise(vmaj, vmin) {
        return None;
    }
    Some(Info { gcap, vmaj, vmin })
}

/// Reset the controller per HDA 1.0a §4.3.
///
/// CRST is software-driven, not self-clearing. Phase 1 — clear CRST (enter reset), poll until
/// it reads 0. Phase 2 — set CRST (exit reset), poll until it reads 1.
/// `wait` is one delay unit (the crate stays timing-free); the caller
/// should budget units for the ~25 µs controller recovery. Returns
/// `false` when either phase times out.
pub fn reset<R: RegsW16 + ?Sized>(regs: &R, polls: usize, mut wait: impl FnMut()) -> bool {
    let st = regs.r16(reg::GCTL);
    regs.w16(reg::GCTL, st & !CRST);
    let mut entered = false;
    for _ in 0..polls {
        if regs.r16(reg::GCTL) & CRST == 0 {
            entered = true;
            break;
        }
        wait();
    }
    if !entered {
        return false;
    }
    regs.w16(reg::GCTL, st | CRST);
    for _ in 0..polls {
        wait();
        if regs.r16(reg::GCTL) & CRST != 0 {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::cell::Cell;

    /// A register file in a box, so the decode is testable off-metal.
    struct FakeRegs {
        buf: [u8; 8],
    }

    impl Regs16 for FakeRegs {
        fn r16(&self, offset: usize) -> u16 {
            u16::from_le_bytes([self.buf[offset], self.buf[offset + 1]])
        }
    }

    fn regs_with(gcap: u16, vmin: u8, vmaj: u8) -> FakeRegs {
        let mut buf = [0u8; 8];
        buf[reg::GCAP..reg::GCAP + 2].copy_from_slice(&gcap.to_le_bytes());
        buf[reg::VMIN] = vmin;
        buf[reg::VMAJ] = vmaj;
        FakeRegs { buf }
    }

    #[test]
    fn gcap_bitfields_decode() {
        // The trashcan's 8-series controller: 4 out, 4 in, 0 bidi, one serial
        // data line (NSDO 0), 64-bit ok: (4<<12)|(4<<8)|1.
        let g = GCap { raw: 0x4401 };
        assert_eq!(g.output_streams(), 4);
        assert_eq!(g.input_streams(), 4);
        assert_eq!(g.bidirectional_streams(), 0);
        assert_eq!(g.serial_data_signals(), 0);
        assert!(g.supports_64bit());

        // Fields maxed: 15/15, BSS is five bits wide (0xF6 >> 3 = 30), NSDO 3,
        // 64-bit off.
        let g = GCap { raw: 0xfff6 };
        assert_eq!(g.output_streams(), 15);
        assert_eq!(g.input_streams(), 15);
        assert_eq!(g.bidirectional_streams(), 30);
        assert_eq!(g.serial_data_signals(), 3);
        assert!(!g.supports_64bit());
    }

    #[test]
    fn discover_reads_a_live_shaped_controller() {
        // The shape the 8-series controller is expected to answer with.
        let r = regs_with(0x4401, 0x00, 0x01);
        let info = discover(&r).expect("version 1.0 is a real answer");
        assert_eq!((info.vmaj, info.vmin), (1, 0));
        assert_eq!(info.gcap.output_streams(), 4);
    }

    #[test]
    fn all_ff_version_is_noise_not_a_version() {
        // Unmapped MMIO: every read is 0xff. Discovery must say "not mapped".
        let r = regs_with(0xffff, 0xff, 0xff);
        assert!(discover(&r).is_none());
        assert!(version_is_noise(0xff, 0xff));
        assert!(!version_is_noise(0x01, 0x00));
    }

    /// A fake that holds CRST until the wait callback self-clears it, so the
    /// reset handshake is testable without hardware. `Cell`, because the wait
    /// closure and the register file share it without borrowing.
    struct FakeReset {
        state: Cell<u16>,
    }

    impl Regs16 for FakeReset {
        fn r16(&self, _offset: usize) -> u16 {
            self.state.get()
        }
    }

    impl RegsW16 for FakeReset {
        fn w16(&self, _offset: usize, value: u16) {
            self.state.set(value);
        }
        fn w32(&self, _offset: usize, _value: u32) {}
    }

    #[test]
    fn reset_full_software_cycle() {
        // CRST is software-driven (HDA 1.0a §4.3): phase 1 clears it
        // (enter reset), phase 2 sets it (exit reset). The fake starts
        // out of reset with unrelated bits set; reset must preserve them.
        let f = FakeReset { state: Cell::new(0x42) };
        let mut waits = 0;
        assert!(reset(&f, 100, || {
            waits += 1;
        }));
        assert_eq!(f.state.get(), 0x43); // 0x42 kept, CRST now set
        assert!(waits <= 200);
    }

    struct Stuck<F>(F);
    impl<F: Regs16> Regs16 for Stuck<F> {
        fn r16(&self, o: usize) -> u16 {
            self.0.r16(o)
        }
    }
    impl<F: RegsW16> RegsW16 for Stuck<F> {
        fn w16(&self, _o: usize, _v: u16) {}
        fn w32(&self, _o: usize, _v: u32) {}
    }

    #[test]
    fn reset_times_out_when_hw_refuses_to_enter() {
        // A wedged controller that never reads CRST low must surface as
        // a false return, not a boot hang — phase 1 timeout path.
        let f = Stuck(FakeReset { state: Cell::new(CRST | 0x40) });
        assert!(!reset(&f, 5, || {}));
    }
}
