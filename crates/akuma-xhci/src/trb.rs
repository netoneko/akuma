//! Transfer Request Blocks — the 16-byte units every xHCI ring is made of — and
//! the cycle-bit bookkeeping for a producer ring (command / transfer) and a
//! consumer ring (event).
//!
//! A TRB is four little-endian dwords (xHCI §4.11, §6.4):
//!
//! ```text
//!   dword 0..1  parameter  — a 64-bit pointer, or immediate data, or event info
//!   dword 2     status     — transfer length / completion code / interrupter
//!   dword 3     control    — bit 0 Cycle, bits 15:10 TRB Type, type-specific rest
//! ```
//!
//! The **cycle bit** is the whole synchronisation mechanism: the producer writes
//! it to its current Producer Cycle State, the consumer follows TRBs whose cycle
//! matches its Consumer Cycle State, and a Link TRB with the Toggle Cycle bit
//! flips both when the ring wraps. [`ProducerRing`] and [`ConsumerRing`] below
//! own that state so the glue never open-codes it.

/// TRB Type values (xHCI Table 6-91), in the positions this driver uses.
pub mod ty {
    pub const NORMAL: u32 = 1;
    pub const SETUP_STAGE: u32 = 2;
    pub const DATA_STAGE: u32 = 3;
    pub const STATUS_STAGE: u32 = 4;
    pub const LINK: u32 = 6;
    pub const ENABLE_SLOT: u32 = 9;
    pub const DISABLE_SLOT: u32 = 10;
    pub const ADDRESS_DEVICE: u32 = 11;
    pub const CONFIGURE_ENDPOINT: u32 = 12;
    pub const EVALUATE_CONTEXT: u32 = 13;
    pub const RESET_ENDPOINT: u32 = 14;
    pub const NO_OP_CMD: u32 = 23;
    pub const TRANSFER_EVENT: u32 = 32;
    pub const COMMAND_COMPLETION_EVENT: u32 = 33;
    pub const PORT_STATUS_CHANGE_EVENT: u32 = 34;
}

/// Completion Codes (xHCI Table 6-90).
///
/// The ones a bring-up path checks for.
pub mod cc {
    pub const INVALID: u8 = 0;
    pub const SUCCESS: u8 = 1;
    pub const DATA_BUFFER_ERROR: u8 = 2;
    pub const BABBLE_DETECTED: u8 = 3;
    pub const USB_TRANSACTION_ERROR: u8 = 4;
    pub const TRB_ERROR: u8 = 5;
    pub const STALL_ERROR: u8 = 6;
    pub const RESOURCE_ERROR: u8 = 7;
    pub const BANDWIDTH_ERROR: u8 = 8;
    pub const NO_SLOTS_AVAILABLE: u8 = 9;
    pub const SHORT_PACKET: u8 = 13;
    pub const RING_UNDERRUN: u8 = 14;
    pub const RING_OVERRUN: u8 = 15;
    pub const EVENT_RING_FULL: u8 = 21;
    pub const COMMAND_RING_STOPPED: u8 = 24;
    pub const COMMAND_ABORTED: u8 = 25;
    pub const STOPPED: u8 = 26;
}

const CYCLE: u32 = 1 << 0;

#[inline]
fn set_type(control_without_cycle_or_type: u32, trb_type: u32) -> u32 {
    (control_without_cycle_or_type & !(0x3f << 10)) | ((trb_type & 0x3f) << 10)
}

#[inline]
fn trb_type_of(control: u32) -> u32 {
    (control >> 10) & 0x3f
}

// ===========================================================================
// Producer TRB builders — each returns the four dwords with the cycle bit
// LEFT CLEAR; `ProducerRing::enqueue` ORs in the live cycle.
// ===========================================================================

/// Enable Slot command. `slot_type` is 0 for a USB device on this controller.
#[must_use]
pub fn enable_slot(slot_type: u8) -> [u32; 4] {
    [0, 0, 0, set_type(u32::from(slot_type & 0x1f) << 16, ty::ENABLE_SLOT)]
}

/// Disable Slot command for `slot_id`.
#[must_use]
pub fn disable_slot(slot_id: u8) -> [u32; 4] {
    [0, 0, 0, set_type(u32::from(slot_id) << 24, ty::DISABLE_SLOT)]
}

/// Address Device command.
///
/// Points the controller at `input_context_phys` (16-byte aligned) for
/// `slot_id`. `block_set_address` (BSR) leaves the device in the Default state
/// (no `SET_ADDRESS` on the bus) — used for the first, descriptor-reading pass
/// on devices that need it; pass `false` for the normal path.
#[must_use]
pub fn address_device(input_context_phys: u64, slot_id: u8, block_set_address: bool) -> [u32; 4] {
    let mut control = u32::from(slot_id) << 24;
    if block_set_address {
        control |= 1 << 9;
    }
    [
        input_context_phys as u32,
        (input_context_phys >> 32) as u32,
        0,
        set_type(control, ty::ADDRESS_DEVICE),
    ]
}

/// Configure Endpoint command for `slot_id`, using `input_context_phys`.
#[must_use]
pub fn configure_endpoint(input_context_phys: u64, slot_id: u8, deconfigure: bool) -> [u32; 4] {
    let mut control = u32::from(slot_id) << 24;
    if deconfigure {
        control |= 1 << 9;
    }
    [
        input_context_phys as u32,
        (input_context_phys >> 32) as u32,
        0,
        set_type(control, ty::CONFIGURE_ENDPOINT),
    ]
}

/// Evaluate Context command (used to update EP0's Max Packet Size once the
/// device descriptor's `bMaxPacketSize0` is known).
#[must_use]
pub fn evaluate_context(input_context_phys: u64, slot_id: u8) -> [u32; 4] {
    [
        input_context_phys as u32,
        (input_context_phys >> 32) as u32,
        0,
        set_type(u32::from(slot_id) << 24, ty::EVALUATE_CONTEXT),
    ]
}

/// Reset Endpoint command — clears a halted (STALL) endpoint's state so the
/// transfer ring can be restarted with a Set TR Dequeue Pointer.
#[must_use]
pub fn reset_endpoint(slot_id: u8, endpoint_dci: u8) -> [u32; 4] {
    let control = (u32::from(slot_id) << 24) | (u32::from(endpoint_dci) << 16);
    [0, 0, 0, set_type(control, ty::RESET_ENDPOINT)]
}

/// No-Op command — used to prove the command ring / event ring loop works
/// before anything real is attempted.
#[must_use]
pub fn no_op_command() -> [u32; 4] {
    [0, 0, 0, set_type(0, ty::NO_OP_CMD)]
}

/// The 8-byte USB SETUP packet as its two dwords, for a Setup Stage TRB.
#[must_use]
pub fn setup_packet(bm_request_type: u8, b_request: u8, w_value: u16, w_index: u16, w_length: u16) -> [u32; 2] {
    [
        u32::from(bm_request_type) | (u32::from(b_request) << 8) | (u32::from(w_value) << 16),
        u32::from(w_index) | (u32::from(w_length) << 16),
    ]
}

/// Transfer Type for a control transfer's Setup Stage / Status Stage (xHCI
/// §6.4.1.2.1, TRT field).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlDir {
    /// No Data stage.
    NoData,
    /// Data stage moves host → device.
    Out,
    /// Data stage moves device → host.
    In,
}

impl ControlDir {
    fn trt(self) -> u32 {
        match self {
            Self::NoData => 0,
            Self::Out => 2,
            Self::In => 3,
        }
    }

    fn data_dir_in(self) -> bool {
        matches!(self, Self::In)
    }
}

/// Setup Stage TRB (always Immediate Data). `dir` sets the Transfer Type so the
/// controller knows whether a Data Stage follows and which way it goes.
#[must_use]
pub fn setup_stage(packet: [u32; 2], dir: ControlDir) -> [u32; 4] {
    let control = (1 << 6) // IDT — the parameter IS the 8 setup bytes
        | (dir.trt() << 16);
    [
        packet[0],
        packet[1],
        8, // TRB Transfer Length is always 8 for a setup packet
        set_type(control, ty::SETUP_STAGE),
    ]
}

/// Data Stage TRB. `chain` links it to the following Status Stage as one TD.
#[must_use]
pub fn data_stage(buffer_phys: u64, len: u32, dir: ControlDir, interrupt_on_complete: bool) -> [u32; 4] {
    let mut control = 0u32;
    if dir.data_dir_in() {
        control |= 1 << 16; // DIR = IN
    }
    if interrupt_on_complete {
        control |= 1 << 5; // IOC
    }
    [
        buffer_phys as u32,
        (buffer_phys >> 32) as u32,
        len & 0x1_ffff, // TRB Transfer Length (bits 16:0); TD Size left 0
        set_type(control, ty::DATA_STAGE),
    ]
}

/// Status Stage TRB. The status direction is opposite the data direction; for a
/// no-data control transfer it is IN.
#[must_use]
pub fn status_stage(data_dir: ControlDir, interrupt_on_complete: bool) -> [u32; 4] {
    // Status stage direction: IN unless the data stage was IN.
    let dir_in = !matches!(data_dir, ControlDir::In);
    let mut control = 0u32;
    if dir_in {
        control |= 1 << 16;
    }
    if interrupt_on_complete {
        control |= 1 << 5;
    }
    [0, 0, 0, set_type(control, ty::STATUS_STAGE)]
}

/// Normal TRB, for a bulk transfer.
///
/// A single TRB's data buffer must not cross a 64 KiB boundary within `len`
/// (xHCI §4.11.2.3); a buffer that would is split into two Normal TRBs with
/// `chain` set on the first (see [`normal_split`]). `interrupt_on_complete` and
/// the short-packet interrupt (`ISP`) belong on the **last** TRB of a TD.
#[must_use]
pub fn normal(buffer_phys: u64, len: u32, interrupt_on_complete: bool) -> [u32; 4] {
    normal_split(buffer_phys, len, false, interrupt_on_complete)
}

/// As [`normal`], but with the Chain (`CH`) bit under caller control — set it on
/// every TRB of a multi-TRB TD except the last.
#[must_use]
pub fn normal_split(buffer_phys: u64, len: u32, chain: bool, interrupt_on_complete: bool) -> [u32; 4] {
    let mut control = 0u32;
    if chain {
        control |= 1 << 4; // CH
    } else {
        control |= 1 << 2; // ISP — interrupt on a short packet (last TRB only)
        if interrupt_on_complete {
            control |= 1 << 5; // IOC
        }
    }
    [
        buffer_phys as u32,
        (buffer_phys >> 32) as u32,
        len & 0x1_ffff, // TRB Transfer Length; TD Size 0
        set_type(control, ty::NORMAL),
    ]
}

/// Split a data buffer into one or two Normal TRBs at the 64 KiB boundary it
/// would otherwise straddle. Returns `(count, [trb; 2])` with `count` 1 or 2.
#[must_use]
pub fn data_trbs(buffer_phys: u64, len: u32) -> (usize, [[u32; 4]; 2]) {
    let to_boundary = 0x1_0000 - (buffer_phys & 0xffff);
    if u64::from(len) <= to_boundary {
        return (1, [normal_split(buffer_phys, len, false, true), [0; 4]]);
    }
    let first = to_boundary as u32;
    (
        2,
        [
            normal_split(buffer_phys, first, true, false),
            normal_split(buffer_phys + u64::from(first), len - first, false, true),
        ],
    )
}

/// Link TRB pointing at `target_phys` (usually the ring's own base, to make it
/// circular). `toggle_cycle` must be set on the Link that sits at the physical
/// end of the ring segment.
#[must_use]
pub fn link(target_phys: u64, toggle_cycle: bool) -> [u32; 4] {
    let mut control = 0u32;
    if toggle_cycle {
        control |= 1 << 1; // TC
    }
    [
        target_phys as u32,
        (target_phys >> 32) as u32,
        0,
        set_type(control, ty::LINK),
    ]
}

// ===========================================================================
// Event decoding
// ===========================================================================

/// A decoded event-ring TRB (xHCI §6.4.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// A transfer on `slot`/`endpoint_dci` finished. `residual` is the number
    /// of bytes **not** transferred (subtract from the requested length).
    /// `trb_pointer` is the physical address of the transfer TRB it refers to.
    Transfer {
        completion_code: u8,
        slot: u8,
        endpoint_dci: u8,
        residual: u32,
        trb_pointer: u64,
        event_data: bool,
    },
    /// A command finished. `trb_pointer` is the physical address of the command
    /// TRB, so the glue can match it to the command it issued.
    CommandCompletion {
        completion_code: u8,
        slot: u8,
        trb_pointer: u64,
    },
    /// A root-hub port changed state. `port` is 1-based.
    PortStatusChange { port: u8, completion_code: u8 },
    /// A TRB type this driver does not handle.
    Other { trb_type: u32 },
}

impl Event {
    /// Decode a raw event TRB. Returns `None` only for a genuinely malformed
    /// TRB (type 0); unknown-but-well-formed types become [`Event::Other`].
    #[must_use]
    pub fn decode(trb: [u32; 4]) -> Option<Self> {
        let t = trb_type_of(trb[3]);
        let ptr = u64::from(trb[0]) | (u64::from(trb[1]) << 32);
        match t {
            0 => None,
            ty::TRANSFER_EVENT => Some(Self::Transfer {
                completion_code: (trb[2] >> 24) as u8,
                slot: (trb[3] >> 24) as u8,
                endpoint_dci: ((trb[3] >> 16) & 0x1f) as u8,
                residual: trb[2] & 0x00ff_ffff,
                trb_pointer: ptr,
                event_data: trb[3] & (1 << 2) != 0,
            }),
            ty::COMMAND_COMPLETION_EVENT => Some(Self::CommandCompletion {
                completion_code: (trb[2] >> 24) as u8,
                slot: (trb[3] >> 24) as u8,
                trb_pointer: ptr & !0xf,
            }),
            ty::PORT_STATUS_CHANGE_EVENT => Some(Self::PortStatusChange {
                port: (trb[0] >> 24) as u8,
                completion_code: (trb[2] >> 24) as u8,
            }),
            other => Some(Self::Other { trb_type: other }),
        }
    }
}

// ===========================================================================
// Ring bookkeeping
// ===========================================================================

/// The cycle-bit state of a producer ring (command ring, or a per-endpoint
/// transfer ring). The caller owns the TRB storage; this owns where the next
/// TRB goes and which cycle bit it carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProducerRing {
    /// Usable TRB slots: the backing array's length minus the one reserved for
    /// the Link TRB at the end.
    capacity: usize,
    enqueue: usize,
    cycle: bool,
}

/// The outcome of [`ProducerRing::enqueue`]: where to write `trb`, and — when
/// the ring wrapped — the Link TRB to refresh before ringing the doorbell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Enqueued {
    /// Backing-array index to write `trb` into.
    pub index: usize,
    /// The TRB, with its cycle bit set to the ring's live value.
    pub trb: [u32; 4],
    /// The physical address the controller will next read from — the address
    /// of `trb`. For a command TRB this is what the completion event's
    /// `trb_pointer` will equal; for a transfer, what to program if the
    /// endpoint needs a Set TR Dequeue Pointer.
    pub trb_phys: u64,
    /// When `Some`: also write these four dwords into the Link slot (index
    /// `capacity`) before the doorbell. Its cycle bit has been advanced so the
    /// controller follows it exactly once.
    pub link: Option<[u32; 4]>,
}

impl ProducerRing {
    /// `array_len` is the length of the backing `[[u32; 4]; N]` — the last slot
    /// is reserved for the Link TRB. Initial Producer Cycle State is 1, which
    /// is what `CRCR.RCS` / the endpoint context's DCS must also be set to.
    #[must_use]
    pub fn new(array_len: usize) -> Self {
        debug_assert!(array_len >= 2);
        Self { capacity: array_len - 1, enqueue: 0, cycle: true }
    }

    /// Producer Cycle State — write this into `CRCR.RCS` for the command ring,
    /// or the endpoint context's Dequeue Cycle State for a transfer ring.
    #[must_use]
    pub fn initial_cycle(&self) -> bool {
        // Always 1 for a freshly-constructed ring; kept as a method so call
        // sites read clearly.
        true
    }

    /// Index the next TRB will be written to (its Dequeue Pointer position).
    #[must_use]
    pub fn enqueue_index(&self) -> usize {
        self.enqueue
    }

    /// Place `trb` (built by one of the free functions above, cycle bit clear)
    /// on the ring. `ring_phys` is the backing array's physical base.
    #[must_use]
    pub fn enqueue(&mut self, mut trb: [u32; 4], ring_phys: u64) -> Enqueued {
        if self.cycle {
            trb[3] |= CYCLE;
        } else {
            trb[3] &= !CYCLE;
        }
        let index = self.enqueue;
        let trb_phys = ring_phys + (index as u64) * 16;
        self.enqueue += 1;

        let mut link_refresh = None;
        if self.enqueue == self.capacity {
            // Refresh the Link TRB with the *current* cycle so the controller
            // follows it once, then toggle our state (the Link's TC bit makes
            // the controller do the same).
            let mut l = link(ring_phys, true);
            if self.cycle {
                l[3] |= CYCLE;
            } else {
                l[3] &= !CYCLE;
            }
            link_refresh = Some(l);
            self.enqueue = 0;
            self.cycle = !self.cycle;
        }

        Enqueued { index, trb, trb_phys, link: link_refresh }
    }
}

/// The cycle-bit state of the event ring (consumer side). One segment, no Link
/// TRB — the driver wraps at the segment boundary and toggles its own Consumer
/// Cycle State (xHCI §4.9.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConsumerRing {
    capacity: usize,
    dequeue: usize,
    cycle: bool,
}

impl ConsumerRing {
    /// `segment_trbs` is the number of TRBs in the single event-ring segment.
    /// Initial Consumer Cycle State is 1.
    #[must_use]
    pub fn new(segment_trbs: usize) -> Self {
        debug_assert!(segment_trbs >= 1);
        Self { capacity: segment_trbs, dequeue: 0, cycle: true }
    }

    /// The dequeue index — multiply by 16 and add the segment base for the
    /// `ERDP` value to write after draining.
    #[must_use]
    pub fn dequeue_index(&self) -> usize {
        self.dequeue
    }

    /// If the TRB at the current dequeue position belongs to the controller
    /// (its cycle bit matches ours), decode and consume it. Returns `None` when
    /// the ring is empty — the caller stops draining and writes `ERDP`.
    #[must_use]
    pub fn poll(&mut self, trb: [u32; 4]) -> Option<Event> {
        let owned = (trb[3] & CYCLE != 0) == self.cycle;
        if !owned {
            return None;
        }
        let event = Event::decode(trb);
        self.dequeue += 1;
        if self.dequeue == self.capacity {
            self.dequeue = 0;
            self.cycle = !self.cycle;
        }
        event
    }
}
