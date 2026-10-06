//! The station: ryzen's RTL8852CE as `/dev/wifi0`'s backend (`rtw89wifi`),
//! wifi stages W3/W4 of `docs/archive/AKUMA_AMD64_ON_RYZEN_LAPTOP.md` § 5.
//!
//! `rtw89.rs` brings the card up to running firmware and hands it over as a
//! [`Card`]; from then on one daemon owns it. The daemon replays Linux's
//! recorded join (`akuma_rtw89::script::JOIN1..4`) at the points the recording
//! says, and sends and answers the frames in between itself:
//!
//! | step | what | replayed / sent |
//! |---|---|---|
//! | start | the post-firmware start and interface setup | `JOIN1` (this station's address filled in) |
//! | scan | every beacon on the channel for [`SCAN_MS`] | RX filter opened, then put back |
//! | prepare | coex, RF calibration, channel, the AP's address CAM entry | `JOIN2` |
//! | authenticate | open system, transaction 1 → 2 | management frame on CH8 |
//! | associate | request (Linux's own elements for this card) → response, AID | management frame on CH8 |
//! | joined | EDCA, CCTL, JOININFO, address CAM with the AID, RA, offload templates | `JOIN3` |
//! | 4-way handshake | message 1 → 2, message 3 → 4 | [`Supplicant`]; EAPOL frames on ACH3 |
//! | keys | pairwise and group key into the security CAM, beacon filter | `JOIN4` (keys and the group key's id filled in), after message 4 |
//!
//! `JOIN1` begins where the firmware download ends — in the recording Linux
//! powered the card up from nothing before it joined — so a boot runs
//! `bring_up` then `JOIN1`, not `script::UP` then `JOIN1` (both are the same
//! start; replaying it twice is not what Linux did). A join leaves the card
//! with a peer in it, so the next join first powers the card off and on again
//! ([`Card::restart`]) and replays `JOIN1` afresh.
//!
//! Joins WPA2-PSK (CCMP, PSK AKM, no PMF) only; anything else answers
//! `unsupported`. The data path (DHCP, `ExternalDevice`) is stage W5.
//!
//! # Privacy
//!
//! The log is read off the laptop's disk. It carries SSIDs only as FNV-1a
//! hashes and BSSIDs only as their OUI, never a key. `/dev/wifi0` reports the
//! real SSID and BSSID: that is its job, and it is `0600`.
//!
//! # Scheduling
//!
//! The daemon waits by parking (`sched::block_until_deadline`), never by
//! spinning, so the join does not take a core away from sshd on a `nosmp`
//! box. The replays themselves are bounded busy stretches (`JOIN1` is the
//! longest, ~0.2 s).

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use akuma_ieee80211::beacon;
use akuma_ieee80211::sta::{self, AssocResp, Auth, Data, Goodbye};
use akuma_rtw89::{rx, script, tx};
use akuma_wifi::cmd::Command;
use akuma_wifi::status::{Bss, Error as JoinError, Link, Status};
use akuma_wifi::{Bssid, IfName, MAX_BSS, PSK_LEN, Security, Ssid};
use akuma_wpa::eapol::{self, Action, Supplicant};
use spinning_top::Spinlock;

use crate::rtw89::{Card, TX_EAPOL, TX_MGMT, fnv1a};
use crate::serial;

/// This station's address. The card's own lives in its efuse, but only the
/// address CAM entries `JOIN1`..`JOIN4` write carry a station address, so the
/// driver picks one: locally administered, `"AKUMA"` in ASCII after the `02`.
const MAC: [u8; 6] = [0x02, 0x41, 0x4b, 0x55, 0x4d, 0x41];
/// The channel `JOIN1` leaves the card on (the recording joined on 1).
const CHANNEL: u16 = 1;
/// How long a scan listens: ~24 beacon intervals.
const SCAN_MS: u64 = 2500;
/// One authentication or association attempt's wait for the answer.
const MGMT_REPLY_MS: u64 = 400;
const MGMT_TRIES: u32 = 3;
/// The 4-way handshake, from the association response to message 4. Access
/// points send message 1 within milliseconds and retry each message about
/// once a second.
const HANDSHAKE_MS: u64 = 8000;
/// How often an idle daemon looks at the RX ring and its request.
const IDLE_MS: u64 = 50;
/// A join's waits poll the RX ring this often.
const POLL_MS: u64 = 2;

static CARD: Spinlock<Option<Card>> = Spinlock::new(None);
static STATUS: Spinlock<Option<Status>> = Spinlock::new(None);
static REQUEST: Spinlock<Option<Command>> = Spinlock::new(None);
static STOP: AtomicBool = AtomicBool::new(false);
static SLOT: AtomicUsize = AtomicUsize::new(usize::MAX);

fn say(what: &str) {
    serial::puts("[rtw] ");
    serial::puts(what);
    serial::puts("\n");
}

fn say_dec(what: &str, v: u64) {
    serial::puts("[rtw] ");
    serial::puts(what);
    serial::puts(" ");
    serial::put_dec(v);
    serial::puts("\n");
}

/// `aa:bb:cc:xx:xx:xx`: enough to tell access points apart in a log, not to
/// name one.
fn put_oui(b: &Bssid) {
    for (i, x) in b[..3].iter().enumerate() {
        if i > 0 {
            serial::puts(":");
        }
        serial::put_hexn(u64::from(*x), 2);
    }
    serial::puts(":xx:xx:xx");
}

/// Take the card `rtw89::init` kept up, and become `/dev/wifi0`'s backend.
/// The daemon ([`spawn`]) starts once the scheduler runs.
pub fn adopt(card: Card) {
    *CARD.lock() = Some(card);
    if let Some(iface) = IfName::new(b"wlan0") {
        let mut s = Status::no_radio(iface);
        s.radio = "rtw89";
        *STATUS.lock() = Some(s);
    }
    crate::wifi::register_rtw89();
    say("card kept for the station (rtw89wifi); /dev/wifi0 is the radio");
}

/// Start the daemon, if a card was adopted. Called once the scheduler runs.
pub fn spawn() {
    if CARD.lock().is_none() {
        return;
    }
    match crate::sched::spawn_daemon(daemon) {
        Some(slot) => SLOT.store(slot, Ordering::Release),
        None => say("no task slot for the station daemon"),
    }
}

/// Render the status for a `/dev/wifi0` read.
pub fn write_status(out: &mut [u8]) -> Option<usize> {
    STATUS.lock().as_ref()?.write(out)
}

/// A command from `/dev/wifi0`. The newest pending one wins; the status shows
/// it taken at once, so a reader polling for the outcome never sees the state
/// from before it.
pub fn request(c: &Command) {
    if let Some(s) = STATUS.lock().as_mut() {
        match c {
            Command::Scan { .. } => {}
            Command::Connect { ssid, .. } => {
                s.link = Link::Associating;
                s.ssid = *ssid;
                s.bssid = [0; 6];
                s.error = JoinError::None;
            }
            Command::Disconnect { .. } => {}
        }
    }
    *REQUEST.lock() = Some(*c);
    let slot = SLOT.load(Ordering::Acquire);
    if slot != usize::MAX {
        crate::sched::wake(slot);
    }
}

/// The reset path is about to power the card off under the daemon: it must
/// touch it no more.
pub fn stop() {
    STOP.store(true, Ordering::Release);
}

fn with_status(f: impl FnOnce(&mut Status)) {
    if let Some(s) = STATUS.lock().as_mut() {
        f(s);
    }
}

fn now_us() -> u64 {
    crate::net::uptime_us()
}

/// Park for `ms` — the scheduler runs everyone else meanwhile.
fn nap(ms: u64) {
    crate::sched::block_until_deadline(now_us() + ms * 1000);
}

fn park_forever() -> ! {
    loop {
        nap(60_000);
    }
}

extern "C" fn daemon() -> ! {
    let Some(mut card) = CARD.lock().take() else { park_forever() };
    let mut st = Station { vars: script::Vars { mac: MAC, ..script::Vars::default() }, ready: false, dirty: false, peer: None };
    st.start(&mut card, false);
    loop {
        if STOP.load(Ordering::Acquire) {
            park_forever();
        }
        let req = REQUEST.lock().take();
        match req {
            Some(Command::Scan { .. }) => st.scan(&mut card),
            Some(Command::Connect { ssid, psk, bssid, .. }) => {
                let outcome = st.join(&mut card, &ssid, psk.as_ref(), bssid);
                if let Err(e) = outcome {
                    serial::puts("[rtw] join failed: ");
                    serial::puts(e.name());
                    serial::puts("\n");
                    with_status(|s| {
                        s.link = Link::Failed;
                        s.error = e;
                        s.bssid = [0; 6];
                        s.chan = 0;
                    });
                }
            }
            Some(Command::Disconnect { .. }) => st.disconnect(),
            None => st.idle(&mut card),
        }
        nap(IDLE_MS);
    }
}

/// The association the station holds.
struct Peer {
    bssid: Bssid,
    supplicant: Supplicant,
    /// Software sequence number for frames of tid 7 (EAPOL).
    seq: u16,
}

struct Station {
    vars: script::Vars,
    /// `JOIN1` ran on the card as it stands.
    ready: bool,
    /// The card holds state from a join attempt; restart it before the next.
    dirty: bool,
    peer: Option<Peer>,
}

/// A join's outcome; the error is the one `/dev/wifi0` reports.
type Joined = Result<(), JoinError>;

/// What a beacon's RSN element and privacy bit make of a network.
fn security_of(b: &beacon::Bss<'_>) -> Security {
    match b.rsn {
        None if !b.privacy() => Security::Open,
        Some(r)
            if r.akm_psk && r.pairwise_ccmp && r.group == beacon::SUITE_CCMP && r.caps & (1 << 6) == 0 =>
        {
            Security::Wpa2Psk
        }
        _ => Security::Other,
    }
}

/// Every received 802.11 frame that arrived intact, with its FCS.
fn frames(card: &mut Card, mut f: impl FnMut(&[u8])) {
    card.poll_rx(|p: &rx::Packet<'_>| {
        if p.desc.pkt_type == rx::kind::WIFI && !p.desc.crc32_err {
            f(p.body);
        }
    });
}

impl Station {
    /// Bring the card to "started, not joined": restart it if a join left
    /// state in it (`restart`), then replay `JOIN1`.
    fn start(&mut self, card: &mut Card, restart: bool) -> bool {
        self.ready = false;
        if restart && !card.restart() {
            say("restart failed");
            return false;
        }
        self.dirty = false;
        self.peer = None;
        if !card.replay("join1 (start)", script::JOIN1, &self.vars) {
            return false;
        }
        self.ready = true;
        with_status(|s| {
            if s.link == Link::NoRadio {
                s.link = Link::Down;
            }
        });
        true
    }

    /// Listen for [`SCAN_MS`] with the RX filter open; the results replace the
    /// status's.
    fn scan(&mut self, card: &mut Card) {
        if !self.ready && !self.start(card, true) {
            with_status(|s| s.scans += 1);
            return;
        }
        let was = STATUS.lock().as_ref().map_or(Link::Down, |s| s.link);
        if was != Link::Connected {
            with_status(|s| s.link = Link::Scanning);
        }
        let mut found = [Bss::EMPTY; MAX_BSS];
        let mut n = 0;
        let mut beacons = 0u32;
        card.open_filter();
        let deadline = now_us() + SCAN_MS * 1000;
        while now_us() < deadline && !STOP.load(Ordering::Acquire) {
            frames(card, |f| {
                let Some(b) = beacon::parse(f, true) else { return };
                beacons += 1;
                if found[..n].iter().any(|e| e.bssid == b.bssid) || n >= MAX_BSS {
                    return;
                }
                let Some(ssid) = Ssid::new(b.ssid) else { return };
                found[n] = Bss {
                    ssid,
                    bssid: b.bssid,
                    chan: b.channel.map_or(CHANNEL, u16::from),
                    signal: 0,
                    security: security_of(&b),
                };
                n += 1;
            });
            nap(POLL_MS);
        }
        card.close_filter();
        serial::puts("[rtw] scan: ");
        serial::put_dec(u64::from(beacons));
        serial::puts(" beacons, ");
        serial::put_dec(n as u64);
        serial::puts(" networks\n");
        for b in &found[..n] {
            serial::puts("[rtw]   bss ");
            put_oui(&b.bssid);
            serial::puts(" ch ");
            serial::put_dec(u64::from(b.chan));
            serial::puts(" ssid#");
            serial::put_hexn(u64::from(fnv1a(b.ssid.as_bytes())), 8);
            serial::puts(" ");
            serial::puts(b.security.name());
            serial::puts("\n");
        }
        with_status(|s| {
            s.bss = found;
            s.nbss = n;
            s.scans += 1;
            if s.link == Link::Scanning {
                s.link = was;
            }
        });
    }

    /// The network `connect` named, from the last scan — or a fresh one.
    fn find(&mut self, card: &mut Card, ssid: &Ssid, bssid: Option<Bssid>) -> Option<Bss> {
        let look = || {
            STATUS.lock().as_ref().and_then(|s| {
                s.results().iter().copied().find(|b| b.ssid == *ssid && bssid.is_none_or(|w| w == b.bssid))
            })
        };
        if let Some(b) = look() {
            return Some(b);
        }
        self.scan(card);
        look()
    }

    fn join(&mut self, card: &mut Card, ssid: &Ssid, psk: Option<&[u8; PSK_LEN]>, bssid: Option<Bssid>) -> Joined {
        let bss = self.find(card, ssid, bssid).ok_or(JoinError::NotFound)?;
        let psk = match (bss.security, psk) {
            (Security::Wpa2Psk, Some(k)) => k,
            (Security::Wpa2Psk, None) => return Err(JoinError::AuthFailed),
            // Open networks need a join without `JOIN4`'s keys, which nobody
            // has recorded yet.
            _ => return Err(JoinError::Unsupported),
        };
        serial::puts("[rtw] join: bss ");
        put_oui(&bss.bssid);
        serial::puts(" ch ");
        serial::put_dec(u64::from(bss.chan));
        serial::puts(" ssid#");
        serial::put_hexn(u64::from(fnv1a(ssid.as_bytes())), 8);
        serial::puts("\n");
        with_status(|s| {
            s.link = Link::Associating;
            s.ssid = *ssid;
            s.bssid = bss.bssid;
            s.chan = bss.chan;
            s.security = bss.security;
        });

        if (self.dirty || !self.ready) && !self.start(card, true) {
            return Err(JoinError::Timeout);
        }
        self.dirty = true;
        self.vars.bssid = bss.bssid;
        self.vars.aid = 0;
        if !card.replay("join2 (prepare)", script::JOIN2, &self.vars) {
            return Err(JoinError::Timeout);
        }

        // Authentication: open system, transaction 1, answered by 2.
        let mut buf = [0u8; 512];
        let mut answer: Option<u16> = None;
        for _ in 0..MGMT_TRIES {
            let len = sta::auth_request(&mut buf, &bss.bssid, &MAC).ok_or(JoinError::Timeout)?;
            send(card, TX_MGMT, &buf[..len], tx::Desc::mgmt(len as u16, 0))?;
            let before = card.tx_idx(TX_MGMT);
            let mut tally = Tally::default();
            answer = wait_for(card, MGMT_REPLY_MS, |f| {
                tally.count(f, &bss.bssid);
                Auth::parse(f)
                    .filter(|a| a.from == bss.bssid && a.to == MAC && a.transaction == 2)
                    .map(|a| a.status)
            });
            tx_report(card, "auth", before, &tally);
            if answer.is_some() {
                break;
            }
        }
        match answer {
            None => {
                say("auth: no answer");
                return Err(JoinError::Timeout);
            }
            Some(0) => say("auth: accepted"),
            Some(st) => {
                say_dec("auth: refused, status", u64::from(st));
                return Err(JoinError::AuthFailed);
            }
        }

        // Association.
        let mut resp: Option<AssocResp> = None;
        for _ in 0..MGMT_TRIES {
            let len = sta::assoc_request(&mut buf, &bss.bssid, &MAC, ssid.as_bytes(), &sta::ASSOC_TAIL_2G)
                .ok_or(JoinError::Timeout)?;
            send(card, TX_MGMT, &buf[..len], tx::Desc::mgmt(len as u16, 0))?;
            resp = wait_for(card, MGMT_REPLY_MS, |f| AssocResp::parse(f).filter(|r| r.from == bss.bssid && r.to == MAC));
            if resp.is_some() {
                break;
            }
        }
        let resp = resp.ok_or_else(|| {
            say("assoc: no answer");
            JoinError::Timeout
        })?;
        if resp.status != 0 {
            say_dec("assoc: refused, status", u64::from(resp.status));
            return Err(JoinError::Unsupported);
        }
        say_dec("assoc: accepted, aid", u64::from(resp.aid));
        self.vars.aid = resp.aid;
        if !card.replay("join3 (associated)", script::JOIN3, &self.vars) {
            return Err(JoinError::Timeout);
        }

        // The 4-way handshake.
        let mut snonce = [0u8; 32];
        if !crate::net::rng_fill_checked(&mut snonce) {
            say("no randomness for the SNonce");
            return Err(JoinError::Timeout);
        }
        let mut peer = Peer { bssid: bss.bssid, supplicant: Supplicant::new(psk, bss.bssid, MAC, snonce, &sta::RSN_IE), seq: 0 };
        let deadline = now_us() + HANDSHAKE_MS * 1000;
        let mut mic_failures = 0u32;
        while now_us() < deadline {
            if STOP.load(Ordering::Acquire) {
                return Err(JoinError::Timeout);
            }
            let mut out = [0u8; eapol::HDR_LEN + eapol::MAX_KEY_DATA];
            let mut step: Option<Result<Action, eapol::Drop>> = None;
            let mut gone: Option<u16> = None;
            frames(card, |f| {
                if step.is_some() {
                    return; // one message per lap; the AP retransmits the rest
                }
                if let Some(g) = Goodbye::parse(f).filter(|g| g.from == peer.bssid && g.to == MAC) {
                    gone = Some(g.reason);
                } else if let Some(d) = Data::parse(f, true)
                    .filter(|d| d.bssid == peer.bssid && d.ethertype == sta::ETHERTYPE_EAPOL)
                {
                    step = Some(peer.supplicant.handle(d.payload, &mut out));
                }
            });
            if let Some(reason) = gone {
                say_dec("deauthenticated during the handshake, reason", u64::from(reason));
                return Err(if mic_failures > 0 { JoinError::AuthFailed } else { JoinError::Timeout });
            }
            match step {
                None => nap(POLL_MS),
                Some(Ok(Action::Send(len))) => {
                    send_eapol(card, &mut peer, &out[..len])?;
                    say("eapol: message 1 answered");
                }
                Some(Ok(Action::Complete { len, tk, gtk })) => {
                    send_eapol(card, &mut peer, &out[..len])?;
                    self.vars.tk = tk;
                    self.vars.gtk = gtk.key;
                    self.vars.gtk_idx = gtk.idx;
                    let ok = card.replay("join4 (keys)", script::JOIN4, &self.vars);
                    if !ok {
                        return Err(JoinError::Timeout);
                    }
                    say_dec("eapol: message 3 answered, keys installed, group key id", u64::from(gtk.idx));
                    self.peer = Some(peer);
                    with_status(|s| {
                        s.link = Link::Connected;
                        s.error = JoinError::None;
                    });
                    return Ok(());
                }
                Some(Ok(Action::Rekey { .. })) => say("eapol: group message before the pairwise keys; ignored"),
                Some(Err(eapol::Drop::Mic)) => {
                    mic_failures += 1;
                    say("eapol: MIC did not verify (wrong key?)");
                }
                Some(Err(e)) => say_dec("eapol: dropped, reason", e as u64),
            }
        }
        say("eapol: handshake timed out");
        Err(if mic_failures > 0 { JoinError::AuthFailed } else { JoinError::Timeout })
    }

    /// Forget the association. No deauthentication frame goes out (none is
    /// built yet): the access point times the station out. The card keeps the
    /// peer until the next join restarts it.
    fn disconnect(&mut self) {
        if self.peer.take().is_some() {
            say("disconnected");
        }
        with_status(|s| {
            s.link = Link::Down;
            s.ssid = Ssid::EMPTY;
            s.bssid = [0; 6];
            s.chan = 0;
            s.signal = 0;
            s.security = Security::Open;
            s.error = JoinError::None;
        });
    }

    /// Between requests: drain the RX ring; while joined, answer group
    /// rekeys and notice being sent away.
    fn idle(&mut self, card: &mut Card) {
        if !self.ready {
            return;
        }
        let Some(peer) = self.peer.as_mut() else {
            frames(card, |_| {});
            return;
        };
        let mut out = [0u8; eapol::HDR_LEN + eapol::MAX_KEY_DATA];
        let mut step: Option<Result<Action, eapol::Drop>> = None;
        let mut gone: Option<u16> = None;
        frames(card, |f| {
            if let Some(g) = Goodbye::parse(f).filter(|g| g.from == peer.bssid && g.to == MAC) {
                gone = Some(g.reason);
            } else if step.is_none()
                && let Some(d) = Data::parse(f, true)
                    .filter(|d| d.bssid == peer.bssid && d.ethertype == sta::ETHERTYPE_EAPOL)
            {
                step = Some(peer.supplicant.handle(d.payload, &mut out));
            }
        });
        if let Some(reason) = gone {
            say_dec("deauthenticated, reason", u64::from(reason));
            self.peer = None;
            with_status(|s| {
                s.link = Link::Down;
                s.bssid = [0; 6];
            });
            return;
        }
        if let Some(Ok(Action::Rekey { len, gtk })) = step {
            // The group key is replayed with `JOIN4` whole — the pairwise key
            // goes in again unchanged. A group-only segment would be exact;
            // nobody has cut one from the recording yet.
            if send_eapol(card, peer, &out[..len]).is_ok() {
                self.vars.gtk = gtk.key;
                self.vars.gtk_idx = gtk.idx;
                let _ = card.replay("join4 (group rekey)", script::JOIN4, &self.vars);
            }
        }
    }
}

fn send(card: &mut Card, which: usize, frame: &[u8], d: tx::Desc) -> Result<(), JoinError> {
    card.send(which, frame, d).map_err(|e| {
        match e {
            tx::Error::TooBig(n) => say_dec("TX: frame too big,", n as u64),
            tx::Error::Stuck { reg, last } => {
                serial::puts("[rtw] TX: channel stuck, reg 0x");
                serial::put_hex(u64::from(reg));
                serial::puts(" = 0x");
                serial::put_hex(u64::from(last));
                serial::puts("\n");
            }
        }
        JoinError::Timeout
    })
}

/// An EAPOL frame to the access point: a QoS data frame of tid 7, in the clear.
fn send_eapol(card: &mut Card, peer: &mut Peer, body: &[u8]) -> Result<(), JoinError> {
    let mut f = [0u8; sta::QOS_HDR_LEN + 8 + eapol::HDR_LEN + eapol::MAX_KEY_DATA];
    let len = sta::data_frame(&mut f, &peer.bssid, &MAC, &peer.bssid, 7, peer.seq, None, sta::ETHERTYPE_EAPOL, body)
        .ok_or(JoinError::Timeout)?;
    peer.seq = (peer.seq + 1) & 0xfff;
    send(card, TX_EAPOL, &f[..len], tx::Desc::eapol(len as u16, 0))
}

/// What was heard while waiting for an answer: everything, and what came
/// from the access point (and of that, management frames addressed to us).
#[derive(Default)]
struct Tally {
    frames: u32,
    from_ap: u32,
    mgmt_to_us: u32,
}

impl Tally {
    fn count(&mut self, f: &[u8], bssid: &Bssid) {
        self.frames += 1;
        if f.len() >= 16 && f[10..16] == *bssid {
            self.from_ap += 1;
            if f[0] & 0x0c == 0 && f[4..10] == MAC {
                self.mgmt_to_us += 1;
            }
        }
    }
}

/// One line per sent frame: did the chip fetch it (its read index moved), what
/// the release reports said, any DMA error, and what was heard meanwhile.
fn tx_report(card: &mut Card, what: &str, before: u32, t: &Tally) {
    let after = card.tx_idx(TX_MGMT);
    let (isr, idct) = card.dma_errors();
    serial::puts("[rtw] ");
    serial::puts(what);
    serial::puts(" tx: idx 0x");
    serial::put_hex(u64::from(before));
    serial::puts(" -> 0x");
    serial::put_hex(u64::from(after));
    serial::puts(", release reports ");
    serial::put_dec(u64::from(card.rpq_seen));
    serial::puts(" last");
    for b in &card.rpq_last[..24] {
        serial::puts(" ");
        serial::put_hexn(u64::from(*b), 2);
    }
    serial::puts(", dmac_err 0x");
    serial::put_hex(u64::from(isr));
    serial::puts(" idct 0x");
    serial::put_hex(u64::from(idct));
    serial::puts("; heard ");
    serial::put_dec(u64::from(t.frames));
    serial::puts(", from ap ");
    serial::put_dec(u64::from(t.from_ap));
    serial::puts(", mgmt to us ");
    serial::put_dec(u64::from(t.mgmt_to_us));
    serial::puts("\n");
}

/// Poll the RX ring for up to `ms` until `pick` recognises a frame.
fn wait_for<T>(card: &mut Card, ms: u64, mut pick: impl FnMut(&[u8]) -> Option<T>) -> Option<T> {
    let deadline = now_us() + ms * 1000;
    loop {
        let mut got = None;
        frames(card, |f| {
            if got.is_none() {
                got = pick(f);
            }
        });
        if got.is_some() || now_us() >= deadline || STOP.load(Ordering::Acquire) {
            return got;
        }
        nap(POLL_MS);
    }
}
