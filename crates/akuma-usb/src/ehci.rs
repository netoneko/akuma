//! EHCI (USB 2.0 host controller) register layout and schedule structures.
//!
//! Enough of the *Enhanced Host Controller Interface Specification for
//! Universal Serial Bus, Rev 1.0* to bring the controller up, hand it off from
//! the firmware, reset the keyboard's port, and run one periodic interrupt
//! transfer — as pure layout and bit math, with the MMIO reads/writes and the
//! DMA allocation left to the caller.
//!
//! # The split transaction, and why it dominates this module
//!
//! The reference machine's keyboard is a **full-speed** device on an EHCI
//! controller, behind the Intel rate-matching hub. EHCI itself only signals at
//! high speed; every transaction to the keyboard is a *split transaction* —
//! EHCI sends a start-split to the hub's transaction translator in one
//! microframe and collects the result with complete-splits in later ones. That
//! is encoded in the queue head's second dword ([`InterruptQueueHead`]: the
//! S-mask, the C-mask, the TT hub address and port), and getting those four
//! fields right is most of what stands between "port resets" and "keys arrive".
//!
//! Register values in `tests/` were read from EHCI `00:1a.0` on the box while
//! Linux was driving it (a read-only mmap of the BAR + `setpci` for the
//! extended-capability dword in PCI config space).

use crate::raw;

// ---------------------------------------------------------------------------
// Capability registers (at the BAR base)
// ---------------------------------------------------------------------------

/// `HCSPARAMS` — structural parameters (EHCI §2.2.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HcsParams(pub u32);

impl HcsParams {
    /// Number of physical downstream ports on this controller.
    #[must_use]
    pub fn n_ports(&self) -> u8 {
        (self.0 & 0xf) as u8
    }

    /// Port power is software-controlled (the `PP` bit in `PORTSC` is live).
    #[must_use]
    pub fn port_power_control(&self) -> bool {
        self.0 & (1 << 4) != 0
    }

    /// Port routing follows the `HCSP-PORTROUTE` array rather than the simple
    /// "first N to companion" rule.
    #[must_use]
    pub fn port_routing_rules(&self) -> bool {
        self.0 & (1 << 7) != 0
    }

    #[must_use]
    pub fn n_ports_per_companion(&self) -> u8 {
        ((self.0 >> 8) & 0xf) as u8
    }

    #[must_use]
    pub fn n_companion_controllers(&self) -> u8 {
        ((self.0 >> 12) & 0xf) as u8
    }

    /// `0` means there is no debug port; otherwise the 1-based port number
    /// that doubles as the USB debug port.
    #[must_use]
    pub fn debug_port_number(&self) -> u8 {
        ((self.0 >> 20) & 0xf) as u8
    }
}

/// `HCCPARAMS` — capability parameters (EHCI §2.2.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HccParams(pub u32);

impl HccParams {
    /// The controller can use 64-bit addresses (`CTRLDSSEGMENT` is live).
    #[must_use]
    pub fn addr64(&self) -> bool {
        self.0 & (1 << 0) != 0
    }

    /// The frame list length can be programmed (via `USBCMD.FLS`); otherwise
    /// it is fixed at 1024.
    #[must_use]
    pub fn programmable_frame_list(&self) -> bool {
        self.0 & (1 << 1) != 0
    }

    #[must_use]
    pub fn async_schedule_park(&self) -> bool {
        self.0 & (1 << 2) != 0
    }

    #[must_use]
    pub fn isochronous_scheduling_threshold(&self) -> u8 {
        ((self.0 >> 4) & 0xf) as u8
    }

    /// PCI **config-space** offset of the first EHCI extended capability, or
    /// `0` if there are none. This is where [`UsbLegSup`] lives.
    #[must_use]
    pub fn extended_capabilities_pointer(&self) -> u8 {
        ((self.0 >> 8) & 0xff) as u8
    }
}

/// The read-only capability registers at the start of the BAR.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapabilityRegisters {
    /// Byte offset from the BAR base to the operational registers.
    pub cap_length: u8,
    pub hci_version: u16,
    pub hcs_params: HcsParams,
    pub hcc_params: HccParams,
}

impl CapabilityRegisters {
    /// Parse from the first 12 bytes of the BAR.
    #[must_use]
    pub fn parse(bar: &[u8]) -> Option<Self> {
        let cap_length = raw::u8(bar, 0)?;
        // A plausible EHCI CAPLENGTH is small (0x20 on every Intel part); a
        // read of 0xFF is an absent controller, the same tell `serial`/`kbd`
        // learned.
        if cap_length == 0xff || cap_length < 8 {
            return None;
        }
        Some(Self {
            cap_length,
            hci_version: raw::u16(bar, 2)?,
            hcs_params: HcsParams(raw::u32(bar, 4)?),
            hcc_params: HccParams(raw::u32(bar, 8)?),
        })
    }

    /// Offset from the BAR base to the operational register block.
    #[must_use]
    pub fn operational_base(&self) -> usize {
        self.cap_length as usize
    }
}

// ---------------------------------------------------------------------------
// Operational registers (at BAR base + CAPLENGTH)
// ---------------------------------------------------------------------------

/// Operational register offsets, relative to [`CapabilityRegisters::operational_base`].
pub mod op {
    pub const USBCMD: usize = 0x00;
    pub const USBSTS: usize = 0x04;
    pub const USBINTR: usize = 0x08;
    pub const FRINDEX: usize = 0x0C;
    pub const CTRLDSSEGMENT: usize = 0x10;
    pub const PERIODICLISTBASE: usize = 0x14;
    pub const ASYNCLISTADDR: usize = 0x18;
    pub const CONFIGFLAG: usize = 0x40;
    pub const PORTSC_BASE: usize = 0x44;

    /// Offset of `PORTSC[port_index]`, 0-based.
    #[must_use]
    pub const fn portsc(port_index: u8) -> usize {
        PORTSC_BASE + 4 * port_index as usize
    }
}

/// `USBCMD` (EHCI §2.3.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UsbCmd(pub u32);

impl UsbCmd {
    pub const RUN: u32 = 1 << 0;
    pub const HC_RESET: u32 = 1 << 1;
    pub const PERIODIC_SCHEDULE_ENABLE: u32 = 1 << 4;
    pub const ASYNC_SCHEDULE_ENABLE: u32 = 1 << 5;
    pub const INT_ON_ASYNC_ADVANCE_DOORBELL: u32 = 1 << 6;

    #[must_use]
    pub fn running(&self) -> bool {
        self.0 & Self::RUN != 0
    }

    #[must_use]
    pub fn resetting(&self) -> bool {
        self.0 & Self::HC_RESET != 0
    }

    #[must_use]
    pub fn periodic_schedule_enabled(&self) -> bool {
        self.0 & Self::PERIODIC_SCHEDULE_ENABLE != 0
    }

    /// Frame list size: 1024, 512 or 256 entries (`FLS`, bits 3:2).
    #[must_use]
    pub fn frame_list_entries(&self) -> u16 {
        match (self.0 >> 2) & 0b11 {
            0 => 1024,
            1 => 512,
            2 => 256,
            _ => 0,
        }
    }

    /// Interrupt threshold control — max interrupt rate, in microframes (`ITC`,
    /// bits 23:16).
    #[must_use]
    pub fn interrupt_threshold(&self) -> u8 {
        ((self.0 >> 16) & 0xff) as u8
    }
}

/// `USBSTS` (EHCI §2.3.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UsbSts(pub u32);

impl UsbSts {
    pub const USB_INT: u32 = 1 << 0;
    pub const USB_ERR_INT: u32 = 1 << 1;
    pub const PORT_CHANGE_DETECT: u32 = 1 << 2;
    pub const FRAME_LIST_ROLLOVER: u32 = 1 << 3;
    pub const HOST_SYSTEM_ERROR: u32 = 1 << 4;
    pub const INT_ON_ASYNC_ADVANCE: u32 = 1 << 5;
    pub const HC_HALTED: u32 = 1 << 12;

    /// The three write-1-to-clear interrupt-status bits the driver
    /// acknowledges each poll.
    pub const ACK_MASK: u32 =
        Self::USB_INT | Self::USB_ERR_INT | Self::PORT_CHANGE_DETECT | Self::FRAME_LIST_ROLLOVER;

    #[must_use]
    pub fn halted(&self) -> bool {
        self.0 & Self::HC_HALTED != 0
    }

    #[must_use]
    pub fn transfer_interrupt(&self) -> bool {
        self.0 & Self::USB_INT != 0
    }

    #[must_use]
    pub fn error_interrupt(&self) -> bool {
        self.0 & Self::USB_ERR_INT != 0
    }

    #[must_use]
    pub fn port_change(&self) -> bool {
        self.0 & Self::PORT_CHANGE_DETECT != 0
    }
}

/// D+/D- line state sampled in `PORTSC` bits 11:10 while the port is not
/// enabled — how a low-speed device is spotted before reset so it can be
/// released to a companion controller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineStatus {
    /// SE0 — not a low-speed device; reset it here.
    Se0,
    /// K-state — a low-speed device; hand the port to a companion controller.
    KState,
    /// J-state.
    JState,
    Undefined,
}

/// `PORTSC[n]` (EHCI §2.3.9).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PortSc(pub u32);

impl PortSc {
    /// Write-1-to-clear status-change bits: CSC, PEC, OCC. A read-modify-write
    /// of `PORTSC` must write these as 0 or it silently clears a change the
    /// driver has not seen yet.
    pub const RWC_MASK: u32 = (1 << 1) | (1 << 3) | (1 << 5);

    pub const CONNECT_STATUS: u32 = 1 << 0;
    pub const CONNECT_CHANGE: u32 = 1 << 1;
    pub const PORT_ENABLED: u32 = 1 << 2;
    pub const ENABLE_CHANGE: u32 = 1 << 3;
    pub const OVER_CURRENT_CHANGE: u32 = 1 << 5;
    pub const PORT_RESET: u32 = 1 << 8;
    pub const PORT_POWER: u32 = 1 << 12;
    pub const PORT_OWNER: u32 = 1 << 13;

    #[must_use]
    pub fn connected(&self) -> bool {
        self.0 & Self::CONNECT_STATUS != 0
    }

    #[must_use]
    pub fn connect_changed(&self) -> bool {
        self.0 & Self::CONNECT_CHANGE != 0
    }

    #[must_use]
    pub fn enabled(&self) -> bool {
        self.0 & Self::PORT_ENABLED != 0
    }

    #[must_use]
    pub fn enable_changed(&self) -> bool {
        self.0 & Self::ENABLE_CHANGE != 0
    }

    #[must_use]
    pub fn resetting(&self) -> bool {
        self.0 & Self::PORT_RESET != 0
    }

    #[must_use]
    pub fn powered(&self) -> bool {
        self.0 & Self::PORT_POWER != 0
    }

    #[must_use]
    pub fn owned_by_companion(&self) -> bool {
        self.0 & Self::PORT_OWNER != 0
    }

    #[must_use]
    pub fn line_status(&self) -> LineStatus {
        match (self.0 >> 10) & 0b11 {
            0b00 => LineStatus::Se0,
            0b01 => LineStatus::KState,
            0b10 => LineStatus::JState,
            _ => LineStatus::Undefined,
        }
    }

    /// A low-speed device is present and should be released to a companion
    /// controller instead of reset here (EHCI §4.2.2).
    #[must_use]
    pub fn is_low_speed_device(&self) -> bool {
        self.connected() && self.line_status() == LineStatus::KState
    }

    /// This value with the write-1-to-clear bits masked to 0 — the safe base
    /// for any read-modify-write that is not deliberately clearing a change.
    #[must_use]
    pub fn preserving_write(&self) -> u32 {
        self.0 & !Self::RWC_MASK
    }

    /// Value to write to begin a port reset (also clears `PORT_ENABLED`, per
    /// spec, and leaves the change bits alone).
    #[must_use]
    pub fn with_reset_asserted(&self) -> u32 {
        (self.preserving_write() & !Self::PORT_ENABLED) | Self::PORT_RESET
    }

    /// Value to write to end a port reset.
    #[must_use]
    pub fn with_reset_cleared(&self) -> u32 {
        self.preserving_write() & !Self::PORT_RESET
    }

    /// Value to write to acknowledge the connect-status-change bit.
    #[must_use]
    pub fn acknowledging_connect_change(&self) -> u32 {
        self.preserving_write() | Self::CONNECT_CHANGE
    }

    /// Value to write to hand this port to a companion controller.
    #[must_use]
    pub fn releasing_to_companion(&self) -> u32 {
        self.preserving_write() | Self::PORT_OWNER
    }
}

// ---------------------------------------------------------------------------
// BIOS -> OS handoff (USBLEGSUP extended capability, in PCI config space)
// ---------------------------------------------------------------------------

/// `USBLEGSUP` — the first dword of the EHCI Legacy Support extended
/// capability, read from PCI config space at
/// [`HccParams::extended_capabilities_pointer`] (EHCI §2.1.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UsbLegSup(pub u32);

impl UsbLegSup {
    pub const CAP_ID_LEGACY_SUPPORT: u8 = 0x01;
    const BIOS_OWNED: u32 = 1 << 16;
    const OS_OWNED: u32 = 1 << 24;

    #[must_use]
    pub fn capability_id(&self) -> u8 {
        (self.0 & 0xff) as u8
    }

    /// PCI config-space offset of the next EHCI extended capability, or `0`.
    #[must_use]
    pub fn next_capability(&self) -> u8 {
        ((self.0 >> 8) & 0xff) as u8
    }

    #[must_use]
    pub fn is_legacy_support(&self) -> bool {
        self.capability_id() == Self::CAP_ID_LEGACY_SUPPORT
    }

    #[must_use]
    pub fn bios_owned(&self) -> bool {
        self.0 & Self::BIOS_OWNED != 0
    }

    #[must_use]
    pub fn os_owned(&self) -> bool {
        self.0 & Self::OS_OWNED != 0
    }

    /// The value to write back to claim the controller for the OS (set the
    /// OS-owned semaphore); then poll until [`bios_owned`](Self::bios_owned)
    /// clears.
    #[must_use]
    pub fn claiming_for_os(&self) -> u32 {
        self.0 | Self::OS_OWNED
    }

    /// Handoff is complete: the OS owns it and the BIOS has let go.
    #[must_use]
    pub fn handoff_complete(&self) -> bool {
        self.os_owned() && !self.bios_owned()
    }
}

/// Offset from the EECP to `USBLEGCTLSTS`, the SMI control/status dword. After
/// the handoff the driver writes `0` here to disable every EHCI SMI source.
pub const USBLEGCTLSTS_OFFSET: u8 = 4;

// ---------------------------------------------------------------------------
// Periodic schedule: frame list, queue head, qTD
// ---------------------------------------------------------------------------

/// Endpoint speed, `EPS` in the queue head (EHCI Table 3-19).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndpointSpeed {
    Full = 0b00,
    Low = 0b01,
    High = 0b10,
}

/// A frame-list entry pointing at a queue head (`Typ = 01b`, `T = 0`).
#[must_use]
pub fn frame_list_qh_entry(qh_phys: u32) -> u32 {
    (qh_phys & !0x1f) | (0b01 << 1)
}

/// A frame-list entry that terminates the list (`T = 1`).
#[must_use]
pub const fn frame_list_terminator() -> u32 {
    1
}

/// Polling period in **frames** for an interrupt endpoint, from its
/// `bInterval` and speed (EHCI §4.10 / USB 2.0 §9.6.6).
#[must_use]
pub fn interrupt_period_frames(b_interval: u8, speed: EndpointSpeed) -> u16 {
    match speed {
        // High speed: bInterval is an exponent, period = 2^(bInterval-1)
        // microframes; 8 microframes to a frame.
        EndpointSpeed::High => {
            let exp = b_interval.clamp(1, 16) - 1;
            ((1u32 << exp) / 8).max(1) as u16
        }
        // Full/low speed: bInterval is the period in frames directly.
        _ => u16::from(b_interval.max(1)),
    }
}

/// Largest power of two `<= period.min(1024)` — the stride at which a queue
/// head is threaded into the 1024-entry frame list so it is serviced no less
/// often than `period` frames.
#[must_use]
pub fn frame_list_stride(period_frames: u16) -> u16 {
    let cap = period_frames.clamp(1, 1024);
    let mut stride = 1u16;
    while stride * 2 <= cap {
        stride *= 2;
    }
    stride
}

/// Parameters for a periodic (interrupt) queue head.
///
/// For a full/low-speed device behind a high-speed hub, `tt_hub_address` /
/// `tt_port_number` name the transaction translator and `s_mask` / `c_mask`
/// schedule the split.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InterruptQueueHead {
    pub device_address: u8,
    pub endpoint_number: u8,
    pub max_packet_size: u16,
    pub speed: EndpointSpeed,
    /// Address of the nearest high-speed hub (the TT). `0` for a high-speed
    /// endpoint with no TT in the path.
    pub tt_hub_address: u8,
    /// That hub's downstream port the device is on, 1-based.
    pub tt_port_number: u8,
    /// Start-split microframe mask. `0x01` (start in microframe 0) is the
    /// usual choice for a low-bandwidth interrupt endpoint.
    pub s_mask: u8,
    /// Complete-split microframe mask. `0x1C` (microframes 2, 3, 4) covers a
    /// full-speed transaction started in microframe 0.
    pub c_mask: u8,
    /// Physical address of the first qTD (32-byte aligned), or `0` for "no
    /// qTD yet" (the T bit is set).
    pub first_qtd_phys: u32,
    /// Physical address this queue head's horizontal link points at — the next
    /// QH in the frame, or the QH itself for a single-entry ring. `None`
    /// terminates the list.
    pub horizontal_link_phys: Option<u32>,
}

impl InterruptQueueHead {
    /// The 12 little-endian dwords of the 48-byte queue head (EHCI §3.6).
    /// 32-byte aligned in DMA memory; the HW-maintained overlay (dwords 3..12)
    /// is initialised so the controller pulls in `first_qtd_phys`.
    #[must_use]
    pub fn build(&self) -> [u32; 12] {
        let horizontal = match self.horizontal_link_phys {
            Some(p) => (p & !0x1f) | (0b01 << 1), // Typ = QH, T = 0
            None => 1,                            // T = 1
        };

        // Dword 1 — endpoint characteristics.
        let dtc = 1u32 << 14; // data toggle comes from the qTD
        let dword1 = u32::from(self.device_address & 0x7f)
            | (u32::from(self.endpoint_number & 0xf) << 8)
            | ((self.speed as u32) << 12)
            | dtc
            | (u32::from(self.max_packet_size & 0x7ff) << 16);
        // RL (NAK reload) = 0 for interrupt; C (control-endpoint flag) = 0.

        // Dword 2 — endpoint capabilities. Mult must be non-zero.
        let mult = 1u32 << 30;
        let dword2 = u32::from(self.s_mask)
            | (u32::from(self.c_mask) << 8)
            | (u32::from(self.tt_hub_address & 0x7f) << 16)
            | (u32::from(self.tt_port_number & 0x7f) << 23)
            | mult;

        let next_qtd = if self.first_qtd_phys == 0 {
            1
        } else {
            self.first_qtd_phys & !0x1f
        };

        [
            horizontal, // 0: horizontal link
            dword1,     // 1: endpoint characteristics
            dword2,     // 2: endpoint capabilities
            0,          // 3: current qTD (HW)
            next_qtd,   // 4: next qTD
            1,          // 5: alternate next qTD = T
            0,          // 6: overlay token — not active, not halted
            0, 0, 0, 0, 0, // 7..12: overlay buffer pointers
        ]
    }
}

/// qTD token bit positions / fields (EHCI §3.5.3), exposed for reading a
/// completion back.
pub mod qtd_token {
    pub const PING: u32 = 1 << 0;
    pub const SPLIT_XACT_STATE: u32 = 1 << 1;
    pub const MISSED_MICROFRAME: u32 = 1 << 2;
    pub const TRANSACTION_ERROR: u32 = 1 << 3;
    pub const BABBLE: u32 = 1 << 4;
    pub const DATA_BUFFER_ERROR: u32 = 1 << 5;
    pub const HALTED: u32 = 1 << 6;
    pub const ACTIVE: u32 = 1 << 7;
    pub const IOC: u32 = 1 << 15;

    pub const PID_OUT: u32 = 0b00 << 8;
    pub const PID_IN: u32 = 0b01 << 8;
    pub const PID_SETUP: u32 = 0b10 << 8;

    /// Any error bit set.
    pub const ERROR_MASK: u32 =
        TRANSACTION_ERROR | BABBLE | DATA_BUFFER_ERROR | HALTED | MISSED_MICROFRAME;

    /// Bytes still to transfer (bits 30:16) — subtract from the requested
    /// length for the count actually moved.
    #[must_use]
    pub const fn bytes_remaining(token: u32) -> u16 {
        ((token >> 16) & 0x7fff) as u16
    }

    #[must_use]
    pub const fn is_active(token: u32) -> bool {
        token & ACTIVE != 0
    }
}

/// A single-buffer IN qTD for polling an interrupt endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InterruptInQtd {
    /// Physical address of the receive buffer. Must not cross a 4 KiB
    /// boundary within `length` bytes (a boot report is 8 bytes — trivial).
    pub buffer_phys: u32,
    pub length: u16,
    pub data_toggle: bool,
    /// Set to have the controller raise `USBSTS.USBINT` on completion — the
    /// driver polls that bit rather than taking an interrupt.
    pub interrupt_on_complete: bool,
}

impl InterruptInQtd {
    /// The 8 little-endian dwords of the 32-byte qTD (EHCI §3.5). 32-byte
    /// aligned in DMA memory.
    #[must_use]
    pub fn build(&self) -> [u32; 8] {
        let mut token = qtd_token::ACTIVE
            | qtd_token::PID_IN
            | (0b11 << 10) // CERR = 3
            | (u32::from(self.length & 0x7fff) << 16);
        if self.data_toggle {
            token |= 1 << 31;
        }
        if self.interrupt_on_complete {
            token |= qtd_token::IOC;
        }

        [
            1,                // 0: next qTD = T
            1,                // 1: alternate next qTD = T
            token,            // 2: token
            self.buffer_phys, // 3: buffer pointer 0 (page + current offset)
            0, 0, 0, 0,       // 4..8: buffer pointers 1..4 (unused: fits one page)
        ]
    }
}
