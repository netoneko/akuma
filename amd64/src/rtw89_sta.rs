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
//! | scan | every beacon, channel by channel, [`SCAN_DWELL_MS`] each | RX filter opened, then put back |
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

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use akuma_ieee80211::{beacon, ccmp};
use akuma_ieee80211::sta::{self, Amsdu, AssocResp, Auth, Data, Goodbye};
use akuma_rtw89::{rx, script, tx};
use akuma_wifi::cmd::Command;
use akuma_wifi::status::{Bss, Error as JoinError, Link, Status};
use akuma_wifi::{Bssid, IfName, MAX_BSS, PSK_LEN, Security, Ssid};
use akuma_net::queued::{FRAME_MAX, FrameQueues};
use akuma_wpa::eapol::{self, Action, Supplicant};
use spinning_top::Spinlock;

use crate::rtw89::{Card, TX_DATA, TX_EAPOL, TX_MGMT, fnv1a};
use crate::serial;

/// This station's address. The card's own lives in its efuse, but only the
/// address CAM entries `JOIN1`..`JOIN4` write carry a station address, so the
/// driver picks one: locally administered, `"AKUMA"` in ASCII after the `02`.
const MAC: [u8; 6] = [0x02, 0x41, 0x4b, 0x55, 0x4d, 0x41];
/// The channel `JOIN1` leaves the card on (the recording joined on 1).
const CHANNEL: u8 = 1;
/// How long a scan listens on each channel, and on the first [`SCAN_LIKELY`]
/// of them (the network's last channel, then 1, 6, 11 — where an access point
/// is almost always found): ~3 and ~6 beacon intervals. Frames are lost often
/// enough that three beacons miss a network (boot 47: an AP at -51 dBm on
/// channel 11 went unheard at 350 ms). A full sweep is ~5 s; one that finds the
/// network it came for stops at that channel.
const SCAN_DWELL_MS: u64 = 300;
const SCAN_DWELL_LIKELY_MS: u64 = 600;
const SCAN_LIKELY: usize = 4;
/// One authentication or association frame's wait for the answer, and how
/// many times each is sent. Access points answer in milliseconds; on a busy
/// channel either frame or its answer is lost often enough that one try is
/// not a join.
const MGMT_REPLY_MS: u64 = 600;
const MGMT_TRIES: u32 = 6;
/// The 4-way handshake, from the association response to message 4. Access
/// points send message 1 within milliseconds and retry each message about
/// once a second, a handful of times.
const HANDSHAKE_MS: u64 = 10_000;
/// Whole joins tried per `connect` before it fails: each from a power-cycled
/// card. Only timeouts are retried — a wrong key or a refusal would only be
/// answered the same way again.
const JOIN_ATTEMPTS: u32 = 3;
/// Rejoin after a lost link or a join that timed out: retried after
/// [`REJOIN_FIRST_MS`], every time. The link drops often on a crowded
/// channel, so the station never gives up on a network it was told to join —
/// only `disconnect` (or a wrong key) ends that. With `wifibackoff` on the
/// command line ([`BACKOFF`]) the wait doubles after each failure, to
/// [`REJOIN_MAX_MS`]; off by default.
const REJOIN_FIRST_MS: u64 = 1000;
const REJOIN_MAX_MS: u64 = 30_000;
static BACKOFF: AtomicBool = AtomicBool::new(false);
/// The firmware's beacon-filter report (`RTW89_MAC_C2H_FUNC_BCNFLTR_RPT`):
/// category MAC (1), class offload (1), function 0xd. Type 0 in word 2 bits
/// 9:8 is beacon loss — the firmware stopped hearing the access point; the
/// report also carries the averaged beacon RSSI (bits 23:16, minus 110 dBm).
const C2H_BCNFLTR: (u8, u8, u8) = (1, 1, 0x0d);
/// Power cycles tried before the radio is given up on (see `Station::start`).
const RESTART_TRIES: u32 = 3;
/// How often an idle daemon looks at the RX ring and its request.
const IDLE_MS: u64 = 50;
/// A join's waits poll the RX ring this often.
const POLL_MS: u64 = 2;

static CARD: Spinlock<Option<Card>> = Spinlock::new(None);
/// The link to the network stack (`ExternalDevice::Queued`): received data
/// frames go in as Ethernet, the stack's frames come out to be sent. Frames
/// pass only while joined; otherwise transmits are dropped.
pub static LINK: FrameQueues = FrameQueues::new();
static ADOPTED: AtomicBool = AtomicBool::new(false);
/// Frames from the stack each lap of a joined daemon may send: the whole
/// queue ([`akuma_net::queued::SLOTS`]) — receive runs again first anyway.
const TX_PER_LAP: usize = akuma_net::queued::SLOTS;
static STATUS: Spinlock<Option<Status>> = Spinlock::new(None);
static REQUEST: Spinlock<Option<Command>> = Spinlock::new(None);
static STOP: AtomicBool = AtomicBool::new(false);
/// Signature of the access points the last scan printed (see `scan`).
static LAST_SCAN_SIG: AtomicU32 = AtomicU32::new(0);
/// Consecutive failed joins. A dark link retries every second; only the 1st,
/// 2nd, 4th, 8th... attempt is logged, so the lines that came before the loss
/// stay in the log ring.
static FAILS: AtomicU32 = AtomicU32::new(0);
/// The BSSID that last sent us away or failed a join (packed, 0 = none). With
/// two access points on one SSID, `find` takes the other while it can: the
/// bad one kept refusing the same station for minutes (measured on ryzen
/// 2026-10-08) while its neighbour accepted at once.
static BAD_BSS: AtomicU64 = AtomicU64::new(0);

fn pack_bssid(b: &Bssid) -> u64 {
    b.iter().fold(0u64, |a, x| (a << 8) | u64::from(*x)) | (1 << 48)
}

fn loud(n: u32) -> bool {
    n <= 1 || n.is_power_of_two()
}
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
pub fn adopt(card: Card, backoff: bool) {
    BACKOFF.store(backoff, Ordering::Relaxed);
    LINK.set_mac(MAC);
    LINK.on_transmit(wake_daemon);
    ADOPTED.store(true, Ordering::Release);
    *CARD.lock() = Some(card);
    if let Some(iface) = IfName::new(b"wlan0") {
        let mut s = Status::no_radio(iface);
        s.radio = "rtw89";
        *STATUS.lock() = Some(s);
    }
    crate::wifi::register_rtw89();
    say("card kept for the station (rtw89wifi); /dev/wifi0 is the radio");
}

/// Is the wifi station this boot's network link? Then the stack is built on
/// [`LINK`], and nothing can reach the network until userspace has asked for
/// a join — DHCP, DNS and the clock all follow the join, not the boot.
#[must_use]
pub fn is_link() -> bool {
    ADOPTED.load(Ordering::Acquire)
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

/// The stack queued a frame: end the daemon's nap now rather than at its
/// deadline.
fn wake_daemon() {
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
    let mut st = Station {
        vars: script::Vars { mac: MAC, ..script::Vars::default() },
        stats: LinkStats::default(),
        next_report: 0,
        ready: false,
        chan: CHANNEL,
        peer: None,
        wanted: None,
        retry_at: 0,
        backoff_ms: REJOIN_FIRST_MS,
    };
    st.start(&mut card, false);
    let mut last_lap = now_us();
    loop {
        if STOP.load(Ordering::Acquire) {
            park_forever();
        }
        let now = now_us();
        st.stats.laps += 1;
        st.stats.max_gap_us = st.stats.max_gap_us.max(now - last_lap);
        last_lap = now;
        let req = REQUEST.lock().take();
        match req {
            Some(Command::Scan { .. }) => st.scan(&mut card),
            Some(Command::Connect { ssid, psk, bssid, .. }) => {
                st.wanted = Some(Wanted { ssid, psk, bssid, chan: 0 });
                st.backoff_ms = REJOIN_FIRST_MS;
                st.join_wanted(&mut card);
            }
            Some(Command::Disconnect { .. }) => st.disconnect(),
            None => {
                st.idle(&mut card);
                if st.peer.is_none() && st.wanted.is_some() && now_us() >= st.retry_at {
                    if loud(FAILS.load(Ordering::Relaxed)) {
                        say("rejoining");
                    }
                    st.join_wanted(&mut card);
                }
            }
        }
        // Joined, the daemon is the link's poll loop: a short nap keeps
        // receive latency at a couple of milliseconds.
        nap(if st.peer.is_some() { POLL_MS } else { IDLE_MS });
    }
}

/// The association the station holds.
struct Peer {
    bssid: Bssid,
    supplicant: Supplicant,
    /// The pairwise key is in the card: EAPOL goes out encrypted from here
    /// on, as 802.11 wants a retransmitted message 4 or a group message 2.
    keyed: bool,
    /// The CCMP packet number of the last protected frame sent.
    pn: u64,
    /// The highest CCMP packet number received, per key and TID.
    replay: ccmp::Replay,
}

/// The network the station was told to join, kept until `disconnect`: what a
/// lost link rejoins.
#[derive(Clone, Copy)]
struct Wanted {
    ssid: Ssid,
    psk: Option<[u8; PSK_LEN]>,
    bssid: Option<Bssid>,
    /// The channel it was last joined on (0: never): where a rejoin looks
    /// first, since an access point that re-picks its channel is the usual
    /// reason a rejoin fails.
    chan: u8,
}

/// What the joined link carried, for the periodic `[rtw] link:` line.
#[derive(Default)]
struct LinkStats {
    /// Data frames from the access point, by what became of them.
    rx_data: u32,
    rx_amsdu: u32,
    rx_msdus: u32,
    /// Not protected after the keys (dropped), or protected but not
    /// decrypted by the card (dropped).
    rx_clear: u32,
    rx_undecrypted: u32,
    /// Protected frames the card decrypted whose packet number was not above
    /// the last one on that key and TID (dropped as replays).
    rx_replay: u32,
    /// From the access point but not a data frame we could take apart.
    rx_unparsed: u32,
    delivered: u32,
    tx: u32,
    tx_failed: u32,
    /// TCP handshake and reset events, both ways (counted; the first few are
    /// logged by port).
    syn_rx: u32,
    rst_rx: u32,
    syn_tx: u32,
    rst_tx: u32,
    traced: u32,
    /// Daemon laps since the last report, and the longest gap between two
    /// (µs): how long a frame can wait for the station.
    laps: u32,
    max_gap_us: u64,
}

/// TCP events logged by port before the trace goes quiet.
const TCP_TRACE_MAX: u32 = 40;
/// How often a joined station prints its `[rtw] link:` line.
const LINK_REPORT_US: u64 = 60_000_000;

struct Station {
    vars: script::Vars,
    stats: LinkStats,
    next_report: u64,
    wanted: Option<Wanted>,
    /// When the next rejoin may start (`now_us` clock), and the wait after it.
    retry_at: u64,
    backoff_ms: u64,
    /// `JOIN1` ran on the card as it stands.
    ready: bool,
    /// The channel the card is tuned to (`JOIN1` leaves it on [`CHANNEL`]).
    chan: u8,
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

/// Drop every scan result for `ssid`: its access point may have changed
/// channel, and the next join must look for it rather than trust the last scan.
fn forget_network(ssid: &Ssid) {
    with_status(|s| {
        let (mut k, mut i) = (0, 0);
        while i < s.nbss {
            if s.bss[i].ssid != *ssid {
                s.bss[k] = s.bss[i];
                k += 1;
            }
            i += 1;
        }
        s.nbss = k;
    });
}

/// Every received 802.11 frame that arrived intact, with its FCS.
fn frames(card: &mut Card, mut f: impl FnMut(&[u8])) {
    frames_a1(card, |b, _| f(b));
}

/// [`frames`], with whether the card's own address match hit for each (the
/// RX descriptor's `A1_MATCH`): with the filter open the station matches
/// addresses itself, and this says whether the card would have.
fn frames_a1(card: &mut Card, mut f: impl FnMut(&[u8], bool)) {
    card.poll_rx(|p: &rx::Packet<'_>| {
        if p.desc.pkt_type == rx::kind::WIFI && !p.desc.crc32_err {
            f(p.body, p.desc.a1_match);
        }
    });
}

impl Station {
    /// Tune the card to 2.4 GHz channel `ch` by replaying Linux's switch to it
    /// (`script::CHAN`), then drop what the RX ring held from the old one.
    /// `false` for a channel the recordings do not cover, or a replay that
    /// went wrong.
    fn set_channel(&mut self, card: &mut Card, ch: u8) -> bool {
        if ch == self.chan {
            return true;
        }
        let Some(seq) = script::chan(ch) else { return false };
        if !card.replay("chan (switch)", seq, &self.vars) {
            return false;
        }
        self.chan = ch;
        frames(card, |_| {});
        true
    }

    /// Bring the card to "started, not joined": restart it if a join left
    /// state in it (`restart`), then replay `JOIN1`.
    fn start(&mut self, card: &mut Card, restart: bool) -> bool {
        self.ready = false;
        // A power cycle re-downloads the firmware, and the card refuses a
        // second download in one boot (`FWDL_SECURITY_FAIL`, status 3 — seen
        // on ryzen 2026-10-06, boot 19). So this runs only when the card is
        // not up at all, and a refusal is retried a few times rather than
        // every second for ever.
        if restart {
            let mut up = false;
            for _ in 0..RESTART_TRIES {
                if card.restart() {
                    up = true;
                    break;
                }
                nap(200);
            }
            if !up {
                say("restart failed; the radio stays down until a reboot");
                return false;
            }
        }
        self.peer = None;
        if !card.replay("join1 (start)", script::JOIN1, &self.vars) {
            return false;
        }
        self.ready = true;
        self.chan = CHANNEL;
        with_status(|s| {
            if s.link == Link::NoRadio {
                s.link = Link::Down;
            }
        });
        true
    }

    /// Listen on every channel in turn; the results replace the status's.
    fn scan(&mut self, card: &mut Card) {
        self.sweep(card, None);
    }

    /// Listen for beacons channel by channel in [`script::scan_order`] — the
    /// wanted network's last channel first — [`SCAN_DWELL_MS`] each. With a
    /// `target` the sweep stops on the channel it is heard on. Associated, the
    /// radio cannot leave the access point's channel without dropping the
    /// link, so it listens there only.
    fn sweep(&mut self, card: &mut Card, target: Option<(&Ssid, Option<Bssid>)>) {
        if !self.ready && !self.start(card, true) {
            with_status(|s| s.scans += 1);
            return;
        }
        let was = STATUS.lock().as_ref().map_or(Link::Down, |s| s.link);
        if was != Link::Connected {
            with_status(|s| s.link = Link::Scanning);
        }
        let mut order = [0u8; script::CHAN_COUNT];
        let mut count = script::scan_order(self.wanted.map_or(0, |w| w.chan), &mut order);
        if self.peer.is_some() {
            order[0] = self.chan;
            count = 1;
        }
        let mut found = [Bss::EMPTY; MAX_BSS];
        let mut n = 0;
        let mut beacons = 0u32;
        let mut hit = false;
        card.open_filter();
        'sweep: for (i, &ch) in order[..count].iter().enumerate() {
            if STOP.load(Ordering::Acquire) {
                break;
            }
            if !self.set_channel(card, ch) {
                continue;
            }
            let dwell = if i < SCAN_LIKELY { SCAN_DWELL_LIKELY_MS } else { SCAN_DWELL_MS };
            let deadline = now_us() + dwell * 1000;
            while now_us() < deadline && !STOP.load(Ordering::Acquire) {
                frames(card, |f| {
                    let Some(b) = beacon::parse(f, true) else { return };
                    beacons += 1;
                    if found[..n].iter().any(|e| e.bssid == b.bssid) || n >= MAX_BSS {
                        return;
                    }
                    let Some(ssid) = Ssid::new(b.ssid) else { return };
                    // A beacon names its own channel; a neighbouring one can
                    // be heard from the next channel over.
                    found[n] = Bss {
                        ssid,
                        bssid: b.bssid,
                        chan: b.channel.map_or(u16::from(ch), u16::from),
                        signal: 0,
                        security: security_of(&b),
                    };
                    hit |= target.is_some_and(|(t, want)| *t == ssid && want.is_none_or(|w| w == b.bssid));
                    n += 1;
                });
                if hit {
                    break 'sweep;
                }
                nap(POLL_MS);
            }
        }
        card.close_filter();
        // A rejoining station scans every second: the report is printed only
        // when the set of access points changed, or it fills the log ring and
        // pushes out whatever made the link drop.
        let mut sig = n as u32;
        for b in &found[..n] {
            sig = (sig ^ fnv1a(&b.bssid)).wrapping_mul(0x0100_0193);
        }
        let changed = LAST_SCAN_SIG.swap(sig, Ordering::Relaxed) != sig;
        if changed {
            serial::puts("[rtw] scan: ");
            serial::put_dec(u64::from(beacons));
            serial::puts(" beacons, ");
            serial::put_dec(n as u64);
            serial::puts(" networks\n");
        }
        for b in found[..n].iter().filter(|_| changed) {
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
                let bad = BAD_BSS.load(Ordering::Relaxed);
                let mut ok = s.results().iter().copied().filter(|b| b.ssid == *ssid && bssid.is_none_or(|w| w == b.bssid));
                let first = ok.next()?;
                if pack_bssid(&first.bssid) != bad {
                    return Some(first);
                }
                Some(ok.next().unwrap_or(first))
            })
        };
        if let Some(b) = look() {
            return Some(b);
        }
        self.sweep(card, Some((ssid, bssid)));
        look()
    }

    /// Join [`Station::wanted`]; on a failure worth retrying, schedule the
    /// next try, on one that is not (wrong key, refused), forget the network.
    fn join_wanted(&mut self, card: &mut Card) {
        let Some(w) = self.wanted else { return };
        match self.join(card, &w.ssid, w.psk.as_ref(), w.bssid) {
            Ok(()) => {
                FAILS.store(0, Ordering::Relaxed);
                self.backoff_ms = REJOIN_FIRST_MS;
                let chan = STATUS.lock().as_ref().map_or(0, |s| s.chan);
                if let (Some(w), Ok(c)) = (self.wanted.as_mut(), u8::try_from(chan)) {
                    w.chan = c;
                }
            }
            Err(e) => {
                let n = FAILS.fetch_add(1, Ordering::Relaxed) + 1;
                let loud = loud(n);
                if loud {
                    serial::puts("[rtw] join failed: ");
                    serial::puts(e.name());
                    serial::puts(" (consecutive ");
                    serial::put_dec(u64::from(n));
                    serial::puts(")");
                }
                with_status(|s| {
                    s.link = Link::Failed;
                    s.error = e;
                    s.bssid = [0; 6];
                    s.chan = 0;
                });
                if matches!(e, JoinError::Timeout | JoinError::NotFound) {
                    if loud {
                        serial::puts("; retrying in ");
                        serial::put_dec(self.backoff_ms / 1000);
                        serial::puts(" s\n");
                    }
                    self.retry_at = now_us() + self.backoff_ms * 1000;
                    if BACKOFF.load(Ordering::Relaxed) {
                        self.backoff_ms = (self.backoff_ms * 2).min(REJOIN_MAX_MS);
                    }
                } else {
                    if !loud {
                        serial::puts("[rtw] join failed: ");
                        serial::puts(e.name());
                    }
                    serial::puts("; not retrying\n");
                    self.wanted = None;
                }
            }
        }
    }

    /// The access point is gone (sent us away, or the firmware stopped
    /// hearing its beacons): drop the association and rejoin at once, on the
    /// running card (`JOIN2`..`JOIN4` overwrite the old peer's entries).
    fn link_lost(&mut self) {
        if let Some(p) = self.peer.as_ref() {
            BAD_BSS.store(pack_bssid(&p.bssid), Ordering::Relaxed);
        }
        self.peer = None;
        // The access point may have moved: the cached scan result would send
        // the rejoin to the channel it just left. Rescan, hint channel first.
        if let Some(w) = self.wanted {
            forget_network(&w.ssid);
        }
        LINK.flush_transmit();
        LINK.link_changed();
        self.retry_at = now_us();
        self.backoff_ms = REJOIN_FIRST_MS;
        with_status(|s| {
            s.link = if self.wanted.is_some() { Link::Associating } else { Link::Down };
            s.bssid = [0; 6];
        });
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
        let mut last = JoinError::Timeout;
        for attempt in 1..=JOIN_ATTEMPTS {
            if STOP.load(Ordering::Acquire) {
                break;
            }
            say_dec("join attempt", u64::from(attempt));
            // The filter stays open for the whole join: the station matches
            // addresses itself, so a card whose address CAM is wrong still
            // joins — and every reply logs whether the card's match hit.
            card.open_filter();
            let r = self.attempt(card, ssid, psk, &bss);
            card.close_filter();
            match r {
                Ok(()) => return Ok(()),
                Err(JoinError::Timeout) => last = JoinError::Timeout,
                Err(e) => return Err(e),
            }
        }
        forget_network(ssid);
        Err(last)
    }

    /// One join from a clean card: `JOIN2`, auth, assoc, `JOIN3`, the
    /// handshake, `JOIN4`.
    fn attempt(&mut self, card: &mut Card, ssid: &Ssid, psk: &[u8; PSK_LEN], bss: &Bss) -> Joined {
        // A retry, or a rejoin after a lost link, reuses the running card:
        // `JOIN2` rewrites the access point's address-CAM entry and `JOIN3`
        // and `JOIN4` overwrite the rest, as Linux re-authenticates without
        // powering anything off. Only a card that never came up is restarted.
        if !self.ready && !self.start(card, true) {
            return Err(JoinError::Timeout);
        }
        self.vars.bssid = bss.bssid;
        self.vars.aid = 0;
        let chan = u8::try_from(bss.chan).ok().filter(|c| script::chan(*c).is_some()).ok_or(JoinError::Unsupported)?;
        if !self.set_channel(card, chan) {
            return Err(JoinError::Timeout);
        }
        if !card.replay("join2 (prepare)", script::JOIN2, &self.vars) {
            return Err(JoinError::Timeout);
        }
        if chan != CHANNEL {
            // `JOIN2`'s last write to 0x19fe4 puts channel 1 back in that one
            // register: switch again, from the card's point of view.
            self.chan = CHANNEL;
            if !self.set_channel(card, chan) {
                return Err(JoinError::Timeout);
            }
        }

        // Authentication: open system, transaction 1, answered by 2.
        let mut buf = [0u8; 512];
        let len = sta::auth_request(&mut buf, &bss.bssid, &MAC).ok_or(JoinError::Timeout)?;
        let auth = mgmt_exchange(card, "auth", &buf[..len], &bss.bssid, |f| {
            Auth::parse(f).filter(|a| a.from == bss.bssid && a.to == MAC && a.transaction == 2).map(|a| a.status)
        })?;
        match auth {
            None => {
                say("auth: no answer");
                return Err(JoinError::Timeout);
            }
            Some(0) => say("auth: accepted"),
            Some(st) => {
                say_dec("auth: refused, status", u64::from(st));
                BAD_BSS.store(pack_bssid(&bss.bssid), Ordering::Relaxed);
                return Err(JoinError::Timeout); // the access point's mood, not the key
            }
        }

        // Association.
        let len = sta::assoc_request(&mut buf, &bss.bssid, &MAC, ssid.as_bytes(), &sta::ASSOC_TAIL_2G)
            .ok_or(JoinError::Timeout)?;
        let resp = mgmt_exchange(card, "assoc", &buf[..len], &bss.bssid, |f| {
            AssocResp::parse(f).filter(|r| r.from == bss.bssid && r.to == MAC)
        })?
        .ok_or_else(|| {
            say("assoc: no answer");
            JoinError::Timeout
        })?;
        if resp.status != 0 {
            say_dec("assoc: refused, status", u64::from(resp.status));
            BAD_BSS.store(pack_bssid(&bss.bssid), Ordering::Relaxed);
            return Err(JoinError::Timeout);
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
        let mut peer = Peer {
            bssid: bss.bssid,
            supplicant: Supplicant::new(psk, bss.bssid, MAC, snonce, &sta::RSN_IE),
            keyed: false,
            pn: 0,
            replay: ccmp::Replay::new(),
        };
        let deadline = now_us() + HANDSHAKE_MS * 1000;
        let mut mic_failures = 0u32;
        while now_us() < deadline {
            if STOP.load(Ordering::Acquire) {
                return Err(JoinError::Timeout);
            }
            let mut out = [0u8; eapol::HDR_LEN + eapol::MAX_KEY_DATA];
            let mut step: Option<(Result<Action, eapol::Drop>, bool)> = None;
            let mut gone: Option<u16> = None;
            frames_a1(card, |f, a1| {
                if step.is_some() {
                    return; // one message per lap; the AP retransmits the rest
                }
                if let Some(g) = Goodbye::parse(f).filter(|g| g.from == peer.bssid && g.to == MAC) {
                    gone = Some(g.reason);
                } else if let Some(d) = Data::parse(f, true)
                    .filter(|d| d.bssid == peer.bssid && d.ethertype == sta::ETHERTYPE_EAPOL)
                {
                    step = Some((peer.supplicant.handle(d.payload, &mut out), a1));
                }
            });
            if let Some(reason) = gone {
                say_dec("sent away during the handshake, reason", u64::from(reason));
                return Err(if mic_failures > 0 { JoinError::AuthFailed } else { JoinError::Timeout });
            }
            let Some((step, a1)) = step else {
                nap(POLL_MS);
                continue;
            };
            match step {
                Ok(Action::Send(len)) => {
                    send_eapol(card, &mut peer, &out[..len])?;
                    say(if a1 { "eapol: message 1 answered (a1 hit)" } else { "eapol: message 1 answered (a1 MISS)" });
                }
                Ok(Action::Complete { len, tk, gtk }) => {
                    let seen = card.rpq_seen;
                    send_eapol(card, &mut peer, &out[..len])?;
                    // Message 4 goes out in the clear, so it has to be on the
                    // air before `JOIN4` installs the keys: queued but not yet
                    // sent, the chip would send it under the new pairwise key
                    // or not at all, and the access point resends message 3
                    // (measured on ryzen 2026-10-06, boot 16: twice).
                    if !wait_released(card, seen, 200) {
                        say("eapol: message 4 not released within 200 ms; installing keys anyway");
                    }
                    self.vars.tk = tk;
                    self.vars.gtk = gtk.key;
                    self.vars.gtk_idx = gtk.idx;
                    if !card.replay("join4 (keys)", script::JOIN4, &self.vars) {
                        return Err(JoinError::Timeout);
                    }
                    say_dec("eapol: message 3 answered, keys installed, group key id", u64::from(gtk.idx));
                    peer.keyed = true;
                    LINK.flush_transmit();
                    LINK.link_changed();
                    self.peer = Some(peer);
                    with_status(|s| {
                        s.link = Link::Connected;
                        s.error = JoinError::None;
                    });
                    return Ok(());
                }
                Ok(Action::Rekey { .. }) => say("eapol: group message before the pairwise keys; ignored"),
                Err(eapol::Drop::Mic) => {
                    mic_failures += 1;
                    say("eapol: MIC did not verify (wrong key?)");
                }
                Err(e) => say_dec("eapol: dropped, reason", e as u64),
            }
        }
        say("eapol: handshake timed out");
        Err(if mic_failures > 0 { JoinError::AuthFailed } else { JoinError::Timeout })
    }

    /// Forget the association. No deauthentication frame goes out (none is
    /// built yet): the access point times the station out. The card keeps the
    /// peer until the next join restarts it.
    fn disconnect(&mut self) {
        self.wanted = None;
        LINK.flush_transmit();
        if self.peer.is_some() {
            LINK.link_changed();
        }
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
            // No link: what the stack sends (DHCP discovers, mostly) has
            // nowhere to go; it retransmits once there is one.
            let mut sink = [0u8; FRAME_MAX];
            while LINK.next_transmit(&mut sink).is_some() {}
            return;
        };
        let mut out = [0u8; eapol::HDR_LEN + eapol::MAX_KEY_DATA];
        let mut step: Option<Result<Action, eapol::Drop>> = None;
        let mut gone: Option<u16> = None;
        let mut beacon_lost = false;
        let mut rssi: Option<i8> = None;
        let mut delivered = 0u32;
        let stats = &mut self.stats;
        card.poll_rx(|p: &rx::Packet<'_>| {
            if p.desc.pkt_type == rx::kind::C2H {
                match bcnfltr(p.body) {
                    Some((0, _)) => beacon_lost = true,
                    Some((_, dbm)) => rssi = Some(dbm),
                    None => {}
                }
                return;
            }
            if p.desc.pkt_type != rx::kind::WIFI || p.desc.crc32_err {
                return;
            }
            let f = p.body;
            if let Some(g) = Goodbye::parse(f).filter(|g| g.from == peer.bssid && g.to == MAC) {
                gone = Some(g.reason);
            } else if let Some(d) = Data::parse(f, true).filter(|d| d.bssid == peer.bssid) {
                stats.rx_data += 1;
                if d.protected && p.desc.hw_dec && !p.desc.icv_err && !fresh(&mut peer.replay, f, stats) {
                    // A replayed frame: counted, never looked at.
                } else if d.ethertype == sta::ETHERTYPE_EAPOL {
                    if step.is_none() {
                        step = Some(peer.supplicant.handle(d.payload, &mut out));
                    }
                } else if !d.protected {
                    stats.rx_clear += 1;
                } else if !p.desc.hw_dec || p.desc.icv_err {
                    // To the stack only what the card decrypted: a frame in
                    // the clear after the keys is not ours to trust.
                    stats.rx_undecrypted += 1;
                } else if deliver(&d.da, &d.sa, d.ethertype, d.payload, stats) {
                    delivered += 1;
                }
            } else if let Some(a) = Amsdu::parse(f, true).filter(|a| a.bssid == peer.bssid) {
                stats.rx_amsdu += 1;
                if !a.protected {
                    stats.rx_clear += 1;
                } else if !p.desc.hw_dec || p.desc.icv_err {
                    stats.rx_undecrypted += 1;
                } else if !fresh(&mut peer.replay, f, stats) {
                    // A replayed frame: counted, never looked at.
                } else {
                    for m in a.subframes() {
                        stats.rx_msdus += 1;
                        if m.ethertype != sta::ETHERTYPE_EAPOL
                            && deliver(&m.da, &m.sa, m.ethertype, m.payload, stats)
                        {
                            delivered += 1;
                        }
                    }
                }
            } else if f.len() >= 16 && f[10..16] == peer.bssid && f[0] & 0x0c == 0x08 {
                stats.rx_unparsed += 1;
            }
        });
        if delivered > 0 {
            crate::net::wake_netpoll();
        }
        // What the stack sent, out as protected data frames.
        let mut eth = [0u8; FRAME_MAX];
        for _ in 0..TX_PER_LAP {
            let Some(n) = LINK.next_transmit(&mut eth) else { break };
            if n < 14 {
                continue;
            }
            let mut da = [0u8; 6];
            da.copy_from_slice(&eth[..6]);
            let ethertype = u16::from_be_bytes([eth[12], eth[13]]);
            trace_tcp(stats, "tx", ethertype, &eth[14..n]);
            stats.tx += 1;
            if send_protected(card, peer, &da, ethertype, &eth[14..n]).is_err() {
                stats.tx_failed += 1;
            }
        }
        if now_us() >= self.next_report {
            self.next_report = now_us() + LINK_REPORT_US;
            link_report(card, &self.stats);
            self.stats.laps = 0;
            self.stats.max_gap_us = 0;
        }
        if let Some(dbm) = rssi {
            with_status(|s| s.signal = dbm);
        }
        if let Some(reason) = gone {
            say_dec("sent away by the access point, reason", u64::from(reason));
            link_report(card, &self.stats);
            self.link_lost();
            return;
        }
        if beacon_lost {
            say("beacon loss: the firmware stopped hearing the access point");
            link_report(card, &self.stats);
            self.link_lost();
            return;
        }
        if let Some(Ok(Action::Complete { len, .. })) = step {
            // Message 3 again: the access point did not get message 4. The
            // keys are the ones already installed; only the answer is resent.
            if send_eapol_clear(card, peer, &out[..len]).is_ok() {
                say("eapol: message 3 retransmitted; message 4 sent again, in the clear");
            }
        }
        if let Some(Ok(Action::Rekey { len, gtk })) = step {
            // Only the group half of `JOIN4` is replayed: the pairwise key
            // and the beacon filter stay as they are.
            if send_eapol(card, peer, &out[..len]).is_ok() {
                self.vars.gtk = gtk.key;
                self.vars.gtk_idx = gtk.idx;
                link_report(card, &self.stats);
                let _ = card.replay("join4 (group rekey)", script::JOIN4_GROUP, &self.vars);
                peer.replay.group_rekeyed();
                link_report(card, &self.stats);
            }
        }
    }
}

/// A decrypted protected frame's packet number is above the last on its key
/// and TID (and is remembered); a frame whose header cannot be read is not.
fn fresh(replay: &mut ccmp::Replay, frame: &[u8], stats: &mut LinkStats) -> bool {
    let ok = ccmp::header(frame, true).is_some_and(|rx| replay.accept(&rx));
    if !ok {
        stats.rx_replay += 1;
    }
    ok
}

/// One received frame to the stack as Ethernet.
fn deliver(da: &Bssid, sa: &Bssid, ethertype: u16, payload: &[u8], stats: &mut LinkStats) -> bool {
    let mut eth = [0u8; FRAME_MAX];
    let n = 14 + payload.len();
    if n > FRAME_MAX {
        return false;
    }
    eth[..6].copy_from_slice(da);
    eth[6..12].copy_from_slice(sa);
    eth[12..14].copy_from_slice(&ethertype.to_be_bytes());
    eth[14..n].copy_from_slice(payload);
    trace_tcp(stats, "rx", ethertype, payload);
    let ok = LINK.deliver(&eth[..n]);
    if ok {
        stats.delivered += 1;
    }
    ok
}

/// Count, and for the first [`TCP_TRACE_MAX`] log by port, the TCP segments
/// that open or reset a connection (`SYN`, `RST`): enough to see a handshake
/// go wrong without logging a single address.
fn trace_tcp(stats: &mut LinkStats, dir: &str, ethertype: u16, ip: &[u8]) {
    if ethertype != sta::ETHERTYPE_IPV4 || ip.len() < 20 || ip[9] != 6 {
        return;
    }
    let ihl = usize::from(ip[0] & 0xf) * 4;
    let Some(tcp) = ip.get(ihl..ihl + 14) else { return };
    let flags = tcp[13];
    let (syn, ack, rst) = (flags & 0x02 != 0, flags & 0x10 != 0, flags & 0x04 != 0);
    if !syn && !rst {
        return;
    }
    match (dir, syn, rst) {
        ("rx", true, _) => stats.syn_rx += 1,
        ("rx", _, true) => stats.rst_rx += 1,
        (_, true, _) => stats.syn_tx += 1,
        _ => stats.rst_tx += 1,
    }
    if stats.traced < TCP_TRACE_MAX {
        stats.traced += 1;
        serial::puts("[rtw] tcp ");
        serial::puts(dir);
        serial::puts(" ");
        serial::put_dec(u64::from(u16::from_be_bytes([tcp[0], tcp[1]])));
        serial::puts(" -> ");
        serial::put_dec(u64::from(u16::from_be_bytes([tcp[2], tcp[3]])));
        serial::puts(match (syn, ack, rst) {
            (true, true, _) => " SYN+ACK",
            (true, false, _) => " SYN",
            (_, _, true) => " RST",
            _ => "",
        });
        // The IP header, for a handshake that never completes: lengths,
        // both checksums, TTL, flags, the two addresses (LAN or public; no
        // MACs).
        let total = usize::from(u16::from_be_bytes([ip[2], ip[3]]));
        serial::puts(" | ip len ");
        serial::put_dec(total as u64);
        serial::puts(" of ");
        serial::put_dec(ip.len() as u64);
        serial::puts(" ttl ");
        serial::put_dec(u64::from(ip[8]));
        serial::puts(" df ");
        serial::put_dec(u64::from(ip[6] >> 6 & 1));
        serial::puts(" ipsum ");
        serial::puts(if ip_checksum_ok(&ip[..ihl]) { "ok" } else { "BAD" });
        serial::puts(" tcpsum ");
        serial::puts(match ip.get(..total) {
            Some(p) if tcp_checksum_ok(p, ihl) => "ok",
            Some(_) => "BAD",
            None => "short",
        });
        serial::puts(" ");
        for (k, b) in ip[12..20].iter().enumerate() {
            serial::put_dec(u64::from(*b));
            serial::puts(match k {
                3 => " > ",
                7 => "\n",
                _ => ".",
            });
        }
    }
}

/// The one's-complement sum of `b` as 16-bit big-endian words, folded.
fn csum(b: &[u8], mut acc: u32) -> u16 {
    for w in b.chunks(2) {
        acc += u32::from(u16::from_be_bytes([w[0], *w.get(1).unwrap_or(&0)]));
    }
    while acc > 0xffff {
        acc = (acc & 0xffff) + (acc >> 16);
    }
    acc as u16
}

fn ip_checksum_ok(hdr: &[u8]) -> bool {
    csum(hdr, 0) == 0xffff
}

/// TCP checksum over the pseudo-header and segment of the IPv4 packet `p`.
fn tcp_checksum_ok(p: &[u8], ihl: usize) -> bool {
    let seg = &p[ihl..];
    let mut acc = 0u32;
    for w in p[12..20].chunks(2) {
        acc += u32::from(u16::from_be_bytes([w[0], w[1]]));
    }
    acc += 6 + seg.len() as u32;
    csum(seg, acc) == 0xffff
}

/// The joined link's counters, one line.
fn link_report(card: &Card, s: &LinkStats) {
    let n = |label: &str, v: u32| {
        serial::puts(label);
        serial::put_dec(u64::from(v));
    };
    serial::puts("[rtw] link:");
    n(" rx data ", s.rx_data);
    n(" amsdu ", s.rx_amsdu);
    n("/", s.rx_msdus);
    n(" clear ", s.rx_clear);
    n(" undecrypted ", s.rx_undecrypted);
    n(" replayed ", s.rx_replay);
    n(" unparsed ", s.rx_unparsed);
    n(" -> stack ", s.delivered);
    n(" (dropped ", LINK.rx_dropped.load(Ordering::Relaxed));
    n("); tx ", s.tx);
    n(" failed ", s.tx_failed);
    n(" (stack dropped ", LINK.tx_dropped.load(Ordering::Relaxed));
    n("); released done ", card.rpq_status[0]);
    n(" retry-limit ", card.rpq_status[1]);
    n(" lifetime ", card.rpq_status[2]);
    n(" dropped ", card.rpq_status[3]);
    n("; tcp syn rx ", s.syn_rx);
    n(" tx ", s.syn_tx);
    n(" rst rx ", s.rst_rx);
    n(" tx ", s.rst_tx);
    n("; laps ", s.laps);
    serial::puts(" max gap ");
    serial::put_dec(s.max_gap_us / 1000);
    serial::puts(" ms\n");
}

/// A firmware event, if it is the beacon filter's report: `(type, rssi dBm)`
/// (type 0 = beacon loss, 1 = RSSI threshold crossed, 2 = notify).
fn bcnfltr(c2h: &[u8]) -> Option<(u8, i8)> {
    let w = |i: usize| c2h.get(i * 4..i * 4 + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
    let w0 = w(0)?;
    if ((w0 & 3) as u8, ((w0 >> 2) & 0x3f) as u8, ((w0 >> 8) & 0xff) as u8) != C2H_BCNFLTR {
        return None;
    }
    let w2 = w(2)?;
    let dbm = (i32::from((w2 >> 16) as u8) - 110).clamp(-128, 127) as i8;
    Some((((w2 >> 8) & 3) as u8, dbm))
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

/// Next software sequence number for a data frame. One counter for the whole
/// run, never restarted by a join: the access point keeps duplicate/reorder
/// state per station across a deauthentication (and across our reboots, the
/// MAC being fixed), so a counter that starts again at 0 gets every frame
/// dropped as old or duplicate while management frames (hardware counter)
/// still pass. The first value is random for the same reason.
fn next_seq() -> u16 {
    static NEXT: AtomicU32 = AtomicU32::new(u32::MAX);
    if NEXT.load(Ordering::Relaxed) == u32::MAX {
        let mut r = [0u8; 2];
        let seed = if crate::net::rng_fill_checked(&mut r) { u32::from(u16::from_le_bytes(r)) } else { (now_us() & 0xfff) as u32 };
        let _ = NEXT.compare_exchange(u32::MAX, seed & 0xfff, Ordering::Relaxed, Ordering::Relaxed);
    }
    (NEXT.fetch_add(1, Ordering::Relaxed) & 0xfff) as u16
}

/// A protected data frame to `da` through the access point: QoS, tid 0,
/// the **Protected** bit set and no CCMP header — the 8852C writes the header
/// from the packet number in the descriptor (`hw_sec_hdr`; the recording's
/// encrypted frames are `0x4188` with no header space) and encrypts with the
/// pairwise key in security-CAM entry 0.
fn send_protected(card: &mut Card, peer: &mut Peer, da: &Bssid, ethertype: u16, payload: &[u8]) -> Result<(), JoinError> {
    let mut f = [0u8; sta::QOS_HDR_LEN + 8 + FRAME_MAX];
    let seq = next_seq();
    let len = sta::data_frame(&mut f, &peer.bssid, &MAC, da, 0, seq, None, ethertype, payload)
        .ok_or(JoinError::Timeout)?;
    f[1] |= 0x40; // Protected
    peer.pn += 1;
    let sec = tx::Sec { cam_idx: 0, keyid: 0, pn: peer.pn };
    send(card, TX_DATA, &f[..len], tx::Desc::data(len as u16, 0, seq, sec))
}

/// An EAPOL frame to the access point: a QoS data frame of tid 7, in the
/// clear until the pairwise key is in, protected after.
fn send_eapol(card: &mut Card, peer: &mut Peer, body: &[u8]) -> Result<(), JoinError> {
    if peer.keyed {
        let bssid = peer.bssid;
        return send_protected(card, peer, &bssid, sta::ETHERTYPE_EAPOL, body);
    }
    send_eapol_clear(card, peer, body)
}

/// An EAPOL frame in the clear whatever the key state. A retransmitted message
/// 4 goes this way: an access point that never got the first one has not
/// installed the pairwise key (it does so on receiving message 4), so a
/// protected reply is unreadable to it and it gives up with a deauthentication.
fn send_eapol_clear(card: &mut Card, peer: &mut Peer, body: &[u8]) -> Result<(), JoinError> {
    let mut f = [0u8; sta::QOS_HDR_LEN + 8 + eapol::HDR_LEN + eapol::MAX_KEY_DATA];
    let seq = next_seq();
    let len = sta::data_frame(&mut f, &peer.bssid, &MAC, &peer.bssid, 7, seq, None, sta::ETHERTYPE_EAPOL, body)
        .ok_or(JoinError::Timeout)?;
    send(card, TX_EAPOL, &f[..len], tx::Desc::eapol(len as u16, 0).with_seq(seq))
}

/// Wait up to `ms` for a release report past `seen`: the chip has finished
/// with a frame queued since.
fn wait_released(card: &mut Card, seen: u32, ms: u64) -> bool {
    let deadline = now_us() + ms * 1000;
    while card.rpq_seen == seen {
        if now_us() >= deadline {
            return false;
        }
        // Received frames are dropped here; the access point resends what
        // matters.
        frames(card, |_| {});
        nap(1);
    }
    true
}

/// Send a management frame and wait for its answer, up to [`MGMT_TRIES`]
/// times. Logs each try: whether the chip took the frame, what it said became
/// of it, what was heard meanwhile, and whether the answer hit the card's own
/// address match.
fn mgmt_exchange<T>(
    card: &mut Card,
    what: &str,
    frame: &[u8],
    bssid: &Bssid,
    mut pick: impl FnMut(&[u8]) -> Option<T>,
) -> Result<Option<T>, JoinError> {
    for _ in 0..MGMT_TRIES {
        if STOP.load(Ordering::Acquire) {
            return Ok(None);
        }
        let seen = card.rpq_seen;
        send(card, TX_MGMT, frame, tx::Desc::mgmt(frame.len() as u16, 0))?;
        let before = card.tx_idx(TX_MGMT);
        let mut tally = Tally::default();
        let mut hit = None;
        let deadline = now_us() + MGMT_REPLY_MS * 1000;
        let got = loop {
            let mut got = None;
            frames_a1(card, |f, a1| {
                tally.count(f, bssid);
                if got.is_none() {
                    got = pick(f);
                    if got.is_some() {
                        hit = Some(a1);
                    }
                }
            });
            if got.is_some() || now_us() >= deadline || STOP.load(Ordering::Acquire) {
                break got;
            }
            nap(POLL_MS);
        };
        tx_report(card, what, before, seen, &tally, hit);
        if got.is_some() {
            return Ok(got);
        }
    }
    Ok(None)
}

/// What was heard while waiting for an answer: everything, and what came
/// from the access point (and of that, management frames addressed to us).
#[derive(Default)]
struct Tally {
    frames: u32,
    from_ap: u32,
    mgmt_to_us: u32,
    /// Frames with us as receiver, whoever sent them.
    to_us: u32,
    /// The access point's frames by their first frame-control byte (type and
    /// subtype; no addresses): up to 8 kinds, with counts.
    ap_kinds: [(u8, u16); 8],
}

impl Tally {
    fn count(&mut self, f: &[u8], bssid: &Bssid) {
        self.frames += 1;
        if f.len() >= 10 && f[4..10] == MAC {
            self.to_us += 1;
        }
        if f.len() >= 16 && f[10..16] == *bssid {
            self.from_ap += 1;
            if let Some(k) = self.ap_kinds.iter_mut().find(|k| k.1 == 0 || k.0 == f[0]) {
                k.0 = f[0];
                k.1 += 1;
            }
            if f[0] & 0x0c == 0 && f[4..10] == MAC {
                self.mgmt_to_us += 1;
            }
        }
    }
}

/// One line per sent frame: did the chip fetch it (its read index moved), what
/// the release reports said, any DMA error, and what was heard meanwhile.
fn tx_report(card: &mut Card, what: &str, before: u32, seen: u32, t: &Tally, hit: Option<bool>) {
    let after = card.tx_idx(TX_MGMT);
    let (isr, idct) = card.dma_errors();
    serial::puts("[rtw] ");
    serial::puts(what);
    serial::puts(" tx: idx 0x");
    serial::put_hex(u64::from(before));
    serial::puts(" -> 0x");
    serial::put_hex(u64::from(after));
    // The release report's record (`rtw89_pci_rpp_fmt`) at byte 20 of the
    // buffer: TX status in bits 15:13 — 0 done (acknowledged), 1 retry
    // limit, 2 lifetime, 3 dropped.
    serial::puts(", release reports +");
    serial::put_dec(u64::from(card.rpq_seen - seen));
    let rpp = u32::from_le_bytes([card.rpq_last[20], card.rpq_last[21], card.rpq_last[22], card.rpq_last[23]]);
    serial::puts(" status ");
    serial::puts(match (rpp >> 13) & 7 {
        0 => "done",
        1 => "retry-limit",
        2 => "lifetime",
        3 => "dropped",
        _ => "?",
    });
    serial::puts(", dmac_err 0x");
    serial::put_hex(u64::from(isr));
    serial::puts(" idct 0x");
    serial::put_hex(u64::from(idct));
    serial::puts("; rx total ");
    serial::put_dec(u64::from(card.rx_types.iter().sum::<u32>()));
    serial::puts(" bad ");
    serial::put_dec(u64::from(card.rx_bad));
    serial::puts(" crc ");
    serial::put_dec(u64::from(card.rx_crc));
    serial::puts(" idx 0x");
    let i = card.rx_idx();
    serial::put_hex(u64::from(i));
    serial::puts("; heard ");
    serial::put_dec(u64::from(t.frames));
    serial::puts(", from ap ");
    serial::put_dec(u64::from(t.from_ap));
    serial::puts(", mgmt to us ");
    serial::put_dec(u64::from(t.mgmt_to_us));
    serial::puts(", to us ");
    serial::put_dec(u64::from(t.to_us));
    serial::puts(match hit {
        None => "; no answer",
        Some(true) => "; answer a1 hit",
        Some(false) => "; answer a1 MISS",
    });
    serial::puts("; ap fc");
    for &(fc, n) in t.ap_kinds.iter().filter(|k| k.1 > 0) {
        serial::puts(" ");
        serial::put_hexn(u64::from(fc), 2);
        serial::puts("x");
        serial::put_dec(u64::from(n));
    }
    serial::puts("\n");
}
