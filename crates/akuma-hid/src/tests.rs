//! Fixtures are hand-assembled descriptors, each item commented; the
//! expected bit positions were derived by walking them with the same rules
//! the C implements (positions exclude the report-ID byte). The boot mouse
//! descriptor is the USB HID spec's own example; the touchpad fixture is
//! shaped like a Windows-precision-touchpad report (Digitizers page, one
//! finger collection, button pad, contact count) — when the real rdesc is
//! dumped from the ryzen box (M0), pin it beside these.

#[cfg(test)]
extern crate std;

use crate::field::FieldTable;
use crate::item::{HIO_CONST, HIO_RELATIVE, HIO_VARIABLE, HidLocation, ItemKind};
use crate::{Filter, HidParser, get_bits, get_ubits, locate, report_size};

// Usage vocabulary used across the fixtures.
const PAGE_GENERIC: u32 = 0x01 << 16;
const PAGE_BUTTON: u32 = 0x09 << 16;
const PAGE_DIGITIZER: u32 = 0x0D << 16;
const USAGE_X: u32 = PAGE_GENERIC | 0x30;
const USAGE_Y: u32 = PAGE_GENERIC | 0x31;
const USAGE_WHEEL: u32 = PAGE_GENERIC | 0x38;
const USAGE_BTN1: u32 = PAGE_BUTTON | 0x01;
const USAGE_TOUCHPAD: u32 = PAGE_DIGITIZER | 0x05;
const USAGE_TIP: u32 = PAGE_DIGITIZER | 0x42;
const USAGE_IN_RANGE: u32 = PAGE_DIGITIZER | 0x32;
const USAGE_CONTACT_ID: u32 = PAGE_DIGITIZER | 0x51;
const USAGE_CONTACT_COUNT: u32 = PAGE_DIGITIZER | 0x54;
const USAGE_CONTACT_COUNT_MAX: u32 = PAGE_DIGITIZER | 0x55;

/// USB HID spec §E.2 boot mouse: 3 buttons, 5-bit pad, relative X/Y.
const BOOT_MOUSE: &[u8] = &[
    0x05, 0x01, //   Usage Page (Generic Desktop)
    0x09, 0x02, //   Usage (Mouse)
    0xA1, 0x01, //   Collection (Application)
    0x09, 0x01, //     Usage (Pointer)
    0xA1, 0x00, //     Collection (Physical)
    0x05, 0x09, //       Usage Page (Button)
    0x19, 0x01, //       Usage Minimum (1)
    0x29, 0x03, //       Usage Maximum (3)
    0x15, 0x00, //       Logical Minimum (0)
    0x25, 0x01, //       Logical Maximum (1)
    0x95, 0x03, //       Report Count (3)
    0x75, 0x01, //       Report Size (1)
    0x81, 0x02, //       Input (Data,Var,Abs)      bits 0..2
    0x95, 0x01, //       Report Count (1)
    0x75, 0x05, //       Report Size (5)
    0x81, 0x01, //       Input (Const,Array,Abs)   bits 3..7
    0x05, 0x01, //       Usage Page (Generic Desktop)
    0x09, 0x30, //       Usage (X)
    0x09, 0x31, //       Usage (Y)
    0x15, 0x81, //       Logical Minimum (-127)
    0x25, 0x7F, //       Logical Maximum (127)
    0x75, 0x08, //       Report Size (8)
    0x95, 0x02, //       Report Count (2)
    0x81, 0x06, //       Input (Data,Var,Rel)      X bits 8..15, Y bits 16..23
    0xC0, 0xC0,
];

/// The precision-touchpad fixture: Digitizers page, report ID 1, one finger
/// collection (tip, in-range, 6-bit pad, X, Y, contact id), a 3-button pad,
/// contact count, and a contact-count-maximum *feature*. Payload = 8 bytes.
const PTP: &[u8] = &[
    0x05, 0x0D, // Usage Page (Digitizer)
    0x09, 0x05, // Usage (Touch Pad)
    0xA1, 0x01, // Collection (Application)
    0x85, 0x01, //   Report ID (1)
    0x09, 0x22, //   Usage (Finger)
    0xA1, 0x02, //   Collection (Logical)
    0x09, 0x42, //     Usage (Tip Switch)
    0x15, 0x00, 0x25, 0x01, //   Logical 0..1
    0x75, 0x01, 0x95, 0x01, //   Size 1, Count 1
    0x81, 0x02, //     Input (Data,Var,Abs)      tip:      bit 0
    0x09, 0x32, //     Usage (In Range)
    0x95, 0x01, 0x81, 0x02, //                       in-range: bit 1
    0x95, 0x06, 0x81, 0x03, //   Const, 6 bits           pad:      bits 2..7
    0x05, 0x01, //     Usage Page (Generic Desktop)
    0x09, 0x30, //     Usage (X)
    0x26, 0xFF, 0x0F, // Logical Maximum (4095)
    0x75, 0x10, 0x95, 0x01, //   Size 16, Count 1
    0x81, 0x02, //     Input (Data,Var,Abs)      X:        bits 8..23
    0x09, 0x31, 0x81, 0x02, //   Y:        bits 24..39
    0x05, 0x0D, //     Usage Page (Digitizer)
    0x09, 0x51, //     Usage (Contact Identifier)
    0x75, 0x08, 0x95, 0x01, 0x81, 0x02, //   cid:      bits 40..47
    0xC0, //   End Collection (finger)
    0x05, 0x09, //   Usage Page (Button)
    0x19, 0x01, 0x29, 0x03, //   Buttons 1..3
    0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x03, 0x81,
    0x02, //                          btn1..3:  bits 48..50
    0x95, 0x05, 0x81, 0x03, //   pad:      bits 51..55
    0x05, 0x0D, //   Usage Page (Digitizer)
    0x09, 0x54, //   Usage (Contact Count)
    0x95, 0x01, 0x75, 0x08, 0x81, 0x02, //   count:    bits 56..63
    0x09, 0x55, //   Usage (Contact Count Maximum)
    0x25, 0x05, //   Logical Maximum (5)
    0x95, 0x01, 0x75, 0x08, 0xB1, 0x02, //   Feature (Data,Var,Abs)    max:      bits 64..71
    0xC0,
];

#[test]
fn boot_mouse_locations() {
    let (loc, flags) = locate(BOOT_MOUSE, USAGE_BTN1, 0, ItemKind::Input).unwrap();
    assert_eq!(
        loc,
        HidLocation {
            pos: 0,
            size: 1,
            count: 1
        }
    );
    assert_eq!(flags, HIO_VARIABLE);

    let (loc, flags) = locate(BOOT_MOUSE, USAGE_X, 0, ItemKind::Input).unwrap();
    assert_eq!(
        loc,
        HidLocation {
            pos: 8,
            size: 8,
            count: 1
        }
    );
    assert_eq!(flags, HIO_VARIABLE | HIO_RELATIVE);

    let (loc, _) = locate(BOOT_MOUSE, USAGE_Y, 0, ItemKind::Input).unwrap();
    assert_eq!(
        loc,
        HidLocation {
            pos: 16,
            size: 8,
            count: 1
        }
    );

    // The 5-bit pad is const — locate refuses it.
    assert!(locate(BOOT_MOUSE, USAGE_BTN1 + 9, 0, ItemKind::Input).is_none());
}

#[test]
fn boot_mouse_report_size_and_decode() {
    assert_eq!(report_size(BOOT_MOUSE, ItemKind::Input, 0), Some(3));

    // buttons=7, X=+16, Y=-16.
    let report: [u8; 3] = [0x07, 0x10, 0xF0];
    let (xloc, _) = locate(BOOT_MOUSE, USAGE_X, 0, ItemKind::Input).unwrap();
    let (yloc, _) = locate(BOOT_MOUSE, USAGE_Y, 0, ItemKind::Input).unwrap();
    assert_eq!(get_bits(&report, &xloc), 16);
    assert_eq!(get_bits(&report, &yloc), -16);
    // The unsigned form does not sign-extend: the raw 8 bits.
    assert_eq!(get_ubits(&report, &yloc), 0xF0);
    // buttons 1..3 via the range: each is its own field.
    for b in 0..3u32 {
        let (loc, _) = locate(BOOT_MOUSE, USAGE_BTN1 + b, 0, ItemKind::Input).unwrap();
        assert_eq!(loc.pos, b);
        assert_eq!(crate::get_ubits(&report, &loc), 1);
    }
}

#[test]
fn report_ids_restart_positions() {
    // 05 01 | 85 01 | 09 30 75 08 95 01 81 02 | 85 02 | 09 31 75 10 95 01 81 02 | C0
    let desc: &[u8] = &[
        0x05, 0x01, 0x85, 0x01, 0x09, 0x30, 0x75, 0x08, 0x95, 0x01, 0x81, 0x02, //
        0x85, 0x02, 0x09, 0x31, 0x75, 0x10, 0x95, 0x01, 0x81, 0x02, 0xC0,
    ];
    let (x, _) = locate(desc, USAGE_X, 1, ItemKind::Input).unwrap();
    let (y, _) = locate(desc, USAGE_Y, 2, ItemKind::Input).unwrap();
    // Each report ID's fields restart at bit 0; sizes differ per ID.
    assert_eq!(
        x,
        HidLocation {
            pos: 0,
            size: 8,
            count: 1
        }
    );
    assert_eq!(
        y,
        HidLocation {
            pos: 0,
            size: 16,
            count: 1
        }
    );
    assert_eq!(report_size(desc, ItemKind::Input, 1), Some(1));
    assert_eq!(report_size(desc, ItemKind::Input, 2), Some(2));
    // And the ID byte adds 1 on the wire, not here.
    assert!(report_size(desc, ItemKind::Input, 3).is_none());
}

#[test]
fn usage_ranges_expand_per_usage() {
    let desc: &[u8] = &[
        0x05, 0x09, 0x19, 0x01, 0x29, 0x05, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x05, 0x81,
        0x02,
    ];
    for i in 0..5u32 {
        let (loc, _) = locate(desc, USAGE_BTN1 + i, 0, ItemKind::Input).unwrap();
        assert_eq!(loc.pos, i);
    }
    assert!(locate(desc, USAGE_BTN1 + 5, 0, ItemKind::Input).is_none());
}

#[test]
fn push_pop_restores_globals() {
    // X (8 bit), then Push, Y at 16 bit, Pop, X again — the popped Size(8)
    // must apply, and positions must not reset.
    let desc: &[u8] = &[
        0x05, 0x01, // Usage Page (Generic Desktop)
        0x09, 0x30, 0x75, 0x08, 0x95, 0x01, 0x81, 0x02, // X: pos 0, 8 bit
        0xA4, // Push
        0x75, 0x10, // Report Size (16)
        0x09, 0x31, 0x81, 0x02, // Y: pos 8, 16 bit
        0xB4, // Pop
        0x09, 0x30, 0x81, 0x02, // X again: pos 24, 8 bit (restored size!)
        0xC0,
    ];
    let (x1, _) = locate(desc, USAGE_X, 0, ItemKind::Input).unwrap();
    assert_eq!(
        x1,
        HidLocation {
            pos: 0,
            size: 8,
            count: 1
        }
    );
    let (y, _) = locate(desc, USAGE_Y, 0, ItemKind::Input).unwrap();
    assert_eq!(
        y,
        HidLocation {
            pos: 8,
            size: 16,
            count: 1
        }
    );
    // first() semantics: locate returns the *first* X.
    let table = FieldTable::collect(desc);
    assert_eq!(table.nth(USAGE_X, 0).unwrap().loc.pos, 0);
    assert_eq!(
        table.nth(USAGE_X, 1).unwrap().loc,
        HidLocation {
            pos: 24,
            size: 8,
            count: 1
        }
    );
}

#[test]
fn ptp_touchpad_fixture() {
    // Application collection shows up only in All mode.
    let mut all = HidParser::new(PTP, Filter::All);
    let first = all.next_item().unwrap();
    assert_eq!(first.kind, ItemKind::Collection);
    assert_eq!(first.usage, USAGE_TOUCHPAD);
    assert_eq!(first.collection, 1); // Application

    let (tip, _) = locate(PTP, USAGE_TIP, 1, ItemKind::Input).unwrap();
    assert_eq!(
        tip,
        HidLocation {
            pos: 0,
            size: 1,
            count: 1
        }
    );
    let (inrange, _) = locate(PTP, USAGE_IN_RANGE, 1, ItemKind::Input).unwrap();
    assert_eq!(
        inrange,
        HidLocation {
            pos: 1,
            size: 1,
            count: 1
        }
    );
    let (x, _) = locate(PTP, USAGE_X, 1, ItemKind::Input).unwrap();
    assert_eq!(
        x,
        HidLocation {
            pos: 8,
            size: 16,
            count: 1
        }
    );
    let (y, _) = locate(PTP, USAGE_Y, 1, ItemKind::Input).unwrap();
    assert_eq!(
        y,
        HidLocation {
            pos: 24,
            size: 16,
            count: 1
        }
    );
    let (cid, _) = locate(PTP, USAGE_CONTACT_ID, 1, ItemKind::Input).unwrap();
    assert_eq!(
        cid,
        HidLocation {
            pos: 40,
            size: 8,
            count: 1
        }
    );
    let (count, _) = locate(PTP, USAGE_CONTACT_COUNT, 1, ItemKind::Input).unwrap();
    assert_eq!(
        count,
        HidLocation {
            pos: 56,
            size: 8,
            count: 1
        }
    );

    // Payload excludes the report-ID byte: 72 bits declared → 8 bytes wait,
    // tip(0)+pad + X..count end at bit 64 → (64-0+7)/8 = 8.
    assert_eq!(report_size(PTP, ItemKind::Input, 1), Some(8));

    // The feature field is visible as Feature, not Input.
    assert!(locate(PTP, USAGE_CONTACT_COUNT_MAX, 1, ItemKind::Input).is_none());
    let (max, _) = locate(PTP, USAGE_CONTACT_COUNT_MAX, 1, ItemKind::Feature).unwrap();
    assert_eq!(
        max,
        HidLocation {
            pos: 64,
            size: 8,
            count: 1
        }
    );
}

#[test]
fn ptp_field_table_decodes_a_real_report() {
    let table = FieldTable::collect(PTP);
    assert!(!table.truncated);
    // Per-finger usages repeat (one finger collection here), contact-count
    // maximum comes out as Feature with logical max 5.
    let max = table
        .first_kind(USAGE_CONTACT_COUNT_MAX, ItemKind::Feature)
        .unwrap();
    assert_eq!(max.logical_max, 5);
    let count = table
        .first_kind(USAGE_CONTACT_COUNT, ItemKind::Input)
        .unwrap();

    // Wire report: rid=1, tip=1, in-range=1, pad, X=1234, Y=2345, cid=0,
    // buttons=001, count=1. bits: [tip1][inr1][pad6] [X16] [Y16] [cid8]
    // [b3][pad5] [count8]
    let x: u16 = 1234;
    let y: u16 = 2345;
    let payload: [u8; 8] = [
        0b0000_0011, // tip (bit 0) + in-range (bit 1) set, pad zero
        x as u8,
        (x >> 8) as u8,
        y as u8,
        (y >> 8) as u8,
        0,           // cid
        0b0000_0001, // button1 pressed (bit 0 of byte 6 = bit 48)
        1,           // contact count
    ];
    // Boolean 1-bit fields decode *signed* like the C's hid_get_data — a
    // set bit reads -1 — so bit fields are consumed through read_unsigned
    // and compared non-zero, the way the BSD drivers do it.
    assert_eq!(table.nth(USAGE_TIP, 0).unwrap().read_unsigned(&payload), 1);
    assert_eq!(
        table
            .nth(USAGE_IN_RANGE, 0)
            .unwrap()
            .read_unsigned(&payload),
        1
    );
    assert_eq!(table.nth(USAGE_X, 0).unwrap().read(&payload), 1234);
    assert_eq!(table.nth(USAGE_Y, 0).unwrap().read(&payload), 2345);
    assert_eq!(table.nth(USAGE_BTN1, 0).unwrap().read_unsigned(&payload), 1);
    assert_eq!(count.read(&payload), 1);

    // Empty report: nothing set.
    let empty = [0u8; 8];
    assert_eq!(table.nth(USAGE_X, 0).unwrap().read(&empty), 0);
    assert_eq!(count.read(&empty), 0);
}

#[test]
fn signed_decode_bounds() {
    // 8-bit field at pos 8 — payload byte 1 — with logical bounds -127..127.
    let (loc, _) = locate(BOOT_MOUSE, USAGE_X, 0, ItemKind::Input).unwrap();
    assert_eq!(get_bits(&[0x07, 0xFF, 0x00], &loc), -1);
    assert_eq!(get_bits(&[0x07, 0x81, 0x00], &loc), -127);
    assert_eq!(get_ubits(&[0x07, 0x81, 0x00], &loc), 0x81);
    // A field's decode is bounded by its size, not by logical bounds —
    // bounds are the consumer's business.
    assert_eq!(get_bits(&[0x07, 0x7F, 0x00], &loc), 127);
}

#[test]
fn out_of_range_positions_read_zero() {
    let (loc, _) = locate(BOOT_MOUSE, USAGE_Y, 0, ItemKind::Input).unwrap();
    // Payload too short: reads as 0, no panic (the C reads in-bounds bytes
    // and shifts zeros in).
    assert_eq!(get_bits(&[0x07, 0x42], &loc), 0);
    // Zero-size location (HidLocation::NONE) reads 0.
    assert_eq!(get_bits(&[0xFF], &HidLocation::NONE), 0);
}

#[test]
fn long_items_are_skipped() {
    // Long item 0xFE, size 2, tag 0xFF, two data bytes, then a real item.
    // The C would desynchronise here; this port consumes the data bytes.
    let desc: &[u8] = &[
        0xFE, 0x02, 0x00, 0xFF, 0xDE, 0xAD, // long item, 2 data bytes
        0x05, 0x01, 0x09, 0x30, 0x75, 0x08, 0x95, 0x01, 0x81, 0x02, 0xC0,
    ];
    let (x, _) = locate(desc, USAGE_X, 0, ItemKind::Input).unwrap();
    assert_eq!(
        x,
        HidLocation {
            pos: 0,
            size: 8,
            count: 1
        }
    );
}

#[test]
fn wheel_usage_is_absent_in_mouse_and_present_detection_works() {
    // Sanity: first_kind on an absent usage is None; the wheel usage is not
    // in the boot mouse descriptor.
    let table = FieldTable::collect(BOOT_MOUSE);
    assert!(table.first_kind(USAGE_WHEEL, ItemKind::Input).is_none());
    assert!(table.first_kind(USAGE_X, ItemKind::Input).is_some());
    // 3 buttons + X + Y — the const pad is skipped.
    assert_eq!(table.len(), 5);
}

#[test]
fn unbalanced_collection_ends_parse() {
    // End Collection with no matching Collection: the C returns 0 (error).
    let desc: &[u8] = &[
        0xC0, 0x05, 0x01, 0x09, 0x30, 0x75, 0x08, 0x95, 0x01, 0x81, 0x02,
    ];
    let mut p = HidParser::new(desc, Filter::All);
    assert!(p.next_item().is_none());
}

#[test]
fn const_items_are_not_in_the_field_table_but_are_in_the_stream() {
    // The boot mouse's 5-bit pad: visible in the stream as CONST...
    let mut p = HidParser::new(BOOT_MOUSE, Filter::Kind(ItemKind::Input));
    let mut consts = 0;
    let mut fields = 0;
    let mut collections = 0;
    while let Some(h) = p.next_item() {
        match h.kind {
            ItemKind::Collection | ItemKind::EndCollection => collections += 1,
            _ => {
                fields += 1;
                if h.flags & HIO_CONST != 0 {
                    consts += 1;
                    assert_eq!(h.loc.pos, 3);
                    assert_eq!(h.loc.size, 5);
                }
            }
        }
    }
    // Collections always pass through a kind-filtered walk (the C's shape).
    assert_eq!(collections, 4); // Application + Physical, and their ends
    assert_eq!(consts, 1);
    // 3 button usages + pad + X + Y.
    assert_eq!(fields, 6);
    // ...but absent from the table.
    let table = FieldTable::collect(BOOT_MOUSE);
    assert_eq!(table.len(), 5);
}
