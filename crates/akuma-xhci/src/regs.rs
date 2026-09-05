//! xHCI register layout.
//!
//! The capability block, the operational registers, the runtime / interrupter
//! registers, and the doorbell array — all as offsets and typed bit decoders,
//! with the MMIO access itself left to the caller.
//!
//! Offsets and field positions are xHCI 1.2 §5. The fixture values in `tests/`
//! were read from `00:14.0` on the reference machine while Linux drove it.

use crate::raw;

// ===========================================================================
// Capability registers (at the BAR base)
// ===========================================================================

/// Byte offsets of the capability registers, from the BAR base (xHCI §5.3).
pub mod cap {
    pub const CAPLENGTH: usize = 0x00; // u8
    pub const HCIVERSION: usize = 0x02; // u16
    pub const HCSPARAMS1: usize = 0x04;
    pub const HCSPARAMS2: usize = 0x08;
    pub const HCSPARAMS3: usize = 0x0c;
    pub const HCCPARAMS1: usize = 0x10;
    pub const DBOFF: usize = 0x14;
    pub const RTSOFF: usize = 0x18;
    pub const HCCPARAMS2: usize = 0x1c;
}

/// `HCSPARAMS1` — structural parameters 1 (xHCI §5.3.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HcsParams1(pub u32);

impl HcsParams1 {
    /// Number of Device Slots the controller supports (`MaxSlots`, bits 7:0).
    /// This is what `CONFIG.MaxSlotsEn` is programmed to and how big the DCBAA
    /// must be (+1 for the index-0 scratchpad-array slot).
    #[must_use]
    pub fn max_slots(&self) -> u8 {
        (self.0 & 0xff) as u8
    }

    /// Number of Interrupters (`MaxIntrs`, bits 18:8). The driver uses only
    /// interrupter 0.
    #[must_use]
    pub fn max_interrupters(&self) -> u16 {
        ((self.0 >> 8) & 0x7ff) as u16
    }

    /// Number of Root Hub Ports (`MaxPorts`, bits 31:24) — the PORTSC array
    /// length.
    #[must_use]
    pub fn max_ports(&self) -> u8 {
        ((self.0 >> 24) & 0xff) as u8
    }
}

/// `HCSPARAMS2` — structural parameters 2 (xHCI §5.3.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HcsParams2(pub u32);

impl HcsParams2 {
    /// Isochronous Scheduling Threshold (`IST`, bits 3:0) — not used by a
    /// disk-only driver, decoded for completeness.
    #[must_use]
    pub fn ist(&self) -> u8 {
        (self.0 & 0xf) as u8
    }

    /// Max number of Event Ring Segment Table entries, as a power of two
    /// (`ERSTMax`, bits 7:4): the table may have up to `2^ERSTMax` segments.
    #[must_use]
    pub fn erst_max(&self) -> u8 {
        ((self.0 >> 4) & 0xf) as u8
    }

    /// Number of scratchpad buffers the controller demands the driver provide
    /// (`Max Scratchpad Bufs Hi` bits 25:21, `Lo` bits 31:27). If non-zero, the
    /// DCBAA's slot 0 must point at an array of this many 64-bit page pointers,
    /// each a `PAGESIZE`-aligned page the driver has reserved.
    #[must_use]
    pub fn max_scratchpad_buffers(&self) -> u32 {
        let hi = (self.0 >> 21) & 0x1f;
        let lo = (self.0 >> 27) & 0x1f;
        (hi << 5) | lo
    }
}

/// `HCCPARAMS1` — capability parameters 1 (xHCI §5.3.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HccParams1(pub u32);

impl HccParams1 {
    /// 64-bit addressing capable (`AC64`, bit 0). When false, every DMA pointer
    /// the driver programs must be below 4 GiB — which `.bss` statics on this
    /// target already are.
    #[must_use]
    pub fn addr64(&self) -> bool {
        self.0 & (1 << 0) != 0
    }

    /// Context Size (`CSZ`, bit 2): `true` = 64-byte contexts, `false` =
    /// 32-byte. Decides the stride of every Slot / Endpoint Context and the
    /// size of the Device Context and Input Context.
    #[must_use]
    pub fn context_size_64(&self) -> bool {
        self.0 & (1 << 2) != 0
    }

    /// Port Power Control (`PPC`, bit 3): port power is software-controlled.
    #[must_use]
    pub fn port_power_control(&self) -> bool {
        self.0 & (1 << 3) != 0
    }

    /// Byte offset from the BAR base to the first extended capability
    /// (`xECP`, bits 31:16 — a *dword* offset, so shift left 2). `0` = none.
    #[must_use]
    pub fn ext_cap_offset(&self) -> usize {
        (((self.0 >> 16) & 0xffff) as usize) * 4
    }

    /// Bytes in a Slot / Endpoint Context.
    #[must_use]
    pub fn context_bytes(&self) -> usize {
        if self.context_size_64() { 64 } else { 32 }
    }
}

/// The parsed capability block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapabilityRegisters {
    /// Byte offset from the BAR base to the operational registers.
    pub cap_length: u8,
    pub hci_version: u16,
    pub hcs_params1: HcsParams1,
    pub hcs_params2: HcsParams2,
    pub hcc_params1: HccParams1,
    /// Byte offset from the BAR base to the doorbell array.
    pub db_offset: u32,
    /// Byte offset from the BAR base to the runtime registers.
    pub rts_offset: u32,
}

impl CapabilityRegisters {
    /// Parse from the first 0x20 bytes of the BAR.
    #[must_use]
    pub fn parse(bar: &[u8]) -> Option<Self> {
        let cap_length = raw::u8(bar, cap::CAPLENGTH)?;
        // A real xHCI CAPLENGTH is 0x20..=0x80 (Intel parts read 0x80); 0xFF is
        // an absent controller, the same tell `serial`/`kbd` learned.
        if cap_length == 0xff || cap_length < 0x20 {
            return None;
        }
        Some(Self {
            cap_length,
            hci_version: raw::u16(bar, cap::HCIVERSION)?,
            hcs_params1: HcsParams1(raw::u32(bar, cap::HCSPARAMS1)?),
            hcs_params2: HcsParams2(raw::u32(bar, cap::HCSPARAMS2)?),
            hcc_params1: HccParams1(raw::u32(bar, cap::HCCPARAMS1)?),
            // Low bits are reserved and read 0, but mask defensively.
            db_offset: raw::u32(bar, cap::DBOFF)? & !0x3,
            rts_offset: raw::u32(bar, cap::RTSOFF)? & !0x1f,
        })
    }

    /// Byte offset from the BAR base to the operational register block.
    #[must_use]
    pub fn operational_base(&self) -> usize {
        self.cap_length as usize
    }
}

// ===========================================================================
// Operational registers (at BAR base + CAPLENGTH)
// ===========================================================================

/// Operational register offsets, relative to [`CapabilityRegisters::operational_base`]
/// (xHCI §5.4).
pub mod op {
    pub const USBCMD: usize = 0x00;
    pub const USBSTS: usize = 0x04;
    pub const PAGESIZE: usize = 0x08;
    pub const DNCTRL: usize = 0x14;
    /// Command Ring Control — 64-bit.
    pub const CRCR: usize = 0x18;
    /// Device Context Base Address Array Pointer — 64-bit.
    pub const DCBAAP: usize = 0x30;
    pub const CONFIG: usize = 0x38;
    /// Base of the port register sets. `PORTSC` for 1-based port `n` is at
    /// `PORT_BASE + (n - 1) * 0x10`.
    pub const PORT_BASE: usize = 0x400;

    /// Offset of `PORTSC` for 1-based port `n`.
    #[must_use]
    pub const fn portsc(port: u8) -> usize {
        PORT_BASE + (port as usize - 1) * 0x10
    }
}

/// `USBCMD` bit positions (xHCI §5.4.1).
pub mod usbcmd {
    /// Run/Stop.
    pub const RS: u32 = 1 << 0;
    /// Host Controller Reset.
    pub const HCRST: u32 = 1 << 1;
    /// Interrupter Enable.
    pub const INTE: u32 = 1 << 2;
    /// Host System Error Enable.
    pub const HSEE: u32 = 1 << 3;
}

/// `USBSTS` bit positions (xHCI §5.4.2). CNR and the change bits are the ones a
/// bring-up sequence polls.
pub mod usbsts {
    /// HCHalted — set while `USBCMD.RS` is 0 or the controller has stopped.
    pub const HCH: u32 = 1 << 0;
    /// Host System Error.
    pub const HSE: u32 = 1 << 2;
    /// Event Interrupt (write-1-to-clear).
    pub const EINT: u32 = 1 << 3;
    /// Port Change Detect (write-1-to-clear).
    pub const PCD: u32 = 1 << 4;
    /// Save/Restore Error.
    pub const SRE: u32 = 1 << 7;
    /// Controller Not Ready — the driver must not write any doorbell or
    /// operational register other than to poll this bit until it reads 0.
    pub const CNR: u32 = 1 << 11;
    /// Host Controller Error.
    pub const HCE: u32 = 1 << 12;
}

/// `CRCR` bit positions (xHCI §5.4.5). The pointer occupies bits 63:6.
pub mod crcr {
    /// Ring Cycle State — the Consumer Cycle State the controller starts with;
    /// must match the cycle bit the driver initialised the command ring's TRBs
    /// with (the driver uses 1).
    pub const RCS: u64 = 1 << 0;
    /// Command Stop.
    pub const CS: u64 = 1 << 1;
    /// Command Abort.
    pub const CA: u64 = 1 << 2;
    /// Command Ring Running (read-only).
    pub const CRR: u64 = 1 << 3;
    /// Mask for the 64-byte-aligned ring base pointer.
    pub const PTR_MASK: u64 = !0x3f;
}

/// `CONFIG` (xHCI §5.4.7): the low byte is `MaxSlotsEn`.
#[must_use]
pub fn config_max_slots_en(n: u8) -> u32 {
    u32::from(n)
}

// ===========================================================================
// PORTSC (xHCI §5.4.8)
// ===========================================================================

/// `PORTSC` — one per root-hub port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PortSc(pub u32);

impl PortSc {
    pub const CCS: u32 = 1 << 0; // Current Connect Status (RO)
    pub const PED: u32 = 1 << 1; // Port Enabled/Disabled (RW1C to disable)
    pub const OCA: u32 = 1 << 3; // Over-current Active (RO)
    pub const PR: u32 = 1 << 4; // Port Reset (RW1S)
    pub const PP: u32 = 1 << 9; // Port Power (RW)
    pub const CSC: u32 = 1 << 17; // Connect Status Change (RW1C)
    pub const PEC: u32 = 1 << 18; // Port Enabled/Disabled Change (RW1C)
    pub const WRC: u32 = 1 << 19; // Warm Port Reset Change (RW1C)
    pub const OCC: u32 = 1 << 20; // Over-current Change (RW1C)
    pub const PRC: u32 = 1 << 21; // Port Reset Change (RW1C)
    pub const PLC: u32 = 1 << 22; // Port Link State Change (RW1C)
    pub const CEC: u32 = 1 << 23; // Port Config Error Change (RW1C)
    pub const WPR: u32 = 1 << 31; // Warm Port Reset (RW1S)

    /// Every write-1-to-clear change bit. A read-modify-write of `PORTSC` that
    /// is not deliberately acknowledging a change must mask all of these to 0,
    /// **and** mask `PED` to 0 (writing `PED`=1 disables the port).
    pub const RW1C_MASK: u32 =
        Self::CSC | Self::PEC | Self::WRC | Self::OCC | Self::PRC | Self::PLC | Self::CEC;

    #[must_use]
    pub fn connected(&self) -> bool {
        self.0 & Self::CCS != 0
    }

    #[must_use]
    pub fn enabled(&self) -> bool {
        self.0 & Self::PED != 0
    }

    #[must_use]
    pub fn powered(&self) -> bool {
        self.0 & Self::PP != 0
    }

    #[must_use]
    pub fn resetting(&self) -> bool {
        self.0 & Self::PR != 0
    }

    #[must_use]
    pub fn reset_changed(&self) -> bool {
        self.0 & Self::PRC != 0
    }

    #[must_use]
    pub fn connect_changed(&self) -> bool {
        self.0 & Self::CSC != 0
    }

    /// Port Speed (bits 13:10) — feed to [`crate::Speed::from_field`].
    #[must_use]
    pub fn speed_field(&self) -> u32 {
        (self.0 >> 10) & 0xf
    }

    /// Port Link State (bits 8:5).
    #[must_use]
    pub fn link_state(&self) -> u32 {
        (self.0 >> 5) & 0xf
    }

    /// This value with `PED` and every RW1C change bit forced to 0 — the safe
    /// base for a read-modify-write that is not clearing a change.
    #[must_use]
    pub fn preserving_write(&self) -> u32 {
        self.0 & !(Self::RW1C_MASK | Self::PED)
    }

    /// Value to write to begin a hot reset of this port (USB2/SuperSpeed both
    /// accept `PR`; SuperSpeed link training also responds to `WPR`).
    #[must_use]
    pub fn with_reset_asserted(&self) -> u32 {
        self.preserving_write() | Self::PR
    }

    /// Value to write to begin a **warm** reset (SuperSpeed link recovery).
    #[must_use]
    pub fn with_warm_reset_asserted(&self) -> u32 {
        self.preserving_write() | Self::WPR
    }

    /// Value to write to acknowledge the port-reset-change and
    /// connect-status-change bits after a completed reset, leaving everything
    /// else intact.
    #[must_use]
    pub fn acknowledging_reset(&self) -> u32 {
        self.preserving_write() | Self::PRC | Self::CSC | Self::PEC | Self::PLC
    }

    /// Value to write to turn port power on.
    #[must_use]
    pub fn with_power_on(&self) -> u32 {
        self.preserving_write() | Self::PP
    }
}

// ===========================================================================
// Runtime / interrupter registers (at BAR base + RTSOFF)
// ===========================================================================

/// Runtime register offsets, relative to [`CapabilityRegisters::rts_offset`]
/// (xHCI §5.5).
pub mod rt {
    /// Microframe Index.
    pub const MFINDEX: usize = 0x00;

    /// Offset of Interrupter Register Set `n` (0-based). The driver uses only 0.
    #[must_use]
    pub const fn interrupter(n: usize) -> usize {
        0x20 + n * 0x20
    }
}

/// Interrupter register offsets, relative to [`rt::interrupter`] (xHCI §5.5.2).
pub mod intr {
    /// Interrupter Management — `IP` (bit 0, RW1C) and `IE` (bit 1).
    pub const IMAN: usize = 0x00;
    /// Interrupter Moderation.
    pub const IMOD: usize = 0x04;
    /// Event Ring Segment Table Size (number of segments).
    pub const ERSTSZ: usize = 0x08;
    /// Event Ring Segment Table Base Address — 64-bit, 64-byte aligned.
    pub const ERSTBA: usize = 0x10;
    /// Event Ring Dequeue Pointer — 64-bit. Low 3 bits are the Dequeue ERST
    /// Segment Index; bit 3 (`EHB`, Event Handler Busy) is write-1-to-clear.
    pub const ERDP: usize = 0x18;

    /// `IMAN.IP` — Interrupter Pending, write-1-to-clear.
    pub const IMAN_IP: u32 = 1 << 0;
    /// `IMAN.IE` — Interrupter Enable.
    pub const IMAN_IE: u32 = 1 << 1;
    /// `ERDP` bit 3 — Event Handler Busy, write-1-to-clear when updating ERDP.
    pub const ERDP_EHB: u64 = 1 << 3;
    /// Mask for the ERDP 16-byte-aligned pointer (bits 63:4).
    pub const ERDP_PTR_MASK: u64 = !0xf;
}

/// One Event Ring Segment Table entry (xHCI §6.5): a 16-byte record of
/// `{ ring_segment_base:u64, ring_segment_size:u16 (in TRBs), reserved }`.
#[must_use]
pub fn erst_entry(segment_phys: u64, trb_count: u16) -> [u32; 4] {
    [
        segment_phys as u32,
        (segment_phys >> 32) as u32,
        u32::from(trb_count),
        0,
    ]
}

// ===========================================================================
// Doorbell array (at BAR base + DBOFF)
// ===========================================================================

/// Doorbell register offsets, relative to [`CapabilityRegisters::db_offset`]
/// (xHCI §5.6). Doorbell 0 is the Command Ring; doorbell `slot` (1-based) is
/// device slot `slot`.
pub mod db {
    /// Byte offset of doorbell `n` (0 = command ring, else a device slot id).
    #[must_use]
    pub const fn doorbell(n: usize) -> usize {
        n * 4
    }

    /// The value to write to the command-ring doorbell (target 0).
    pub const COMMAND_RING_TARGET: u32 = 0;

    /// The value to write to a device slot's doorbell to ring endpoint
    /// `dci` (Doorbell Context Index: EP0 = 1, then `ep*2 + dir_in` — see
    /// [`crate::context::dci`]).
    #[must_use]
    pub const fn endpoint_target(dci: u8) -> u32 {
        dci as u32
    }
}
