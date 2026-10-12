//! A simulated radio for `/dev/wifi0` (`wifisim` on the kernel command line).
//!
//! It makes the tool, the device and the protocol testable end to end — in
//! QEMU, on any machine — before a real driver exists. Deterministic on purpose: the same networks every boot, scans complete
//! immediately, and `connect` succeeds or fails by fixed rules. A test that
//! passes here fails later only because the *real* radio differs.
//!
//! | network | security | chan | signal | joins with |
//! |---|---|---|---|---|
//! | `akuma-sim-open` | open | 1 | -40 | `-` |
//! | `akuma-sim-wpa2` | WPA2-PSK | 6 | -55 | [`SIM_WPA2_PSK`] — PBKDF2 of passphrase `akuma-sim-passphrase` |
//! | `akuma-sim-far` | WPA2-PSK | 11 | -82 | any key: the weak one, for selection tests |
//! | `sim☃` (non-ASCII) | open | 36 | -70 | `-`: proves SSIDs are bytes |
//! | `akuma-sim-sae` | other (WPA3) | 44 | -50 | refused: `unsupported` |
//!
//! # The lossy mode (`wifisim=lossy`)
//!
//! The deterministic radio proves the protocol; it says nothing about how
//! the tool copes with a radio like the real one, whose scans miss networks,
//! whose joins take seconds and sometimes time out, and whose link drops and
//! comes back on its own (the kernel station rejoins by itself, reporting
//! `associating` meanwhile). [`SimRadio::new_lossy`] is that radio: the same
//! networks, with a seeded fault model on top —
//!
//! | fault | how often |
//! |---|---|
//! | a scan misses a network | 1 in 4, per network |
//! | a join sits in `associating` for 2–6 reads first | 1 in 3 |
//! | ...and then fails with `timeout` instead | 1 in 8 of those |
//! | a `connected` link drops to `associating` for 3–8 reads | 1 in 25, per read |
//! | ...and comes back `failed`/`timeout` instead of `connected` | 1 in 4 of those |
//! | the signal wanders | ±4 dBm, per read |
//!
//! "Per read" is per [`SimRadio::observe`], which the kernel calls once per
//! snapshot: a tool polling every 100 ms sees a drop every few seconds. The
//! sequence is fixed by the seed, so a `wifitest` transcript is comparable
//! between runs.

use crate::cmd::Command;
use crate::status::{Bss, Error, Link, Status};
use crate::{IfName, MAX_BSS, PSK_LEN, Security, Ssid};

/// PBKDF2-SHA1(`akuma-sim-passphrase`, `akuma-sim-wpa2`, 4096, 32). The tool's
/// host tests derive it independently and compare.
pub const SIM_WPA2_PSK: [u8; PSK_LEN] = [
    0xbf, 0xde, 0xc9, 0x42, 0xb3, 0x87, 0xbd, 0xb0, 0x37, 0x19, 0x9c, 0x71, 0xc3, 0xc9, 0xc5, 0x0d,
    0x6c, 0xbd, 0x2b, 0x50, 0xfe, 0x91, 0xf1, 0x0c, 0x4f, 0xb7, 0x61, 0xe1, 0x07, 0xfb, 0xa9, 0x38,
];

/// The simulated networks, in the order a scan reports them.
fn networks() -> [(Bss, Option<[u8; PSK_LEN]>, bool); 5] {
    let bss = |ssid: &[u8], last: u8, chan, signal, security| Bss {
        ssid: Ssid::new(ssid).unwrap_or(Ssid::EMPTY),
        bssid: [0x02, 0, 0, 0, 0, last],
        chan,
        signal,
        security,
    };
    // (network, the one key it accepts, accepts any key)
    [
        (bss(b"akuma-sim-open", 1, 1, -40, Security::Open), None, false),
        (bss(b"akuma-sim-wpa2", 2, 6, -55, Security::Wpa2Psk), Some(SIM_WPA2_PSK), false),
        (bss(b"akuma-sim-far", 3, 11, -82, Security::Wpa2Psk), None, true),
        (bss("sim\u{2603}".as_bytes(), 4, 36, -70, Security::Open), None, false),
        (bss(b"akuma-sim-sae", 5, 44, -50, Security::Other), None, false),
    ]
}

/// What a transient state resolves to once its reads are up.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Then {
    Connected,
    Failed,
}

/// The lossy mode's fault model: a seeded xorshift and the transient state
/// in progress.
#[derive(Clone, Copy, Debug)]
struct Fault {
    rng: u64,
    /// Reads left in the current transient `associating`; 0 = none.
    pending: u8,
    then: Then,
    /// The network the transient state is about: what `connected` restores.
    bss: Bss,
    /// Faults injected so far, by kind: scan misses, slow joins, failed
    /// joins, link drops, drops that ended `failed`.
    pub counts: [u32; 5],
}

impl Fault {
    fn next(&mut self) -> u64 {
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng = x;
        x
    }

    /// True once in `n`.
    fn one_in(&mut self, n: u64) -> bool {
        self.next().is_multiple_of(n)
    }

    fn between(&mut self, lo: u8, hi: u8) -> u8 {
        lo + (self.next() % u64::from(hi - lo + 1)) as u8
    }
}

/// The simulated radio's whole state.
pub struct SimRadio {
    status: Status,
    fault: Option<Fault>,
}

impl SimRadio {
    #[must_use]
    pub fn new(iface: IfName) -> Self {
        let mut status = Status::no_radio(iface);
        status.radio = "sim";
        status.link = Link::Down;
        Self { status, fault: None }
    }

    /// The lossy radio (module docs); `seed` fixes its sequence of faults.
    #[must_use]
    pub fn new_lossy(iface: IfName, seed: u64) -> Self {
        let mut r = Self::new(iface);
        r.fault = Some(Fault { rng: seed | 1, pending: 0, then: Then::Connected, bss: Bss::EMPTY, counts: [0; 5] });
        r
    }

    #[must_use]
    pub const fn status(&self) -> &Status {
        &self.status
    }

    /// Faults injected so far — scan misses, slow joins, failed joins, link
    /// drops, drops that ended `failed` — all zero for the deterministic radio.
    #[must_use]
    pub fn faults(&self) -> [u32; 5] {
        self.fault.map_or([0; 5], |f| f.counts)
    }

    /// The status, as one read of the device sees it: in the lossy mode,
    /// time passes here — a pending join resolves, a joined link may drop or
    /// come back, the signal wanders.
    pub fn observe(&mut self) -> &Status {
        let Some(mut f) = self.fault else { return &self.status };
        let s = &mut self.status;
        if f.pending > 0 {
            f.pending -= 1;
            if f.pending == 0 {
                match f.then {
                    Then::Connected => Self::set_connected(s, &f.bss),
                    Then::Failed => Self::set_failed(s, f.bss.ssid, Error::Timeout),
                }
            }
        } else if s.link == Link::Connected {
            if f.one_in(25) {
                // The link drops; the kernel station rejoins by itself.
                f.bss = Bss { ssid: s.ssid, bssid: s.bssid, chan: s.chan, signal: s.signal, security: s.security };
                f.pending = f.between(3, 8);
                f.then = if f.one_in(4) { Then::Failed } else { Then::Connected };
                f.counts[3] += 1;
                if f.then == Then::Failed {
                    f.counts[4] += 1;
                }
                s.link = Link::Associating;
                s.bssid = [0; 6];
                s.error = Error::None;
            } else {
                let base = networks().into_iter().find(|(b, _, _)| b.bssid == s.bssid).map_or(s.signal, |(b, _, _)| b.signal);
                s.signal = base.saturating_add(f.between(0, 8).cast_signed() - 4);
            }
        }
        self.fault = Some(f);
        &self.status
    }

    fn set_connected(s: &mut Status, b: &Bss) {
        s.link = Link::Connected;
        s.ssid = b.ssid;
        s.bssid = b.bssid;
        s.chan = b.chan;
        s.signal = b.signal;
        s.security = b.security;
        s.error = Error::None;
    }

    fn set_failed(s: &mut Status, ssid: Ssid, e: Error) {
        s.link = Link::Failed;
        s.ssid = ssid;
        s.bssid = [0; 6];
        s.chan = 0;
        s.signal = 0;
        s.error = e;
    }

    /// Carry out `cmd`. The caller has already checked it names this radio's
    /// interface.
    pub fn apply(&mut self, cmd: &Command) {
        let s = &mut self.status;
        let mut fault = self.fault;
        match cmd {
            Command::Scan { .. } => {
                s.nbss = 0;
                for (b, _, _) in networks().into_iter().take(MAX_BSS) {
                    if let Some(f) = fault.as_mut()
                        && f.one_in(4)
                    {
                        f.counts[0] += 1;
                        continue;
                    }
                    s.bss[s.nbss] = b;
                    s.nbss += 1;
                }
                s.scans += 1;
            }
            Command::Disconnect { .. } => {
                s.link = Link::Down;
                s.ssid = Ssid::EMPTY;
                s.bssid = [0; 6];
                s.chan = 0;
                s.signal = 0;
                s.security = Security::Open;
                s.error = Error::None;
            }
            Command::Connect { ssid, psk, bssid, .. } => {
                let found = networks().into_iter().find(|(b, _, _)| {
                    b.ssid == *ssid && bssid.is_none_or(|want| want == b.bssid)
                });
                let outcome = match found {
                    None => Err(Error::NotFound),
                    Some((b, want, any)) => match (b.security, psk) {
                        (Security::Other, _) => Err(Error::Unsupported),
                        (Security::Open, None) => Ok(b),
                        (Security::Open, Some(_)) | (Security::Wpa2Psk, None) => Err(Error::AuthFailed),
                        (Security::Wpa2Psk, Some(k)) if any || want == Some(*k) => Ok(b),
                        (Security::Wpa2Psk, Some(_)) => Err(Error::AuthFailed),
                    },
                };
                match outcome {
                    Ok(b) => {
                        // A new join cancels a pending transient.
                        if let Some(f) = fault.as_mut() {
                            f.pending = 0;
                        }
                        if let Some(f) = fault.as_mut()
                            && f.one_in(3)
                        {
                            // A slow join: `associating` for a few reads, then
                            // joined — or timed out, as the kernel reports a
                            // join whose access point stopped answering.
                            f.bss = b;
                            f.pending = f.between(2, 6);
                            f.then = if f.one_in(8) { Then::Failed } else { Then::Connected };
                            f.counts[1] += 1;
                            if f.then == Then::Failed {
                                f.counts[2] += 1;
                            }
                            s.link = Link::Associating;
                            s.ssid = b.ssid;
                            s.bssid = [0; 6];
                            s.chan = b.chan;
                            s.security = b.security;
                            s.error = Error::None;
                        } else {
                            Self::set_connected(s, &b);
                        }
                    }
                    Err(e) => {
                        if let Some(f) = fault.as_mut() {
                            f.pending = 0;
                        }
                        Self::set_failed(s, *ssid, e);
                    }
                }
            }
        }
        if let (Command::Disconnect { .. }, Some(f)) = (cmd, fault.as_mut()) {
            f.pending = 0;
        }
        self.fault = fault;
    }
}
