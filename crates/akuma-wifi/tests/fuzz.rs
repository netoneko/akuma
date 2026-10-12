//! Fuzz the `/dev/wifi0` protocol: the command parser with what any root
//! process can `write`, the status parser with what a future kernel might
//! print, and the writer/parser pair for every representable status.
//!
//! The command parser runs in the kernel; a panic there is a local kernel
//! abort from a one-line `echo`. Deterministic; no dependencies.

use akuma_wifi::cmd::{self, Command};
use akuma_wifi::sim::SimRadio;
use akuma_wifi::status::{Bss, Error, Link, Status};
use akuma_wifi::{IfName, MAX_BSS, Security, Ssid, hex};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn byte(&mut self) -> u8 {
        self.next() as u8
    }
    fn pick<'a>(&mut self, items: &[&'a [u8]]) -> &'a [u8] {
        items[self.below(items.len())]
    }
}

fn wlan0() -> IfName {
    IfName::new(b"wlan0").unwrap()
}

fn random_ssid(rng: &mut Rng) -> Ssid {
    let n = rng.below(33);
    let b: Vec<u8> = (0..n).map(|_| rng.byte()).collect();
    Ssid::new(&b).unwrap()
}

/// A network name: the grammar carries no empty SSID (see
/// `empty_ssid_has_no_spelling`).
fn random_name(rng: &mut Rng) -> Ssid {
    let n = rng.below(32) + 1;
    let b: Vec<u8> = (0..n).map(|_| rng.byte()).collect();
    Ssid::new(&b).unwrap()
}

fn random_status(rng: &mut Rng) -> Status {
    let mut s = Status::no_radio(wlan0());
    s.radio = ["none", "sim", "rtw89"][rng.below(3)];
    s.link = [Link::NoRadio, Link::Down, Link::Scanning, Link::Associating, Link::Connected, Link::Failed][rng.below(6)];
    s.ssid = random_ssid(rng);
    s.bssid = core::array::from_fn(|_| rng.byte());
    s.chan = rng.next() as u16;
    s.signal = rng.byte().cast_signed();
    s.security = [Security::Open, Security::Wpa2Psk, Security::Other][rng.below(3)];
    s.error = [Error::None, Error::NotFound, Error::AuthFailed, Error::Unsupported, Error::Timeout][rng.below(5)];
    s.scans = rng.next() as u32;
    s.nbss = rng.below(MAX_BSS + 1);
    for i in 0..s.nbss {
        s.bss[i] = Bss {
            ssid: random_ssid(rng),
            bssid: core::array::from_fn(|_| rng.byte()),
            chan: rng.next() as u16,
            signal: rng.byte().cast_signed(),
            security: [Security::Open, Security::Wpa2Psk, Security::Other][rng.below(3)],
        };
    }
    s
}

#[test]
fn every_status_round_trips_through_4k() {
    let mut rng = Rng(0x0123_4567_89ab_cdef);
    let mut buf = [0u8; 4096];
    for _ in 0..20_000 {
        let s = random_status(&mut rng);
        let n = s.write(&mut buf).expect("4 KiB holds every status");
        let back = Status::parse(&buf[..n]).expect("what the kernel writes, the tool reads");
        assert_eq!(back, s);
        // A truncated read (the tool reads in 1 KiB pieces and may be cut
        // short by a dying process) parses or does not, but never panics.
        let cut = rng.below(n + 1);
        let _ = Status::parse(&buf[..cut]);
        // Too small an output buffer is refused, never overrun.
        let short = rng.below(n);
        assert!(s.write(&mut buf[..short]).is_none());
    }
}

#[test]
fn status_parser_survives_random_text() {
    let mut rng = Rng(0xfeed_face_dead_beef);
    let words: [&[u8]; 20] = [
        b"iface=", b"radio=", b"state=", b"ssid=", b"bssid=", b"chan=", b"signal=", b"security=", b"error=", b"scans=",
        b"bss ", b"wlan0", b"connected", b"wpa2-psk", b"-55", b"02:00:00:00:00:01", b"6162", b"\n", b" ", b"=",
    ];
    for _ in 0..100_000 {
        let mut t = Vec::new();
        for _ in 0..rng.below(40) {
            match rng.below(4) {
                0 => t.push(rng.byte()),
                1 => t.extend_from_slice(&(0..rng.below(6)).map(|_| b"0123456789abcdef"[rng.below(16)]).collect::<Vec<_>>()),
                _ => t.extend_from_slice(rng.pick(&words)),
            }
        }
        if let Some(s) = Status::parse(&t) {
            assert!(s.nbss <= MAX_BSS);
            assert!(s.results().len() <= MAX_BSS);
        }
    }
}

#[test]
fn command_parser_survives_random_lines_and_round_trips() {
    let mut rng = Rng(0xabad_1dea_0000_0001);
    let words: [&[u8]; 14] = [
        b"scan", b"connect", b"disconnect", b"wlan0", b"wlan1", b"-", b" ", b"\t", b"\n", b"\r\n", b"02:00:00:00:00:02", b"6162", b"=", b"\0",
    ];
    let key64: Vec<u8> = b"00".repeat(32);
    let mut line = [0u8; 512];
    for _ in 0..200_000 {
        let mut t = Vec::new();
        match rng.below(3) {
            0 => {
                for _ in 0..rng.below(12) {
                    match rng.below(5) {
                        0 => t.push(rng.byte()),
                        1 => t.extend_from_slice(&key64[..rng.below(65)]),
                        2 => t.extend_from_slice(&(0..rng.below(70)).map(|_| b"0123456789abcdefABCDEF"[rng.below(22)]).collect::<Vec<_>>()),
                        _ => t.extend_from_slice(rng.pick(&words)),
                    }
                }
            }
            1 => t = (0..rng.below(600)).map(|_| rng.byte()).collect(),
            _ => {
                // A valid command, mutated.
                let c = match rng.below(3) {
                    0 => Command::Scan { iface: wlan0() },
                    1 => Command::Disconnect { iface: wlan0() },
                    _ => Command::Connect {
                        iface: wlan0(),
                        ssid: random_name(&mut rng),
                        psk: (rng.below(2) == 0).then(|| core::array::from_fn(|_| rng.byte())),
                        bssid: (rng.below(2) == 0).then(|| core::array::from_fn(|_| rng.byte())),
                    },
                };
                let n = cmd::write(&c, &mut line).unwrap();
                assert_eq!(cmd::parse(&line[..n]), Ok(c));
                t = line[..n].to_vec();
                for _ in 0..rng.below(3) {
                    let i = rng.below(t.len());
                    t[i] = rng.byte();
                }
                t.truncate(rng.below(t.len() + 1));
            }
        }
        if let Ok(c) = cmd::parse(&t) {
            // What parses, writes back and parses again to the same thing.
            let n = cmd::write(&c, &mut line).unwrap();
            assert_eq!(cmd::parse(&line[..n]), Ok(c));
        }
    }
}

#[test]
fn hex_decoders_survive_anything() {
    let mut rng = Rng(5);
    let mut out = [0u8; 64];
    for _ in 0..100_000 {
        let n = rng.below(80);
        let t: Vec<u8> = (0..n).map(|_| if rng.below(4) == 0 { rng.byte() } else { b"0123456789abcdefABCDEF:"[rng.below(23)] }).collect();
        if let Some(k) = hex::decode(&t, &mut out) {
            assert_eq!(k, t.len() / 2);
        }
        let _ = hex::decode(&t, &mut out[..rng.below(64)]);
        if let Some(m) = hex::decode_mac(&t) {
            let mut back = [0u8; 17];
            hex::encode_mac(&m, &mut back).unwrap();
            assert!(back.eq_ignore_ascii_case(&t));
        }
    }
}

/// The simulated radio takes any sequence of commands without complaint and
/// its status always fits a snapshot.
#[test]
fn simulated_radio_survives_any_command_sequence() {
    let mut rng = Rng(77);
    let mut radio = SimRadio::new(wlan0());
    let mut buf = [0u8; 4096];
    for _ in 0..20_000 {
        let c = match rng.below(4) {
            0 => Command::Scan { iface: wlan0() },
            1 => Command::Disconnect { iface: wlan0() },
            _ => Command::Connect {
                iface: wlan0(),
                ssid: if rng.below(2) == 0 { Ssid::new(b"akuma-sim-wpa2").unwrap() } else { random_ssid(&mut rng) },
                psk: (rng.below(2) == 0).then(|| core::array::from_fn(|_| rng.byte())),
                bssid: (rng.below(3) == 0).then(|| core::array::from_fn(|_| rng.byte())),
            },
        };
        radio.apply(&c);
        let n = radio.status().write(&mut buf).unwrap();
        assert_eq!(Status::parse(&buf[..n]).unwrap(), *radio.status());
    }
}

/// `connect` with an empty SSID: the writer refuses it rather than emit a
/// line the parser cannot read back (two blanks, which it skips).
#[test]
fn empty_ssid_has_no_spelling() {
    let c = Command::Connect { iface: wlan0(), ssid: Ssid::EMPTY, psk: None, bssid: None };
    assert_eq!(cmd::write(&c, &mut [0u8; 256]), None);
    // The parser skips the blank word, so `-` is read as the SSID.
    assert_eq!(cmd::parse(b"connect wlan0  -\n"), Err(cmd::CmdError::BadSsid));
    assert_eq!(cmd::parse(b"connect wlan0  \n"), Err(cmd::CmdError::MissingArgument));
}
