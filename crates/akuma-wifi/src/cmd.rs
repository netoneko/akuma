//! What userspace writes to `/dev/wifi0`: one command per line.
//!
//! ```text
//! scan <if>
//! connect <if> <ssid-hex> <psk-hex | -> [<bssid aa:bb:cc:dd:ee:ff>]
//! disconnect <if>
//! ```
//!
//! `-` for the key joins an open network. SSIDs and keys are hex because an
//! SSID is up to 32 arbitrary bytes, not a string. The key is the 32-byte WPA2
//! PSK, never the passphrase: the kernel does no PBKDF2.

use crate::{Bssid, IfName, PSK_LEN, Ssid, hex};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Command {
    Scan { iface: IfName },
    Connect { iface: IfName, ssid: Ssid, psk: Option<[u8; PSK_LEN]>, bssid: Option<Bssid> },
    Disconnect { iface: IfName },
}

impl Command {
    #[must_use]
    pub const fn iface(&self) -> &IfName {
        match self {
            Self::Scan { iface } | Self::Connect { iface, .. } | Self::Disconnect { iface } => iface,
        }
    }
}

/// Why a line was refused. Each maps to an errno in the kernel.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CmdError {
    Empty,
    UnknownVerb,
    MissingArgument,
    TooManyArguments,
    BadInterface,
    BadSsid,
    /// Not exactly 64 hex digits (or `-`).
    BadKey,
    BadBssid,
}

/// Parse one line (a trailing `\n` / `\r\n` is allowed).
pub fn parse(line: &[u8]) -> Result<Command, CmdError> {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    let mut words = line.split(|&c| c == b' ' || c == b'\t').filter(|w| !w.is_empty());
    let verb = words.next().ok_or(CmdError::Empty)?;
    let iface = |w: Option<&[u8]>| IfName::new(w.ok_or(CmdError::MissingArgument)?).ok_or(CmdError::BadInterface);
    let cmd = match verb {
        b"scan" => Command::Scan { iface: iface(words.next())? },
        b"disconnect" => Command::Disconnect { iface: iface(words.next())? },
        b"connect" => {
            let iface = iface(words.next())?;
            let ssid_hex = words.next().ok_or(CmdError::MissingArgument)?;
            let mut raw = [0u8; crate::SSID_MAX];
            if ssid_hex.is_empty() || ssid_hex.len() > 2 * crate::SSID_MAX {
                return Err(CmdError::BadSsid);
            }
            let n = hex::decode(ssid_hex, &mut raw).ok_or(CmdError::BadSsid)?;
            let ssid = Ssid::new(&raw[..n]).ok_or(CmdError::BadSsid)?;
            let key = words.next().ok_or(CmdError::MissingArgument)?;
            let psk = if key == b"-" {
                None
            } else {
                let mut k = [0u8; PSK_LEN];
                if key.len() != 2 * PSK_LEN || hex::decode(key, &mut k) != Some(PSK_LEN) {
                    return Err(CmdError::BadKey);
                }
                Some(k)
            };
            let bssid = match words.next() {
                Some(b) => Some(hex::decode_mac(b).ok_or(CmdError::BadBssid)?),
                None => None,
            };
            Command::Connect { iface, ssid, psk, bssid }
        }
        _ => return Err(CmdError::UnknownVerb),
    };
    if words.next().is_some() {
        return Err(CmdError::TooManyArguments);
    }
    Ok(cmd)
}

/// Render `cmd` as the line [`parse`] reads (with its `\n`), into `out`;
/// returns its length. The tool's half of the grammar.
pub fn write(cmd: &Command, out: &mut [u8]) -> Option<usize> {
    let mut w = crate::status::Writer::new(out);
    match cmd {
        Command::Scan { iface } => {
            w.str("scan ")?;
            w.str(iface.as_str())?;
        }
        Command::Disconnect { iface } => {
            w.str("disconnect ")?;
            w.str(iface.as_str())?;
        }
        Command::Connect { iface, ssid, psk, bssid } => {
            w.str("connect ")?;
            w.str(iface.as_str())?;
            w.str(" ")?;
            w.hex(ssid.as_bytes())?;
            w.str(" ")?;
            match psk {
                Some(k) => w.hex(k)?,
                None => w.str("-")?,
            }
            if let Some(b) = bssid {
                w.str(" ")?;
                w.mac(b)?;
            }
        }
    }
    w.str("\n")?;
    Some(w.len())
}
