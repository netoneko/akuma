extern crate std;

use akuma_wifi::cmd::Command;
use akuma_wifi::sim::{SIM_WPA2_PSK, SimRadio};
use akuma_wifi::status::{Bss, Error, Link};
use akuma_wifi::{IfName, Security, Ssid, hex};

use crate::{AutoStep, ConfigError, auto_backoff, auto_step, choose, derive_psk, parse_config, printable_ssid, valid_name};

fn h(s: &str) -> [u8; 32] {
    let mut k = [0u8; 32];
    assert_eq!(hex::decode(s.as_bytes(), &mut k), Some(32));
    k
}

#[test]
fn psk_matches_the_80211i_test_vector() {
    // IEEE 802.11i-2004, Annex H.4.1: "password", SSID "IEEE".
    assert_eq!(derive_psk(b"password", b"IEEE"), h("f42c6fc52df0ebef9ebb4b90b38a5f902e83fe1b135a70e23aed762e9710a12e"));
}

#[test]
fn psk_matches_the_simulated_radio() {
    // The kernel's `wifisim` network accepts exactly this key.
    assert_eq!(derive_psk(b"akuma-sim-passphrase", b"akuma-sim-wpa2"), SIM_WPA2_PSK);
}

#[test]
fn passphrase_becomes_psk_and_is_not_kept() {
    let n = parse_config("home", "# c\nssid = akuma-sim-wpa2\npassphrase = akuma-sim-passphrase\npriority = 5\n").unwrap();
    assert_eq!(n.psk, Some(SIM_WPA2_PSK));
    assert_eq!(n.priority, 5);
    let text = n.to_config();
    assert!(!text.contains("passphrase"), "{text}");
    assert!(text.contains("psk = bfdec942"), "{text}");
    assert_eq!(parse_config("home", &text), Ok(n));
}

#[test]
fn ssids_round_trip_including_odd_bytes() {
    for raw in [&b"plain"[..], b"with space inside", b" leading", b"trailing ", "sim\u{2603}".as_bytes(), b"\x00\xff\x7f", b"#hash=eq"] {
        let n = crate::Network {
            name: "x".into(),
            ssid: Ssid::new(raw).unwrap(),
            psk: None,
            priority: 0,
            autoconnect: true,
            bssid: Some([2, 0, 0, 0, 0, 1]),
        };
        assert_eq!(parse_config("x", &n.to_config()), Ok(n.clone()), "{:?}", n.to_config());
    }
}

#[test]
fn config_refusals() {
    assert_eq!(parse_config("x", "psk = 00\n"), Err(ConfigError::BadPsk));
    assert_eq!(parse_config("x", "priority = 1\n"), Err(ConfigError::NoSsid));
    assert_eq!(parse_config("x", "ssid = a\npassphrase = short\n"), Err(ConfigError::BadPassphrase), "7 chars");
    let both = std::format!("ssid = a\npassphrase = 12345678\npsk = {}\n", "00".repeat(32));
    assert_eq!(parse_config("x", &both), Err(ConfigError::TwoKeys));
    assert_eq!(parse_config("x", "ssid = a\nautoconnect = maybe\n"), Err(ConfigError::BadAutoconnect));
    assert_eq!(parse_config("x", "ssid = a\nchannel = 6\n"), Err(ConfigError::UnknownKey));
    assert_eq!(parse_config("x", "ssid = a\nbssid = 02:00\n"), Err(ConfigError::BadBssid));
    let long = std::format!("ssid = {}\n", "a".repeat(33));
    assert_eq!(parse_config("x", &long), Err(ConfigError::BadSsid));
}

fn bss(ssid: &[u8], last: u8, signal: i8, security: Security) -> Bss {
    Bss { ssid: Ssid::new(ssid).unwrap(), bssid: [2, 0, 0, 0, 0, last], chan: 1, signal, security }
}

fn net(name: &str, ssid: &[u8], psk: bool, priority: i32) -> crate::Network {
    crate::Network {
        name: name.into(),
        ssid: Ssid::new(ssid).unwrap(),
        psk: psk.then_some([1; 32]),
        priority,
        autoconnect: true,
        bssid: None,
    }
}

#[test]
fn choose_priority_then_signal_and_only_joinable() {
    let scan = [
        bss(b"a", 1, -40, Security::Open),
        bss(b"b", 2, -70, Security::Wpa2Psk),
        bss(b"b", 3, -50, Security::Wpa2Psk),
        bss(b"c", 4, -30, Security::Other),
    ];
    let known = [net("a", b"a", false, 0), net("b", b"b", true, 1), net("c", b"c", true, 9)];
    let (n, b) = choose(&known, &scan).unwrap();
    assert_eq!((n.name.as_str(), b.bssid[5]), ("b", 3), "priority beats signal; then the stronger AP");
    let known = [net("a", b"a", false, 0), net("b2", b"b", true, 0)];
    assert_eq!(choose(&known, &scan).unwrap().0.name, "a", "equal priority: strongest signal");
    let mut off = net("a", b"a", false, 10);
    off.autoconnect = false;
    assert!(choose(&[off], &scan).is_none(), "autoconnect = false is never picked");
    assert!(choose(&[net("a", b"a", true, 0)], &scan).is_none(), "a key for an open network");
    assert!(choose(&[net("b", b"b", false, 0)], &scan).is_none(), "no key for a WPA2 network");
    assert!(choose(&[net("c", b"c", true, 0)], &scan).is_none(), "security we cannot join");
}

#[test]
fn names_and_display() {
    assert!(valid_name("home-5g.v2"));
    for bad in ["", ".hidden", "a/b", "spa ce", "../x", &"x".repeat(65)] {
        assert!(!valid_name(bad), "{bad:?}");
    }
    assert_eq!(printable_ssid(&Ssid::new(b"a\x00b\\").unwrap()), "a\\x00b\\x5c");
}

#[test]
fn auto_policy_waits_for_the_kernel_and_backs_off() {
    assert_eq!(auto_step(Link::Connected), AutoStep::Keep);
    assert_eq!(auto_step(Link::NoRadio), AutoStep::NoRadio);
    assert_eq!(auto_step(Link::Scanning), AutoStep::Wait);
    assert_eq!(auto_step(Link::Associating), AutoStep::Wait, "the kernel rejoins a dropped link itself");
    assert_eq!(auto_step(Link::Down), AutoStep::Join);
    assert_eq!(auto_step(Link::Failed), AutoStep::Join);
    assert_eq!(auto_backoff(5, true), 5);
    assert_eq!(auto_backoff(5, false), 10);
    assert_eq!(auto_backoff(40, false), 60);
    assert_eq!(auto_backoff(60, false), 60);
    assert_eq!(auto_backoff(60, true), 5);
}

/// `wifi auto` against the lossy simulated radio (`wifisim=lossy`), on the
/// host: the loop of `cmd_auto` with the device calls replaced by the
/// simulator. Over a long run it keeps the network joined, never sends a
/// command while the kernel is joining, and recovers from every drop and
/// every failed join.
#[test]
fn auto_keeps_the_lossy_radio_joined() {
    let iface = IfName::new(b"wlan0").unwrap();
    let known = [crate::Network {
        name: "sim".into(),
        ssid: Ssid::new(b"akuma-sim-wpa2").unwrap(),
        psk: Some(SIM_WPA2_PSK),
        priority: 0,
        autoconnect: true,
        bssid: None,
    }];
    for seed in 1..=20u64 {
        let mut radio = SimRadio::new_lossy(iface, seed * 0x9e37_79b9);
        let (mut joins, mut waits, mut no_range, mut connected_reads, mut reads) = (0u32, 0u32, 0u32, 0u32, 0u32);
        let mut backoff = 5;
        // One iteration = one look by `cmd_auto`; `backoff` seconds of
        // reads (10 per second) pass in between, as the kernel's clock does.
        for look in 0..2000 {
            let s = *radio.observe();
            reads += 1;
            let joined = match auto_step(s.link) {
                AutoStep::Keep => {
                    connected_reads += 1;
                    true
                }
                AutoStep::NoRadio => panic!("seed {seed}: the sim radio is always there"),
                AutoStep::Wait => {
                    waits += 1;
                    true
                }
                AutoStep::Join => {
                    // `scan()`: send, wait for `scans` to move.
                    let before = radio.observe().scans;
                    radio.apply(&Command::Scan { iface });
                    assert_ne!(radio.observe().scans, before, "seed {seed}: scan {look} did not complete");
                    let results = radio.observe().results().to_vec();
                    match choose(&known, &results) {
                        None => {
                            no_range += 1;
                            false
                        }
                        Some((n, _)) => {
                            joins += 1;
                            radio.apply(&Command::Connect { iface, ssid: n.ssid, psk: n.psk, bssid: n.bssid });
                            // `join()`: read until connected or failed.
                            let mut outcome = None;
                            for _ in 0..900 {
                                let s = *radio.observe();
                                if s.ssid == n.ssid && matches!(s.link, Link::Connected | Link::Failed) {
                                    outcome = Some(s);
                                    break;
                                }
                            }
                            let st = outcome.unwrap_or_else(|| panic!("seed {seed}: join {look} never answered"));
                            if st.link == Link::Failed {
                                assert_eq!(st.error, Error::Timeout, "seed {seed}: the right key and a known network fail only by timeout");
                            }
                            st.link == Link::Connected
                        }
                    }
                }
            };
            backoff = auto_backoff(backoff, joined);
            for _ in 0..backoff * 10 {
                radio.observe();
            }
        }
        let faults = radio.faults();
        assert!(joins > 0 && connected_reads > 0, "seed {seed}");
        assert!(faults[3] > 0, "seed {seed}: the link never dropped in {reads} reads");
        assert!(waits > 0, "seed {seed}: never caught the kernel rejoining");
        // A drop that ended `failed` is a join the tool must redo; the tool
        // must have done at least that many, and not many more than the
        // faults account for.
        assert!(joins >= faults[4], "seed {seed}: {joins} joins for {} failed drops", faults[4]);
        assert!(joins <= 2 + faults[2] + faults[4] + no_range, "seed {seed}: {joins} joins, faults {faults:?}, out of range {no_range}");
    }
}

/// `parse_config` over random text: every outcome is `Ok` or a named error.
#[test]
fn config_parser_survives_random_text() {
    let mut x = 0x1234_5678_9abc_def0u64;
    let mut next = move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let words = ["ssid", "ssid_hex", "psk", "passphrase", "priority", "autoconnect", "bssid", " = ", "=", "\n", "#", "true", "-1", "02:00:00:00:00:01", "6162", "a", " "];
    for _ in 0..20_000 {
        let mut t = std::string::String::new();
        for _ in 0..(next() % 30) {
            match next() % 5 {
                0 => t.push(char::from_u32((next() % 0x80) as u32).unwrap_or('x')),
                1 => t.push_str(&"0".repeat((next() % 70) as usize)),
                // No `passphrase =` with a valid value: PBKDF2 is 4096 rounds.
                _ => t.push_str(words[(next() % words.len() as u64) as usize]),
            }
        }
        if t.contains("passphrase") {
            continue;
        }
        if let Ok(n) = parse_config("x", &t) {
            assert!(!n.ssid.is_empty());
            assert_eq!(parse_config("x", &n.to_config()).as_ref(), Ok(&n), "{t:?}");
        }
    }
}
