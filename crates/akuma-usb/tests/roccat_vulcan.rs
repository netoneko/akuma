//! Golden fixtures: the ROCCAT Vulcan AIMO keyboard as it enumerated on the
//! HP 500-502nj, read off the running Linux with `lsusb -v` and `usbhid-dump`
//! on 2026-09-05.
//!
//! `1e7d:3098`, USB 2.00, negotiated **full speed**, 4 interfaces. Interface 0
//! is the HID boot keyboard; its report descriptor is the App. B.1 boilerplate.

use akuma_usb::descriptor::{self, DeviceDescriptor};
use akuma_usb::hid::{self, BootKeyboardDecoder, BootReport};

/// `/sys/bus/usb/devices/2-1.6/descriptors` — device descriptor followed by
/// the single configuration and its whole interface/endpoint/HID tree.
const ROCCAT_DESCRIPTORS: &[u8] = &[
    0x12, 0x01, 0x00, 0x02, 0x00, 0x00, 0x00, 0x40, 0x7d, 0x1e, 0x98, 0x30, 0x00, 0x01, 0x01, 0x02,
    0x00, 0x01, 0x09, 0x02, 0x74, 0x00, 0x04, 0x01, 0x00, 0xa0, 0xfa, 0x09, 0x04, 0x00, 0x00, 0x01,
    0x03, 0x01, 0x01, 0x00, 0x09, 0x21, 0x11, 0x01, 0x00, 0x01, 0x22, 0x40, 0x00, 0x07, 0x05, 0x81,
    0x03, 0x40, 0x00, 0x01, 0x09, 0x04, 0x01, 0x00, 0x01, 0x03, 0x00, 0x00, 0x00, 0x09, 0x21, 0x11,
    0x01, 0x00, 0x01, 0x22, 0x34, 0x01, 0x07, 0x05, 0x82, 0x03, 0x40, 0x00, 0x01, 0x09, 0x04, 0x02,
    0x00, 0x01, 0x03, 0x00, 0x00, 0x00, 0x09, 0x21, 0x11, 0x01, 0x00, 0x01, 0x22, 0x18, 0x00, 0x07,
    0x05, 0x83, 0x03, 0x40, 0x00, 0x01, 0x09, 0x04, 0x03, 0x00, 0x02, 0x03, 0x00, 0x00, 0x00, 0x09,
    0x21, 0x11, 0x01, 0x00, 0x01, 0x22, 0x26, 0x00, 0x07, 0x05, 0x84, 0x03, 0x40, 0x00, 0x01, 0x07,
    0x05, 0x04, 0x03, 0x40, 0x00, 0x01,
];

/// Interface 0's HID report descriptor (`usbhid-dump`, 64 bytes) — the
/// canonical boot keyboard.
const BOOT_KEYBOARD_REPORT_DESC: &[u8] = &[
    0x05, 0x01, 0x09, 0x06, 0xa1, 0x01, 0x05, 0x07, 0x19, 0xe0, 0x29, 0xe7, 0x15, 0x00, 0x25, 0x01,
    0x95, 0x08, 0x75, 0x01, 0x81, 0x02, 0x95, 0x08, 0x75, 0x01, 0x81, 0x01, 0x95, 0x05, 0x75, 0x01,
    0x05, 0x08, 0x19, 0x01, 0x29, 0x05, 0x91, 0x02, 0x95, 0x01, 0x75, 0x03, 0x91, 0x01, 0x95, 0x06,
    0x75, 0x08, 0x15, 0x00, 0x26, 0xa4, 0x00, 0x05, 0x07, 0x19, 0x00, 0x29, 0xa4, 0x81, 0x00, 0xc0,
];

/// Interface 2's report descriptor (`2-1.6:1.2`, 24 bytes) — opens
/// `Usage Page (Generic Desktop), Usage (Keypad)` but has no modifier byte, so
/// it is HID-keyboard-*ish* and must still be rejected as a boot keyboard.
const KEYPAD_REPORT_DESC: &[u8] = &[
    0x05, 0x01, 0x09, 0x07, 0xa1, 0x01, 0x05, 0x07, 0x19, 0x00, 0x29, 0x91, 0x15, 0x00, 0x26, 0xff,
    0x00, 0x75, 0x08, 0x95, 0x18, 0x81, 0x00, 0xc0,
];

/// `/sys/bus/usb/devices/2-1/descriptors` — the Intel Integrated Rate Matching
/// Hub (`8087:8008`) the keyboard sits behind. Single-TT USB 2.0 hub.
const RATE_MATCHING_HUB_DESCRIPTORS: &[u8] = &[
    0x12, 0x01, 0x00, 0x02, 0x09, 0x00, 0x01, 0x40, 0x87, 0x80, 0x08, 0x80, 0x05, 0x00, 0x00, 0x00,
    0x00, 0x01, 0x09, 0x02, 0x19, 0x00, 0x01, 0x01, 0x00, 0xe0, 0x00, 0x09, 0x04, 0x00, 0x00, 0x01,
    0x09, 0x00, 0x00, 0x00, 0x07, 0x05, 0x81, 0x03, 0x01, 0x00, 0x0c,
];

#[test]
fn device_descriptor_matches_the_hardware() {
    let d = DeviceDescriptor::parse(ROCCAT_DESCRIPTORS).expect("device descriptor");
    assert_eq!(d.vendor, 0x1e7d);
    assert_eq!(d.product, 0x3098);
    assert_eq!(d.bcd_usb, 0x0200);
    assert_eq!(d.max_packet_size0, 64);
    assert_eq!(d.class, 0, "class is declared per-interface");
    assert_eq!(d.num_configurations, 1);
    assert_eq!(d.i_manufacturer, 1);
    assert_eq!(d.i_product, 2);
    assert_eq!(d.i_serial_number, 0);
}

#[test]
fn configuration_descriptor_is_the_first_record_after_the_device() {
    let cfg = descriptor::descriptors(ROCCAT_DESCRIPTORS)
        .find(|r| r.descriptor_type == descriptor::descriptor_type::CONFIGURATION)
        .and_then(|r| descriptor::ConfigurationDescriptor::parse(r.bytes))
        .expect("configuration descriptor");
    assert_eq!(cfg.total_length, 0x74);
    assert_eq!(cfg.num_interfaces, 4);
    assert_eq!(cfg.configuration_value, 1);
    assert_eq!(cfg.max_power_ma, 500);
    assert!(!cfg.self_powered());
    assert!(cfg.remote_wakeup());
}

#[test]
fn every_record_in_the_blob_is_well_formed_and_walked() {
    // 1 device + 1 config + 4 interfaces + 4 HID + 5 endpoints = 15 records.
    let n = descriptor::descriptors(ROCCAT_DESCRIPTORS).count();
    assert_eq!(n, 15);
    // The walk consumes the blob exactly — no trailing bytes, no overrun.
    let consumed: usize = descriptor::descriptors(ROCCAT_DESCRIPTORS)
        .map(|r| r.bytes.len())
        .sum();
    assert_eq!(consumed, ROCCAT_DESCRIPTORS.len());
}

#[test]
fn find_boot_keyboard_locates_interface_0_and_its_interrupt_in_endpoint() {
    let kb = descriptor::find_boot_keyboard(ROCCAT_DESCRIPTORS).expect("boot keyboard");
    assert_eq!(kb.configuration_value, 1);
    assert_eq!(kb.interface_number, 0);
    assert_eq!(kb.alternate_setting, 0);
    assert_eq!(kb.endpoint_address, 0x81);
    assert_eq!(kb.endpoint_max_packet_size, 64);
    assert_eq!(kb.endpoint_interval, 1);
    assert_eq!(kb.report_descriptor_length, Some(64));
}

#[test]
fn the_hub_is_not_a_keyboard() {
    let d = DeviceDescriptor::parse(RATE_MATCHING_HUB_DESCRIPTORS).expect("hub device descriptor");
    assert_eq!(d.class, descriptor::class::HUB);
    assert_eq!(d.vendor, 0x8087);
    assert_eq!(d.protocol, 1, "single TT");
    assert!(descriptor::find_boot_keyboard(RATE_MATCHING_HUB_DESCRIPTORS).is_none());
}

#[test]
fn report_descriptor_is_recognised_as_a_boot_keyboard() {
    assert!(hid::is_boot_keyboard_report_descriptor(BOOT_KEYBOARD_REPORT_DESC));

    let mut fields = [hid::ReportField::default(); 8];
    let n = hid::input_fields(BOOT_KEYBOARD_REPORT_DESC, &mut fields);
    assert_eq!(n, 3, "modifier byte, reserved byte, 6-key array");

    let modifiers = fields[0];
    assert_eq!(modifiers.usage_page, hid::USAGE_PAGE_KEYBOARD);
    assert!(modifiers.variable);
    assert_eq!(modifiers.report_size, 1);
    assert_eq!(modifiers.report_count, 8);
    assert_eq!(modifiers.usage_minimum, 0xE0);
    assert_eq!(modifiers.usage_maximum, 0xE7);

    assert!(fields[1].constant, "the reserved byte");

    let keys = fields[2];
    assert!(!keys.variable, "the key slots are an array, not per-key bits");
    assert_eq!(keys.report_size, 8);
    assert_eq!(keys.report_count, 6);
    assert_eq!(keys.logical_maximum, 0xA4);
}

#[test]
fn a_keyboardish_but_non_boot_descriptor_is_rejected() {
    assert!(!hid::is_boot_keyboard_report_descriptor(KEYPAD_REPORT_DESC));
}

/// Build an 8-byte boot report the way the endpoint delivers it.
fn report(modifiers: u8, keys: [u8; 6]) -> BootReport {
    let mut buf = [0u8; 8];
    buf[0] = modifiers;
    buf[2..8].copy_from_slice(&keys);
    BootReport::parse(&buf).unwrap()
}

#[test]
fn decoder_types_shifted_letters_ctrl_and_enter() {
    use akuma_usb::keymap::{MOD_LCTRL, MOD_LSHIFT};
    let mut dec = BootKeyboardDecoder::new();
    let mut out = std::vec::Vec::new();
    let mut push = |b: u8| out.push(b);

    // Shift + 'a' (usage 0x04) -> 'A'
    dec.feed(&report(MOD_LSHIFT, [0x04, 0, 0, 0, 0, 0]), &mut push);
    // all released
    dec.feed(&report(0, [0; 6]), &mut push);
    // 'b' (0x05) -> 'b'
    dec.feed(&report(0, [0x05, 0, 0, 0, 0, 0]), &mut push);
    dec.feed(&report(0, [0; 6]), &mut push);
    // Enter (0x28) -> '\r'
    dec.feed(&report(0, [0x28, 0, 0, 0, 0, 0]), &mut push);
    dec.feed(&report(0, [0; 6]), &mut push);
    // Ctrl + 'c' (0x06) -> 0x03
    dec.feed(&report(MOD_LCTRL, [0x06, 0, 0, 0, 0, 0]), &mut push);

    assert_eq!(out, [b'A', b'b', b'\r', 0x03]);
}

#[test]
fn a_held_key_does_not_auto_repeat() {
    let mut dec = BootKeyboardDecoder::new();
    let mut count = 0usize;
    // Same key present in three consecutive reports.
    for _ in 0..3 {
        dec.feed(&report(0, [0x1b, 0, 0, 0, 0, 0]), |_| count += 1); // 'x'
    }
    assert_eq!(count, 1, "only the key-down edge emits");
}

#[test]
fn caps_lock_is_tracked_host_side() {
    let mut dec = BootKeyboardDecoder::new();
    let mut out = std::vec::Vec::new();

    dec.feed(&report(0, [0x39, 0, 0, 0, 0, 0]), |_| unreachable!()); // CapsLock down
    dec.feed(&report(0, [0; 6]), |_| {}); // up
    assert!(dec.caps_lock());
    dec.feed(&report(0, [0x04, 0, 0, 0, 0, 0]), |b| out.push(b)); // 'a' -> 'A'
    dec.feed(&report(0, [0; 6]), |_| {});
    dec.feed(&report(0, [0x39, 0, 0, 0, 0, 0]), |_| {}); // CapsLock again
    dec.feed(&report(0, [0; 6]), |_| {});
    assert!(!dec.caps_lock());
    dec.feed(&report(0, [0x04, 0, 0, 0, 0, 0]), |b| out.push(b)); // 'a' -> 'a'

    assert_eq!(out, [b'A', b'a']);
}

#[test]
fn a_rollover_report_emits_nothing_and_does_not_lose_the_next_press() {
    let mut dec = BootKeyboardDecoder::new();
    let mut out = std::vec::Vec::new();
    dec.feed(&report(0, [0x01; 6]), |_| unreachable!()); // ErrorRollOver in every slot
    dec.feed(&report(0, [0x07, 0, 0, 0, 0, 0]), |b| out.push(b)); // 'd'
    assert_eq!(out, [b'd']);
}
