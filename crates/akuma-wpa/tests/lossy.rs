//! The 4-way handshake and the rekeys over a bad radio link, simulated.
//!
//! The home network this station was built on loses frames often enough
//! that three beacons miss a network, the access point resends message 3
//! two or three times, and the link drops every so often (ryzen boots
//! 16–47, `docs/reference/subsystems/wifi.md`). This file puts
//! [`Supplicant`] and the kernel station's EAPOL policy (`amd64/src/
//! rtw89_sta.rs`, `attempt` and `idle`) through the same conditions on the
//! host, many thousands of times, with the knobs turned past what the real
//! link does:
//!
//! - an **access point** that behaves as hostapd does: every transmission
//!   gets the next replay counter, each message is retried a few times
//!   (`AP_TRIES`) and then the station is deauthenticated, a new handshake
//!   gets a new ANonce, and protected EAPOL from the station is readable
//!   only once the access point has installed the pairwise key;
//! - a **channel** that loses, duplicates, delays (reorders) and corrupts
//!   frames in both directions, and injects spoofed message 1s (no MIC: any
//!   radio can send one) with nonces and counters of its own;
//! - a **station driver** that does exactly what the kernel does with each
//!   [`Action`]: clear EAPOL until the keys are in, protected after, keys
//!   installed after message 4 is released, a retransmitted message 3
//!   answered in the clear, a pairwise rekey's message 4 under the old key
//!   and then the new keys installed, a deauthentication followed by a
//!   rejoin with a fresh supplicant.
//!
//! What is asserted: the station ends up holding the access point's keys
//! (pairwise and group) after every handshake and every rekey, however many
//! retries that took; a lossless link needs exactly two frames each way; a
//! replayed message 3 is refused; nothing panics. The seeds are fixed, so a
//! failure names its scenario and seed. No dependencies.

use akuma_wpa::aes;
use akuma_wpa::eapol::{self, Action, Drop, Key, Ptk, Supplicant, info};

const AA: [u8; 6] = [0x02, 0x11, 0x22, 0x33, 0x44, 0x55];
const SPA: [u8; 6] = [0x02, 0x66, 0x77, 0x88, 0x99, 0xaa];
const RSN: [u8; 22] = [48, 20, 1, 0, 0x00, 0x0f, 0xac, 4, 1, 0, 0x00, 0x0f, 0xac, 4, 1, 0, 0x00, 0x0f, 0xac, 2, 0, 0];
/// PBKDF2-SHA1("password", "IEEE", 4096): the Annex H.4.1 vector.
const PMK: [u8; 32] = [
    0xf4, 0x2c, 0x6f, 0xc5, 0x2d, 0xf0, 0xeb, 0xef, 0x9e, 0xbb, 0x4b, 0x90, 0xb3, 0x8a, 0x5f, 0x90, 0x2e, 0x83, 0xfe, 0x1b, 0x13,
    0x5a, 0x70, 0xe2, 0x3a, 0xed, 0x76, 0x2e, 0x97, 0x10, 0xa1, 0x2e,
];

/// hostapd's `wpa_pairwise_update_count` / `wpa_group_update_count`: how
/// many times a message is sent before the station is given up on.
const AP_TRIES: u32 = 4;
/// Ticks between an access point's retransmissions (hostapd: 100 ms for
/// the first, 1 s after; one tick here is "a poll of the RX ring").
const AP_RETRY_TICKS: u32 = 10;
const OUT: usize = eapol::HDR_LEN + eapol::MAX_KEY_DATA;

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
    /// True with probability `p` (per mille).
    fn chance(&mut self, p: u32) -> bool {
        (self.next() % 1000) < u64::from(p)
    }
    fn nonce(&mut self) -> [u8; 32] {
        core::array::from_fn(|_| self.byte())
    }
}

// ---------------------------------------------------------------- the air

/// An EAPOL frame on the air, with what the sending side did to it: `tk`
/// is the pairwise key it was encrypted under, `None` for the clear.
#[derive(Clone, Debug)]
struct Frame {
    body: Vec<u8>,
    tk: Option<[u8; 16]>,
    /// Ticks until delivery (delay / reordering).
    due: u32,
}

/// The channel's knobs, per mille.
#[derive(Clone, Copy, Debug, Default)]
struct Knobs {
    loss: u32,
    dup: u32,
    delay: u32,
    corrupt: u32,
    /// Spoofed message 1s injected towards the station.
    spoof: u32,
}

/// One direction of the air.
#[derive(Default)]
struct Pipe {
    inflight: Vec<Frame>,
    sent: u32,
    lost: u32,
}

impl Pipe {
    fn send(&mut self, rng: &mut Rng, k: &Knobs, body: &[u8], tk: Option<[u8; 16]>) {
        self.sent += 1;
        let copies = 1 + usize::from(rng.chance(k.dup));
        for _ in 0..copies {
            if rng.chance(k.loss) {
                self.lost += 1;
                continue;
            }
            let mut body = body.to_vec();
            if rng.chance(k.corrupt) {
                let i = rng.below(body.len());
                body[i] ^= 1 << rng.below(8);
            }
            let due = if rng.chance(k.delay) { 1 + rng.below(6) as u32 } else { 0 };
            self.inflight.push(Frame { body, tk, due });
        }
    }

    /// What arrives this tick.
    fn tick(&mut self) -> Vec<Frame> {
        let mut now = Vec::new();
        let mut later = Vec::new();
        for mut f in self.inflight.drain(..) {
            if f.due == 0 {
                now.push(f);
            } else {
                f.due -= 1;
                later.push(f);
            }
        }
        self.inflight = later;
        now
    }
}

// ---------------------------------------------------------------- the AP

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ApState {
    /// Nothing in progress; `installed` holds the keys of the last handshake.
    Idle,
    /// Message 1 sent, message 2 awaited.
    WaitMsg2,
    /// Message 3 sent, message 4 awaited.
    WaitMsg4,
    /// Group message 1 sent, group message 2 awaited.
    WaitGroup2,
}

struct Ap {
    state: ApState,
    anonce: [u8; 32],
    replay: u64,
    /// The key of the handshake in progress (derived at message 2).
    ptk: Option<Ptk>,
    /// The pairwise key in the access point's hardware, if any.
    installed: Option<Ptk>,
    gtk: [u8; 16],
    gtk_idx: u8,
    /// Retransmission bookkeeping: the last frame sent, tries so far, ticks
    /// until the next try.
    last: Vec<u8>,
    tries: u32,
    timer: u32,
    deauths: u32,
    /// The station deauthenticated: its next frame is dropped and it must
    /// start over (the station learns this from the deauthentication frame,
    /// modelled as a flag it reads).
    kicked: bool,
    /// Handshakes completed (message 4 accepted).
    completed: u32,
    group_done: u32,
    rejected_protected: u32,
}

impl Ap {
    fn new(rng: &mut Rng) -> Self {
        Self {
            state: ApState::Idle,
            anonce: rng.nonce(),
            replay: 0,
            ptk: None,
            installed: None,
            gtk: core::array::from_fn(|_| rng.byte()),
            gtk_idx: 1,
            last: Vec::new(),
            tries: 0,
            timer: 0,
            deauths: 0,
            kicked: false,
            completed: 0,
            group_done: 0,
            rejected_protected: 0,
        }
    }

    fn frame(&self, key_info: u16, data: &[u8], mic_key: Option<&Ptk>) -> Vec<u8> {
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
        if let Some(p) = mic_key {
            let m = eapol::mic(&p.kck, &out);
            out[81..97].copy_from_slice(&m);
        }
        out
    }

    fn key_data(&self, ptk: &Ptk) -> Vec<u8> {
        let mut plain = [0u8; 48];
        plain[..22].copy_from_slice(&RSN);
        plain[22..28].copy_from_slice(&[0xdd, 22, 0x00, 0x0f, 0xac, 0x01]);
        plain[28] = self.gtk_idx;
        plain[30..46].copy_from_slice(&self.gtk);
        plain[46] = 0xdd;
        let mut out = [0u8; 64];
        let n = aes::wrap(&ptk.kek, &plain, &mut out).unwrap();
        out[..n].to_vec()
    }

    /// Send `body` now (in the clear: the access point measured never
    /// encrypts EAPOL) and arm the retransmit timer.
    fn transmit(&mut self, rng: &mut Rng, k: &Knobs, air: &mut Pipe, body: Vec<u8>) {
        air.send(rng, k, &body, None);
        self.last = body;
        self.timer = AP_RETRY_TICKS;
    }

    /// Start a 4-way handshake: the first, or a pairwise rekey.
    fn start_handshake(&mut self, rng: &mut Rng, k: &Knobs, air: &mut Pipe) {
        self.anonce = rng.nonce();
        self.ptk = None;
        self.replay += 1;
        self.tries = 1;
        self.state = ApState::WaitMsg2;
        let m1 = self.frame(info::VERSION_2 | info::PAIRWISE | info::ACK, &[], None);
        self.transmit(rng, k, air, m1);
    }

    /// Start a group rekey: a new GTK under the installed pairwise key.
    fn start_group_rekey(&mut self, rng: &mut Rng, k: &Knobs, air: &mut Pipe) {
        let Some(p) = self.installed else { return };
        self.gtk = core::array::from_fn(|_| rng.byte());
        self.gtk_idx = if self.gtk_idx == 1 { 2 } else { 1 };
        self.replay += 1;
        self.tries = 1;
        self.state = ApState::WaitGroup2;
        let kd = self.key_data(&p);
        let g1 = self.frame(info::VERSION_2 | info::ACK | info::MIC | info::SECURE | info::ENCRYPTED, &kd, Some(&p));
        self.transmit(rng, k, air, g1);
    }

    fn send_msg3(&mut self, rng: &mut Rng, k: &Knobs, air: &mut Pipe) {
        let p = self.ptk.unwrap();
        self.replay += 1;
        self.state = ApState::WaitMsg4;
        let kd = self.key_data(&p);
        let ki = info::VERSION_2 | info::PAIRWISE | info::INSTALL | info::ACK | info::MIC | info::SECURE | info::ENCRYPTED;
        let m3 = self.frame(ki, &kd, Some(&p));
        self.transmit(rng, k, air, m3);
    }

    /// Time passes: retransmit, or give up and deauthenticate.
    fn tick(&mut self, rng: &mut Rng, k: &Knobs, air: &mut Pipe) {
        if self.state == ApState::Idle {
            return;
        }
        if self.timer > 0 {
            self.timer -= 1;
            return;
        }
        if self.tries >= AP_TRIES {
            self.deauths += 1;
            self.kicked = true;
            self.state = ApState::Idle;
            self.installed = None;
            self.ptk = None;
            return;
        }
        self.tries += 1;
        // hostapd re-sends with the next replay counter, same contents.
        self.replay += 1;
        let mut body = core::mem::take(&mut self.last);
        body[9..17].copy_from_slice(&self.replay.to_be_bytes());
        let mic_key = match self.state {
            ApState::WaitMsg2 => None,
            ApState::WaitMsg4 => self.ptk,
            ApState::WaitGroup2 | ApState::Idle => self.installed,
        };
        if let Some(p) = mic_key {
            body[81..97].fill(0);
            let m = eapol::mic(&p.kck, &body);
            body[81..97].copy_from_slice(&m);
        }
        self.transmit(rng, k, air, body);
    }

    /// A frame from the station.
    fn receive(&mut self, rng: &mut Rng, k: &Knobs, air: &mut Pipe, f: &Frame) {
        // Protected EAPOL is readable only under the installed pairwise key.
        if let Some(tk) = f.tk
            && self.installed.map(|p| p.tk) != Some(tk)
        {
            self.rejected_protected += 1;
            return;
        }
        let Some(key) = Key::parse(&f.body) else { return };
        if key.replay != self.replay {
            return; // not an answer to the last message sent
        }
        match self.state {
            ApState::WaitMsg2 if key.info & info::PAIRWISE != 0 && key.info & info::MIC != 0 => {
                let mut sn = [0u8; 32];
                sn.copy_from_slice(key.nonce);
                let p = Ptk::derive(&PMK, &AA, &SPA, &self.anonce, &sn);
                if eapol::mic(&p.kck, key.frame) != key.mic || key.data != RSN {
                    return;
                }
                self.ptk = Some(p);
                self.tries = 1;
                self.send_msg3(rng, k, air);
            }
            ApState::WaitMsg4 if key.info & info::PAIRWISE != 0 && key.info & info::SECURE != 0 => {
                let p = self.ptk.unwrap();
                if eapol::mic(&p.kck, key.frame) != key.mic {
                    return;
                }
                self.installed = Some(p);
                self.completed += 1;
                self.state = ApState::Idle;
            }
            ApState::WaitGroup2 if key.info & info::PAIRWISE == 0 && key.info & info::MIC != 0 => {
                let p = self.installed.unwrap();
                if eapol::mic(&p.kck, key.frame) != key.mic {
                    return;
                }
                self.group_done += 1;
                self.state = ApState::Idle;
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------- the station

/// The kernel station's EAPOL half, as `attempt` and `idle` do it.
struct Sta {
    sup: Supplicant,
    /// The pairwise key in the card (`Peer::keyed` and `Vars::tk`).
    tk: Option<[u8; 16]>,
    gtk: Option<([u8; 16], u8)>,
    /// Counts, for the assertions.
    mic_failures: u32,
    replays: u32,
    joins: u32,
    rekeys: u32,
    group_rekeys: u32,
    retransmit_answers: u32,
}

impl Sta {
    fn new(rng: &mut Rng) -> Self {
        Self {
            sup: Supplicant::new(&PMK, AA, SPA, rng.nonce(), &RSN),
            tk: None,
            gtk: None,
            mic_failures: 0,
            replays: 0,
            joins: 0,
            rekeys: 0,
            group_rekeys: 0,
            retransmit_answers: 0,
        }
    }

    /// Deauthenticated: a fresh association, fresh supplicant, no keys.
    fn rejoin(&mut self, rng: &mut Rng) {
        self.sup = Supplicant::new(&PMK, AA, SPA, rng.nonce(), &RSN);
        self.tk = None;
        self.gtk = None;
        self.joins += 1;
    }

    /// `send_eapol`: in the clear until the pairwise key is in, protected after.
    fn send_eapol(&self, rng: &mut Rng, k: &Knobs, air: &mut Pipe, body: &[u8]) {
        air.send(rng, k, body, self.tk);
    }

    fn receive(&mut self, rng: &mut Rng, k: &Knobs, air: &mut Pipe, f: &Frame) {
        // The card decrypts under the installed key or not at all; a frame
        // under some other key is garbage to it. (The access point never
        // encrypts EAPOL in this model, like the one measured.)
        if f.tk.is_some() && f.tk != self.tk {
            return;
        }
        let mut out = [0u8; OUT];
        match self.sup.handle(&f.body, &mut out) {
            Ok(Action::Send(len)) => self.send_eapol(rng, k, air, &out[..len]),
            Ok(Action::Complete { len, tk, gtk, fresh: true }) => {
                // Message 4 under whatever key is in, then the new keys.
                self.send_eapol(rng, k, air, &out[..len]);
                if self.tk.is_some() {
                    self.rekeys += 1;
                }
                self.tk = Some(tk);
                self.gtk = Some((gtk.key, gtk.idx));
            }
            Ok(Action::Complete { len, fresh: false, .. }) => {
                // Message 3 again: message 4 again, in the clear.
                air.send(rng, k, &out[..len], None);
                self.retransmit_answers += 1;
            }
            Ok(Action::Rekey { len, gtk }) => {
                if self.tk.is_some() {
                    self.send_eapol(rng, k, air, &out[..len]);
                    self.gtk = Some((gtk.key, gtk.idx));
                    self.group_rekeys += 1;
                }
            }
            Err(Drop::Mic) => self.mic_failures += 1,
            Err(Drop::Replay) => self.replays += 1,
            Err(_) => {}
        }
    }
}

// ---------------------------------------------------------------- the world

struct World {
    rng: Rng,
    knobs: Knobs,
    ap: Ap,
    sta: Sta,
    to_sta: Pipe,
    to_ap: Pipe,
    tick: u32,
    /// A valid message 3 recorded off the air, for the replay test.
    recorded_msg3: Option<Vec<u8>>,
}

impl World {
    fn new(seed: u64, knobs: Knobs) -> Self {
        let mut rng = Rng(seed | 1);
        let ap = Ap::new(&mut rng);
        let sta = Sta::new(&mut rng);
        Self { rng, knobs, ap, sta, to_sta: Pipe::default(), to_ap: Pipe::default(), tick: 0, recorded_msg3: None }
    }

    fn step(&mut self) {
        self.tick += 1;
        let k = self.knobs;
        // Spoofed message 1s: a nonce and counter of the attacker's choice.
        if self.rng.chance(k.spoof) {
            let spoof = Ap { anonce: self.rng.nonce(), replay: self.rng.next() >> 40, ..Ap::new(&mut self.rng) };
            let m1 = spoof.frame(info::VERSION_2 | info::PAIRWISE | info::ACK, &[], None);
            self.to_sta.inflight.push(Frame { body: m1, tk: None, due: 0 });
        }
        for f in self.to_sta.tick() {
            if let Some(key) = Key::parse(&f.body)
                && key.info & (info::PAIRWISE | info::MIC | info::INSTALL) == (info::PAIRWISE | info::MIC | info::INSTALL)
                && self.recorded_msg3.is_none()
            {
                self.recorded_msg3 = Some(f.body.clone());
            }
            self.sta.receive(&mut self.rng, &k, &mut self.to_ap, &f);
        }
        for f in self.to_ap.tick() {
            self.ap.receive(&mut self.rng, &k, &mut self.to_sta, &f);
        }
        self.ap.tick(&mut self.rng, &k, &mut self.to_sta);
        if self.ap.kicked {
            self.ap.kicked = false;
            self.sta.rejoin(&mut self.rng);
            self.to_sta.inflight.clear();
            self.to_ap.inflight.clear();
            self.ap.start_handshake(&mut self.rng, &k, &mut self.to_sta);
        }
    }

    /// Both sides idle with the same keys.
    fn in_sync(&self) -> bool {
        self.ap.state == ApState::Idle
            && self.ap.installed.map(|p| p.tk) == self.sta.tk
            && self.sta.tk.is_some()
            && self.sta.gtk == Some((self.ap.gtk, self.ap.gtk_idx))
    }

    /// Run until in sync or `budget` ticks pass.
    fn settle(&mut self, budget: u32) -> bool {
        for _ in 0..budget {
            if self.in_sync() {
                return true;
            }
            self.step();
        }
        self.in_sync()
    }
}

fn scenario(name: &str, knobs: Knobs, seeds: u64, mut f: impl FnMut(&mut World, u64)) {
    for seed in 1..=seeds {
        let mut w = World::new(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15), knobs);
        f(&mut w, seed);
        let _ = name;
    }
}

#[test]
fn a_clean_link_joins_with_two_frames_each_way() {
    scenario("clean", Knobs::default(), 50, |w, seed| {
        w.ap.start_handshake(&mut w.rng, &w.knobs, &mut w.to_sta);
        assert!(w.settle(100), "seed {seed}");
        assert_eq!((w.to_sta.sent, w.to_ap.sent, w.ap.deauths), (2, 2, 0), "seed {seed}");
        assert!(w.sta.sup.done());
    });
}

/// The measured network, and worse: up to half of every frame lost, some
/// duplicated, some delayed past their retransmission, some corrupted. The
/// station must end up holding the access point's keys every time — after
/// retransmissions, or after a deauthentication and a rejoin.
#[test]
fn the_handshake_converges_under_loss_duplication_reordering_and_corruption() {
    for (loss, dup, delay, corrupt) in [(100, 0, 0, 0), (300, 100, 100, 50), (500, 200, 200, 100), (0, 500, 500, 0), (0, 0, 0, 300)] {
        let knobs = Knobs { loss, dup, delay, corrupt, spoof: 0 };
        let mut deauths = 0;
        let mut retransmits = 0;
        scenario("lossy", knobs, 400, |w, seed| {
            w.ap.start_handshake(&mut w.rng, &w.knobs, &mut w.to_sta);
            assert!(w.settle(20_000), "loss {loss}: seed {seed} never converged: ap {:?} sta keyed {}", w.ap.state, w.sta.tk.is_some());
            deauths += w.ap.deauths;
            retransmits += w.sta.retransmit_answers;
            if corrupt == 0 {
                assert_eq!(w.sta.mic_failures, 0, "a MIC failure on a frame the right key signed: seed {seed}");
            }
        });
        // Under real loss the retransmission paths actually ran.
        if loss > 0 {
            assert!(retransmits > 0, "loss {loss}: message 3 was never retransmitted in 400 runs");
        }
        if loss >= 300 {
            assert!(deauths > 0, "loss {loss}: hostapd's give-up path never ran in 400 runs");
        }
    }
}

/// Once joined, the access point rekeys the pairwise key and the group key
/// over and over on a bad link; the keys never go out of step for good.
#[test]
fn rekeys_converge_on_a_lossy_link() {
    for loss in [0, 150, 400] {
        let knobs = Knobs { loss, dup: 100, delay: 100, corrupt: 20, spoof: 0 };
        let mut rekeys = 0;
        let mut group = 0;
        scenario("rekey", knobs, 200, |w, seed| {
            w.ap.start_handshake(&mut w.rng, &w.knobs, &mut w.to_sta);
            assert!(w.settle(20_000), "loss {loss}: seed {seed}: the join");
            for round in 0..6 {
                if round % 2 == 0 {
                    w.ap.start_handshake(&mut w.rng, &w.knobs, &mut w.to_sta);
                } else {
                    w.ap.start_group_rekey(&mut w.rng, &w.knobs, &mut w.to_sta);
                }
                assert!(w.settle(20_000), "loss {loss}: seed {seed}: rekey round {round}: ap {:?}", w.ap.state);
            }
            rekeys += w.sta.rekeys;
            group += w.sta.group_rekeys;
            // The channel flips bits (`corrupt`), so a MIC failure or two is
            // the channel's doing; the FCS catches these on a real link.
            assert!(w.sta.mic_failures <= 4, "seed {seed}: {} MIC failures", w.sta.mic_failures);
        });
        assert!(rekeys > 0 && group > 0, "loss {loss}: rekeys {rekeys} group {group}");
    }
}

/// Anyone can send message 1. Spoofed ones, before and after the keys are
/// in, must neither stop the real handshake nor cost the station its keys.
#[test]
fn spoofed_message_1s_are_harmless() {
    // The same link without the spoofs, for what loss alone costs.
    let mut control = 0;
    scenario("spoof control", Knobs { loss: 100, dup: 50, delay: 100, corrupt: 0, spoof: 0 }, 300, |w, seed| {
        w.ap.start_handshake(&mut w.rng, &w.knobs, &mut w.to_sta);
        assert!(w.settle(20_000), "seed {seed}");
        for round in 0..4 {
            if round % 2 == 0 {
                w.ap.start_group_rekey(&mut w.rng, &w.knobs, &mut w.to_sta);
            } else {
                w.ap.start_handshake(&mut w.rng, &w.knobs, &mut w.to_sta);
            }
            assert!(w.settle(20_000), "seed {seed}: rekey round {round}");
        }
        control += w.ap.deauths;
    });
    let knobs = Knobs { loss: 100, dup: 50, delay: 100, corrupt: 0, spoof: 150 };
    let mut deauths = 0;
    scenario("spoof", knobs, 300, |w, seed| {
        w.ap.start_handshake(&mut w.rng, &w.knobs, &mut w.to_sta);
        assert!(w.settle(20_000), "seed {seed}: the join");
        deauths += w.ap.deauths;
        // A quiet stretch full of spoofs: the keys stay as they are.
        let before = (w.sta.tk, w.sta.gtk, w.ap.deauths);
        for _ in 0..500 {
            w.step();
        }
        assert_eq!((w.sta.tk, w.sta.gtk, w.ap.deauths), before, "seed {seed}: a spoof changed something");
        // Then real rekeys, with spoofs still arriving.
        for round in 0..4 {
            if round % 2 == 0 {
                w.ap.start_group_rekey(&mut w.rng, &w.knobs, &mut w.to_sta);
            } else {
                w.ap.start_handshake(&mut w.rng, &w.knobs, &mut w.to_sta);
            }
            assert!(w.settle(20_000), "seed {seed}: rekey round {round}");
        }
        assert_eq!(w.sta.mic_failures, 0, "seed {seed}");
    });
    // A spoof flood of 15 % per poll can still evict the real candidate
    // when four land inside one retransmission window; that costs the
    // occasional deauthentication and rejoin, never the keys. Before the
    // candidate ring and the replay-counter fix this was 241 and 114 of 300
    // runs; the control (same link, no spoofs) says what loss alone costs.
    eprintln!("spoofed message 1s: {deauths} deauthentications in 300 runs, {control} without spoofs");
    assert!(
        deauths <= control + 15,
        "spoofs cost {deauths} deauthentications in 300 runs against {control} without them"
    );
}

/// A message 3 recorded off the air and played back later is refused.
#[test]
fn a_replayed_message_3_is_refused() {
    scenario("replay", Knobs::default(), 50, |w, seed| {
        w.ap.start_handshake(&mut w.rng, &w.knobs, &mut w.to_sta);
        assert!(w.settle(100), "seed {seed}");
        let m3 = w.recorded_msg3.clone().expect("message 3 was heard");
        let (tk, gtk) = (w.sta.tk, w.sta.gtk);
        let mut out = [0u8; OUT];
        assert_eq!(w.sta.sup.handle(&m3, &mut out), Err(Drop::Replay), "seed {seed}");
        // After a group rekey the old message 3 is older still.
        w.ap.start_group_rekey(&mut w.rng, &w.knobs, &mut w.to_sta);
        assert!(w.settle(100));
        assert_eq!(w.sta.sup.handle(&m3, &mut out), Err(Drop::Replay));
        assert_eq!(w.sta.tk, tk);
        assert_ne!(w.sta.gtk, gtk);
    });
}

/// Everything at once, for a long time: the station is still keyed in step
/// with the access point at the end, and nothing panicked on the way.
#[test]
fn a_long_bad_day() {
    let knobs = Knobs { loss: 250, dup: 150, delay: 150, corrupt: 100, spoof: 50 };
    scenario("bad day", knobs, 40, |w, seed| {
        w.ap.start_handshake(&mut w.rng, &w.knobs, &mut w.to_sta);
        for round in 0..30 {
            assert!(w.settle(30_000), "seed {seed}: round {round}: ap {:?}", w.ap.state);
            match w.rng.below(3) {
                0 => w.ap.start_handshake(&mut w.rng, &w.knobs, &mut w.to_sta),
                1 => w.ap.start_group_rekey(&mut w.rng, &w.knobs, &mut w.to_sta),
                _ => {
                    // The access point forgets the station (a reboot).
                    w.ap.deauths += 1;
                    w.ap.kicked = true;
                    w.ap.state = ApState::Idle;
                    w.ap.installed = None;
                    w.ap.replay = 0;
                }
            }
        }
        assert!(w.settle(30_000), "seed {seed}: the end");
        assert!(w.sta.joins > 0 || w.sta.rekeys > 0);
    });
}
