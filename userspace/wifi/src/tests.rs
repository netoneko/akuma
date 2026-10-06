extern crate std;

use akuma_wifi::sim::SIM_WPA2_PSK;
use akuma_wifi::status::Bss;
use akuma_wifi::{Security, Ssid, hex};

use crate::{ConfigError, choose, derive_psk, parse_config, printable_ssid, valid_name};

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
