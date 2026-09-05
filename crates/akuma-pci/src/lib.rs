// This crate is unsafe-free by design and stays that way: `forbid` (not
// `deny`) so no module can opt back in with a local `allow`.
#![forbid(unsafe_code)]
#![no_std]
//! Pure PCI / PCIe configuration-space parsing.
//!
//! The amd64 bare-metal target has no PCI enumeration — every device it drives
//! so far arrived pre-announced by a VMM (virtio-MMIO on the command line). On
//! real hardware nothing announces anything: the kernel has to walk config
//! space itself to find the xHCI/EHCI controllers, the Realtek NIC and the
//! AHCI disk. That walk is two separable halves —
//!
//! * a **mechanism**: form the `0xCF8` address word, do the `inl`/`outl`, and
//!   the write-1s/read-back/restore dance that sizes a BAR (in
//!   `amd64/src/pci.rs`, because it is `unsafe` port I/O), and
//! * **parsing**: the type-0 header, the BAR encoding, the capability list —
//!
//! and this crate is the second half, so it can be host-tested against real
//! config-space dumps (`tests/`, read off the HP 500-502nj) rather than only
//! exercised by booting the machine, exactly as `akuma-multiboot2` and
//! `akuma-ryzen-amd64` are.

/// A device's location in the PCI topology.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Address {
    pub bus: u8,
    /// 0..=31.
    pub device: u8,
    /// 0..=7.
    pub function: u8,
}

impl Address {
    #[must_use]
    pub const fn new(bus: u8, device: u8, function: u8) -> Self {
        Self { bus, device, function }
    }

    /// The value to write to the `0xCF8` CONFIG_ADDRESS port to select
    /// `offset` (which is aligned down to a dword) of this device's config
    /// space. Bit 31 is the "enable" bit.
    #[must_use]
    pub const fn config_address(self, offset: u8) -> u32 {
        0x8000_0000
            | ((self.bus as u32) << 16)
            | (((self.device as u32) & 0x1f) << 11)
            | (((self.function as u32) & 0x07) << 8)
            | ((offset as u32) & 0xfc)
    }
}

/// `0xFFFF` in the vendor-ID slot — no device is present at that address.
pub const INVALID_VENDOR: u16 = 0xffff;

/// PCI base class codes (config offset `0x0B`).
pub mod class {
    pub const MASS_STORAGE: u8 = 0x01;
    pub const NETWORK: u8 = 0x02;
    pub const DISPLAY: u8 = 0x03;
    pub const BRIDGE: u8 = 0x06;
    pub const SERIAL_BUS: u8 = 0x0c;
}

/// Subclass codes, paired with their base class.
pub mod subclass {
    /// `class::MASS_STORAGE`
    pub const SATA: u8 = 0x06;
    /// `class::NETWORK`
    pub const ETHERNET: u8 = 0x00;
    /// `class::BRIDGE`
    pub const PCI_TO_PCI: u8 = 0x04;
    /// `class::SERIAL_BUS`
    pub const USB: u8 = 0x03;
}

/// `prog_if` for a USB controller (`class 0x0C`, subclass `0x03`).
pub mod usb_prog_if {
    pub const UHCI: u8 = 0x00;
    pub const OHCI: u8 = 0x10;
    pub const EHCI: u8 = 0x20;
    pub const XHCI: u8 = 0x30;
}

/// `prog_if` for a SATA controller in AHCI 1.0 mode.
pub const AHCI_PROG_IF: u8 = 0x01;

/// Command-register bits (config offset `0x04`).
pub mod command {
    pub const IO_SPACE: u16 = 1 << 0;
    pub const MEMORY_SPACE: u16 = 1 << 1;
    pub const BUS_MASTER: u16 = 1 << 2;
    pub const INTERRUPT_DISABLE: u16 = 1 << 10;
}

/// Capability IDs (the first byte of each capability record).
pub mod capability_id {
    pub const POWER_MANAGEMENT: u8 = 0x01;
    pub const VPD: u8 = 0x03;
    pub const MSI: u8 = 0x05;
    pub const VENDOR: u8 = 0x09;
    pub const PCI_EXPRESS: u8 = 0x10;
    pub const MSI_X: u8 = 0x11;
}

/// The type-0 (non-bridge) PCI configuration header — the first 64 bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub vendor_id: u16,
    pub device_id: u16,
    pub command: u16,
    pub status: u16,
    pub revision: u8,
    pub prog_if: u8,
    pub subclass: u8,
    pub class_code: u8,
    /// Bits 6:0 — `0` device, `1` PCI-to-PCI bridge, `2` CardBus.
    pub header_layout: u8,
    /// Header bit 7 — this device has functions 1..7 too.
    pub multifunction: bool,
    pub subsystem_vendor_id: u16,
    pub subsystem_id: u16,
    /// Config offset of the first capability, or `0` if `status` bit 4 is
    /// clear (no capability list).
    pub capabilities_pointer: u8,
    pub interrupt_line: u8,
    pub interrupt_pin: u8,
}

impl Header {
    /// Parse from at least the first 64 bytes of config space. Returns `None`
    /// for an absent device (`vendor_id == 0xFFFF`) or a truncated buffer.
    #[must_use]
    pub fn parse(cfg: &[u8]) -> Option<Self> {
        if cfg.len() < 64 {
            return None;
        }
        let u16_at = |o: usize| u16::from_le_bytes([cfg[o], cfg[o + 1]]);
        let vendor_id = u16_at(0x00);
        if vendor_id == INVALID_VENDOR {
            return None;
        }
        let header_type = cfg[0x0e];
        Some(Self {
            vendor_id,
            device_id: u16_at(0x02),
            command: u16_at(0x04),
            status: u16_at(0x06),
            revision: cfg[0x08],
            prog_if: cfg[0x09],
            subclass: cfg[0x0a],
            class_code: cfg[0x0b],
            header_layout: header_type & 0x7f,
            multifunction: header_type & 0x80 != 0,
            subsystem_vendor_id: u16_at(0x2c),
            subsystem_id: u16_at(0x2e),
            capabilities_pointer: if u16_at(0x06) & (1 << 4) != 0 { cfg[0x34] & 0xfc } else { 0 },
            interrupt_line: cfg[0x3c],
            interrupt_pin: cfg[0x3d],
        })
    }

    #[must_use]
    pub fn is_bridge(&self) -> bool {
        self.header_layout == 0x01
    }

    #[must_use]
    pub fn is_class(&self, class_code: u8, subclass: u8) -> bool {
        self.class_code == class_code && self.subclass == subclass
    }

    #[must_use]
    pub fn is_xhci(&self) -> bool {
        self.is_class(class::SERIAL_BUS, subclass::USB) && self.prog_if == usb_prog_if::XHCI
    }

    #[must_use]
    pub fn is_ehci(&self) -> bool {
        self.is_class(class::SERIAL_BUS, subclass::USB) && self.prog_if == usb_prog_if::EHCI
    }

    #[must_use]
    pub fn is_ethernet(&self) -> bool {
        self.is_class(class::NETWORK, subclass::ETHERNET)
    }

    #[must_use]
    pub fn is_ahci(&self) -> bool {
        self.is_class(class::MASS_STORAGE, subclass::SATA) && self.prog_if == AHCI_PROG_IF
    }
}

/// One decoded Base Address Register.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bar {
    /// Memory-mapped. `address` is the full (possibly 64-bit) base; `size` is
    /// `0` unless a probe supplied it.
    Memory {
        address: u64,
        size: u64,
        prefetchable: bool,
        is_64bit: bool,
    },
    /// Port I/O. `port` is 16-bit in practice; `size` `0` unless probed.
    Io { port: u32, size: u64 },
}

impl Bar {
    /// The number of BAR slots this entry occupies — a 64-bit memory BAR eats
    /// the next dword too.
    #[must_use]
    pub fn slots(&self) -> usize {
        if matches!(self, Self::Memory { is_64bit: true, .. }) {
            2
        } else {
            1
        }
    }

    #[must_use]
    pub fn is_memory(&self) -> bool {
        matches!(self, Self::Memory { .. })
    }
}

/// Decode the six raw BAR dwords into `Bar`s, skipping the empty ones and
/// consuming the extra slot after a 64-bit memory BAR.
///
/// Sizes are left at `0`; call [`bar_size`] with a probe result to fill them.
#[must_use]
pub fn decode_bars(raw: &[u32; 6]) -> [Option<Bar>; 6] {
    let mut out: [Option<Bar>; 6] = [None; 6];
    let mut i = 0;
    while i < 6 {
        let v = raw[i];
        if v == 0 {
            i += 1;
            continue;
        }
        if v & 1 != 0 {
            out[i] = Some(Bar::Io { port: v & 0xffff_fffc, size: 0 });
            i += 1;
            continue;
        }
        let is_64bit = (v >> 1) & 0b11 == 0b10;
        let prefetchable = v & (1 << 3) != 0;
        let low = u64::from(v & 0xffff_fff0);
        let address = if is_64bit && i + 1 < 6 {
            low | (u64::from(raw[i + 1]) << 32)
        } else {
            low
        };
        out[i] = Some(Bar::Memory { address, size: 0, prefetchable, is_64bit });
        i += if is_64bit { 2 } else { 1 };
    }
    out
}

/// Size of a BAR region from a write-all-ones probe.
///
/// `probed` is the value read back after writing `0xFFFF_FFFF` to the BAR (and,
/// for a 64-bit BAR, `probed_high` the readback of the next dword — pass `0`
/// otherwise). The mechanism — save, write ones, read, restore — is the
/// caller's; this is the arithmetic that turns the readback into a length.
#[must_use]
pub fn bar_size(probed_low: u32, probed_high: u32, is_io: bool, is_64bit: bool) -> u64 {
    let mask_low: u32 = if is_io { 0xffff_fffc } else { 0xffff_fff0 };
    if is_64bit {
        let masked = u64::from(probed_low & mask_low) | (u64::from(probed_high) << 32);
        if masked == 0 {
            return 0;
        }
        (!masked).wrapping_add(1)
    } else {
        // Only the low 32 bits are meaningful; invert within that width or a
        // 16 KiB BAR reports as ~2^64.
        let masked = probed_low & mask_low;
        if masked == 0 {
            return 0;
        }
        u64::from((!masked).wrapping_add(1))
    }
}

/// One entry in a device's capability list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capability {
    pub id: u8,
    /// Config-space offset of this capability record.
    pub offset: u8,
}

/// Walk the capability list. Bounded (16 iterations) against a config space
/// whose next-pointers form a loop.
#[derive(Debug, Clone)]
pub struct Capabilities<'a> {
    cfg: &'a [u8],
    next: u8,
    budget: u8,
}

impl Iterator for Capabilities<'_> {
    type Item = Capability;

    fn next(&mut self) -> Option<Capability> {
        // Capabilities live in 0x40..0x100 and are dword-aligned.
        if self.budget == 0 || self.next < 0x40 || usize::from(self.next) + 1 >= self.cfg.len() {
            return None;
        }
        self.budget -= 1;
        let offset = self.next & 0xfc;
        let id = self.cfg[usize::from(offset)];
        self.next = self.cfg[usize::from(offset) + 1] & 0xfc;
        Some(Capability { id, offset })
    }
}

/// Iterate a device's PCI capability list, starting from
/// [`Header::capabilities_pointer`].
#[must_use]
pub fn capabilities(cfg: &[u8], first: u8) -> Capabilities<'_> {
    Capabilities { cfg, next: first, budget: 16 }
}

/// The six raw BAR dwords out of a config-space buffer, for [`decode_bars`].
#[must_use]
pub fn raw_bars(cfg: &[u8]) -> Option<[u32; 6]> {
    if cfg.len() < 0x28 {
        return None;
    }
    let mut out = [0u32; 6];
    for (i, slot) in out.iter_mut().enumerate() {
        let o = 0x10 + i * 4;
        *slot = u32::from_le_bytes([cfg[o], cfg[o + 1], cfg[o + 2], cfg[o + 3]]);
    }
    Some(out)
}
