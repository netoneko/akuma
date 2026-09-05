//! BOT wire-format fixtures. The CBW/CSW byte layouts are from USB MSC BOT 1.0
//! §5; the SCSI CDB layouts from SBC-3. Values cross-checked against a `READ(10)`
//! CBW captured from Linux `usbmon` driving the same enclosure class.

use akuma_usb_storage::cdb;
use akuma_usb_storage::{
    Cbw, CBW_SIGNATURE, CSW_SIGNATURE, Csw, CswStatus, Direction, InquiryData, ReadCapacity10,
    RequestSense,
};

#[test]
fn cbw_encodes_a_read10_the_way_the_wire_expects() {
    let cbw = Cbw {
        tag: 0xdead_beef,
        command: cdb::read_10(0x0000_1000, 8, 512),
        lun: 0,
    };
    let b = cbw.encode();
    assert_eq!(b.len(), 31);
    assert_eq!(u32::from_le_bytes([b[0], b[1], b[2], b[3]]), CBW_SIGNATURE);
    assert_eq!(&b[4..8], &0xdead_beefu32.to_le_bytes());
    // dCBWDataTransferLength = 8 blocks * 512.
    assert_eq!(u32::from_le_bytes([b[8], b[9], b[10], b[11]]), 8 * 512);
    assert_eq!(b[12], 0x80, "bmCBWFlags: data IN");
    assert_eq!(b[13], 0, "LUN 0");
    assert_eq!(b[14], 10, "READ(10) CDB is 10 bytes");
    // CDB: opcode 0x28, LBA 0x00001000 big-endian at bytes 2..6, blocks 8 at 7..9.
    assert_eq!(b[15], 0x28);
    assert_eq!(&b[17..21], &0x0000_1000u32.to_be_bytes());
    assert_eq!(&b[22..24], &8u16.to_be_bytes());
    assert_eq!(&b[24..31], &[0u8; 7], "CDB zero-padded to 16");
}

#[test]
fn cbw_encodes_a_write10_as_data_out() {
    let cbw = Cbw { tag: 1, command: cdb::write_10(42, 1, 512), lun: 0 };
    let b = cbw.encode();
    assert_eq!(b[12], 0x00, "bmCBWFlags: data OUT");
    assert_eq!(b[15], 0x2a, "WRITE(10) opcode");
    assert_eq!(u32::from_le_bytes([b[8], b[9], b[10], b[11]]), 512);
}

#[test]
fn commands_with_no_data_phase() {
    let tur = cdb::test_unit_ready();
    assert_eq!(tur.direction, Direction::None);
    assert_eq!(tur.data_len, 0);
    assert_eq!(tur.cdb_len, 6);
    assert_eq!(tur.cdb[0], 0x00);

    let cap = cdb::read_capacity_10();
    assert_eq!(cap.direction, Direction::In);
    assert_eq!(cap.data_len, 8);
    assert_eq!(cap.cdb_len, 10);
    assert_eq!(cap.cdb[0], 0x25);

    let inq = cdb::inquiry(36);
    assert_eq!(inq.data_len, 36);
    assert_eq!(inq.cdb[4], 36, "allocation length");
}

#[test]
fn csw_parse_good_bad_and_desynced() {
    let mut good = [0u8; 13];
    good[0..4].copy_from_slice(&CSW_SIGNATURE.to_le_bytes());
    good[4..8].copy_from_slice(&0x1234_5678u32.to_le_bytes());
    good[8..12].copy_from_slice(&0u32.to_le_bytes());
    good[12] = 0;
    let csw = Csw::parse(&good).expect("valid CSW");
    assert_eq!(csw.tag, 0x1234_5678);
    assert_eq!(csw.status, CswStatus::Passed);
    assert!(csw.is_good(0x1234_5678));
    assert!(!csw.is_good(0x9999_9999), "wrong tag is not good");

    let mut failed = good;
    failed[12] = 1;
    failed[8..12].copy_from_slice(&512u32.to_le_bytes());
    let csw = Csw::parse(&failed).unwrap();
    assert_eq!(csw.status, CswStatus::Failed);
    assert_eq!(csw.residue, 512);
    assert!(!csw.is_good(0x1234_5678));

    let mut phase = good;
    phase[12] = 2;
    assert_eq!(Csw::parse(&phase).unwrap().status, CswStatus::PhaseError);

    // Wrong signature -> the bulk pipe is desynced; None forces a reset.
    let mut bad_sig = good;
    bad_sig[0] = 0;
    assert!(Csw::parse(&bad_sig).is_none());
    assert!(Csw::parse(&good[..5]).is_none(), "short buffer");
}

#[test]
fn inquiry_response_identifies_a_disk() {
    // A minimal INQUIRY response: direct-access device, not removable, SPC-4.
    let mut r = [0u8; 36];
    r[0] = 0x00; // peripheral device type 0
    r[1] = 0x00; // not removable
    r[2] = 0x06; // SPC-4
    let inq = InquiryData::parse(&r).unwrap();
    assert!(inq.is_disk());
    assert!(!inq.removable);
    assert_eq!(inq.version, 0x06);

    // A removable CD (device type 5) is not a disk.
    r[0] = 0x05;
    r[1] = 0x80;
    let cd = InquiryData::parse(&r).unwrap();
    assert!(!cd.is_disk());
    assert!(cd.removable);
}

#[test]
fn read_capacity_gives_block_count_and_size() {
    // last LBA 0x0EFFFFFF, block length 512 -> 0x0F000000 blocks -> ~120 GB.
    let mut r = [0u8; 8];
    r[0..4].copy_from_slice(&0x0eff_ffffu32.to_be_bytes());
    r[4..8].copy_from_slice(&512u32.to_be_bytes());
    let cap = ReadCapacity10::parse(&r).unwrap();
    assert_eq!(cap.last_lba, 0x0eff_ffff);
    assert_eq!(cap.block_len, 512);
    assert_eq!(cap.block_count(), 0x0f00_0000);
    assert_eq!(cap.capacity_bytes(), 0x0f00_0000 * 512);

    // The real ST1000LM035 through this bridge: 1953525168 512-byte blocks.
    let mut real = [0u8; 8];
    real[0..4].copy_from_slice(&(1_953_525_168u32 - 1).to_be_bytes());
    real[4..8].copy_from_slice(&512u32.to_be_bytes());
    let cap = ReadCapacity10::parse(&real).unwrap();
    assert_eq!(cap.block_count(), 1_953_525_168);
    assert_eq!(cap.capacity_bytes(), 1_000_204_886_016);
}

#[test]
fn request_sense_spots_a_spinning_up_drive() {
    let mut r = [0u8; 18];
    r[0] = 0x70; // fixed-format, current error
    r[2] = 0x02; // sense key: Not Ready
    r[12] = 0x04; // ASC: Logical Unit Not Ready
    r[13] = 0x01; // ASCQ: Becoming Ready
    let s = RequestSense::parse(&r).unwrap();
    assert_eq!(s.sense_key, 0x02);
    assert_eq!(s.asc, 0x04);
    assert_eq!(s.ascq, 0x01);
    assert!(s.is_becoming_ready());

    // No sense = ready.
    let ok = RequestSense::parse(&[0u8; 18]).unwrap();
    assert_eq!(ok.sense_key, 0);
    assert!(!ok.is_becoming_ready());
}

#[test]
fn read_and_write_10_cdb_layout_round_trips_the_lba() {
    for &lba in &[0u32, 1, 2048, 0x0012_3456, 0xffff_fffe] {
        let r = cdb::read_10(lba, 64, 512);
        assert_eq!(u32::from_be_bytes([r.cdb[2], r.cdb[3], r.cdb[4], r.cdb[5]]), lba);
        assert_eq!(u16::from_be_bytes([r.cdb[7], r.cdb[8]]), 64);
        assert_eq!(r.data_len, 64 * 512);
        assert_eq!(r.direction, Direction::In);

        let w = cdb::write_10(lba, 64, 512);
        assert_eq!(w.cdb[0], 0x2a);
        assert_eq!(u32::from_be_bytes([w.cdb[2], w.cdb[3], w.cdb[4], w.cdb[5]]), lba);
        assert_eq!(w.direction, Direction::Out);
    }
}
