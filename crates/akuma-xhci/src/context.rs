//! Device-context and Input-context builders (xHCI §6.2).
//!
//! A context is 8 dwords (32 bytes) on this controller — `HCCPARAMS1.CSZ` is 0.
//! On a CSZ=1 controller each would be 64 bytes; the builders here return the
//! meaningful 32 bytes and the glue zero-pads to the stride it read from CSZ.
//!
//! Two structures share the context shape:
//!
//! * the **Device Context** in the DCBAA — `[Slot][EP DCI 1..=31]` — which the
//!   controller owns and writes back, and
//! * the **Input Context** the driver hands to Address Device / Configure
//!   Endpoint — `[Input Control][Slot][EP DCI 1..=31]` — which says which of
//!   those the command should add or drop.

/// Doorbell / Context Index for an endpoint address (xHCI §4.5.1).
///
/// EP0 (`0x00`) is DCI 1. For `bEndpointAddress` with endpoint number `n` and
/// direction bit `0x80`: DCI = `n * 2 + (IN ? 1 : 0)`. So `0x81` → 3, `0x02` → 4.
#[must_use]
pub fn dci(endpoint_address: u8) -> u8 {
    let n = endpoint_address & 0x0f;
    if n == 0 {
        return 1;
    }
    let dir_in = endpoint_address & 0x80 != 0;
    n * 2 + u8::from(dir_in)
}

/// The Add-Context flag bit for a DCI, for the Input Control Context (A0 = slot,
/// A1 = EP0, …).
#[must_use]
pub fn add_flag(dci: u8) -> u32 {
    1u32 << dci
}

/// Endpoint Context `EP Type` field (xHCI Table 6-9).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EpType {
    Control = 4,
    BulkOut = 2,
    BulkIn = 6,
    InterruptOut = 3,
    InterruptIn = 7,
}

impl EpType {
    /// From a `bmAttributes` transfer-type (low 2 bits) and a direction.
    ///
    /// `0` control, `2` bulk, `3` interrupt (isoch `1` is not something this
    /// driver configures and maps to `None`).
    #[must_use]
    pub fn from_attributes(transfer_type: u8, dir_in: bool) -> Option<Self> {
        match (transfer_type & 0x3, dir_in) {
            (0, _) => Some(Self::Control),
            (2, false) => Some(Self::BulkOut),
            (2, true) => Some(Self::BulkIn),
            (3, false) => Some(Self::InterruptOut),
            (3, true) => Some(Self::InterruptIn),
            _ => None,
        }
    }
}

/// The Slot Context (xHCI §6.2.2), input half only — the fields the driver sets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlotConfig {
    /// Route String — 0 for a device on a root-hub port.
    pub route_string: u32,
    /// Port Speed (xHCI Speed ID: 4 = SuperSpeed).
    pub speed: u8,
    /// 1-based root-hub port number the device is attached to.
    pub root_hub_port: u8,
    /// Highest Doorbell Context Index the device has a valid endpoint context
    /// for — 1 right after Address Device, then the max DCI after Configure
    /// Endpoint.
    pub context_entries: u8,
}

impl SlotConfig {
    /// The 8 dwords of the Slot Context. Output-only fields (device address,
    /// slot state) are left 0 — the controller fills them.
    #[must_use]
    pub fn build(&self) -> [u32; 8] {
        let dword0 = (self.route_string & 0x000f_ffff)
            | (u32::from(self.speed & 0xf) << 20)
            | (u32::from(self.context_entries & 0x1f) << 27);
        // Max Exit Latency 0, Root Hub Port Number in bits 23:16, Number of
        // Ports 0 (not a hub).
        let dword1 = u32::from(self.root_hub_port) << 16;
        // TT fields 0 (no transaction translator for a root-port SS device);
        // Interrupter Target 0.
        [dword0, dword1, 0, 0, 0, 0, 0, 0]
    }
}

/// An Endpoint Context (xHCI §6.2.3), input half.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EndpointConfig {
    pub ep_type: EpType,
    /// `wMaxPacketSize` low 11 bits (512 for SS control, 1024 for SS bulk).
    pub max_packet_size: u16,
    /// SuperSpeed `bMaxBurst` from the endpoint companion descriptor (0 for
    /// control / for USB2). The enclosure's bulk endpoints report 15.
    pub max_burst: u8,
    /// Physical base of this endpoint's transfer ring (16-byte aligned).
    pub tr_dequeue_phys: u64,
    /// Dequeue Cycle State — must equal the transfer ring's initial Producer
    /// Cycle State (this driver uses 1).
    pub dequeue_cycle: bool,
    /// Average TRB Length hint (xHCI §4.14.1.1) — 8 for control, a typical
    /// transfer size for bulk. Non-zero is required.
    pub average_trb_length: u16,
}

impl EndpointConfig {
    /// The 8 dwords of the Endpoint Context.
    #[must_use]
    pub fn build(&self) -> [u32; 8] {
        // dword 0: EP State 0, Mult 0, MaxPStreams 0, LSA 0, Interval 0,
        //          Max ESIT Payload Hi 0.
        let dword0 = 0u32;
        // dword 1: CErr = 3 (bits 2:1), EP Type (bits 5:3), Max Burst Size
        //          (bits 15:8), Max Packet Size (bits 31:16).
        let dword1 = (3u32 << 1)
            | ((self.ep_type as u32) << 3)
            | (u32::from(self.max_burst) << 8)
            | (u32::from(self.max_packet_size & 0x7ff) << 16);
        // dwords 2:3 — TR Dequeue Pointer, 16-byte aligned, bit 0 = DCS.
        let ptr = (self.tr_dequeue_phys & !0xf) | u64::from(self.dequeue_cycle);
        let dword2 = ptr as u32;
        let dword3 = (ptr >> 32) as u32;
        // dword 4: Average TRB Length (bits 15:0), Max ESIT Payload Lo 0.
        let dword4 = u32::from(self.average_trb_length);
        [dword0, dword1, dword2, dword3, dword4, 0, 0, 0]
    }
}

/// The Input Control Context (xHCI §6.2.5.1) — the first context of an Input
/// Context.
///
/// `add` / `drop` are bitmaps of DCIs (bit 0 = slot context, bit 1 = EP0, …).
/// For Configure Endpoint the Configuration Value goes in dword 7.
#[must_use]
pub fn input_control_context(add: u32, drop: u32, configuration_value: u8) -> [u32; 8] {
    // Drop flags bits 1:0 are reserved-zero (the slot and EP0 contexts can
    // never be dropped, only the higher DCIs).
    let dword0 = drop & !0x3;
    let dword1 = add;
    let dword7 = u32::from(configuration_value);
    [dword0, dword1, 0, 0, 0, 0, 0, dword7]
}

/// Byte offset of context index `i` within a context array of the given stride
/// (`i` = 0 is the Input Control / Slot Context depending on the array).
#[must_use]
pub fn context_offset(i: usize, context_bytes: usize) -> usize {
    i * context_bytes
}
