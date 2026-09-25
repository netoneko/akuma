#![cfg_attr(not(test), no_std)]
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

/// The byte offset of a register, from the HDA specification's controller
/// register map. BAR0 points at the start of it.
pub mod reg {
    /// Global capabilities — stream counts, serial data lines, 64-bit support.
    pub const GCAP: usize = 0x00;
    /// Minor version (8-bit). With [`VMAJ`] this is the "is the BAR mapped"
    /// gate: unmapped MMIO reads as `0xff`.
    pub const VMIN: usize = 0x02;
    /// Major version (8-bit).
    pub const VMAJ: usize = 0x03;
    /// GCTL - the reset-and-status word; CRST (bit 0) is the
    /// controller reset; it self-clears when the reset completes.
    pub const GCTL: usize = 0x08;
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
    /// Bidirectional streams supported (`raw[7:4]`).
    #[must_use]
    pub fn bidirectional_streams(&self) -> u8 {
        ((self.raw >> 4) & 0xf) as u8
    }
    /// Serial data signals out (`raw[3:1]`) — codec count territory, M2's
    /// business, decoded now so the print can grow without a re-derive.
    #[must_use]
    pub fn serial_data_signals(&self) -> u8 {
        ((self.raw >> 1) & 0x7) as u8
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

/// The runbook's discovery gate: a version of `0xffff` means the BAR is not
/// mapped, and "everything after this is noise".
#[must_use]
/// `GCTL.CRST` — set to begin a controller reset; self-clears when
/// the controller and its codecs are ready again.
pub const CRST: u16 = 1;

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
    let vmin = (regs.r16(reg::VMIN) & 0xff) as u8;
    let vmaj = (regs.r16(reg::VMAJ) & 0xff) as u8;
    if version_is_noise(vmaj, vmin) {
        return None;
    }
    Some(Info { gcap, vmaj, vmin })
}

/// Reset the controller: set `CRST`, poll until it self-clears.
/// `wait` is one delay unit — the caller decides what a unit is, so
/// the crate stays free of timing. Returns `false` on timeout.
pub fn reset<R: RegsW16 + ?Sized>(regs: &R, polls: usize, wait: impl Fn()) -> bool {
    let st = regs.r16(reg::GCTL);
    regs.w16(reg::GCTL, st | CRST);
    for _ in 0..polls {
        wait();
        if regs.r16(reg::GCTL) & CRST == 0 {
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
        // 4 out, 4 in, 0 bidi, 1 serial line, 64-bit ok: (4<<12)|(4<<8)|1.
        let g = GCap { raw: 0x4401 };
        assert_eq!(g.output_streams(), 4);
        assert_eq!(g.input_streams(), 4);
        assert_eq!(g.bidirectional_streams(), 0);
        assert_eq!(g.serial_data_signals(), 1);
        assert!(g.supports_64bit());

        // Everything maxed: 15/15/15, NS=7, 64-bit off.
        let g = GCap { raw: 0xfff6 };
        assert_eq!(g.output_streams(), 15);
        assert_eq!(g.input_streams(), 15);
        assert_eq!(g.bidirectional_streams(), 15);
        assert_eq!(g.serial_data_signals(), 7);
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
    }

    #[test]
    fn reset_sets_crst_and_waits_for_self_clear() {
        // GCTL starts with unrelated bits set; reset must OR CRST in,
        // preserve them, and return true once CRST self-clears.
        let f = FakeReset { state: Cell::new(0x42) };
        let mut waits = 0;
        assert!(reset(&f, 100, || {
            waits += 1;
            if waits == 3 {
                f.state.set(0x42); // the controller finishes on the third wait
            }
        }));
        assert_eq!(waits, 3);
        assert_eq!(f.state.get(), 0x42);
    }

    #[test]
    fn reset_times_out_when_crst_never_clears() {
        let f = FakeReset { state: Cell::new(0x00) };
        assert!(!reset(&f, 5, || { f.state.set(f.state.get() | CRST); }));
        // CRST stuck: the timeout leaves it set — the caller reports and
        // leaves the controller alone rather than hanging the boot.
    }
}
