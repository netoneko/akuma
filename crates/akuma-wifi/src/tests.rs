extern crate std;

use crate::cmd::{self, CmdError, Command};
use crate::sim::{SIM_WPA2_PSK, SimRadio};
use crate::status::{Error, Link, Status};
use crate::{IfName, Security, Ssid, hex};

fn wlan0() -> IfName {
    IfName::new(b"wlan0").unwrap()
}

fn line(c: &Command) -> std::vec::Vec<u8> {
    let mut buf = [0u8; 256];
    let n = cmd::write(c, &mut buf).unwrap();
    buf[..n].to_vec()
}

// ---------------------------------------------------------------- hex

#[test]
fn hex_round_trips_and_refuses() {
    let mut e = [0u8; 8];
    assert_eq!(hex::encode(&[0x00, 0xab, 0xff], &mut e), Some(6));
    assert_eq!(&e[..6], b"00abff");
    let mut d = [0u8; 3];
    assert_eq!(hex::decode(b"00ABff", &mut d), Some(3));
    assert_eq!(d, [0x00, 0xab, 0xff]);
    assert_eq!(hex::decode(b"abc", &mut d), None, "odd length");
    assert_eq!(hex::decode(b"zz", &mut d), None, "not hex");
    assert_eq!(hex::decode(b"0011223344", &mut d), None, "short destination");
    assert_eq!(hex::encode(&[1, 2, 3], &mut [0u8; 5]), None, "short destination");
    let mut m = [0u8; 17];
    hex::encode_mac(&[0x02, 0, 0, 0xaa, 0xbb, 0xcc], &mut m).unwrap();
    assert_eq!(&m, b"02:00:00:aa:bb:cc");
    assert_eq!(hex::decode_mac(b"02:00:00:AA:bb:cc"), Some([0x02, 0, 0, 0xaa, 0xbb, 0xcc]));
    assert_eq!(hex::decode_mac(b"02:00:00:aa:bb"), None);
    assert_eq!(hex::decode_mac(b"02-00-00-aa-bb-cc"), None);
}

// ---------------------------------------------------------------- cmd

#[test]
fn commands_round_trip_through_their_text() {
    let ssid = Ssid::new("sim\u{2603}".as_bytes()).unwrap();
    for c in [
        Command::Scan { iface: wlan0() },
        Command::Disconnect { iface: wlan0() },
        Command::Connect { iface: wlan0(), ssid, psk: None, bssid: None },
        Command::Connect { iface: wlan0(), ssid, psk: Some(SIM_WPA2_PSK), bssid: Some([2, 0, 0, 0, 0, 2]) },
    ] {
        assert_eq!(cmd::parse(&line(&c)), Ok(c), "{}", std::string::String::from_utf8_lossy(&line(&c)));
    }
}

#[test]
fn connect_line_is_what_the_doc_says() {
    let c = Command::Connect { iface: wlan0(), ssid: Ssid::new(b"ab").unwrap(), psk: None, bssid: None };
    assert_eq!(line(&c), b"connect wlan0 6162 -\n");
}

#[test]
fn command_refusals() {
    assert_eq!(cmd::parse(b""), Err(CmdError::Empty));
    assert_eq!(cmd::parse(b"   \n"), Err(CmdError::Empty));
    assert_eq!(cmd::parse(b"join wlan0"), Err(CmdError::UnknownVerb));
    assert_eq!(cmd::parse(b"scan"), Err(CmdError::MissingArgument));
    assert_eq!(cmd::parse(b"scan wlan0 now"), Err(CmdError::TooManyArguments));
    assert_eq!(cmd::parse(b"scan wl/an0"), Err(CmdError::BadInterface));
    assert_eq!(cmd::parse(b"scan wlan0wlan0wlan0x"), Err(CmdError::BadInterface), "16 bytes");
    assert_eq!(cmd::parse(b"connect wlan0 616"), Err(CmdError::BadSsid), "odd hex");
    assert_eq!(cmd::parse(b"connect wlan0 61"), Err(CmdError::MissingArgument), "no key");
    let long = [b'a'; 66];
    let mut l = std::vec::Vec::from(&b"connect wlan0 "[..]);
    l.extend_from_slice(&long);
    l.extend_from_slice(b" -");
    assert_eq!(cmd::parse(&l), Err(CmdError::BadSsid), "33-byte SSID");
    assert_eq!(cmd::parse(b"connect wlan0 61 abcd"), Err(CmdError::BadKey), "short key");
    assert_eq!(cmd::parse(b"connect wlan0 61 - 02:00"), Err(CmdError::BadBssid));
    assert!(cmd::parse(b"scan wlan0\r\n").is_ok(), "CRLF");
}

// ---------------------------------------------------------------- status

#[test]
fn status_round_trips_and_fits_4k() {
    let mut r = SimRadio::new(wlan0());
    r.apply(&Command::Scan { iface: wlan0() });
    r.apply(&Command::Connect {
        iface: wlan0(),
        ssid: Ssid::new(b"akuma-sim-wpa2").unwrap(),
        psk: Some(SIM_WPA2_PSK),
        bssid: None,
    });
    let s = *r.status();
    let mut buf = [0u8; 4096];
    let n = s.write(&mut buf).unwrap();
    assert_eq!(Status::parse(&buf[..n]), Some(s));
    // A full table of 32-byte SSIDs still fits a 4 KiB read.
    let mut full = s;
    full.nbss = crate::MAX_BSS;
    for b in &mut full.bss {
        b.ssid = Ssid::new(&[0xff; 32]).unwrap();
        b.signal = -100;
        b.chan = 165;
    }
    assert!(full.write(&mut buf).is_some());
}

#[test]
fn status_text_is_what_the_doc_says() {
    let s = Status::no_radio(wlan0());
    let mut buf = [0u8; 512];
    let n = s.write(&mut buf).unwrap();
    let t = core::str::from_utf8(&buf[..n]).unwrap();
    assert!(t.starts_with("iface=wlan0\nradio=none\nstate=no-radio\nssid=\nbssid=00:00:00:00:00:00\n"), "{t}");
    assert!(t.ends_with("error=none\nscans=0\n"), "{t}");
}

#[test]
fn status_parser_skips_unknown_keys_and_needs_state() {
    let t = b"iface=wlan0\nradio=rtw89\nfuture=thing\nstate=down\nscans=3\nnot a kv line\n";
    let s = Status::parse(t).unwrap();
    assert_eq!((s.link, s.radio, s.scans), (Link::Down, "rtw89", 3));
    assert_eq!(Status::parse(b"iface=wlan0\n"), None, "no state line");
    assert_eq!(Status::parse(b"state=sleepy\n"), None, "unknown state");
}

// ---------------------------------------------------------------- sim

fn connect(r: &mut SimRadio, ssid: &[u8], psk: Option<[u8; 32]>) -> (Link, Error) {
    r.apply(&Command::Connect { iface: wlan0(), ssid: Ssid::new(ssid).unwrap(), psk, bssid: None });
    (r.status().link, r.status().error)
}

#[test]
fn sim_scan_reports_every_network_and_counts() {
    let mut r = SimRadio::new(wlan0());
    assert_eq!((r.status().link, r.status().radio, r.status().nbss), (Link::Down, "sim", 0));
    r.apply(&Command::Scan { iface: wlan0() });
    r.apply(&Command::Scan { iface: wlan0() });
    assert_eq!(r.status().scans, 2);
    assert_eq!(r.status().nbss, 5);
    assert!(r.status().results().iter().any(|b| b.ssid.as_bytes() == "sim\u{2603}".as_bytes()));
}

#[test]
fn sim_connect_rules() {
    let mut r = SimRadio::new(wlan0());
    assert_eq!(connect(&mut r, b"akuma-sim-open", None), (Link::Connected, Error::None));
    assert_eq!(r.status().security, Security::Open);
    assert_eq!(connect(&mut r, b"akuma-sim-wpa2", Some(SIM_WPA2_PSK)), (Link::Connected, Error::None));
    assert_eq!(r.status().chan, 6);
    assert_eq!(connect(&mut r, b"akuma-sim-wpa2", Some([0; 32])), (Link::Failed, Error::AuthFailed));
    assert_eq!(connect(&mut r, b"akuma-sim-wpa2", None), (Link::Failed, Error::AuthFailed));
    assert_eq!(connect(&mut r, b"akuma-sim-open", Some([1; 32])), (Link::Failed, Error::AuthFailed));
    assert_eq!(connect(&mut r, b"akuma-sim-far", Some([7; 32])), (Link::Connected, Error::None));
    assert_eq!(connect(&mut r, b"akuma-sim-sae", Some([7; 32])), (Link::Failed, Error::Unsupported));
    assert_eq!(connect(&mut r, b"nope", None), (Link::Failed, Error::NotFound));
    r.apply(&Command::Disconnect { iface: wlan0() });
    assert_eq!((r.status().link, r.status().ssid), (Link::Down, Ssid::EMPTY));
}

#[test]
fn sim_bssid_pin() {
    let mut r = SimRadio::new(wlan0());
    let ssid = Ssid::new(b"akuma-sim-open").unwrap();
    r.apply(&Command::Connect { iface: wlan0(), ssid, psk: None, bssid: Some([2, 0, 0, 0, 0, 9]) });
    assert_eq!(r.status().error, Error::NotFound, "wrong BSSID");
    r.apply(&Command::Connect { iface: wlan0(), ssid, psk: None, bssid: Some([2, 0, 0, 0, 0, 1]) });
    assert_eq!(r.status().link, Link::Connected);
}
