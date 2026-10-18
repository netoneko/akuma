// `deny`, not `forbid`: the four `in`/`out` instructions live in `raw.rs` behind
// one `#[allow(unsafe_code)]`, and that is the only place in the crate that
// attribute appears. Everything else — every device, every method — is safe.
#![deny(unsafe_code)]
#![no_std]
//! x86 legacy port I/O, as devices.
//!
//! # Why this is a crate
//!
//! `amd64/src/port.rs` was four `unsafe fn`s — `inb`, `outb`, `inl`, `outl` —
//! and 22 call sites in seven files, each of which carried an `unsafe` block
//! and a comment saying which device it believed was at that port. That is the
//! wrong shape for the obligation. "This port is the i8042" is a fact about the
//! *machine*, not about the line of code, and it was being re-asserted at every
//! line.
//!
//! Here it is asserted once, by a type: [`COM1`] is the 16550 at `0x3F8`, and
//! its methods can only reach `0x3F8 + 0..=7`. A driver that wants the keyboard
//! controller takes [`I8042`] and has no way to name `0xCF9`. The crate has no
//! public function taking a port number.
//!
//! # What this does and does not claim
//!
//! Memory safety, not harmlessness. [`ResetControl::request_hard_reset`] is a
//! safe function that resets the machine, because it is memory-safe and its
//! name says what it does; it is not a function a driver calls by accident. The
//! wrong design — a safe `outb(port, val)` — would let any code in the tree
//! power the box off or reprogram the interrupt controller with a typo, which is
//! exactly why `akuma-cpu` refuses `msr ttbr0_el1` and `smc`. Here no function
//! takes a port at all.
//!
//! # What is deliberately not here
//!
//! * **Serialisation.** `CMOS` and `PCI_CONFIG` are index/data pairs: two
//!   accesses that mean nothing if another core slips between them. The caller
//!   owns that (the CMOS reads run on one thread; `pci.rs` enumerates under its
//!   registry lock), as it did before. Making the pair atomic here would need a
//!   lock this `no_std` leaf does not have, and an IRQ-masking policy it should
//!   not choose.
//! * **Policy.** What to write to the UART to get 115200 8N1, when to mask the
//!   PICs, how long to wait for the i8042 — those stay with the drivers. This
//!   crate knows registers, not sequences, except where a sequence *is* the
//!   register's definition (the 8254's load-count dance, the CMOS index mask).
//! * **PCI itself.** `akuma-pci` is the config-space decode; this is only the
//!   two ports that move a dword.
//!
//! # Testing
//!
//! Under `cfg(test)` the bus is a per-thread script and log (`raw.rs`), so each
//! device's access sequence — and the fact that it stays inside its own port
//! range — is asserted on the host. A test on an x86 host never executes a real
//! `out`.

mod raw;

/// An inclusive range of I/O ports one device decodes, and how wide the
/// accesses to it are.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PortRange {
    pub name: &'static str,
    pub first: u16,
    pub last: u16,
    /// Access width in bytes this crate uses: 1 or 4.
    pub width: u8,
}

/// Every I/O port this crate can touch. The registry the types are checked
/// against, and the list to read when asking "who owns port N?".
pub const RANGES: [PortRange; 11] = [
    PortRange { name: "pic-master-mask", first: 0x21, last: 0x21, width: 1 },
    PortRange { name: "pit", first: 0x40, last: 0x43, width: 1 },
    PortRange { name: "i8042-data", first: 0x60, last: 0x60, width: 1 },
    PortRange { name: "i8042-status-command", first: 0x64, last: 0x64, width: 1 },
    PortRange { name: "cmos", first: 0x70, last: 0x71, width: 1 },
    PortRange { name: "pic-slave-mask", first: 0xA1, last: 0xA1, width: 1 },
    PortRange { name: "debugcon", first: 0xE9, last: 0xE9, width: 1 },
    PortRange { name: "uart-com1", first: 0x3F8, last: 0x3FF, width: 1 },
    PortRange { name: "pci-config-address", first: 0xCF8, last: 0xCFB, width: 4 },
    PortRange { name: "reset-control", first: 0xCF9, last: 0xCF9, width: 1 },
    PortRange { name: "pci-config-data", first: 0xCFC, last: 0xCFF, width: 4 },
];

// ---------------------------------------------------------------------------
// 16550 UART
// ---------------------------------------------------------------------------

/// A 16550 UART's register block.
#[derive(Clone, Copy, Debug)]
pub struct Uart16550 {
    base: u16,
}

/// COM1. Fixed by the PC architecture since the IBM PC; not discovered.
pub const COM1: Uart16550 = Uart16550 { base: 0x3F8 };

/// The 16550 registers this kernel uses, as offsets from the block's base.
///
/// With DLAB set in `LineCtrl`, offsets 0 and 1 are the baud divisor's low and
/// high bytes instead of `Data` and `IntEnable`; the driver sequences that.
/// Offset 2 reads as the interrupt-identification register and writes as
/// `FifoCtrl`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u16)]
pub enum UartReg {
    Data = 0,
    IntEnable = 1,
    FifoCtrl = 2,
    LineCtrl = 3,
    ModemCtrl = 4,
    LineStatus = 5,
    /// A byte the chip stores and hands back and does nothing else with: the
    /// probe register, for exactly that reason.
    Scratch = 7,
}

impl Uart16550 {
    #[must_use]
    pub fn read(&self, reg: UartReg) -> u8 {
        raw::inb(self.base + reg as u16)
    }

    pub fn write(&self, reg: UartReg, value: u8) {
        raw::outb(self.base + reg as u16, value);
    }
}

// ---------------------------------------------------------------------------
// CMOS / RTC
// ---------------------------------------------------------------------------

/// The CMOS RAM and real-time clock, behind its index/data port pair.
#[derive(Clone, Copy, Debug)]
pub struct Cmos;

/// `0x70`/`0x71` on every PC.
pub const CMOS: Cmos = Cmos;

impl Cmos {
    const INDEX: u16 = 0x70;
    const DATA: u16 = 0x71;
    /// Bit 7 of the index port disables NMI. It is stripped here so no caller
    /// can switch NMI off by passing a register number with the high bit set.
    const NMI_DISABLE: u8 = 0x80;

    /// Read register `reg` (0–0x7F).
    #[must_use]
    pub fn read(&self, reg: u8) -> u8 {
        raw::outb(Self::INDEX, reg & !Self::NMI_DISABLE);
        raw::inb(Self::DATA)
    }

    /// Write register `reg` (0–0x7F).
    pub fn write(&self, reg: u8, value: u8) {
        raw::outb(Self::INDEX, reg & !Self::NMI_DISABLE);
        raw::outb(Self::DATA, value);
    }
}

// ---------------------------------------------------------------------------
// 8259 PICs
// ---------------------------------------------------------------------------

/// The legacy pair of 8259 interrupt controllers.
#[derive(Clone, Copy, Debug)]
pub struct Pic8259;

pub const PIC: Pic8259 = Pic8259;

impl Pic8259 {
    const MASTER_MASK: u16 = 0x21;
    const SLAVE_MASK: u16 = 0xA1;

    /// Mask every line on both controllers: the conservative direction. They
    /// power up able to deliver on vectors that overlap the CPU exception range.
    pub fn mask_all(&self) {
        raw::outb(Self::MASTER_MASK, 0xFF);
        raw::outb(Self::SLAVE_MASK, 0xFF);
    }
}

// ---------------------------------------------------------------------------
// 8254 PIT
// ---------------------------------------------------------------------------

/// The 8254 programmable interval timer. Only channel 0 is driven.
#[derive(Clone, Copy, Debug)]
pub struct Pit;

pub const PIT: Pit = Pit;

impl Pit {
    const CH0_DATA: u16 = 0x40;
    const COMMAND: u16 = 0x43;
    /// Channel 0, lobyte then hibyte, mode 0 (interrupt on terminal count),
    /// binary.
    const CH0_LOAD_MODE0: u8 = 0b0011_0000;
    /// Read-back command (`11` in bits 7:6) for channel 0's status only: bit 5
    /// set = don't latch a count, bit 4 clear = do latch status, bit 1 =
    /// channel 0. Standard 8254, defined by the chip and not by board glue.
    const CH0_STATUS_LATCH: u8 = 0b1110_0010;
    /// The OUTPUT bit in a latched status byte. Mode 0 holds it low until the
    /// count reaches zero, then it latches high.
    const STATUS_OUTPUT: u8 = 1 << 7;

    /// Load `count` into channel 0 in mode 0. The gate is hardwired high, so
    /// the load itself starts the countdown — there is no separate arm step.
    pub fn load_channel0_oneshot(&self, count: u16) {
        raw::outb(Self::COMMAND, Self::CH0_LOAD_MODE0);
        raw::outb(Self::CH0_DATA, (count & 0xFF) as u8);
        raw::outb(Self::CH0_DATA, (count >> 8) as u8);
    }

    /// Latch and return channel 0's status byte.
    #[must_use]
    pub fn channel0_status(&self) -> u8 {
        raw::outb(Self::COMMAND, Self::CH0_STATUS_LATCH);
        raw::inb(Self::CH0_DATA)
    }

    /// Has channel 0's count expired (OUTPUT high)?
    ///
    /// **`inb` on an unimplemented port reads `0xFF`, not zero**, so a machine
    /// with no PIT answers "expired" at once. A caller that needs to tell a
    /// real expiry from an absent chip checks this *immediately after loading*,
    /// when a live channel must still read low.
    #[must_use]
    pub fn channel0_output_high(&self) -> bool {
        self.channel0_status() & Self::STATUS_OUTPUT != 0
    }
}

// ---------------------------------------------------------------------------
// i8042
// ---------------------------------------------------------------------------

/// The PS/2 controller.
#[derive(Clone, Copy, Debug)]
pub struct I8042Controller;

pub const I8042: I8042Controller = I8042Controller;

impl I8042Controller {
    const DATA: u16 = 0x60;
    const STATUS_COMMAND: u16 = 0x64;

    /// The status register. Reading it has no side effect.
    #[must_use]
    pub fn status(&self) -> u8 {
        raw::inb(Self::STATUS_COMMAND)
    }

    /// Take one byte from the output buffer, clearing its full flag.
    #[must_use]
    pub fn read_data(&self) -> u8 {
        raw::inb(Self::DATA)
    }

    /// Send a controller command. The caller waits for the input buffer to
    /// drain first (status bit 1).
    pub fn write_command(&self, command: u8) {
        raw::outb(Self::STATUS_COMMAND, command);
    }
}

// ---------------------------------------------------------------------------
// Reset control
// ---------------------------------------------------------------------------

/// The chipset reset-control register at `0xCF9`.
#[derive(Clone, Copy, Debug)]
pub struct ResetControl;

pub const RESET_CONTROL: ResetControl = ResetControl;

impl ResetControl {
    const PORT: u16 = 0xCF9;

    /// Ask the chipset for a hard reset. On a machine that honours it this does
    /// not come back; the caller sequences everything that must happen first
    /// (filesystem flush, watchdog stop) and whatever fallback follows.
    ///
    /// `0x02` selects "system reset" (as opposed to CPU-only), `0x0E` then
    /// requests a full hard reset. Architectural on every PC chipset since ICH.
    pub fn request_hard_reset(&self) {
        raw::outb(Self::PORT, 0x02);
        raw::outb(Self::PORT, 0x0E);
    }
}

// ---------------------------------------------------------------------------
// PCI configuration mechanism #1
// ---------------------------------------------------------------------------

/// The `0xCF8`/`0xCFC` pair that moves one dword of PCI config space.
#[derive(Clone, Copy, Debug)]
pub struct PciConfigPorts;

pub const PCI_CONFIG: PciConfigPorts = PciConfigPorts;

impl PciConfigPorts {
    const ADDRESS: u16 = 0xCF8;
    const DATA: u16 = 0xCFC;

    /// Select `config_address` (bus/device/function/dword, enable bit set) and
    /// read the dword. Selecting has no effect on any device.
    ///
    /// Not atomic across cores — see the crate docs.
    #[must_use]
    pub fn read_dword(&self, config_address: u32) -> u32 {
        raw::outl(Self::ADDRESS, config_address);
        raw::inl(Self::DATA)
    }

    /// Select `config_address` and write `value` to the dword. The caller owns
    /// the meaning of writing `value` to that register.
    pub fn write_dword(&self, config_address: u32, value: u32) {
        raw::outl(Self::ADDRESS, config_address);
        raw::outl(Self::DATA, value);
    }
}

// ---------------------------------------------------------------------------
// QEMU debugcon
// ---------------------------------------------------------------------------

/// Port `0xE9`: QEMU's `isa-debugcon`, and nothing on real PC hardware.
#[derive(Clone, Copy, Debug)]
pub struct Debugcon;

pub const DEBUGCON: Debugcon = Debugcon;

impl Debugcon {
    const PORT: u16 = 0xE9;

    /// Emit one byte. Only for a development flag: on real hardware this is a
    /// write to a port nothing owns.
    pub fn write_byte(&self, byte: u8) {
        raw::outb(Self::PORT, byte);
    }
}

#[cfg(test)]
mod tests;
