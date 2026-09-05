//! Golden fixtures: xHCI controller `00:14.0` (Intel 8 Series / C220, `8086:8c31`,
//! HCIVERSION 1.0.0) on the HP 500-502nj — the controller the USB-to-SATA
//! enclosure sits on.
//!
//! Register values were read on 2026-09-06 from a read-only mmap of BAR0
//! (`/sys/bus/pci/devices/0000:00:14.0/resource0`) while Linux drove the
//! controller, plus `setpci` for PCI config space and `lsusb -v` for the
//! device's own descriptors. The enclosure (ASMedia `174c:55aa`) was on
//! **root-hub port 20**, SuperSpeed, BOT interface, bulk EP `0x81` IN / `0x02`
//! OUT, `wMaxPacketSize` 1024, `bMaxBurst` 15.

use akuma_xhci::context::{dci, input_control_context, EndpointConfig, EpType, SlotConfig};
use akuma_xhci::regs::{CapabilityRegisters, PortSc};
use akuma_xhci::trb::{self, cc, ConsumerRing, ControlDir, Event, ProducerRing};
use akuma_xhci::xcap::{self, SupportedProtocol, UsbLegSup};
use akuma_xhci::Speed;

/// First 0x20 bytes of BAR0, little-endian:
/// CAPLENGTH=0x80, HCIVERSION=0x0100, HCSPARAMS1=0x15000820,
/// HCSPARAMS2=0x84000054, HCSPARAMS3=0x0200000a, HCCPARAMS1=0x200077c1,
/// DBOFF=0x00003000, RTSOFF=0x00002000, HCCPARAMS2=0x00000000.
const CAP_REGS: &[u8] = &[
    0x80, 0x00, 0x00, 0x01, // CAPLENGTH, rsvd, HCIVERSION
    0x20, 0x08, 0x00, 0x15, // HCSPARAMS1
    0x54, 0x00, 0x00, 0x84, // HCSPARAMS2
    0x0a, 0x00, 0x00, 0x02, // HCSPARAMS3
    0xc1, 0x77, 0x00, 0x20, // HCCPARAMS1
    0x00, 0x30, 0x00, 0x00, // DBOFF
    0x00, 0x20, 0x00, 0x00, // RTSOFF
    0x00, 0x00, 0x00, 0x00, // HCCPARAMS2
];

/// Operational registers, live, while Linux ran the controller.
const USBSTS_RUNNING: u32 = 0x0000_0000;
const CONFIG_LIVE: u32 = 0x0000_0020;
/// `PORTSC[20]` — the SuperSpeed port the enclosure was on.
const PORTSC20: u32 = 0x0000_1203;

#[test]
fn capability_registers_decode() {
    let caps = CapabilityRegisters::parse(CAP_REGS).expect("caps");
    assert_eq!(caps.cap_length, 0x80);
    assert_eq!(caps.operational_base(), 0x80);
    assert_eq!(caps.hci_version, 0x0100);

    assert_eq!(caps.hcs_params1.max_slots(), 32);
    assert_eq!(caps.hcs_params1.max_interrupters(), 8);
    assert_eq!(caps.hcs_params1.max_ports(), 21);

    assert_eq!(caps.hcs_params2.erst_max(), 5);
    assert_eq!(
        caps.hcs_params2.max_scratchpad_buffers(),
        16,
        "the controller demands a 16-entry scratchpad buffer array"
    );

    assert!(caps.hcc_params1.addr64());
    assert!(
        !caps.hcc_params1.context_size_64(),
        "CSZ=0: 32-byte contexts on this part"
    );
    assert_eq!(caps.hcc_params1.context_bytes(), 32);
    assert_eq!(caps.hcc_params1.ext_cap_offset(), 0x8000);

    assert_eq!(caps.db_offset, 0x3000);
    assert_eq!(caps.rts_offset, 0x2000);
}

#[test]
fn an_absent_controller_reads_all_ones() {
    assert!(CapabilityRegisters::parse(&[0xff; 0x20]).is_none());
    assert!(CapabilityRegisters::parse(&[0x00; 0x20]).is_none(), "CAPLENGTH 0 is not a real controller");
}

#[test]
fn operational_offsets() {
    assert_eq!(akuma_xhci::regs::op::USBCMD, 0x00);
    assert_eq!(akuma_xhci::regs::op::CRCR, 0x18);
    assert_eq!(akuma_xhci::regs::op::DCBAAP, 0x30);
    assert_eq!(akuma_xhci::regs::op::CONFIG, 0x38);
    // PORTSC for 1-based port n = 0x400 + (n-1)*0x10.
    assert_eq!(akuma_xhci::regs::op::portsc(1), 0x400);
    assert_eq!(akuma_xhci::regs::op::portsc(20), 0x400 + 19 * 0x10);
}

#[test]
fn usbsts_shows_a_ready_not_halted_controller() {
    use akuma_xhci::regs::usbsts;
    assert_eq!(USBSTS_RUNNING & usbsts::CNR, 0, "controller ready");
    assert_eq!(USBSTS_RUNNING & usbsts::HCH, 0, "not halted");
    assert_eq!(CONFIG_LIVE & 0xff, 32, "Linux enabled all 32 slots");
}

#[test]
fn portsc20_is_a_connected_enabled_superspeed_port() {
    let p = PortSc(PORTSC20);
    assert!(p.connected());
    assert!(p.enabled());
    assert!(p.powered());
    assert!(!p.resetting());
    assert_eq!(p.speed_field(), 4);
    assert_eq!(Speed::from_field(p.speed_field()), Some(Speed::Super));
    assert_eq!(Speed::Super.default_ep0_max_packet(), 512);
}

#[test]
fn portsc_rmw_helpers_never_disable_the_port_or_eat_a_change() {
    // A port that just completed a reset: CCS | PED | PP | PRC | CSC.
    let p = PortSc(PortSc::CCS | PortSc::PED | PortSc::PP | PortSc::PRC | PortSc::CSC);
    // A preserving write keeps power and never writes PED=1 (which disables).
    let w = p.preserving_write();
    assert_eq!(w & PortSc::PED, 0, "must not write PED=1");
    assert_eq!(w & PortSc::PP, PortSc::PP, "keeps port power");
    assert_eq!(w & PortSc::RW1C_MASK, 0, "does not ack a change");
    // Asserting reset keeps power, sets PR, acks nothing.
    let r = p.with_reset_asserted();
    assert_eq!(r & PortSc::PR, PortSc::PR);
    assert_eq!(r & PortSc::PP, PortSc::PP);
    assert_eq!(r & PortSc::RW1C_MASK, 0);
    // Acknowledging a reset clears PRC and CSC.
    let a = p.acknowledging_reset();
    assert_eq!(a & PortSc::PRC, PortSc::PRC);
    assert_eq!(a & PortSc::CSC, PortSc::CSC);
}

#[test]
fn extended_capability_walk_matches_the_box() {
    // (byte offset, header dword) as read from the BAR.
    let caps = [
        (0x8000usize, 0x0200_0802u32), // Supported Protocol, USB 2.0
        (0x8020, 0x0300_0802),          // Supported Protocol, USB 3.0
        (0x8040, 0x0001_0cc1),          // Intel-specific 0xC1
        (0x8070, 0x0000_ffc0),          // Intel-specific 0xC0
        (0x846c, 0x0000_0001),          // USBLEGSUP, end of list
    ];
    for (i, &(off, hdr)) in caps.iter().enumerate() {
        let next = xcap::next_cap_offset(off, hdr);
        if i + 1 < caps.len() {
            assert_eq!(next, Some(caps[i + 1].0), "cap at {off:#x} points at the next");
        } else {
            assert_eq!(next, None, "USBLEGSUP ends the list");
        }
    }
    assert_eq!(xcap::cap_id(caps[0].1), xcap::CAP_ID_SUPPORTED_PROTOCOL);
    assert_eq!(xcap::cap_id(caps[4].1), xcap::CAP_ID_LEGACY_SUPPORT);
}

#[test]
fn supported_protocol_says_port_20_is_superspeed() {
    // USB 3.0 block: header 0x03000802, name "USB ", dw2 = offset 16 + count 6.
    let name = u32::from_le_bytes(*b"USB ");
    let usb3 = SupportedProtocol::parse(0x0300_0802, name, (6 << 8) | 16, 0);
    assert_eq!(usb3.major, 3);
    assert_eq!(&usb3.name, b"USB ");
    assert_eq!(usb3.port_offset, 16);
    assert_eq!(usb3.port_count, 6);
    assert!(usb3.is_superspeed());
    assert!(usb3.covers_port(20));
    assert!(usb3.covers_port(21));
    assert!(!usb3.covers_port(22));
    assert!(!usb3.covers_port(15));

    let usb2 = SupportedProtocol::parse(0x0200_0802, name, (14 << 8) | 1, 0);
    assert_eq!(usb2.major, 2);
    assert!(!usb2.is_superspeed());
    assert!(usb2.covers_port(1));
    assert!(usb2.covers_port(14));
    assert!(!usb2.covers_port(20));
}

#[test]
fn usblegsup_handoff() {
    // The box read 0x00000001: nobody owns it (Linux had already released).
    let leg = UsbLegSup(0x0000_0001);
    assert!(!leg.bios_owned());
    assert!(!leg.os_owned());
    // Claiming sets bit 24.
    assert_eq!(leg.claiming_for_os(), 0x0100_0001);
    // A BIOS-owned controller: claim, then wait for bios_owned to clear.
    let bios = UsbLegSup(0x0001_0001);
    assert!(bios.bios_owned() && !bios.handoff_complete());
    assert_eq!(bios.claiming_for_os(), 0x0101_0001);
    assert!(UsbLegSup(0x0100_0001).handoff_complete());
    // USBLEGCTLSTS on the box read 0x40000000; disabling all SMIs clears the
    // enables (none set here) and acks bits 29/30/31.
    let ctl = xcap::usblegctlsts_disable_all(0x4000_0000);
    assert_eq!(ctl & ((1 << 0) | (1 << 4) | (1 << 13) | (1 << 14) | (1 << 15) | (1 << 16)), 0);
    assert_eq!(ctl & ((1 << 29) | (1 << 30) | (1 << 31)), (1 << 29) | (1 << 30) | (1 << 31));
}

// ---------------------------------------------------------------------------
// TRB encoding
// ---------------------------------------------------------------------------

fn trb_type(control: u32) -> u32 {
    (control >> 10) & 0x3f
}

#[test]
fn command_trb_encodings() {
    let en = trb::enable_slot(0);
    assert_eq!(en, [0, 0, 0, 9 << 10]);
    assert_eq!(trb_type(en[3]), trb::ty::ENABLE_SLOT);

    let ad = trb::address_device(0x1_2340, 5, false);
    assert_eq!(ad[0], 0x1_2340);
    assert_eq!(ad[1], 0);
    assert_eq!(trb_type(ad[3]), trb::ty::ADDRESS_DEVICE);
    assert_eq!(ad[3] >> 24, 5, "slot id in bits 31:24");
    assert_eq!(ad[3] & (1 << 9), 0, "BSR clear");
    assert_eq!(trb::address_device(0, 5, true)[3] & (1 << 9), 1 << 9, "BSR set");

    let ce = trb::configure_endpoint(0x9_0000, 5, false);
    assert_eq!(ce[0], 0x9_0000);
    assert_eq!(trb_type(ce[3]), trb::ty::CONFIGURE_ENDPOINT);
    assert_eq!(ce[3] >> 24, 5);
}

#[test]
fn control_transfer_td_encoding() {
    // GET_DESCRIPTOR(device), the first control transfer enumeration makes.
    let pkt = trb::setup_packet(0x80, 6, 0x0100, 0, 18);
    assert_eq!(pkt, [0x0100_0680, 0x0012_0000]);

    let setup = trb::setup_stage(pkt, ControlDir::In);
    assert_eq!(setup[0], 0x0100_0680);
    assert_eq!(setup[1], 0x0012_0000);
    assert_eq!(setup[2], 8, "setup TRB length is always 8");
    assert_eq!(trb_type(setup[3]), trb::ty::SETUP_STAGE);
    assert_eq!(setup[3] & (1 << 6), 1 << 6, "IDT set");
    assert_eq!((setup[3] >> 16) & 0x3, 3, "TRT = IN Data");

    let data = trb::data_stage(0x5_5000, 18, ControlDir::In, false);
    assert_eq!(data[0], 0x5_5000);
    assert_eq!(data[2], 18);
    assert_eq!(trb_type(data[3]), trb::ty::DATA_STAGE);
    assert_eq!(data[3] & (1 << 16), 1 << 16, "DIR = IN");

    // Status stage of an IN control transfer goes OUT (dir bit clear).
    let status = trb::status_stage(ControlDir::In, true);
    assert_eq!(trb_type(status[3]), trb::ty::STATUS_STAGE);
    assert_eq!(status[3] & (1 << 16), 0, "status of an IN transfer is OUT");
    assert_eq!(status[3] & (1 << 5), 1 << 5, "IOC set");
    // For a no-data transfer the status stage is IN.
    assert_eq!(trb::status_stage(ControlDir::NoData, true)[3] & (1 << 16), 1 << 16);
}

#[test]
fn bulk_normal_trb() {
    let n = trb::normal(0x10_0000, 4096, true);
    assert_eq!(n[0], 0x10_0000);
    assert_eq!(n[1], 0);
    assert_eq!(n[2], 4096);
    assert_eq!(trb_type(n[3]), trb::ty::NORMAL);
    assert_eq!(n[3] & (1 << 5), 1 << 5, "IOC");
    assert_eq!(n[3] & (1 << 2), 1 << 2, "ISP");
}

// ---------------------------------------------------------------------------
// Ring bookkeeping
// ---------------------------------------------------------------------------

#[test]
fn producer_ring_sets_cycle_and_wraps_with_a_link_trb() {
    const LEN: usize = 8; // 7 usable slots + 1 link
    let base = 0x20_0000u64;
    let mut ring = ProducerRing::new(LEN);
    assert!(ring.initial_cycle(), "PCS starts at 1");

    // First enqueue: index 0, cycle bit set, no link.
    let e = ring.enqueue(trb::no_op_command(), base);
    assert_eq!(e.index, 0);
    assert_eq!(e.trb[3] & 1, 1, "cycle bit set to PCS");
    assert_eq!(e.trb_phys, base);
    assert!(e.link.is_none());

    // Fill to the wrap. Slot 7 is the link, so the 7th enqueue (index 6) wraps.
    let mut last = e;
    for _ in 1..7 {
        last = ring.enqueue(trb::no_op_command(), base);
        assert_eq!(last.trb[3] & 1, 1);
    }
    assert_eq!(last.index, 6);
    let link = last.link.expect("wrap emits a link TRB");
    assert_eq!(trb_type(link[3]), trb::ty::LINK);
    assert_eq!(link[3] & (1 << 1), 1 << 1, "Toggle Cycle set");
    assert_eq!(link[3] & 1, 1, "link cycle bit written with the pre-wrap PCS");
    assert_eq!(u64::from(link[0]), base, "link points back at the ring base");

    // Next enqueue is back at index 0 with the toggled cycle (0).
    let after = ring.enqueue(trb::no_op_command(), base);
    assert_eq!(after.index, 0);
    assert_eq!(after.trb[3] & 1, 0, "cycle toggled after the link");
}

#[test]
fn consumer_ring_follows_the_cycle_bit() {
    const SEG: usize = 4;
    let mut evr = ConsumerRing::new(SEG);

    // A command-completion event the controller wrote with cycle 1.
    let mk = |cyc: u32, code: u8, slot: u8, ptr: u64| {
        [
            ptr as u32,
            (ptr >> 32) as u32,
            u32::from(code) << 24,
            (trb::ty::COMMAND_COMPLETION_EVENT << 10) | (u32::from(slot) << 24) | cyc,
        ]
    };

    let ev = evr.poll(mk(1, cc::SUCCESS, 3, 0x4_0000)).expect("owned event");
    match ev {
        Event::CommandCompletion { completion_code, slot, trb_pointer } => {
            assert_eq!(completion_code, cc::SUCCESS);
            assert_eq!(slot, 3);
            assert_eq!(trb_pointer, 0x4_0000);
        }
        other => panic!("wrong event: {other:?}"),
    }
    assert_eq!(evr.dequeue_index(), 1);

    // A TRB still carrying the *old* cycle (0) is not ours — stop draining.
    assert!(evr.poll(mk(0, cc::SUCCESS, 0, 0)).is_none());

    // Fill to the wrap; after SEG events the consumer cycle toggles to 0.
    for _ in 1..SEG {
        assert!(evr.poll(mk(1, cc::SUCCESS, 1, 0)).is_some());
    }
    assert_eq!(evr.dequeue_index(), 0, "wrapped");
    // Now events carry cycle 0.
    assert!(evr.poll(mk(1, cc::SUCCESS, 1, 0)).is_none(), "cycle 1 is stale after the wrap");
    assert!(evr.poll(mk(0, cc::SUCCESS, 1, 0)).is_some());
}

#[test]
fn transfer_event_decodes_residual_and_endpoint() {
    // A short read: asked for 512, 100 bytes not transferred, on slot 3 DCI 3.
    let trb = [
        0x7000u32,
        0,
        (u32::from(cc::SHORT_PACKET) << 24) | 100,
        (trb::ty::TRANSFER_EVENT << 10) | (3 << 24) | (3 << 16) | 1,
    ];
    match Event::decode(trb).expect("decodes") {
        Event::Transfer { completion_code, slot, endpoint_dci, residual, trb_pointer, .. } => {
            assert_eq!(completion_code, cc::SHORT_PACKET);
            assert_eq!(slot, 3);
            assert_eq!(endpoint_dci, 3);
            assert_eq!(residual, 100);
            assert_eq!(trb_pointer, 0x7000);
        }
        other => panic!("wrong event: {other:?}"),
    }
}

#[test]
fn port_status_change_event_carries_the_port_number() {
    let trb = [20u32 << 24, 0, 0, (trb::ty::PORT_STATUS_CHANGE_EVENT << 10) | 1];
    assert_eq!(Event::decode(trb), Some(Event::PortStatusChange { port: 20, completion_code: 0 }));
}

// ---------------------------------------------------------------------------
// Contexts
// ---------------------------------------------------------------------------

#[test]
fn doorbell_context_index() {
    assert_eq!(dci(0x00), 1, "EP0");
    assert_eq!(dci(0x81), 3, "EP1 IN — the enclosure's bulk IN");
    assert_eq!(dci(0x02), 4, "EP2 OUT — the enclosure's bulk OUT");
    assert_eq!(dci(0x83), 7);
}

#[test]
fn slot_context_for_a_superspeed_root_port_device() {
    let sc = SlotConfig { route_string: 0, speed: 4, root_hub_port: 20, context_entries: 1 }.build();
    // dword 0: speed 4 in bits 23:20, context entries 1 in bits 31:27.
    assert_eq!(sc[0] & (0xf << 20), 4 << 20);
    assert_eq!(sc[0] >> 27, 1);
    assert_eq!(sc[0] & 0x000f_ffff, 0, "route string 0");
    // dword 1: root hub port 20 in bits 23:16.
    assert_eq!((sc[1] >> 16) & 0xff, 20);
    assert_eq!(&sc[2..], &[0, 0, 0, 0, 0, 0]);
}

#[test]
fn endpoint_context_ep0_and_bulk_in() {
    let ep0 = EndpointConfig {
        ep_type: EpType::Control,
        max_packet_size: 512,
        max_burst: 0,
        tr_dequeue_phys: 0x4_0000,
        dequeue_cycle: true,
        average_trb_length: 8,
    }
    .build();
    // dword 1: CErr=3 (bits 2:1), EP type 4 (bits 5:3), max packet 512 (bits 31:16).
    assert_eq!((ep0[1] >> 1) & 0x3, 3, "CErr");
    assert_eq!((ep0[1] >> 3) & 0x7, EpType::Control as u32);
    assert_eq!(ep0[1] >> 16, 512);
    // dwords 2:3 — TR dequeue pointer | DCS.
    assert_eq!(ep0[2], 0x4_0001, "pointer | DCS");
    assert_eq!(ep0[3], 0);
    assert_eq!(ep0[4] & 0xffff, 8, "average TRB length");

    let bulk_in = EndpointConfig {
        ep_type: EpType::BulkIn,
        max_packet_size: 1024,
        max_burst: 15,
        tr_dequeue_phys: 0x5_0000,
        dequeue_cycle: true,
        average_trb_length: 3072,
    }
    .build();
    assert_eq!((bulk_in[1] >> 3) & 0x7, EpType::BulkIn as u32);
    assert_eq!((bulk_in[1] >> 8) & 0xff, 15, "max burst from the SS companion descriptor");
    assert_eq!(bulk_in[1] >> 16, 1024);
}

#[test]
fn ep_type_from_attributes() {
    assert_eq!(EpType::from_attributes(0, false), Some(EpType::Control));
    assert_eq!(EpType::from_attributes(2, true), Some(EpType::BulkIn));
    assert_eq!(EpType::from_attributes(2, false), Some(EpType::BulkOut));
    assert_eq!(EpType::from_attributes(1, true), None, "isoch not configured");
}

#[test]
fn input_control_context_add_flags() {
    // Address Device: add the slot context (A0) and EP0 (A1).
    let icc = input_control_context(akuma_xhci::context::add_flag(0) | akuma_xhci::context::add_flag(1), 0, 0);
    assert_eq!(icc[1], 0b11, "A0 | A1");
    assert_eq!(icc[0], 0, "no drops");

    // Configure Endpoint: add EP1 IN (DCI 3) and EP2 OUT (DCI 4), plus the slot
    // context (A0) whose Context Entries changed, config value 1.
    let add = akuma_xhci::context::add_flag(0)
        | akuma_xhci::context::add_flag(dci(0x81))
        | akuma_xhci::context::add_flag(dci(0x02));
    let icc = input_control_context(add, 0, 1);
    assert_eq!(icc[1], (1 << 0) | (1 << 3) | (1 << 4));
    assert_eq!(icc[7], 1, "configuration value");
}
