//! The xHCI extended-capability list (xHCI §7).
//!
//! Walking it, the BIOS→OS handoff (`USBLEGSUP`, cap id 1, §7.1), and the
//! Supported Protocol capability (id 2, §7.2) that says which ports are USB 2.0
//! and which are SuperSpeed.
//!
//! Each capability starts with a header dword: `Cap ID` (bits 7:0), `Next
//! Capability Pointer` (bits 15:8, a **dword** offset from the current
//! capability; 0 ends the list), and cap-specific bits 31:16.

/// Cap ID of `USBLEGSUP` (USB Legacy Support).
pub const CAP_ID_LEGACY_SUPPORT: u8 = 1;
/// Cap ID of a Supported Protocol capability.
pub const CAP_ID_SUPPORTED_PROTOCOL: u8 = 2;

/// Capability ID from a header dword.
#[must_use]
pub fn cap_id(header: u32) -> u8 {
    (header & 0xff) as u8
}

/// Byte offset of the next capability, given the current capability's byte
/// offset and its header dword. `None` ends the list.
#[must_use]
pub fn next_cap_offset(current_offset: usize, header: u32) -> Option<usize> {
    let next_dwords = ((header >> 8) & 0xff) as usize;
    if next_dwords == 0 {
        None
    } else {
        Some(current_offset + next_dwords * 4)
    }
}

/// `USBLEGSUP` — the first dword of the USB Legacy Support capability
/// (xHCI §7.1.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UsbLegSup(pub u32);

impl UsbLegSup {
    const BIOS_OWNED: u32 = 1 << 16;
    const OS_OWNED: u32 = 1 << 24;

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

/// Offset from the `USBLEGSUP` dword to `USBLEGCTLSTS` (xHCI §7.1.2).
pub const USBLEGCTLSTS_OFFSET: usize = 4;

/// The value to write to `USBLEGCTLSTS` after the handoff.
///
/// All SMI **enables** cleared (bits 0, 4, 13, 14, 15, 16) and the three RW1C
/// SMI **status** bits (29 SMI on OS Ownership Change, 30 SMI on PCI Command,
/// 31 SMI on BAR) acknowledged. Writing the status bits as 1 clears them;
/// writing the enables as 0 disables every EHCI-style SMI the firmware may have
/// armed.
#[must_use]
pub fn usblegctlsts_disable_all(current: u32) -> u32 {
    let enables = (1 << 0) | (1 << 4) | (1 << 13) | (1 << 14) | (1 << 15) | (1 << 16);
    let ack_status = (1u32 << 29) | (1 << 30) | (1 << 31);
    (current & !enables) | ack_status
}

/// A Supported Protocol capability (xHCI §7.2), decoded from its first three
/// dwords.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SupportedProtocol {
    /// Major USB revision (2 or 3).
    pub major: u8,
    /// Minor USB revision, BCD (e.g. `0x00` for 2.0, `0x00` for 3.0, `0x10`
    /// for 3.1).
    pub minor: u8,
    /// The 4-byte name string — always `b"USB "` on real parts.
    pub name: [u8; 4],
    /// 1-based number of the first root-hub port this protocol covers.
    pub port_offset: u8,
    /// How many consecutive ports it covers.
    pub port_count: u8,
    /// Protocol Slot Type — the value to put in an Enable Slot command's Slot
    /// Type field for a device on one of these ports (0 for USB).
    pub slot_type: u8,
}

impl SupportedProtocol {
    /// Decode from dwords 0..=3 of the capability.
    #[must_use]
    pub fn parse(dw0: u32, dw1: u32, dw2: u32, dw3: u32) -> Self {
        Self {
            major: (dw0 >> 24) as u8,
            minor: (dw0 >> 16) as u8,
            name: dw1.to_le_bytes(),
            port_offset: (dw2 & 0xff) as u8,
            port_count: ((dw2 >> 8) & 0xff) as u8,
            slot_type: (dw3 & 0x1f) as u8,
        }
    }

    /// Does 1-based root-hub port `port` fall in this capability's range?
    #[must_use]
    pub fn covers_port(&self, port: u8) -> bool {
        port >= self.port_offset && port < self.port_offset.saturating_add(self.port_count)
    }

    /// True for a SuperSpeed (USB 3.x) protocol block.
    #[must_use]
    pub fn is_superspeed(&self) -> bool {
        self.major >= 3
    }
}
