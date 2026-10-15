//! Byte-exact vectors for the command encoder and the framing decisions.

use crate::*;

/// A hand-built 30-byte descriptor shaped like every ACPI pad's: report
/// descriptor at 0x0002, commands at 0x0003, data at 0x0004, Synaptics ids.
fn desc_bytes() -> [u8; 30] {
    let mut b = [0u8; 30];
    let put = |b: &mut [u8], o: usize, v: u16| {
        b[o] = v as u8;
        b[o + 1] = (v >> 8) as u8;
    };
    put(&mut b, 0, 30); // wHIDDescLength
    put(&mut b, 2, 0x0100); // bcdVersion
    put(&mut b, 4, 0x0143); // wReportDescLength
    put(&mut b, 6, 0x0002); // wReportDescRegister
    put(&mut b, 8, 0x0001); // wInputRegister
    put(&mut b, 10, 0x0041); // wMaxInputLength
    put(&mut b, 12, 0x0000); // wOutputRegister
    put(&mut b, 14, 0x0000); // wMaxOutputLength
    put(&mut b, 16, 0x0003); // wCommandRegister
    put(&mut b, 18, 0x0004); // wDataRegister
    put(&mut b, 20, 0x06CB); // Synaptics
    put(&mut b, 22, 0xCEFE);
    put(&mut b, 24, 0x0100); // version
    b
}

#[test]
fn descriptor_parses_and_validates() {
    let d = HidDescriptor::parse(&desc_bytes()).unwrap();
    assert_eq!(d.report_desc_len, 0x0143);
    assert_eq!(d.report_desc_register, 0x0002);
    assert_eq!(d.command_register, 0x0003);
    assert_eq!(d.data_register, 0x0004);
    assert_eq!(d.max_input_len, 0x0041);
    assert_eq!(d.vendor_id, 0x06CB);
    assert_eq!(d.product_id, 0xCEFE);
    assert!(d.max_output_len == 0, "this pad takes no output reports");
}

#[test]
fn descriptor_rejections() {
    // Short read.
    assert_eq!(
        HidDescriptor::parse(&desc_bytes()[..29]),
        Err(DescError::Short(29))
    );
    // Bad length word.
    let mut b = desc_bytes();
    b[0] = 0x20; // 32 != 30 (b[1] is already 0)
    assert_eq!(HidDescriptor::parse(&b), Err(DescError::BadLength(32)));
    // Bad bcdVersion (offset 2 is the LOW byte of the bcd word).
    let mut b = desc_bytes();
    b[3] = 0x02;
    assert_eq!(
        HidDescriptor::parse(&b),
        Err(DescError::BadBcdVersion(0x0200))
    );
    // max_input_len < 2.
    let mut b = desc_bytes();
    b[10] = 1;
    assert_eq!(HidDescriptor::parse(&b), Err(DescError::BadMaxInput(1)));
}

#[test]
fn command_encoding_byte_exact() {
    let mut buf = [0u8; 8];

    // SET_POWER ON: cmd_reg=3, then type 0 | id 0, opcode 0x08.
    let n = encode_set_power(&mut buf, 0x0003, PowerState::On);
    assert_eq!(n, 4);
    assert_eq!(&buf[..4], &[0x03, 0x00, 0x00, opcode::SET_POWER]);

    // SET_POWER SLEEP: the state rides in the id nibble.
    let n = encode_set_power(&mut buf, 0x0003, PowerState::Sleep);
    assert_eq!(&buf[..4], &[0x03, 0x00, 0x01, opcode::SET_POWER]);
    assert_eq!(n, 4);

    // RESET.
    let n = encode_reset(&mut buf, 0x0003);
    assert_eq!(&buf[..4], &[0x03, 0x00, 0x00, opcode::RESET]);
    assert_eq!(n, 4);

    // GET_REPORT input id 2: type 1 << 4 | 2.
    let n = encode_command(&mut buf, 0x0003, opcode::GET_REPORT, ReportType::Input, 2);
    assert_eq!(&buf[..4], &[0x03, 0x00, 0x12, opcode::GET_REPORT]);
    assert_eq!(n, 4);

    // Feature id 14 (still one nibble): 0x3E.
    let n = encode_command(
        &mut buf,
        0x0003,
        opcode::GET_REPORT,
        ReportType::Feature,
        14,
    );
    assert_eq!(&buf[..4], &[0x03, 0x00, 0x3E, opcode::GET_REPORT]);
    assert_eq!(n, 4);

    // Report id 15+: the escape — low byte 0x0F, opcode, then the real id.
    let n = encode_command(&mut buf, 0x0003, opcode::GET_REPORT, ReportType::Input, 42);
    assert_eq!(n, 5);
    assert_eq!(&buf[..5], &[0x03, 0x00, 0x1F, opcode::GET_REPORT, 42]);
    // A two-byte command register (nonstandard, but the descriptor may say).
    let n = encode_reset(&mut buf, 0x000B);
    assert_eq!(&buf[..4], &[0x0B, 0x00, 0x00, opcode::RESET]);
    assert_eq!(n, 4);
}

#[test]
fn output_framing() {
    let mut buf = [0u8; 16];

    // No output reports at all → the descriptor said so and the caller
    // should never frame one; encode_output still needs a register, so the
    // "not supported" answer belongs to the caller checking max_output_len.
    let d = HidDescriptor::parse(&desc_bytes()).unwrap();
    assert_eq!(d.max_output_len, 0);

    // Unnumbered report: [reg][len=2+data][data].
    let n = encode_output(&mut buf, 0x0006, 0, &[0xAA, 0xBB]).unwrap();
    assert_eq!(&buf[..n], &[0x06, 0x00, 0x04, 0x00, 0xAA, 0xBB]);

    // Numbered: length counts the id byte.
    let n = encode_output(&mut buf, 0x0006, 7, &[0xCC]).unwrap();
    assert_eq!(&buf[..n], &[0x06, 0x00, 0x04, 0x00, 0x07, 0xCC]);
}

#[test]
fn input_framing() {
    let max_len = 0x41;

    // Plain read of max_input_len bytes: [len][report]. len includes the
    // two length bytes.
    let mut frame = [0u8; 0x41];
    let report = [0x01, 0x02, 0x03];
    frame[0] = (report.len() + 2) as u8;
    frame[1] = 0;
    frame[2..2 + report.len()].copy_from_slice(&report);
    assert_eq!(decode_input(&frame), Some(&report[..]));

    // Zero length: nothing pending / RESET ack.
    let empty = [0u8; 0x41];
    assert_eq!(decode_input(&empty), None);
    assert!(is_reset_ack(&empty));

    // len of 2 = "zero-length report": not decodable, but also not an ack
    // unless the bytes are actually zero — len 2 means 0x02 0x00.
    let mut two = [0u8; 0x41];
    two[0] = 2;
    assert_eq!(decode_input(&two), None);
    assert!(!is_reset_ack(&two));

    // Bogus 0xFFFF (ELAN-shaped).
    let mut bogus = [0u8; 0x41];
    bogus[0] = 0xFF;
    bogus[1] = 0xFF;
    assert_eq!(decode_input(&bogus), None);
    assert!(is_bogus_input(&bogus));

    // Truncated frame (device sent less than it claimed).
    let mut short = [0u8; 8];
    short[0] = max_len as u8; // claims 65 > 8 present
    assert_eq!(decode_input(&short), None);
}

#[test]
fn duplicate_detection() {
    let a = [0x05, 0x00, 1, 2, 3];
    let b = [0x05, 0x00, 1, 2, 3];
    let c = [0x05, 0x00, 1, 2, 4];
    assert!(!is_duplicate(None, &a), "first read is never a duplicate");
    assert!(is_duplicate(Some(&a), &b));
    assert!(!is_duplicate(Some(&a), &c));
}

#[test]
fn quirks_table() {
    assert_eq!(
        Quirks::for_vid_pid(0x27C6, 0x0D42).post_power_delay_ms,
        1500
    );
    assert!(Quirks::for_vid_pid(0x04F3, 0x1234).bogus_input_size);
    assert_eq!(Quirks::for_vid_pid(0x06CB, 0xCEFE), Quirks::NONE);
}

#[test]
fn timing_constants_match_the_references() {
    // Pin the numbers the doc argues for, so a change is a decision.
    assert_eq!(POWER_ON_DELAY_MS, 60);
    assert_eq!(RESET_SETTLE_MS, 100);
    assert_eq!(ADDR_RETRY_DELAY_US, 400);
    assert_eq!(BRINGUP_ATTEMPTS, 3);
    assert_eq!(HID_DESC_REGISTER, 0x0001);
    assert_eq!(HID_DESC_SIZE, 30);
}
