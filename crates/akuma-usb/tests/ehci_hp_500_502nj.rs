//! Golden fixtures: EHCI controller `00:1a.0` (Intel 8 Series, `8086:8c2d`) on
//! the HP 500-502nj — the controller the keyboard is on.
//!
//! Register values were read on 2026-09-05 from a read-only mmap of the BAR
//! (`/sys/bus/pci/devices/0000:00:1a.0/resource0`) while Linux was driving the
//! controller, and `setpci` for the extended-capability dword in PCI config
//! space. `XUSB2PRM` on the xHCI read `0x00000000`, so the keyboard can never
//! move off EHCI — hence a split-transaction interrupt QH is the real target.

use akuma_usb::ehci::{
    self, CapabilityRegisters, EndpointSpeed, InterruptInQtd, InterruptQueueHead, LineStatus,
    PortSc, UsbCmd, UsbLegSup, UsbSts,
};

/// First 12 bytes of the BAR: CAPLENGTH=0x20, HCIVERSION=0x0100,
/// HCSPARAMS=0x00200002, HCCPARAMS=0x00036881.
const CAP_REGS: &[u8] = &[
    0x20, 0x00, 0x00, 0x01, 0x02, 0x00, 0x20, 0x00, 0x81, 0x68, 0x03, 0x00,
];

/// Operational registers, live, while Linux ran the controller.
const USBCMD: u32 = 0x0001_0011;
const USBSTS: u32 = 0x0000_6008;
/// `PORTSC1` — the port the rate-matching hub (a high-speed device) is on.
const PORTSC1: u32 = 0x0000_1005;
/// `USBLEGSUP` at PCI config offset 0x68 (== `HCCPARAMS.EECP`).
const USBLEGSUP: u32 = 0x0000_0001;

#[test]
fn capability_registers_decode() {
    let caps = CapabilityRegisters::parse(CAP_REGS).expect("caps");
    assert_eq!(caps.cap_length, 0x20);
    assert_eq!(caps.operational_base(), 0x20);
    assert_eq!(caps.hci_version, 0x0100);

    assert_eq!(caps.hcs_params.n_ports(), 2);
    assert_eq!(caps.hcs_params.debug_port_number(), 2);
    assert!(!caps.hcs_params.port_routing_rules());

    assert!(caps.hcc_params.addr64());
    assert_eq!(caps.hcc_params.isochronous_scheduling_threshold(), 8);
    assert_eq!(caps.hcc_params.extended_capabilities_pointer(), 0x68);
}

#[test]
fn an_absent_controller_reads_all_ones() {
    assert!(CapabilityRegisters::parse(&[0xff; 12]).is_none());
}

#[test]
fn operational_offsets_are_relative_to_caplength() {
    assert_eq!(ehci::op::USBCMD, 0x00);
    assert_eq!(ehci::op::CONFIGFLAG, 0x40);
    assert_eq!(ehci::op::portsc(0), 0x44);
    assert_eq!(ehci::op::portsc(1), 0x48);
}

#[test]
fn usbcmd_shows_a_running_controller_with_the_periodic_schedule_on() {
    let cmd = UsbCmd(USBCMD);
    assert!(cmd.running());
    assert!(!cmd.resetting());
    assert!(cmd.periodic_schedule_enabled());
    assert_eq!(cmd.frame_list_entries(), 1024);
    assert_eq!(cmd.interrupt_threshold(), 0x01);
}

#[test]
fn usbsts_shows_a_live_not_halted_controller() {
    let sts = UsbSts(USBSTS);
    assert!(!sts.halted());
    assert!(!sts.error_interrupt());
}

#[test]
fn portsc_shows_a_connected_enabled_high_speed_downstream_hub() {
    let p = PortSc(PORTSC1);
    assert!(p.connected());
    assert!(p.enabled());
    assert!(p.powered());
    assert!(!p.owned_by_companion());
    assert!(!p.resetting());
    assert_eq!(p.line_status(), LineStatus::Se0);
    assert!(!p.is_low_speed_device());
}

#[test]
fn portsc_read_modify_write_helpers_never_clear_a_change_bit_by_accident() {
    // A port that just saw a connect: CCS + CSC + PED + PP.
    let p = PortSc(0x0000_1007);
    assert!(p.connect_changed());
    // Beginning a reset must not ack the connect change...
    assert_eq!(p.with_reset_asserted() & PortSc::CONNECT_CHANGE, 0);
    assert_eq!(p.with_reset_asserted() & PortSc::PORT_RESET, PortSc::PORT_RESET);
    assert_eq!(p.with_reset_asserted() & PortSc::PORT_ENABLED, 0, "reset disables the port");
    // ...but acknowledging it explicitly does.
    assert_eq!(
        p.acknowledging_connect_change() & PortSc::CONNECT_CHANGE,
        PortSc::CONNECT_CHANGE
    );
}

#[test]
fn usblegsup_is_the_legacy_support_capability_and_nobody_owns_it() {
    let leg = UsbLegSup(USBLEGSUP);
    assert!(leg.is_legacy_support());
    assert_eq!(leg.capability_id(), UsbLegSup::CAP_ID_LEGACY_SUPPORT);
    assert_eq!(leg.next_capability(), 0);
    assert!(!leg.bios_owned());
    assert!(!leg.os_owned());
    // The write the driver would issue to claim it for the OS.
    assert_eq!(leg.claiming_for_os(), 0x0100_0001);
    // A BIOS-owned controller: claim, then wait for bios_owned to clear.
    let bios = UsbLegSup(0x0001_0001);
    assert!(bios.bios_owned() && !bios.handoff_complete());
    assert_eq!(bios.claiming_for_os(), 0x0101_0001);
    assert!(UsbLegSup(0x0100_0001).handoff_complete());
}

#[test]
fn interrupt_period_and_stride_math() {
    // The keyboard: full speed, bInterval 1 -> every frame.
    assert_eq!(ehci::interrupt_period_frames(1, EndpointSpeed::Full), 1);
    assert_eq!(ehci::frame_list_stride(1), 1);
    // High speed bInterval is an exponent of microframes.
    assert_eq!(ehci::interrupt_period_frames(4, EndpointSpeed::High), 1);
    assert_eq!(ehci::interrupt_period_frames(8, EndpointSpeed::High), 16);
    // Stride rounds down to a power of two, capped at the 1024-entry list.
    assert_eq!(ehci::frame_list_stride(3), 2);
    assert_eq!(ehci::frame_list_stride(1000), 512);
    assert_eq!(ehci::frame_list_stride(4096), 1024);
}

#[test]
fn frame_list_entries_carry_the_qh_type_tag() {
    assert_eq!(ehci::frame_list_qh_entry(0x0020_0040), 0x0020_0042);
    assert_eq!(ehci::frame_list_qh_entry(0x0020_005f), 0x0020_0042, "low 5 bits are alignment");
    assert_eq!(ehci::frame_list_terminator(), 1);
}

/// The queue head for the ROCCAT's interrupt IN endpoint: full-speed device 4,
/// endpoint 1, behind the TT at hub address 2 port 6, start-split in
/// microframe 0 and complete-splits in microframes 2-4.
#[test]
fn split_transaction_queue_head_dwords() {
    let qh = InterruptQueueHead {
        device_address: 4,
        endpoint_number: 1,
        max_packet_size: 8,
        speed: EndpointSpeed::Full,
        tt_hub_address: 2,
        tt_port_number: 6,
        s_mask: 0x01,
        c_mask: 0x1C,
        first_qtd_phys: 0x0020_0000,
        horizontal_link_phys: Some(0x0020_0040),
    };
    let w = qh.build();

    // dword 0: horizontal link -> 0x200040, Typ = QH (01b), T = 0.
    assert_eq!(w[0], 0x0020_0042);
    // dword 1: dev 4, ep 1<<8, EPS full (00), DTC 1<<14, MPL 8<<16.
    assert_eq!(w[1], 0x0008_4104);
    // dword 2: S-mask 0x01, C-mask 0x1C<<8, hub 2<<16, port 6<<23, Mult 1<<30.
    assert_eq!(w[2], 0x4302_1C01);
    // overlay: current qTD 0, next qTD = first_qtd_phys, alt = T.
    assert_eq!(w[3], 0);
    assert_eq!(w[4], 0x0020_0000);
    assert_eq!(w[5], 1);
    assert_eq!(&w[6..], &[0, 0, 0, 0, 0, 0]);
}

#[test]
fn queue_head_with_no_qtd_terminates_the_overlay() {
    let qh = InterruptQueueHead {
        device_address: 4,
        endpoint_number: 1,
        max_packet_size: 8,
        speed: EndpointSpeed::Full,
        tt_hub_address: 2,
        tt_port_number: 6,
        s_mask: 0x01,
        c_mask: 0x1C,
        first_qtd_phys: 0,
        horizontal_link_phys: None,
    };
    let w = qh.build();
    assert_eq!(w[0], 1, "horizontal link T bit");
    assert_eq!(w[4], 1, "next qTD T bit");
}

#[test]
fn interrupt_in_qtd_dwords() {
    let qtd = InterruptInQtd {
        buffer_phys: 0x0030_0000,
        length: 8,
        data_toggle: false,
        interrupt_on_complete: true,
    };
    let w = qtd.build();
    assert_eq!(w[0], 1, "next qTD = T");
    assert_eq!(w[1], 1, "alternate next qTD = T");
    // token: Active(0x80) | PID_IN(0x100) | CERR=3(0xC00) | IOC(0x8000) | len 8<<16.
    assert_eq!(w[2], 0x0008_8D80);
    assert_eq!(w[3], 0x0030_0000, "buffer pointer 0");
    assert_eq!(&w[4..], &[0, 0, 0, 0]);

    // With the data toggle set, bit 31 of the token flips.
    let toggled = InterruptInQtd { data_toggle: true, ..qtd }.build();
    assert_eq!(toggled[2], 0x8008_8D80);
}

#[test]
fn qtd_token_completion_readback() {
    use ehci::qtd_token;
    // Controller wrote back: not active, IOC, 0 bytes remaining, no errors.
    let done = qtd_token::IOC;
    assert!(!qtd_token::is_active(done));
    assert_eq!(qtd_token::bytes_remaining(done), 0);
    assert_eq!(done & qtd_token::ERROR_MASK, 0);
    // A halted transfer with a transaction error.
    let bad = qtd_token::HALTED | qtd_token::TRANSACTION_ERROR;
    assert_ne!(bad & qtd_token::ERROR_MASK, 0);
}
