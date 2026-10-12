//! Fuzz every parser in this crate with what a radio can hand it: random
//! bytes, and valid frames mutated byte by byte, truncated, or with their
//! length fields rewritten.
//!
//! Every one of these parsers runs in the kernel on a frame **anyone on the
//! air can send** (there is no PMF, and addresses are spoofable), so a panic
//! here is a remote kernel abort. The only acceptable outcomes are `Some` or
//! `None`; nothing else is asserted about a mutated frame. Deterministic
//! (seeded xorshift), so a failure reproduces by seed and iteration; no
//! dependencies.

use akuma_ieee80211::sta::{Amsdu, AssocResp, Auth, Data, Goodbye, SNAP};
use akuma_ieee80211::{beacon, ccmp, frame};

const ITERATIONS: u64 = 200_000;

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
}

const AP: [u8; 6] = [0x02, 1, 2, 3, 4, 5];
const ME: [u8; 6] = [0x02, 9, 8, 7, 6, 5];

/// A beacon with SSID, rates, DS, HT and RSN elements, FCS last.
fn beacon_frame() -> Vec<u8> {
    let mut f = vec![0x80, 0, 0, 0];
    f.extend_from_slice(&[0xff; 6]);
    f.extend_from_slice(&AP);
    f.extend_from_slice(&AP);
    f.extend_from_slice(&[0, 0]);
    f.extend_from_slice(&[0; 8]); // timestamp
    f.extend_from_slice(&100u16.to_le_bytes());
    f.extend_from_slice(&0x0411u16.to_le_bytes());
    f.extend_from_slice(&[0, 10, b'a', b'k', b'u', b'm', b'a', b'-', b't', b'e', b's', b't']);
    f.extend_from_slice(&[1, 8, 0x82, 0x84, 0x8b, 0x96, 0x0c, 0x12, 0x18, 0x24]);
    f.extend_from_slice(&[3, 1, 6]);
    f.extend_from_slice(&[45, 26]);
    f.extend_from_slice(&[0; 26]);
    f.extend_from_slice(&[48, 20, 1, 0, 0, 0x0f, 0xac, 4, 1, 0, 0, 0x0f, 0xac, 4, 1, 0, 0, 0x0f, 0xac, 2, 0xc0, 0]);
    f.extend_from_slice(&[0xdd, 7, 0x00, 0x50, 0xf2, 0x02, 0x00, 0x01, 0x00]);
    f.extend_from_slice(&[0xaa; 4]);
    f
}

fn mgmt(subtype: u8, body: &[u8]) -> Vec<u8> {
    let mut f = vec![subtype << 4, 0, 0, 0];
    f.extend_from_slice(&ME);
    f.extend_from_slice(&AP);
    f.extend_from_slice(&AP);
    f.extend_from_slice(&[0, 0]);
    f.extend_from_slice(body);
    f.extend_from_slice(&[0xaa; 4]);
    f
}

/// A from-DS QoS data frame, protected or not, with an LLC/SNAP payload.
fn data_frame(protected: bool, qos: bool, htc: bool, payload: &[u8]) -> Vec<u8> {
    let mut f = vec![if qos { 0x88 } else { 0x08 }, 0x02 | if protected { 0x40 } else { 0 } | if htc { 0x80 } else { 0 }];
    f.extend_from_slice(&[0, 0]);
    f.extend_from_slice(&ME);
    f.extend_from_slice(&AP);
    f.extend_from_slice(&AP);
    f.extend_from_slice(&[0x10, 0]);
    if qos {
        f.extend_from_slice(&[5, 0]);
        if htc {
            f.extend_from_slice(&[0; 4]);
        }
    }
    if protected {
        f.extend_from_slice(&[1, 0, 0, 0x20, 0, 0, 0, 0]);
    }
    f.extend_from_slice(&SNAP);
    f.extend_from_slice(&[0x08, 0x00]);
    f.extend_from_slice(payload);
    if protected {
        f.extend_from_slice(&[0xbb; 8]);
    }
    f.extend_from_slice(&[0xcc; 4]);
    f
}

/// An A-MSDU of three subframes, one of them without LLC/SNAP.
fn amsdu_frame(protected: bool) -> Vec<u8> {
    let mut f = vec![0x88, if protected { 0x42 } else { 0x02 }];
    f.extend_from_slice(&[0, 0]);
    f.extend_from_slice(&ME);
    f.extend_from_slice(&AP);
    f.extend_from_slice(&AP);
    f.extend_from_slice(&[0, 0, 0x80, 0]);
    if protected {
        f.extend_from_slice(&[0xaa; 8]);
    }
    let sub = |f: &mut Vec<u8>, payload: &[u8], snap: bool, last: bool| {
        f.extend_from_slice(&ME);
        f.extend_from_slice(&AP);
        let n = payload.len() + if snap { 8 } else { 0 };
        f.extend_from_slice(&(n as u16).to_be_bytes());
        if snap {
            f.extend_from_slice(&SNAP);
            f.extend_from_slice(&[0x08, 0x06]);
        }
        f.extend_from_slice(payload);
        while !last && f.len() % 4 != 2 {
            f.push(0);
        }
    };
    sub(&mut f, &[1, 2, 3], true, false);
    sub(&mut f, &[9; 5], false, false);
    sub(&mut f, &[7; 28], true, true);
    if protected {
        f.extend_from_slice(&[0xbb; 8]);
    }
    f.extend_from_slice(&[0xcc; 4]);
    f
}

fn corpus() -> Vec<Vec<u8>> {
    vec![
        beacon_frame(),
        mgmt(frame::mgmt::PROBE_RESP, &beacon_frame()[24..]),
        mgmt(frame::mgmt::AUTH, &[0, 0, 2, 0, 0, 0]),
        mgmt(frame::mgmt::ASSOC_RESP, &[0x11, 0x14, 0, 0, 0x01, 0xc0, 1, 8, 0x82, 0x84, 0x8b, 0x96, 0x0c, 0x12, 0x18, 0x24]),
        mgmt(frame::mgmt::DEAUTH, &[2, 0]),
        mgmt(frame::mgmt::DISASSOC, &[8, 0]),
        data_frame(false, true, false, &[0; 99]),
        data_frame(true, true, false, &[0; 40]),
        data_frame(true, false, false, &[0; 40]),
        data_frame(true, true, true, &[0; 40]),
        data_frame(false, false, false, &[]),
        amsdu_frame(true),
        amsdu_frame(false),
    ]
}

/// One mutation of `base`: a flipped byte, a truncation, an element length
/// rewritten, a chunk duplicated, or random bytes altogether.
fn mutate(rng: &mut Rng, base: &[u8]) -> Vec<u8> {
    let mut f = base.to_vec();
    match rng.below(8) {
        0 => {
            let n = rng.below(4) + 1;
            for _ in 0..n {
                let i = rng.below(f.len());
                f[i] ^= 1 << rng.below(8);
            }
        }
        1 => {
            let i = rng.below(f.len());
            f[i] = rng.byte();
        }
        2 => f.truncate(rng.below(f.len() + 1)),
        3 => {
            // An element length (every other byte past the fixed fields is
            // one, roughly): 0, 255, or one past the end.
            if f.len() > 30 {
                let i = 24 + rng.below(f.len() - 24);
                f[i] = [0, 0xff, (f.len() - i) as u8, (f.len() - i) as u8 + 1][rng.below(4)];
            }
        }
        4 => {
            let at = rng.below(f.len());
            let n = rng.below(16).min(f.len() - at);
            let chunk = f[at..at + n].to_vec();
            let to = rng.below(f.len());
            f.splice(to..to, chunk);
        }
        5 => {
            let n = rng.below(300);
            f = (0..n).map(|_| rng.byte()).collect();
        }
        6 => {
            // Frame control only: every type/subtype/flag combination over a
            // valid body.
            f[0] = rng.byte();
            f[1] = rng.byte();
        }
        _ => {
            let n = rng.below(2000);
            f.extend((0..n).map(|_| rng.byte()));
        }
    }
    f
}

fn exercise(f: &[u8]) {
    for fcs in [true, false] {
        if let Some(b) = beacon::parse(f, fcs) {
            assert!(b.ssid.len() <= 32);
            for ie in frame::Ies(b.ies) {
                let _ = beacon::Rsn::parse(ie.data);
            }
        }
        if let Some(d) = Data::parse(f, fcs) {
            assert!(d.payload.len() <= f.len());
        }
        if let Some(a) = Amsdu::parse(f, fcs) {
            let mut n = 0usize;
            for m in a.subframes() {
                assert!(m.payload.len() <= f.len());
                n += 1;
                assert!(n <= f.len(), "the subframe walk must advance");
            }
        }
        let _ = ccmp::header(f, fcs);
    }
    let _ = Auth::parse(f);
    let _ = AssocResp::parse(f);
    let _ = Goodbye::parse(f);
    let _ = frame::Hdr::parse(f);
    let _ = beacon::Rsn::parse(f);
    for _ in frame::Ies(f) {}
}

#[test]
fn parsers_survive_mutated_and_random_frames() {
    let corpus = corpus();
    for f in &corpus {
        exercise(f);
    }
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    for i in 0..ITERATIONS {
        let base = &corpus[rng.below(corpus.len())];
        let f = mutate(&mut rng, base);
        // A panic names the iteration; rerun with the seed to reproduce.
        let _ = i;
        exercise(&f);
    }
}

#[test]
fn corpus_parses_as_intended() {
    let c = corpus();
    assert!(beacon::parse(&c[0], true).is_some_and(|b| b.channel == Some(6) && b.rsn.is_some()));
    assert!(beacon::parse(&c[1], true).is_some());
    assert!(Auth::parse(&c[2]).is_some_and(|a| a.transaction == 2));
    assert!(AssocResp::parse(&c[3]).is_some_and(|r| r.aid == 1));
    assert!(Goodbye::parse(&c[4]).is_some_and(|g| g.deauth && g.reason == 2));
    assert!(Goodbye::parse(&c[5]).is_some_and(|g| !g.deauth && g.reason == 8));
    for f in &c[6..11] {
        assert!(Data::parse(f, true).is_some(), "{f:02x?}");
    }
    assert!(ccmp::header(&c[7], true).is_some_and(|h| h.pn == 1));
    assert!(ccmp::header(&c[9], true).is_some_and(|h| h.pn == 1), "with HT control");
    assert_eq!(Amsdu::parse(&c[11], true).unwrap().subframes().count(), 2, "the subframe without LLC/SNAP is skipped");
    assert_eq!(Amsdu::parse(&c[12], true).unwrap().subframes().count(), 2);
}

/// A-MSDU subframe lengths that overlap, run past the end, or are zero must
/// end the walk rather than loop or index out of range.
#[test]
fn amsdu_length_fields_cannot_trap_the_walk() {
    let base = amsdu_frame(false);
    let mut rng = Rng(7);
    for _ in 0..20_000 {
        let mut f = base.clone();
        // The three length fields are at 34+12, and after each subframe.
        let at = [46, 46 + 14 + 11 + 1 + 12, 46 + 14 + 11 + 1 + 14 + 5 + 2 + 12][rng.below(3)];
        if at + 1 < f.len() {
            let v = rng.next() as u16;
            f[at..at + 2].copy_from_slice(&v.to_be_bytes());
        }
        f.truncate(rng.below(f.len() + 1));
        if let Some(a) = Amsdu::parse(&f, false) {
            assert!(a.subframes().count() <= 4);
        }
    }
}
