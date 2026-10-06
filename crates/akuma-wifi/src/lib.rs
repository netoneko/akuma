//! The `/dev/wifi0` control protocol — the one boundary between userspace and
//! the kernel's wifi driver (`proposals/AKUMA_WIFI_CONTROL.md`).
//!
//! Akuma has no supplicant: the WPA2 handshake runs in the kernel, so userspace
//! only *chooses* a network and hands the driver one line — "join this network
//! with this key" — and reads back what the driver is doing. Both halves of
//! that live here, so the kernel (`amd64/src/wifi.rs`) and the tool
//! (`userspace/wifi`) cannot disagree on the grammar:
//!
//! | module | |
//! |---|---|
//! | [`hex`] | the encoding SSIDs, keys and BSSIDs travel in |
//! | [`cmd`] | what userspace writes: `scan`, `connect`, `disconnect` |
//! | [`status`] | what userspace reads: `key=value` lines, then one `bss` line per scan result — a writer for the kernel, a parser for the tool |
//! | [`sim`] | a deterministic simulated radio (`wifisim` on the kernel command line), so the tool and the device are testable before a real driver exists |
//!
//! `no_std`, no allocation, no dependencies: the kernel side runs on a path
//! that must not allocate, and the tool links it as an ordinary crate.

#![no_std]
#![forbid(unsafe_code)]

pub mod cmd;
pub mod hex;
pub mod sim;
pub mod status;

#[cfg(test)]
mod tests;

/// Longest SSID 802.11 allows.
pub const SSID_MAX: usize = 32;
/// A WPA2-Personal pre-shared key: PBKDF2-SHA1(passphrase, ssid, 4096, 32).
pub const PSK_LEN: usize = 32;
/// Scan results one status read carries.
pub const MAX_BSS: usize = 24;
/// Longest interface name (`IFNAMSIZ` - 1).
pub const IFNAME_MAX: usize = 15;

/// An SSID: up to 32 arbitrary bytes — not a string, which is why it travels
/// hex-encoded.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Ssid {
    len: u8,
    bytes: [u8; SSID_MAX],
}

impl Ssid {
    pub const EMPTY: Self = Self { len: 0, bytes: [0; SSID_MAX] };

    /// `None` if longer than 32 bytes.
    #[must_use]
    pub fn new(b: &[u8]) -> Option<Self> {
        if b.len() > SSID_MAX {
            return None;
        }
        let mut s = Self::EMPTY;
        s.bytes[..b.len()].copy_from_slice(b);
        s.len = b.len() as u8;
        Some(s)
    }

    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..usize::from(self.len)]
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl core::fmt::Debug for Ssid {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Ssid(")?;
        for &b in self.as_bytes() {
            if b.is_ascii_graphic() || b == b' ' {
                write!(f, "{}", b as char)?;
            } else {
                write!(f, "\\x{b:02x}")?;
            }
        }
        write!(f, ")")
    }
}

/// An interface name (`wlan0`), so a second radio — or a later socket
/// transport that addresses interfaces — does not change the grammar.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct IfName {
    len: u8,
    bytes: [u8; IFNAME_MAX],
}

impl IfName {
    /// ASCII alphanumerics only, 1..=15 bytes.
    #[must_use]
    pub fn new(s: &[u8]) -> Option<Self> {
        if s.is_empty() || s.len() > IFNAME_MAX || !s.iter().all(u8::is_ascii_alphanumeric) {
            return None;
        }
        let mut n = Self { len: s.len() as u8, bytes: [0; IFNAME_MAX] };
        n.bytes[..s.len()].copy_from_slice(s);
        Some(n)
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        core::str::from_utf8(&self.bytes[..usize::from(self.len)]).unwrap_or("?")
    }
}

/// A BSSID (the access point's MAC address).
pub type Bssid = [u8; 6];

/// Link security, as advertised and as joined.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Security {
    Open,
    Wpa2Psk,
    /// Anything else (WEP, WPA3-SAE, enterprise): seen, not joinable.
    Other,
}

impl Security {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Wpa2Psk => "wpa2-psk",
            Self::Other => "other",
        }
    }

    #[must_use]
    pub fn from_name(s: &str) -> Option<Self> {
        match s {
            "open" => Some(Self::Open),
            "wpa2-psk" => Some(Self::Wpa2Psk),
            "other" => Some(Self::Other),
            _ => None,
        }
    }
}
