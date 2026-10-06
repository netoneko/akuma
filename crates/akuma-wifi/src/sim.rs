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

/// The simulated radio's whole state.
pub struct SimRadio {
    status: Status,
}

impl SimRadio {
    #[must_use]
    pub fn new(iface: IfName) -> Self {
        let mut status = Status::no_radio(iface);
        status.radio = "sim";
        status.link = Link::Down;
        Self { status }
    }

    #[must_use]
    pub const fn status(&self) -> &Status {
        &self.status
    }

    /// Carry out `cmd`. The caller has already checked it names this radio's
    /// interface.
    pub fn apply(&mut self, cmd: &Command) {
        let s = &mut self.status;
        match cmd {
            Command::Scan { .. } => {
                for (i, (b, _, _)) in networks().into_iter().enumerate().take(MAX_BSS) {
                    s.bss[i] = b;
                    s.nbss = i + 1;
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
                        s.link = Link::Connected;
                        s.ssid = b.ssid;
                        s.bssid = b.bssid;
                        s.chan = b.chan;
                        s.signal = b.signal;
                        s.security = b.security;
                        s.error = Error::None;
                    }
                    Err(e) => {
                        s.link = Link::Failed;
                        s.ssid = *ssid;
                        s.bssid = [0; 6];
                        s.chan = 0;
                        s.signal = 0;
                        s.error = e;
                    }
                }
            }
        }
    }
}
