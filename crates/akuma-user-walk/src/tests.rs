extern crate std;
use super::*;
use core::cell::RefCell;
use std::collections::BTreeMap;
use std::vec;

/// Sparse pages keyed by page-aligned VA; an absent page does not translate.
struct Mem(RefCell<BTreeMap<u64, [u8; 4096]>>);

impl Mem {
    fn with_pages(pages: &[u64]) -> Self {
        let m = Mem(RefCell::new(BTreeMap::new()));
        for &p in pages {
            m.0.borrow_mut().insert(p, [0; 4096]);
        }
        m
    }
}

impl Walk for Mem {
    fn read_page(&self, va: u64, out: &mut [u8]) -> bool {
        assert!(out.len() <= page_remaining(va), "a Walk call must stay inside one page");
        let pages = self.0.borrow();
        let Some(pg) = pages.get(&(va & !(PAGE - 1))) else { return false };
        let o = (va & (PAGE - 1)) as usize;
        out.copy_from_slice(&pg[o..o + out.len()]);
        true
    }
    fn write_page(&self, va: u64, src: &[u8]) -> bool {
        assert!(src.len() <= page_remaining(va));
        let mut pages = self.0.borrow_mut();
        let Some(pg) = pages.get_mut(&(va & !(PAGE - 1))) else { return false };
        let o = (va & (PAGE - 1)) as usize;
        pg[o..o + src.len()].copy_from_slice(src);
        true
    }
}

#[test]
fn regions_reject_wrap_kernel_half_and_gaps() {
    let r = [(0x1000, 0x3000), (0x10000, 0x11000)];
    assert!(in_regions(&r, 0x1000, 0x2000));
    assert!(!in_regions(&r, 0x2000, 0x2000), "spills past the end of a region");
    assert!(!in_regions(&r, 0x3000, 0x8000), "spans the gap between regions");
    assert!(in_regions(&r, 0xdead_beef, 0), "zero length is always fine");
    assert!(!in_regions(&r, u64::MAX - 3, 8), "wraps");
    assert!(!in_regions(&[(0, u64::MAX)], USER_END - 2, 4), "crosses into the kernel half");
}

#[test]
fn copies_cross_page_boundaries_and_stop_at_a_hole() {
    let m = Mem::with_pages(&[0x1000, 0x2000, 0x4000]);
    let src: std::vec::Vec<u8> = (0..300u32).map(|i| i as u8).collect();
    assert!(copy_to(&m, 0x1000 + 4096 - 100, &src), "straddles pages 0x1000 and 0x2000");
    let mut back = vec![0u8; 300];
    assert!(copy_from(&m, &mut back, 0x1000 + 4096 - 100));
    assert_eq!(back, src);
    // 0x3000 is not mapped: a copy across it fails, having written the part before.
    assert!(!copy_to(&m, 0x2000 + 4096 - 4, &[1u8; 8]));
    let mut b = [0u8; 4];
    assert!(copy_from(&m, &mut b, 0x2000 + 4096 - 4));
    assert_eq!(b, [1; 4]);
}

#[test]
fn cstr_ends_at_the_nul_even_before_an_unmapped_page() {
    let m = Mem::with_pages(&[0x1000]);
    // "abc\0" at the very end of the last mapped page.
    assert!(copy_to(&m, 0x1000 + 4096 - 4, b"abc\0"));
    assert_eq!(read_cstr(&m, 0x1000 + 4096 - 4, 64).as_deref(), Ok(&b"abc"[..]));
    // No NUL before the hole: Bad, not TooLong.
    assert!(copy_to(&m, 0x1000 + 4096 - 3, b"xyz"));
    assert_eq!(read_cstr(&m, 0x1000 + 4096 - 3, 64), Err(Fault::Bad));
}

#[test]
fn cstr_cap_is_too_long_and_a_longer_than_chunk_string_is_whole() {
    let m = Mem::with_pages(&[0x1000, 0x2000]);
    let long = vec![b'q'; 700];
    assert!(copy_to(&m, 0x1000 + 4000, &long)); // crosses into 0x2000, then a NUL (zero page)
    let got = read_cstr(&m, 0x1000 + 4000, 4096).unwrap();
    assert_eq!(got.len(), 700);
    assert_eq!(read_cstr(&m, 0x1000 + 4000, 600), Err(Fault::TooLong));
    assert_eq!(read_cstr(&m, 0x1000, 0), Err(Fault::TooLong), "max 0 reads nothing and finds no NUL");
}

#[test]
fn ptr_array_terminates_caps_and_checks_each_pointer() {
    let m = Mem::with_pages(&[0x1000]);
    let mut raw = std::vec::Vec::new();
    for p in [0x1800u64, 0x1900, 0] {
        raw.extend_from_slice(&p.to_le_bytes());
    }
    assert!(copy_to(&m, 0x1000, &raw));
    let ok = |p: u64, _l: u64| p < 0x2000;
    assert_eq!(read_ptr_array(&m, 0x1000, 8, ok).unwrap(), vec![0x1800, 0x1900]);
    assert_eq!(read_ptr_array(&m, 0x1000, 2, ok), Err(Fault::TooLong), "terminator beyond the cap");
    assert_eq!(read_ptr_array(&m, 0x1000, 8, |p, _| p != 0x1900), Err(Fault::Bad), "a bad entry");
    assert_eq!(read_ptr_array(&m, 0x5000, 8, ok), Err(Fault::Bad), "the array itself is unmapped");
}
