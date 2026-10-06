use std::vec;
use std::vec::Vec;

use crate::aml::{Region, find_region};
use crate::ec::{BLOCK_LEN, Reading, State};
use crate::render::{Source, render};
use crate::sim;

/// The block read from the laptop on battery (docs §5.8): 13028 mV, remaining
/// 5294, full 5442, design 5700, RSOC 97 %, status 0b00000110 (no AC, battery
/// in, BTST 1), power 6983 mW.
fn measured() -> [u8; BLOCK_LEN] {
    let mut b = [0u8; BLOCK_LEN];
    b[0x00] = 0b0000_0110;
    b[0x04..0x06].copy_from_slice(&5700u16.to_le_bytes());
    b[0x06..0x08].copy_from_slice(&11_310u16.to_le_bytes());
    b[0x08..0x0A].copy_from_slice(&5442u16.to_le_bytes());
    // 6983 mW / 13.028 V = 536 mA
    b[0x0C..0x0E].copy_from_slice(&536u16.to_le_bytes());
    b[0x0E..0x10].copy_from_slice(&5294u16.to_le_bytes());
    b[0x10..0x12].copy_from_slice(&13_028u16.to_le_bytes());
    b[0x12] = 97;
    b
}

#[test]
fn decodes_the_measured_block() {
    let r = Reading::decode(&measured());
    assert!(!r.ac);
    assert!(r.battery_present);
    assert_eq!(r.state, State::Discharging);
    assert_eq!(r.remaining_mwh, 52_940);
    assert_eq!(r.full_mwh, 54_420);
    assert_eq!(r.design_mwh, 57_000);
    assert_eq!(r.voltage_mv, 13_028);
    assert_eq!(r.percent(), 97);
    assert_eq!(r.power_mw(), 6_983);
    assert!(r.validate());
    // 52.94 Wh at 6.983 W ≈ 7 h 34 min
    assert_eq!(r.minutes(), Some(454));
}

#[test]
fn garbage_is_not_a_battery() {
    for fill in [0x00u8, 0xFF] {
        let r = Reading::decode(&[fill; BLOCK_LEN]);
        assert!(!r.validate(), "fill {fill:#x}");
        let mut out = [0u8; 512];
        let n = render(&mut out, Source::Ec, Some(&[fill; BLOCK_LEN]));
        let s = core::str::from_utf8(&out[..n]).unwrap();
        assert!(s.contains("valid=0"), "{s}");
        assert!(!s.contains("percent="), "garbage shown as a charge: {s}");
    }
}

#[test]
fn unknown_btst_is_not_guessed() {
    let mut b = measured();
    b[0] = 0b0011_1111; // AC, battery, BTST = 15
    let r = Reading::decode(&b);
    assert_eq!(r.state, State::Unknown);
    assert_eq!(r.btst, 15);
    assert_eq!(r.minutes(), None);
}

#[test]
fn ac_with_idle_btst_is_not_charging() {
    let mut b = measured();
    b[0] = 0b0000_0011;
    assert_eq!(Reading::decode(&b).state, State::NotCharging);
}

#[test]
fn render_matches_the_documented_shape() {
    let mut out = [0u8; 512];
    let n = render(&mut out, Source::Ec, Some(&measured()));
    let s = core::str::from_utf8(&out[..n]).unwrap();
    for line in [
        "source=ec\n",
        "ac=0\n",
        "battery=1\n",
        "valid=1\n",
        "status=Discharging\n",
        "percent=97\n",
        "voltage_mv=13028\n",
        "power_mw=6983\n",
        "energy_now_mwh=52940\n",
        "minutes=454\n",
        "btst=1\n",
    ] {
        assert!(s.contains(line), "missing {line:?} in:\n{s}");
    }
    assert!(s.contains("ec_raw=06"), "{s}");
}

#[test]
fn render_with_no_source() {
    let mut out = [0u8; 64];
    let n = render(&mut out, Source::None, None);
    assert_eq!(&out[..n], b"source=none\nac=unknown\nbattery=0\n");
}

#[test]
fn render_truncates_instead_of_panicking() {
    let mut out = [0u8; 10];
    let n = render(&mut out, Source::Ec, Some(&measured()));
    assert_eq!(n, 10);
}

#[test]
fn sim_cycles_and_decodes() {
    for t in 0..240 {
        let r = Reading::decode(&sim::block(t));
        assert!(r.validate(), "t={t}");
        assert!((40..=100).contains(&r.percent()), "t={t} pct={}", r.percent());
        assert_eq!(r.state, if t % 120 < 60 { State::Discharging } else { State::Charging });
    }
    assert_eq!(Reading::decode(&sim::block(0)).percent(), 100);
    assert_eq!(Reading::decode(&sim::block(59)).percent(), 41);
    assert_eq!(Reading::decode(&sim::block(60)).percent(), 40);
}

// ---- AML ------------------------------------------------------------------

fn decl(name_string: &[u8], space: u8, tail: &[u8]) -> Vec<u8> {
    let mut v = vec![0x5B, 0x80];
    v.extend_from_slice(name_string);
    v.push(space);
    v.extend_from_slice(tail);
    v
}

#[test]
fn finds_a_dword_region() {
    // OperationRegion (ERAM, SystemMemory, 0xFEEC2380, 0x100)
    let d = decl(b"ERAM", 0, &[0x0C, 0x80, 0x23, 0xEC, 0xFE, 0x0B, 0x00, 0x01]);
    let mut buf = vec![0xAAu8; 37];
    buf.extend_from_slice(&d);
    buf.extend_from_slice(&[0x5B, 0x80, 1, 2, 3]);
    assert_eq!(
        find_region(&buf, b"ERAM"),
        Some(Region { space: 0, offset: 0xFEEC_2380, len: 0x100 })
    );
}

#[test]
fn skips_other_names_and_other_spaces() {
    let mut buf = decl(b"ECOR", 3, &[0x0A, 0x00, 0x0B, 0x00, 0x01]);
    buf.extend(decl(b"ERAM", 3, &[0x0C, 1, 2, 3, 4, 0x0A, 8])); // EmbeddedControl, not memory
    assert_eq!(find_region(&buf, b"ERAM"), None);
    buf.extend(decl(b"ERAM", 0, &[0x0C, 0x00, 0x10, 0x00, 0x80, 0x0A, 0x20]));
    assert_eq!(find_region(&buf, b"ERAM").map(|r| r.offset), Some(0x8000_1000));
}

#[test]
fn name_prefixes() {
    // \ERAM, ^ERAM, \_SB_.EC0_.ERAM (multi), and dual
    for ns in [
        &b"\\ERAM"[..],
        &b"^ERAM"[..],
        &b"\\\x2E_SB_ERAM"[..],
        &b"\\\x2F\x03_SB_PCI0ERAM"[..],
    ] {
        let d = decl(ns, 0, &[0x0C, 0x80, 0x23, 0xEC, 0xFE, 0x0A, 0x40]);
        assert_eq!(find_region(&d, b"ERAM").map(|r| r.offset), Some(0xFEEC_2380), "{ns:?}");
    }
}

#[test]
fn qword_and_constants() {
    let d = decl(b"ERAM", 0, &[0x0E, 0, 0, 0, 0, 1, 0, 0, 0, 0x01]);
    assert_eq!(find_region(&d, b"ERAM"), Some(Region { space: 0, offset: 1 << 32, len: 1 }));
}

#[test]
fn truncated_declarations_are_not_regions() {
    let d = decl(b"ERAM", 0, &[0x0C, 0x80, 0x23]);
    assert_eq!(find_region(&d, b"ERAM"), None);
    assert_eq!(find_region(&[0x5B], b"ERAM"), None);
    assert_eq!(find_region(&[], b"ERAM"), None);
    // a computed offset (e.g. a name reference) is not an integer constant
    let d = decl(b"ERAM", 0, &[b'B', b'A', b'S', b'E', 0x0A, 1]);
    assert_eq!(find_region(&d, b"ERAM"), None);
}
