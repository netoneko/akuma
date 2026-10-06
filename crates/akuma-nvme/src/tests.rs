extern crate std;
use std::vec;
use std::vec::Vec;

use crate::chunk;
use crate::cmd::{self, CqHead, Cqe, SqTail};
use crate::gpt::{self, GptError, Header};
use crate::identify::{self, Controller, Namespace};
use crate::prp::{self, PrpError};
use crate::regs::{self, Cap, Csts};
use crate::window::Window;

// ---------------------------------------------------------------- regs

#[test]
fn cap_decodes_every_field() {
    // MQES 2047, CQR, TO 40 (20 s), DSTRD 1, CSS NVM, MPSMIN 0, MPSMAX 4.
    let v: u64 = 2047 | (1 << 16) | (40 << 24) | (1 << 32) | (1 << 37) | (4 << 52);
    let c = Cap::decode(v);
    assert_eq!(c.mqes, 2047);
    assert_eq!(c.max_queue_entries(), 2048);
    assert!(c.cqr && c.css_nvm && c.supports_4k_pages());
    assert_eq!(c.ready_timeout_ms(), 20_000);
    assert_eq!(c.doorbell_stride(), 8);
    assert_eq!(c.mpsmax, 4);
}

#[test]
fn zero_timeout_still_waits() {
    assert_eq!(Cap::decode(0).ready_timeout_ms(), 500);
}

#[test]
fn doorbells_follow_the_stride() {
    let c0 = Cap::decode(0);
    assert_eq!(c0.sq_tail_doorbell(0), 0x1000);
    assert_eq!(c0.cq_head_doorbell(0), 0x1004);
    assert_eq!(c0.sq_tail_doorbell(1), 0x1008);
    assert_eq!(c0.cq_head_doorbell(1), 0x100C);
    let c2 = Cap::decode(2 << 32); // 16-byte stride
    assert_eq!(c2.cq_head_doorbell(1), 0x1000 + 3 * 16);
}

#[test]
fn cc_enable_is_nvm_4k_64_16() {
    let cc = regs::cc_enable();
    assert_eq!(cc & 1, 1);
    assert_eq!((cc >> 4) & 7, 0, "CSS = NVM");
    assert_eq!((cc >> 7) & 0xf, 0, "MPS = 4 KiB");
    assert_eq!((cc >> 16) & 0xf, 6);
    assert_eq!((cc >> 20) & 0xf, 4);
    assert_eq!(cc & regs::CC_SHN_MASK, 0);
}

#[test]
fn csts_and_aqa() {
    let s = Csts::decode(0b1001);
    assert!(s.ready && !s.fatal);
    assert_eq!(s.shutdown, regs::SHST_COMPLETE);
    assert!(Csts::is_absent(u32::MAX));
    assert_eq!(regs::aqa(64, 64), (63 << 16) | 63);
}

// ---------------------------------------------------------------- cmd

#[test]
fn rw_places_lba_count_and_prps() {
    let e = cmd::rw(true, 7, 1, 0x1_2345_6789, 8, 0xAAAA_0000, 0x1_BBBB_0000);
    assert_eq!(e[0], u32::from(cmd::io::WRITE) | (7 << 16));
    assert_eq!(e[1], 1);
    assert_eq!((e[6], e[7]), (0xAAAA_0000, 0));
    assert_eq!((e[8], e[9]), (0xBBBB_0000, 1));
    assert_eq!((e[10], e[11]), (0x2345_6789, 1));
    assert_eq!(e[12], 7, "NLB is 0-based");
    let r = cmd::rw(false, 1, 1, 0, 1, 0, 0);
    assert_eq!(r[0] & 0xff, u32::from(cmd::io::READ));
    assert_eq!(r[12], 0);
}

#[test]
fn queue_creation_is_polled_and_contiguous() {
    let cq = cmd::create_io_cq(3, 1, 64, 0x9000);
    assert_eq!(cq[0] & 0xff, u32::from(cmd::admin::CREATE_IO_CQ));
    assert_eq!(cq[10], (63 << 16) | 1);
    assert_eq!(cq[11], 1, "PC=1, IEN=0");
    let sq = cmd::create_io_sq(4, 1, 64, 0xA000, 1);
    assert_eq!(sq[11], (1 << 16) | 1);
    assert_eq!(cmd::identify(1, cmd::cns::CONTROLLER, 0, 0x1000)[10], 1);
    assert_eq!(cmd::flush(9, 1)[0], 9 << 16);
}

#[test]
fn cqe_decode_status_and_phase() {
    // sq_head 5, sqid 1, cid 0x42, phase 1, SCT 2 SC 0x81, DNR.
    let status: u32 = 0x81 | (2 << 8) | (1 << 14);
    let c = Cqe::decode([0xdead, 0, 5 | (1 << 16), 0x42 | (1 << 16) | (status << 17)]);
    assert_eq!((c.result, c.sq_head, c.sq_id, c.cid), (0xdead, 5, 1, 0x42));
    assert!(c.phase);
    assert_eq!((c.status.sct, c.status.sc, c.status.dnr), (2, 0x81, true));
    assert!(!c.status.ok());
    assert!(Cqe::decode([0, 0, 0, 1 << 16]).status.ok());
}

#[test]
fn completion_phase_flips_each_lap() {
    let mut h = CqHead::new(2);
    let new = |p| Cqe::decode([0, 0, 0, u32::from(p) << 16]);
    assert!(h.is_new(&new(true)) && !h.is_new(&new(false)), "zeroed memory is stale");
    assert_eq!(h.advance(), 1);
    assert_eq!(h.advance(), 0);
    assert!(!h.phase());
    assert!(h.is_new(&new(false)) && !h.is_new(&new(true)), "last lap is stale");
    let mut t = SqTail::new(2);
    assert_eq!((t.slot(), t.advance(), t.advance()), (0, 1, 0));
}

// ---------------------------------------------------------------- identify

fn ns_page(nsze: u64, nlbaf: u8, flbas: u8, formats: &[(u16, u8)]) -> Vec<u8> {
    let mut p = vec![0u8; identify::PAGE_BYTES];
    p[0..8].copy_from_slice(&nsze.to_le_bytes());
    p[25] = nlbaf;
    p[26] = flbas;
    for (i, (ms, lbads)) in formats.iter().enumerate() {
        let v = u32::from(*ms) | (u32::from(*lbads) << 16);
        p[128 + 4 * i..132 + 4 * i].copy_from_slice(&v.to_le_bytes());
    }
    p
}

#[test]
fn namespace_uses_the_active_format() {
    let p = ns_page(1_000_215_216, 1, 1, &[(0, 9), (0, 12)]);
    let n = Namespace::parse(&p).unwrap();
    assert_eq!(n.size_lbas, 1_000_215_216);
    assert_eq!(n.lba_bytes, 4096);
    let p = ns_page(10, 1, 0, &[(0, 9), (0, 12)]);
    assert_eq!(Namespace::parse(&p).unwrap().lba_bytes, 512);
}

#[test]
fn namespace_refusals() {
    assert_eq!(Namespace::parse(&ns_page(0, 0, 0, &[(0, 9)])), None, "inactive");
    assert_eq!(Namespace::parse(&ns_page(8, 0, 1, &[(0, 9)])), None, "index past nlbaf");
    assert_eq!(Namespace::parse(&ns_page(8, 0, 0, &[(0, 8)])), None, "256-byte blocks");
    assert_eq!(Namespace::parse(&[0u8; 100]), None, "short");
}

#[test]
fn controller_fields_and_mdts() {
    let mut p = vec![0u8; identify::PAGE_BYTES];
    p[0..2].copy_from_slice(&0x1c5cu16.to_le_bytes());
    p[4..24].copy_from_slice(b"SN0001              ");
    p[24..64].copy_from_slice(b"SKHynix_HFS512GEJ4X112N                 ");
    p[77] = 5;
    p[516..520].copy_from_slice(&1u32.to_le_bytes());
    p[525] = 1;
    let c = Controller::parse(&p).unwrap();
    assert_eq!(c.vid, 0x1c5c);
    assert_eq!(identify::text(&c.model), "SKHynix_HFS512GEJ4X112N");
    assert_eq!(identify::text(&c.serial), "SN0001");
    assert_eq!(c.max_transfer_bytes(), Some(128 * 1024));
    assert!(c.vwc);
    p[77] = 0;
    assert_eq!(Controller::parse(&p).unwrap().max_transfer_bytes(), None);
}

// ---------------------------------------------------------------- prp

#[test]
fn prp_one_two_and_list() {
    let mut list = [0u64; 512];
    assert_eq!(prp::plan(0x10_0000, 4096, 0, &mut list), Ok((0x10_0000, 0, 0)));
    assert_eq!(prp::plan(0x10_0000, 8192, 0, &mut list), Ok((0x10_0000, 0x10_1000, 0)));
    // Unaligned start: 512 bytes in the first page, then 2 more pages -> list.
    assert_eq!(prp::plan(0x10_0e00, 512 + 4096 + 1, 0x20_0000, &mut list), Ok((0x10_0e00, 0x20_0000, 2)));
    assert_eq!(&list[..2], &[0x10_1000, 0x10_2000]);
    assert_eq!(prp::plan(0x10_0000, 64 * 1024, 0x20_0000, &mut list), Ok((0x10_0000, 0x20_0000, 15)));
    assert_eq!(list[14], 0x10_f000);
}

#[test]
fn prp_refusals() {
    let mut list = [0u64; 4];
    assert_eq!(prp::plan(0, 0, 0, &mut list), Err(PrpError::Empty));
    assert_eq!(prp::plan(2, 8, 0, &mut list), Err(PrpError::Misaligned));
    assert_eq!(prp::plan(0, 4 * 4096, 0x123, &mut list), Err(PrpError::ListMisaligned));
    assert_eq!(prp::plan(0, 6 * 4096, 0x1000, &mut list), Err(PrpError::TooLong));
}

// ---------------------------------------------------------------- chunk

#[test]
fn chunks_cover_exactly_the_requested_bytes() {
    for &(off, len, bl, max) in &[(0u64, 4096usize, 512u32, 128u32), (100, 10_000, 512, 4), (4095, 2, 4096, 16), (1_000_000_007, 70_000, 4096, 16)] {
        let mut done = 0;
        let mut expect_lba = off / u64::from(bl);
        while done < len {
            let c = chunk::next(off, done, len, bl, max);
            assert!(c.blocks >= 1 && c.blocks <= max);
            assert_eq!(c.lba, expect_lba, "contiguous");
            assert!(c.within + c.take <= c.span(bl));
            if done > 0 {
                assert_eq!(c.within, 0, "only the first chunk starts mid-block");
            }
            done += c.take;
            expect_lba = c.lba + u64::from(c.blocks);
        }
        assert_eq!(done, len);
    }
}

#[test]
fn partial_means_read_modify_write() {
    let whole = chunk::next(4096, 0, 8192, 4096, 16);
    assert!(!whole.partial(4096));
    assert!(chunk::next(4096 + 1, 0, 100, 4096, 16).partial(4096), "head");
    assert!(chunk::next(4096, 0, 100, 4096, 16).partial(4096), "tail");
}

// ---------------------------------------------------------------- window

#[test]
fn window_refuses_everything_outside() {
    let w = Window::new(1000, 1999, 10_000, 512).unwrap();
    assert_eq!(w.bytes(), 1000 * 512);
    assert_eq!(w.absolute(0, 1), Some(1000));
    assert_eq!(w.absolute(999, 1), Some(1999));
    assert_eq!(w.absolute(999, 2), None, "one block past the end");
    assert_eq!(w.absolute(u64::MAX, 2), None, "overflow");
    assert_eq!(w.absolute(0, 0), None);
    assert!(w.fits(0, 512_000) && !w.fits(1, 512_000) && !w.fits(u64::MAX, 1));
    assert_eq!(Window::new(5, 4, 10, 512), None, "inverted");
    assert_eq!(Window::new(0, 10, 10, 512), None, "past the namespace");
}

// ---------------------------------------------------------------- gpt

#[test]
fn crc32_check_value() {
    assert_eq!(gpt::crc32(b"123456789"), 0xCBF4_3926);
}

const LINUX_FS: [u8; 16] = [0xaf, 0x3d, 0xc6, 0x0f, 0x83, 0x84, 0x72, 0x47, 0x8e, 0x79, 0x3d, 0x69, 0xd8, 0x47, 0x7d, 0xe4];

/// A disk of 512-byte blocks: LBA 1 header + 128 x 128-byte entries at LBA 2,
/// with the given `(slot, first, last, name)` partitions and both CRCs right.
fn gpt_disk(parts: &[(u32, u64, u64, &str)]) -> (Vec<u8>, Vec<u8>) {
    let mut entries = vec![0u8; 128 * 128];
    for &(slot, first, last, name) in parts {
        let e = &mut entries[(slot as usize - 1) * 128..slot as usize * 128];
        e[0..16].copy_from_slice(&LINUX_FS);
        e[16] = slot as u8; // unique guid, distinct per slot
        e[32..40].copy_from_slice(&first.to_le_bytes());
        e[40..48].copy_from_slice(&last.to_le_bytes());
        for (i, c) in name.encode_utf16().enumerate() {
            e[56 + 2 * i..58 + 2 * i].copy_from_slice(&c.to_le_bytes());
        }
    }
    let mut h = vec![0u8; 512];
    h[0..8].copy_from_slice(&gpt::SIGNATURE);
    h[8..12].copy_from_slice(&0x0001_0000u32.to_le_bytes());
    h[12..16].copy_from_slice(&92u32.to_le_bytes());
    h[24..32].copy_from_slice(&1u64.to_le_bytes());
    h[32..40].copy_from_slice(&999_999u64.to_le_bytes());
    h[40..48].copy_from_slice(&34u64.to_le_bytes());
    h[48..56].copy_from_slice(&999_966u64.to_le_bytes());
    h[72..80].copy_from_slice(&2u64.to_le_bytes());
    h[80..84].copy_from_slice(&128u32.to_le_bytes());
    h[84..88].copy_from_slice(&128u32.to_le_bytes());
    h[88..92].copy_from_slice(&gpt::crc32(&entries).to_le_bytes());
    let crc = gpt::crc32(&h[..92]);
    h[16..20].copy_from_slice(&crc.to_le_bytes());
    (h, entries)
}

#[test]
fn gpt_reads_partitions_by_linux_number() {
    // ryzen's shape: 1 ESP, 2 MSR, 3 the big one, 4 recovery, slot 5 unused.
    let (h, e) = gpt_disk(&[(1, 2048, 534_527, "EFI system partition"), (2, 534_528, 567_295, "MSR"), (3, 567_296, 900_000, "Basic data partition"), (4, 900_001, 999_966, "WinRE")]);
    let hdr = Header::parse(&h).unwrap();
    assert_eq!((hdr.entries, hdr.entry_size, hdr.entries_lba), (128, 128, 2));
    hdr.check_entries(&e).unwrap();
    let p3 = hdr.partition(&e, 3).unwrap();
    assert_eq!((p3.first_lba, p3.last_lba), (567_296, 900_000));
    let mut name = [0u8; 36];
    let n = p3.name_ascii(&mut name);
    assert_eq!(&name[..n], b"Basic data partition");
    assert_eq!(hdr.partition(&e, 5), None, "unused slot");
    assert_eq!(hdr.partition(&e, 0), None);
    assert_eq!(hdr.partition(&e, 129), None);
}

#[test]
fn gpt_refuses_corruption() {
    let (h, e) = gpt_disk(&[(3, 100, 200, "x")]);
    let mut bad = h.clone();
    bad[40] ^= 1; // first_usable flipped, CRC not recomputed
    assert_eq!(Header::parse(&bad), Err(GptError::HeaderCrc));
    let mut bad = h.clone();
    bad[0] = b'X';
    assert_eq!(Header::parse(&bad), Err(GptError::Signature));
    let hdr = Header::parse(&h).unwrap();
    let mut bad_e = e.clone();
    bad_e[2 * 128 + 40] ^= 0x80; // p3's last LBA, a misread sector
    assert_eq!(hdr.check_entries(&bad_e), Err(GptError::EntriesCrc));
    assert_eq!(hdr.check_entries(&e[..100]), Err(GptError::Short));
}

#[test]
fn gpt_refuses_entries_outside_the_usable_area() {
    let (h, e) = gpt_disk(&[(1, 10, 20, "below first_usable"), (2, 999_960, 999_999, "past last_usable"), (3, 500, 400, "inverted")]);
    let hdr = Header::parse(&h).unwrap();
    hdr.check_entries(&e).unwrap();
    for n in 1..=3 {
        assert_eq!(hdr.partition(&e, n), None, "partition {n}");
    }
}

/// The real table off a disk, when one is provided:
/// `AKUMA_NVME_GPT=<file of LBA 1..33, 512-byte blocks> cargo test -- --ignored`.
/// Kept out of the repo: a partition table carries the disk's GUIDs.
#[test]
#[ignore = "needs AKUMA_NVME_GPT=<dump of LBA 1..33>"]
fn gpt_real_dump() {
    let path = std::env::var("AKUMA_NVME_GPT").unwrap();
    let raw = std::fs::read(path).unwrap();
    let hdr = Header::parse(&raw[..512]).unwrap();
    let e = &raw[512..512 + hdr.entry_bytes()];
    hdr.check_entries(e).unwrap();
    for n in 1..=hdr.entries {
        if let Some(p) = hdr.partition(e, n) {
            let mut name = [0u8; 36];
            let len = p.name_ascii(&mut name);
            std::println!("p{n}: {}..={} {}", p.first_lba, p.last_lba, core::str::from_utf8(&name[..len]).unwrap());
        }
    }
}
