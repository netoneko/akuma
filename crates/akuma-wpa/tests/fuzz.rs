//! Fuzz the EAPOL-Key parser, the supplicant and the key unwrap with what an
//! access point — or anyone spoofing one, since message 1 carries no MIC and
//! addresses are not authenticated — can send: random bytes, valid messages
//! with bytes flipped, lengths rewritten, truncated, or extended.
//!
//! These run in the kernel on frames from the air, so a panic is a remote
//! kernel abort. Nothing is asserted about a mutated frame beyond "accepted
//! or dropped", and that a supplicant that was handed garbage still finishes
//! a clean handshake afterwards. Deterministic (seeded xorshift); no
//! dependencies.

use akuma_wpa::aes;
use akuma_wpa::eapol::{self, Action, Drop, Key, Ptk, Supplicant, info};
use akuma_wpa::sha1::pbkdf2;

const ITERATIONS: u64 = 100_000;

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

const AA: [u8; 6] = [0x02, 0x11, 0x22, 0x33, 0x44, 0x55];
const SPA: [u8; 6] = [0x02, 0x66, 0x77, 0x88, 0x99, 0xaa];
const RSN: [u8; 22] = [48, 20, 1, 0, 0x00, 0x0f, 0xac, 4, 1, 0, 0x00, 0x0f, 0xac, 4, 1, 0, 0x00, 0x0f, 0xac, 2, 0, 0];

/// PBKDF2-SHA1("password", "IEEE", 4096, 32): IEEE 802.11i Annex H.4.1,
/// checked once below rather than derived per iteration (4096 rounds each).
const PMK: [u8; 32] = [
    0xf4, 0x2c, 0x6f, 0xc5, 0x2d, 0xf0, 0xeb, 0xef, 0x9e, 0xbb, 0x4b, 0x90, 0xb3, 0x8a, 0x5f, 0x90, 0x2e, 0x83, 0xfe, 0x1b, 0x13,
    0x5a, 0x70, 0xe2, 0x3a, 0xed, 0x76, 0x2e, 0x97, 0x10, 0xa1, 0x2e,
];

fn pmk() -> [u8; 32] {
    PMK
}

#[test]
fn the_constant_pmk_is_the_annex_vector() {
    let mut p = [0u8; 32];
    pbkdf2(b"password", b"IEEE", 4096, &mut p);
    assert_eq!(p, PMK);
}

/// The access point's half, enough to produce every message kind.
struct Ap {
    anonce: [u8; 32],
    replay: u64,
    ptk: Option<Ptk>,
    gtk: [u8; 16],
}

impl Ap {
    fn frame(&self, key_info: u16, data: &[u8], with_mic: bool) -> Vec<u8> {
        let len = eapol::HDR_LEN + data.len();
        let mut out = vec![0u8; len];
        out[0] = 2;
        out[1] = 3;
        out[2..4].copy_from_slice(&((len - 4) as u16).to_be_bytes());
        out[4] = 2;
        out[5..7].copy_from_slice(&key_info.to_be_bytes());
        out[7..9].copy_from_slice(&16u16.to_be_bytes());
        out[9..17].copy_from_slice(&self.replay.to_be_bytes());
        out[17..49].copy_from_slice(&self.anonce);
        out[65] = 0x2a;
        out[97..99].copy_from_slice(&(data.len() as u16).to_be_bytes());
        out[eapol::HDR_LEN..].copy_from_slice(data);
        if with_mic {
            let m = eapol::mic(&self.ptk.unwrap().kck, &out);
            out[81..97].copy_from_slice(&m);
        }
        out
    }

    fn msg1(&mut self) -> Vec<u8> {
        self.replay += 1;
        self.frame(info::VERSION_2 | info::PAIRWISE | info::ACK, &[], false)
    }

    fn key_data(&self) -> Vec<u8> {
        let mut plain = [0u8; 48];
        plain[..22].copy_from_slice(&RSN);
        plain[22..28].copy_from_slice(&[0xdd, 22, 0x00, 0x0f, 0xac, 0x01]);
        plain[28] = 1;
        plain[30..46].copy_from_slice(&self.gtk);
        plain[46] = 0xdd;
        let mut out = [0u8; 64];
        let n = aes::wrap(&self.ptk.unwrap().kek, &plain, &mut out).unwrap();
        out[..n].to_vec()
    }

    fn msg3(&mut self) -> Vec<u8> {
        self.replay += 1;
        let kd = self.key_data();
        let ki = info::VERSION_2 | info::PAIRWISE | info::INSTALL | info::ACK | info::MIC | info::SECURE | info::ENCRYPTED;
        self.frame(ki, &kd, true)
    }

    fn group1(&mut self) -> Vec<u8> {
        self.replay += 1;
        let kd = self.key_data();
        self.frame(info::VERSION_2 | info::ACK | info::MIC | info::SECURE | info::ENCRYPTED, &kd, true)
    }

    /// Read SNonce from message 2 and derive the PTK.
    fn take_msg2(&mut self, m2: &[u8]) {
        let k = Key::parse(m2).unwrap();
        let mut sn = [0u8; 32];
        sn.copy_from_slice(k.nonce);
        self.ptk = Some(Ptk::derive(&pmk(), &AA, &SPA, &self.anonce, &sn));
    }
}

fn fresh() -> (Supplicant, Ap) {
    (
        Supplicant::new(&pmk(), AA, SPA, [0x5a; 32], &RSN),
        Ap { anonce: [0xa5; 32], replay: 0, ptk: None, gtk: [0x77; 16] },
    )
}

/// Run a whole clean handshake; the supplicant must hand out the AP's keys.
fn complete(sta: &mut Supplicant, ap: &mut Ap) {
    let mut out = [0u8; eapol::HDR_LEN + eapol::MAX_KEY_DATA];
    let m1 = ap.msg1();
    let Ok(Action::Send(n)) = sta.handle(&m1, &mut out) else { panic!("message 1 refused") };
    ap.take_msg2(&out[..n]);
    let m3 = ap.msg3();
    match sta.handle(&m3, &mut out) {
        Ok(Action::Complete { tk, gtk, .. }) => {
            assert_eq!(tk, ap.ptk.unwrap().tk);
            assert_eq!(gtk.key, ap.gtk);
        }
        other => panic!("message 3: {other:?}"),
    }
}

fn mutate(rng: &mut Rng, base: &[u8]) -> Vec<u8> {
    let mut f = base.to_vec();
    match rng.below(7) {
        0 => {
            for _ in 0..=rng.below(4) {
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
            // The 802.1X body length, or the key data length.
            let at = [2, 97][rng.below(2)];
            let v = [0u16, 1, 94, 95, 96, 255, 256, 0xffff, rng.next() as u16][rng.below(9)];
            f[at..at + 2].copy_from_slice(&v.to_be_bytes());
        }
        4 => {
            // Key information: every bit set.
            f[5] = rng.byte();
            f[6] = rng.byte();
        }
        5 => f = (0..rng.below(400)).map(|_| rng.byte()).collect(),
        _ => f.extend((0..rng.below(600)).map(|_| rng.byte())),
    }
    f
}

#[test]
fn key_parse_and_supplicant_survive_mutated_messages() {
    let (mut sta, mut ap) = fresh();
    complete(&mut sta, &mut ap);
    let corpus: Vec<Vec<u8>> = {
        let (mut s, mut a) = fresh();
        let mut out = [0u8; 512];
        let m1 = a.msg1();
        let Ok(Action::Send(n)) = s.handle(&m1, &mut out) else { panic!() };
        a.take_msg2(&out[..n]);
        let m2 = out[..n].to_vec();
        let m3 = a.msg3();
        let g1 = a.group1();
        vec![m1, m2, m3, g1]
    };
    let mut rng = Rng(0x2545_f491_4f6c_dd1d);
    let mut out = [0u8; eapol::HDR_LEN + eapol::MAX_KEY_DATA];
    let mut accepted = 0u32;
    for _ in 0..ITERATIONS {
        let which_msg = rng.below(corpus.len());
        let f = mutate(&mut rng, &corpus[which_msg]);
        if let Some(k) = Key::parse(&f) {
            assert!(k.frame.len() <= f.len());
            assert!(k.data.len() <= eapol::MAX_KEY_DATA + 0xffff);
            let _ = eapol::find_gtk(k.data);
        }
        // Supplicants in every state: idle, after message 2, done.
        let which = rng.below(3);
        let mut s = match which {
            0 => fresh().0,
            1 => {
                let (mut s, mut a) = fresh();
                let m1 = a.msg1();
                let _ = s.handle(&m1, &mut out);
                s
            }
            _ => {
                let (mut s, mut a) = fresh();
                complete(&mut s, &mut a);
                s
            }
        };
        match s.handle(&f, &mut out) {
            Ok(_) => accepted += 1,
            Err(Drop::Buffer) => panic!("the reply buffer is sized for every message"),
            Err(_) => {}
        }
        // Too small a reply buffer is refused, never overrun.
        let mut tiny = [0u8; 50];
        let _ = s.handle(&f, &mut tiny);
    }
    // Some mutations (a flipped bit in an unused field of message 1) are
    // still valid messages; the point is only that the loop ran them all.
    assert!(accepted > 0);
}

/// A supplicant fed garbage between the real messages — spoofed message 1s
/// with new nonces among them — still finishes the handshake with the real
/// access point and hands out the right keys.
#[test]
fn garbage_between_the_real_messages_does_not_derail_the_handshake() {
    let mut rng = Rng(0xdead_beef_cafe_f00d);
    let mut out = [0u8; eapol::HDR_LEN + eapol::MAX_KEY_DATA];
    for round in 0..300 {
        let (mut sta, mut ap) = fresh();
        let m1 = ap.msg1();
        let noise = |rng: &mut Rng, base: &[u8]| -> Vec<u8> {
            let mut f = mutate(rng, base);
            // Keep the replay counter out of the real AP's future: a
            // mutated counter above the AP's would make the real message 3
            // look replayed, which is the AP's problem to retransmit past,
            // not this test's.
            if f.len() >= 17 {
                f[9..17].copy_from_slice(&0u64.to_be_bytes());
            }
            f
        };
        for _ in 0..rng.below(4) {
            let _ = sta.handle(&noise(&mut rng, &m1), &mut out);
        }
        let Ok(Action::Send(n)) = sta.handle(&m1, &mut out) else { panic!("round {round}: message 1 refused") };
        ap.take_msg2(&out[..n]);
        let m3 = ap.msg3();
        for _ in 0..rng.below(4) {
            let _ = sta.handle(&noise(&mut rng, &m3), &mut out);
        }
        match sta.handle(&m3, &mut out) {
            Ok(Action::Complete { tk, gtk, .. }) => {
                assert_eq!(tk, ap.ptk.unwrap().tk, "round {round}");
                assert_eq!(gtk.key, ap.gtk);
            }
            other => panic!("round {round}: message 3: {other:?}"),
        }
    }
}

#[test]
fn key_unwrap_survives_every_length_and_random_bytes() {
    let mut rng = Rng(42);
    let kek = [0x13; 16];
    let mut out = [0u8; 512];
    for _ in 0..20_000 {
        let n = rng.below(300);
        let w: Vec<u8> = (0..n).map(|_| rng.byte()).collect();
        let _ = aes::unwrap(&kek, &w, &mut out);
        let _ = aes::unwrap(&kek, &w, &mut out[..rng.below(300)]);
        let _ = aes::wrap(&kek, &w, &mut out);
        let _ = eapol::find_gtk(&w);
    }
}
