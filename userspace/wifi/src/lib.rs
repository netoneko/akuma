//! Library half of `wifi`, the pure parts — host-testable without `libakuma`:
//!
//! ```text
//! cargo test -p wifi --lib --no-default-features --target $(rustc -vV | grep '^host:' | cut -d' ' -f2)
//! ```
//!
//! | | |
//! |---|---|
//! | [`Network`] / [`parse_config`] / [`Network::to_config`] | one `/etc/wifi/<network>` file |
//! | [`derive_psk`] | WPA2's PBKDF2-SHA1(passphrase, ssid, 4096, 32) — done here so the kernel never has to |
//! | [`choose`] | which known network to join, given a scan |
//! | [`valid_name`] / [`printable_ssid`] | file names and display |
//!
//! The design is `proposals/AKUMA_WIFI_CONTROL.md`; the device protocol is
//! `akuma-wifi`, shared with the kernel.

#![no_std]

extern crate alloc;

use alloc::string::String;

use akuma_wifi::status::Bss;
use akuma_wifi::{Bssid, PSK_LEN, Security, Ssid, hex};

/// Where known networks live, one file each.
pub const CONFIG_DIR: &str = "/etc/wifi";

/// One known network: the contents of `/etc/wifi/<name>`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Network {
    /// The file name — a label, not the SSID.
    pub name: String,
    pub ssid: Ssid,
    /// `None` = an open network.
    pub psk: Option<[u8; PSK_LEN]>,
    pub priority: i32,
    pub autoconnect: bool,
    pub bssid: Option<Bssid>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigError {
    /// No `ssid =` line.
    NoSsid,
    BadSsid,
    /// `psk` not 64 hex digits.
    BadPsk,
    /// `passphrase` not 8..=63 printable ASCII characters (802.11i's rule).
    BadPassphrase,
    /// Both `psk` and `passphrase`.
    TwoKeys,
    BadPriority,
    BadAutoconnect,
    BadBssid,
    UnknownKey,
}

/// WPA2-Personal's key: PBKDF2-HMAC-SHA1(passphrase, ssid, 4096 rounds, 32 bytes).
#[must_use]
pub fn derive_psk(passphrase: &[u8], ssid: &[u8]) -> [u8; PSK_LEN] {
    let mut psk = [0u8; PSK_LEN];
    pbkdf2::pbkdf2_hmac::<sha1::Sha1>(passphrase, ssid, 4096, &mut psk);
    psk
}

/// 802.11i: a passphrase is 8..=63 printable ASCII characters.
#[must_use]
pub fn valid_passphrase(p: &[u8]) -> bool {
    (8..=63).contains(&p.len()) && p.iter().all(|c| (0x20..=0x7e).contains(c))
}

/// Parse `/etc/wifi/<name>`: `key = value` lines, `#` comments. A
/// `passphrase` is turned into the PSK here and not kept.
pub fn parse_config(name: &str, text: &str) -> Result<Network, ConfigError> {
    let mut ssid = None;
    let mut psk = None;
    let mut passphrase: Option<&str> = None;
    let mut net = Network {
        name: String::from(name),
        ssid: Ssid::EMPTY,
        psk: None,
        priority: 0,
        autoconnect: true,
        bssid: None,
    };
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else { return Err(ConfigError::UnknownKey) };
        let (k, v) = (k.trim(), v.trim());
        match k {
            // The SSID as written: an SSID may contain spaces and `#`, so the
            // whole rest of the line counts (only surrounding blanks trimmed).
            "ssid" => ssid = Some(Ssid::new(v.as_bytes()).filter(|s| !s.is_empty()).ok_or(ConfigError::BadSsid)?),
            // Any SSID, including bytes that are not text or blanks at either
            // end, which `ssid =` cannot carry. `to_config` picks this form
            // exactly when the plain one would not round-trip.
            "ssid_hex" => {
                let mut raw = [0u8; akuma_wifi::SSID_MAX];
                if v.len() > 2 * akuma_wifi::SSID_MAX {
                    return Err(ConfigError::BadSsid);
                }
                let n = hex::decode(v.as_bytes(), &mut raw).ok_or(ConfigError::BadSsid)?;
                ssid = Some(Ssid::new(&raw[..n]).filter(|s| !s.is_empty()).ok_or(ConfigError::BadSsid)?);
            }
            "psk" => {
                let mut k = [0u8; PSK_LEN];
                if v.len() != 2 * PSK_LEN || hex::decode(v.as_bytes(), &mut k) != Some(PSK_LEN) {
                    return Err(ConfigError::BadPsk);
                }
                psk = Some(k);
            }
            "passphrase" => {
                if !valid_passphrase(v.as_bytes()) {
                    return Err(ConfigError::BadPassphrase);
                }
                passphrase = Some(v);
            }
            "priority" => net.priority = v.parse().map_err(|_| ConfigError::BadPriority)?,
            "autoconnect" => {
                net.autoconnect = match v {
                    "true" | "yes" | "1" => true,
                    "false" | "no" | "0" => false,
                    _ => return Err(ConfigError::BadAutoconnect),
                }
            }
            "bssid" => net.bssid = Some(hex::decode_mac(v.as_bytes()).ok_or(ConfigError::BadBssid)?),
            _ => return Err(ConfigError::UnknownKey),
        }
    }
    net.ssid = ssid.ok_or(ConfigError::NoSsid)?;
    net.psk = match (psk, passphrase) {
        (Some(_), Some(_)) => return Err(ConfigError::TwoKeys),
        (Some(k), None) => Some(k),
        (None, Some(p)) => Some(derive_psk(p.as_bytes(), net.ssid.as_bytes())),
        (None, None) => None,
    };
    Ok(net)
}

impl Network {
    /// The file contents: always the PSK, never a passphrase.
    #[must_use]
    pub fn to_config(&self) -> String {
        let mut s = String::from("# /etc/wifi/");
        s.push_str(&self.name);
        s.push_str(" — written by `wifi`. root-only (0600): the psk is the network's key.\n");
        match core::str::from_utf8(self.ssid.as_bytes()) {
            Ok(t) if t.trim() == t && !t.chars().any(char::is_control) => {
                s.push_str("ssid = ");
                s.push_str(t);
            }
            _ => {
                let mut h = [0u8; 2 * akuma_wifi::SSID_MAX];
                let n = hex::encode(self.ssid.as_bytes(), &mut h).unwrap_or(0);
                s.push_str("ssid_hex = ");
                s.push_str(core::str::from_utf8(&h[..n]).unwrap_or(""));
            }
        }
        s.push('\n');
        if let Some(k) = self.psk {
            let mut h = [0u8; 2 * PSK_LEN];
            if let Some(n) = hex::encode(&k, &mut h) {
                s.push_str("psk = ");
                s.push_str(core::str::from_utf8(&h[..n]).unwrap_or(""));
                s.push('\n');
            }
        }
        s.push_str("priority = ");
        s.push_str(&alloc::format!("{}", self.priority));
        s.push_str("\nautoconnect = ");
        s.push_str(if self.autoconnect { "true" } else { "false" });
        s.push('\n');
        if let Some(b) = self.bssid {
            let mut m = [0u8; 17];
            if hex::encode_mac(&b, &mut m).is_some() {
                s.push_str("bssid = ");
                s.push_str(core::str::from_utf8(&m).unwrap_or(""));
                s.push('\n');
            }
        }
        s
    }

    /// Can this network be joined over `b`, security-wise?
    #[must_use]
    pub fn can_join(&self, b: &Bss) -> bool {
        b.ssid == self.ssid
            && self.bssid.is_none_or(|want| want == b.bssid)
            && matches!((b.security, self.psk), (Security::Open, None) | (Security::Wpa2Psk, Some(_)))
    }
}

/// Which known network to join: `autoconnect`, in the scan, joinable; then
/// highest `priority`, then strongest signal.
#[must_use]
pub fn choose<'a>(known: &'a [Network], scan: &[Bss]) -> Option<(&'a Network, Bss)> {
    let mut best: Option<(&Network, Bss)> = None;
    for n in known.iter().filter(|n| n.autoconnect) {
        for b in scan.iter().filter(|b| n.can_join(b)) {
            let better = match &best {
                None => true,
                Some((bn, bb)) => (n.priority, b.signal) > (bn.priority, bb.signal),
            };
            if better {
                best = Some((n, *b));
            }
        }
    }
    best
}

/// A safe `/etc/wifi` file name: 1..=64 of `[A-Za-z0-9._-]`, not starting with
/// a dot.
#[must_use]
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !name.starts_with('.')
        && name.bytes().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'))
}

/// An SSID for a terminal: printable ASCII as is, anything else `\xNN`.
#[must_use]
pub fn printable_ssid(s: &Ssid) -> String {
    let mut out = String::new();
    for &b in s.as_bytes() {
        if (0x20..=0x7e).contains(&b) && b != b'\\' {
            out.push(b as char);
        } else {
            out.push_str(&alloc::format!("\\x{b:02x}"));
        }
    }
    out
}

#[cfg(test)]
mod tests;
