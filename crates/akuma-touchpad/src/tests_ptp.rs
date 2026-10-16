//! PTP decode against the real Goodix descriptor, and the wire format.

use akuma_hid::fixtures::GOODIX_GXTP5100 as G;

use crate::ptp::{EVENT_BYTES, PtpDecoder, PtpLayout, encode, kind};
use crate::{Button, PointerEvent};

/// Build a report-4 payload: contacts as (tip, id, x, y), then count/button.
fn payload(contacts: &[(bool, u8, u16, u16)], button: bool) -> [u8; 29] {
    let mut p = [0u8; 29];
    for (i, &(tip, id, x, y)) in contacts.iter().enumerate() {
        let b = i * 5;
        p[b] = (u8::from(tip) << 1) | 1 | (id << 4); // confidence | tip | id
        p[b + 1..b + 3].copy_from_slice(&x.to_le_bytes());
        p[b + 3..b + 5].copy_from_slice(&y.to_le_bytes());
    }
    p[27] = contacts.len() as u8;
    p[28] = u8::from(button);
    p
}

#[test]
fn layout_of_the_real_descriptor() {
    let l = PtpLayout::from_descriptor(G).unwrap();
    assert_eq!(l.report_id, 4);
    assert_eq!(l.payload_len, 29);
    assert_eq!(l.contacts, 5);
    assert!(l.x_max > l.x_min && l.y_max > l.y_min);
    assert_eq!((l.x_max, l.y_max), (3751, 2327));
    assert!(l.geometry(3840, 2400).is_ok());
}

#[test]
fn descriptors_without_a_tip_switch_are_not_touchpads() {
    // Boot mouse: X/Y and buttons, no digitizer.
    let mouse = [
        0x05, 0x01, 0x09, 0x02, 0xa1, 0x01, 0x09, 0x01, 0xa1, 0x00, 0x05, 0x09, 0x19, 0x01, 0x29, 0x03,
        0x15, 0x00, 0x25, 0x01, 0x95, 0x03, 0x75, 0x01, 0x81, 0x02, 0x95, 0x01, 0x75, 0x05, 0x81, 0x03,
        0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x15, 0x81, 0x25, 0x7f, 0x75, 0x08, 0x95, 0x02, 0x81, 0x06,
        0xc0, 0xc0,
    ];
    assert!(PtpLayout::from_descriptor(&mouse).is_none());
    assert!(PtpLayout::from_descriptor(&[]).is_none());
}

#[test]
fn one_finger_decodes_with_coordinates() {
    let mut d = PtpDecoder::new(PtpLayout::from_descriptor(G).unwrap());
    assert!(d.decode(&payload(&[(true, 3, 1200, 800)], false)));
    let f = d.frame();
    assert!(f.contacts[0].down);
    assert_eq!((f.contacts[0].x, f.contacts[0].y), (1200, 800));
    assert!(!f.contacts[1].down);
    assert!(!f.button);
}

#[test]
fn a_finger_keeps_its_slot_when_an_earlier_one_lifts() {
    let mut d = PtpDecoder::new(PtpLayout::from_descriptor(G).unwrap());
    d.decode(&payload(&[(true, 1, 100, 100), (true, 2, 900, 100)], false));
    assert_eq!(d.frame().contacts[0].x, 100);
    assert_eq!(d.frame().contacts[1].x, 900);
    // Finger id 1 lifts; id 2 is now the first entry in the report.
    d.decode(&payload(&[(true, 2, 910, 100)], false));
    let f = d.frame();
    assert!(!f.contacts[0].down);
    assert!(f.contacts[1].down);
    assert_eq!(f.contacts[1].x, 910, "id 2 stayed in slot 1");
}

#[test]
fn a_finger_not_listed_has_lifted_and_its_slot_is_reusable() {
    let mut d = PtpDecoder::new(PtpLayout::from_descriptor(G).unwrap());
    d.decode(&payload(&[(true, 7, 5, 5)], false));
    d.decode(&payload(&[], false));
    assert!(!d.frame().contacts[0].down);
    d.decode(&payload(&[(true, 9, 50, 60)], false));
    assert!(d.frame().contacts[0].down, "freed slot reused");
}

#[test]
fn explicit_tip_up_clears_the_finger() {
    let mut d = PtpDecoder::new(PtpLayout::from_descriptor(G).unwrap());
    d.decode(&payload(&[(true, 4, 10, 10)], false));
    d.decode(&payload(&[(false, 4, 10, 10)], false));
    assert!(!d.frame().contacts[0].down);
}

#[test]
fn button_and_count_bound_the_entries() {
    let mut d = PtpDecoder::new(PtpLayout::from_descriptor(G).unwrap());
    let mut p = payload(&[(true, 1, 10, 10)], true);
    assert!(d.decode(&p));
    assert!(d.frame().button);
    // Garbage in the second entry beyond the declared count is ignored.
    p[5] = 0xff;
    p[6] = 0xff;
    d.decode(&p);
    assert!(!d.frame().contacts[1].down);
}

#[test]
fn short_payload_is_rejected_without_state_change() {
    let mut d = PtpDecoder::new(PtpLayout::from_descriptor(G).unwrap());
    d.decode(&payload(&[(true, 1, 10, 10)], false));
    assert!(!d.decode(&[0u8; 10]));
    assert!(d.frame().contacts[0].down);
}

#[test]
fn the_decoder_feeds_the_clickpad_end_to_end() {
    let l = PtpLayout::from_descriptor(G).unwrap();
    let mut d = PtpDecoder::new(l);
    let mut pad = crate::Clickpad::new(l.geometry(1920, 1200).unwrap(), crate::Config::DEFAULT);
    let mid = u16::try_from(i32::midpoint(l.x_min, l.x_max)).unwrap();
    let ymid = u16::try_from((l.y_min + l.y_max) / 3).unwrap();
    d.decode(&payload(&[(true, 1, mid, ymid)], false));
    pad.update(&d.frame());
    assert!(matches!(pad.pop(), Some(PointerEvent::Move { .. })));
}

#[test]
fn wire_records() {
    assert_eq!(encode(&PointerEvent::Move { x: 0x0102, y: 0x0304 }), [1, 0, 0, 0, 2, 1, 4, 3]);
    assert_eq!(
        encode(&PointerEvent::Button { button: Button::Right, down: true }),
        [kind::BUTTON, 1, 1, 0, 0, 0, 0, 0]
    );
    assert_eq!(
        encode(&PointerEvent::Button { button: Button::Left, down: false }),
        [kind::BUTTON, 0, 0, 0, 0, 0, 0, 0]
    );
    assert_eq!(encode(&PointerEvent::Wheel { dx: -1, dy: 2 }), [3, 0, 0, 0, 0xff, 0xff, 2, 0]);
    // Saturation, not wrap.
    let r = encode(&PointerEvent::Move { x: 70_000, y: 1 });
    assert_eq!(&r[4..6], &[0xff, 0xff]);
    assert_eq!(r.len(), EVENT_BYTES);
}
