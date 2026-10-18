use super::*;
use crate::raw::imp::{Op, queue_read, reset, take_log};

fn w(port: u16, value: u32) -> Op {
    Op { write: true, port, width: 1, value }
}
fn r(port: u16, value: u32) -> Op {
    Op { write: false, port, width: 1, value }
}
fn w4(port: u16, value: u32) -> Op {
    Op { write: true, port, width: 4, value }
}
fn r4(port: u16, value: u32) -> Op {
    Op { write: false, port, width: 4, value }
}

fn range(name: &str) -> PortRange {
    *RANGES.iter().find(|p| p.name == name).unwrap_or_else(|| panic!("no range named {name}"))
}

/// Every access in `log` hit `name`'s range at its declared width.
fn assert_within(log: &[Op], names: &[&str]) {
    for op in log {
        let hit = names.iter().map(|n| range(n)).any(|p| {
            (p.first..=p.last).contains(&op.port) && p.width == op.width
        });
        assert!(hit, "{op:?} is outside {names:?}");
    }
}

// ---- the registry -------------------------------------------------------

/// `0xCF9` is a byte register that sits inside the dword `0xCF8..=0xCFB`. The PC
/// architecture allows that - the chipset tells a byte access to `0xCF9` from a
/// dword access to `0xCF8` - so it is the one overlap, named, rather than a
/// reason to loosen the check.
const KNOWN_ALIASES: [(&str, &str); 1] = [("reset-control", "pci-config-address")];

#[test]
fn no_two_devices_share_a_port_except_the_named_alias() {
    for (i, a) in RANGES.iter().enumerate() {
        assert!(a.first <= a.last, "{} is backwards", a.name);
        for b in &RANGES[i + 1..] {
            let overlap = a.first <= b.last && b.first <= a.last;
            let known = KNOWN_ALIASES
                .iter()
                .any(|&(x, y)| (x == a.name && y == b.name) || (x == b.name && y == a.name));
            assert!(!overlap || known, "{} and {} overlap", a.name, b.name);
        }
    }
}

#[test]
fn names_are_unique() {
    for (i, a) in RANGES.iter().enumerate() {
        assert!(RANGES[i + 1..].iter().all(|b| b.name != a.name), "duplicate {}", a.name);
    }
}

/// The architectural addresses, pinned. A typo here is a different device.
#[test]
fn the_architectural_addresses_are_what_the_pc_says() {
    let pins: [(&str, u16); 10] = [
        ("pic-master-mask", 0x21),
        ("pit", 0x40),
        ("i8042-data", 0x60),
        ("i8042-status-command", 0x64),
        ("cmos", 0x70),
        ("pic-slave-mask", 0xA1),
        ("debugcon", 0xE9),
        ("uart-com1", 0x3F8),
        ("pci-config-address", 0xCF8),
        ("pci-config-data", 0xCFC),
    ];
    for (name, first) in pins {
        assert_eq!(range(name).first, first, "{name}");
    }
    assert_eq!(range("reset-control").first, 0xCF9);
}

// ---- the empty bus ------------------------------------------------------

#[test]
fn an_absent_device_reads_as_all_ones() {
    reset();
    // The fact the PIT check and the i8042 probe both lean on.
    assert_eq!(I8042.status(), 0xFF);
    assert_eq!(COM1.read(UartReg::LineStatus), 0xFF);
    assert_eq!(PCI_CONFIG.read_dword(0x8000_0000), 0xFFFF_FFFF);
    assert!(PIT.channel0_output_high(), "an absent PIT looks already expired");
}

// ---- UART ---------------------------------------------------------------

#[test]
fn uart_registers_land_at_base_plus_offset() {
    reset();
    COM1.write(UartReg::Data, 0x41);
    COM1.write(UartReg::IntEnable, 0x00);
    COM1.write(UartReg::FifoCtrl, 0xC7);
    COM1.write(UartReg::LineCtrl, 0x03);
    COM1.write(UartReg::ModemCtrl, 0x0B);
    COM1.write(UartReg::Scratch, 0x5A);
    queue_read(0x3FD, 0x60);
    assert_eq!(COM1.read(UartReg::LineStatus), 0x60);
    let log = take_log();
    assert_eq!(
        log,
        [
            w(0x3F8, 0x41),
            w(0x3F9, 0x00),
            w(0x3FA, 0xC7),
            w(0x3FB, 0x03),
            w(0x3FC, 0x0B),
            w(0x3FF, 0x5A),
            r(0x3FD, 0x60),
        ]
    );
    assert_within(&log, &["uart-com1"]);
}

#[test]
fn the_scratch_probe_round_trips() {
    // The sequence `serial::init` runs: a chip hands back what it was given, an
    // empty bus hands back 0xFF for both patterns.
    reset();
    queue_read(0x3FF, 0x5A);
    queue_read(0x3FF, 0xA5);
    COM1.write(UartReg::Scratch, 0x5A);
    let a = COM1.read(UartReg::Scratch);
    COM1.write(UartReg::Scratch, 0xA5);
    let b = COM1.read(UartReg::Scratch);
    assert_eq!((a, b), (0x5A, 0xA5));

    reset();
    COM1.write(UartReg::Scratch, 0x5A);
    assert_eq!(COM1.read(UartReg::Scratch), 0xFF, "empty bus is not a 16550");
}

// ---- CMOS ---------------------------------------------------------------

#[test]
fn cmos_selects_then_transfers() {
    reset();
    queue_read(0x71, 0x42);
    assert_eq!(CMOS.read(0x09), 0x42);
    CMOS.write(0x0B, 0x82);
    let log = take_log();
    assert_eq!(log, [w(0x70, 0x09), r(0x71, 0x42), w(0x70, 0x0B), w(0x71, 0x82)]);
    assert_within(&log, &["cmos"]);
}

/// Bit 7 of the index port is "NMI disable". No caller may reach it by passing
/// a register number with the high bit set.
#[test]
fn cmos_never_lets_a_register_number_switch_nmi_off() {
    reset();
    let _ = CMOS.read(0x8A);
    CMOS.write(0xFF, 0x00);
    for op in take_log().iter().filter(|o| o.port == 0x70) {
        assert_eq!(op.value & 0x80, 0, "{op:?} set the NMI-disable bit");
    }
}

// ---- PIC ----------------------------------------------------------------

#[test]
fn masking_the_pics_writes_all_ones_to_both_mask_registers() {
    reset();
    PIC.mask_all();
    let log = take_log();
    assert_eq!(log, [w(0x21, 0xFF), w(0xA1, 0xFF)]);
    assert_within(&log, &["pic-master-mask", "pic-slave-mask"]);
}

// ---- PIT ----------------------------------------------------------------

#[test]
fn loading_channel_0_is_command_then_low_then_high() {
    reset();
    PIT.load_channel0_oneshot(0x1234);
    let log = take_log();
    // 0b0011_0000: channel 0, lobyte/hibyte, mode 0, binary.
    assert_eq!(log, [w(0x43, 0x30), w(0x40, 0x34), w(0x40, 0x12)]);
    assert_within(&log, &["pit"]);
}

#[test]
fn the_pit_status_read_latches_before_it_reads() {
    reset();
    queue_read(0x40, 0x00);
    queue_read(0x40, 0x80);
    assert!(!PIT.channel0_output_high(), "output low while counting");
    assert!(PIT.channel0_output_high(), "output high once expired");
    let log = take_log();
    assert_eq!(log[0], w(0x43, 0xE2), "read-back, status only, channel 0");
    assert_eq!(log[1], r(0x40, 0x00));
    assert_within(&log, &["pit"]);
}

#[test]
fn only_the_output_bit_decides_expiry() {
    reset();
    queue_read(0x40, 0x7F); // every other bit set
    assert!(!PIT.channel0_output_high());
}

// ---- i8042 --------------------------------------------------------------

#[test]
fn the_ps2_controller_status_data_and_command_ports() {
    reset();
    queue_read(0x64, 0x1D);
    queue_read(0x60, 0x1E);
    assert_eq!(I8042.status(), 0x1D);
    assert_eq!(I8042.read_data(), 0x1E);
    I8042.write_command(0xAE);
    let log = take_log();
    assert_eq!(log, [r(0x64, 0x1D), r(0x60, 0x1E), w(0x64, 0xAE)]);
    assert_within(&log, &["i8042-data", "i8042-status-command"]);
}

// ---- reset control ------------------------------------------------------

#[test]
fn a_hard_reset_is_system_reset_then_full_reset() {
    reset();
    RESET_CONTROL.request_hard_reset();
    let log = take_log();
    assert_eq!(log, [w(0xCF9, 0x02), w(0xCF9, 0x0E)]);
    assert_within(&log, &["reset-control"]);
}

// ---- PCI config ---------------------------------------------------------

#[test]
fn pci_config_selects_with_a_dword_then_moves_a_dword() {
    reset();
    queue_read(0xCFC, 0x1234_8086);
    assert_eq!(PCI_CONFIG.read_dword(0x8000_0000), 0x1234_8086);
    PCI_CONFIG.write_dword(0x8000_0804, 0x0000_0006);
    let log = take_log();
    assert_eq!(
        log,
        [
            w4(0xCF8, 0x8000_0000),
            r4(0xCFC, 0x1234_8086),
            w4(0xCF8, 0x8000_0804),
            w4(0xCFC, 0x0000_0006),
        ]
    );
    assert_within(&log, &["pci-config-address", "pci-config-data"]);
}

/// The dword-wide access to `0xCF8` must not be mistaken for the byte-wide
/// reset register that lives inside it.
#[test]
fn pci_selection_is_never_a_byte_access_to_the_reset_register() {
    reset();
    let _ = PCI_CONFIG.read_dword(0x8000_0000);
    assert!(take_log().iter().all(|o| o.port != 0xCF9));
}

// ---- debugcon -----------------------------------------------------------

#[test]
fn debugcon_is_one_byte_to_0xe9() {
    reset();
    DEBUGCON.write_byte(b'x');
    let log = take_log();
    assert_eq!(log, [w(0xE9, u32::from(b'x'))]);
    assert_within(&log, &["debugcon"]);
}

// ---- the whole crate ----------------------------------------------------

/// Exercise every device and check nothing strayed outside the registry: the
/// property the types exist to give.
#[test]
fn nothing_ever_touches_a_port_the_registry_does_not_list() {
    reset();
    let _ = COM1.read(UartReg::LineStatus);
    COM1.write(UartReg::Data, 0);
    let _ = CMOS.read(0);
    CMOS.write(0, 0);
    PIC.mask_all();
    PIT.load_channel0_oneshot(1);
    let _ = PIT.channel0_output_high();
    let _ = I8042.status();
    let _ = I8042.read_data();
    I8042.write_command(0);
    RESET_CONTROL.request_hard_reset();
    let _ = PCI_CONFIG.read_dword(0);
    PCI_CONFIG.write_dword(0, 0);
    DEBUGCON.write_byte(0);
    let all: [&str; 11] = [
        "pic-master-mask", "pit", "i8042-data", "i8042-status-command", "cmos", "pic-slave-mask",
        "debugcon", "uart-com1", "pci-config-address", "reset-control", "pci-config-data",
    ];
    assert_within(&take_log(), &all);
}
