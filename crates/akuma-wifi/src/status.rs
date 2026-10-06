//! What userspace reads from `/dev/wifi0`: the driver's state as `key=value`
//! lines, then one `bss` line per scan result.
//!
//! ```text
//! iface=wlan0
//! radio=sim
//! state=connected
//! ssid=616b756d612d73696d2d77706132
//! bssid=02:00:00:00:00:02
//! chan=6
//! signal=-55
//! security=wpa2-psk
//! error=none
//! scans=1
//! bss ssid=616b756d612d73696d2d6f70656e bssid=02:00:00:00:00:01 chan=1 signal=-40 security=open
//! ```
//!
//! A reader ignores keys it does not know, so the kernel can add some.

use crate::{Bssid, IfName, MAX_BSS, Security, Ssid, hex};

/// The link, as the driver sees it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Link {
    /// No radio behind this device (no driver, or none bound yet).
    NoRadio,
    /// Radio up, not associated, nothing in progress.
    Down,
    Scanning,
    Associating,
    Connected,
    /// The last `connect` failed; [`Status::error`] says why.
    Failed,
}

impl Link {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::NoRadio => "no-radio",
            Self::Down => "down",
            Self::Scanning => "scanning",
            Self::Associating => "associating",
            Self::Connected => "connected",
            Self::Failed => "failed",
        }
    }

    #[must_use]
    pub fn from_name(s: &str) -> Option<Self> {
        [Self::NoRadio, Self::Down, Self::Scanning, Self::Associating, Self::Connected, Self::Failed]
            .into_iter()
            .find(|l| l.name() == s)
    }
}

/// Why the last `connect` failed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Error {
    None,
    /// No such network in range.
    NotFound,
    /// The key was wrong (the 4-way handshake failed).
    AuthFailed,
    /// The network's security is not one this driver joins.
    Unsupported,
    Timeout,
}

impl Error {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::NotFound => "not-found",
            Self::AuthFailed => "auth-failed",
            Self::Unsupported => "unsupported",
            Self::Timeout => "timeout",
        }
    }

    #[must_use]
    pub fn from_name(s: &str) -> Option<Self> {
        [Self::None, Self::NotFound, Self::AuthFailed, Self::Unsupported, Self::Timeout]
            .into_iter()
            .find(|e| e.name() == s)
    }
}

/// One scan result.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Bss {
    pub ssid: Ssid,
    pub bssid: Bssid,
    pub chan: u16,
    /// dBm.
    pub signal: i8,
    pub security: Security,
}

impl Bss {
    pub const EMPTY: Self =
        Self { ssid: Ssid::EMPTY, bssid: [0; 6], chan: 0, signal: 0, security: Security::Open };
}

/// Everything one read of `/dev/wifi0` reports.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Status {
    pub iface: IfName,
    /// `none`, `sim`, `rtw89`, …
    pub radio: &'static str,
    pub link: Link,
    /// The joined (or being-joined) network; empty when down.
    pub ssid: Ssid,
    pub bssid: Bssid,
    pub chan: u16,
    pub signal: i8,
    pub security: Security,
    pub error: Error,
    /// Completed scans since boot: a reader that asked for a scan waits for
    /// this to move.
    pub scans: u32,
    pub bss: [Bss; MAX_BSS],
    pub nbss: usize,
}

impl Status {
    /// A device with no radio behind it.
    #[must_use]
    pub fn no_radio(iface: IfName) -> Self {
        Self {
            iface,
            radio: "none",
            link: Link::NoRadio,
            ssid: Ssid::EMPTY,
            bssid: [0; 6],
            chan: 0,
            signal: 0,
            security: Security::Open,
            error: Error::None,
            scans: 0,
            bss: [Bss::EMPTY; MAX_BSS],
            nbss: 0,
        }
    }

    #[must_use]
    pub fn results(&self) -> &[Bss] {
        &self.bss[..self.nbss.min(MAX_BSS)]
    }

    /// Render as text into `out`; `None` if it does not fit. 4 KiB always
    /// does: a full table is ~24 * 120 bytes.
    pub fn write(&self, out: &mut [u8]) -> Option<usize> {
        let mut w = Writer::new(out);
        w.kv_str("iface", self.iface.as_str())?;
        w.kv_str("radio", self.radio)?;
        w.kv_str("state", self.link.name())?;
        w.str("ssid=")?;
        w.hex(self.ssid.as_bytes())?;
        w.str("\nbssid=")?;
        w.mac(&self.bssid)?;
        w.str("\n")?;
        w.kv_num("chan", i64::from(self.chan))?;
        w.kv_num("signal", i64::from(self.signal))?;
        w.kv_str("security", self.security.name())?;
        w.kv_str("error", self.error.name())?;
        w.kv_num("scans", i64::from(self.scans))?;
        for b in self.results() {
            w.str("bss ssid=")?;
            w.hex(b.ssid.as_bytes())?;
            w.str(" bssid=")?;
            w.mac(&b.bssid)?;
            w.str(" chan=")?;
            w.num(i64::from(b.chan))?;
            w.str(" signal=")?;
            w.num(i64::from(b.signal))?;
            w.str(" security=")?;
            w.str(b.security.name())?;
            w.str("\n")?;
        }
        Some(w.len())
    }

    /// Parse what [`Status::write`] produced. Unknown keys and unknown line
    /// kinds are skipped; `radio` is kept as one of the known names (else
    /// `"other"`), since the type holds a `&'static str`.
    #[must_use]
    pub fn parse(text: &[u8]) -> Option<Self> {
        let mut s = Self::no_radio(IfName::new(b"wlan0")?);
        let mut saw_state = false;
        for line in text.split(|&c| c == b'\n') {
            if let Some(rest) = line.strip_prefix(b"bss ") {
                if s.nbss < MAX_BSS {
                    s.bss[s.nbss] = parse_bss(rest)?;
                    s.nbss += 1;
                }
                continue;
            }
            let Some(eq) = line.iter().position(|&c| c == b'=') else { continue };
            let (k, v) = (&line[..eq], &line[eq + 1..]);
            let vs = core::str::from_utf8(v).ok()?;
            match k {
                b"iface" => s.iface = IfName::new(v)?,
                b"radio" => s.radio = ["none", "sim", "rtw89"].into_iter().find(|r| r.as_bytes() == v).unwrap_or("other"),
                b"state" => {
                    s.link = Link::from_name(vs)?;
                    saw_state = true;
                }
                b"ssid" => s.ssid = parse_ssid(v)?,
                b"bssid" => s.bssid = hex::decode_mac(v)?,
                b"chan" => s.chan = vs.parse().ok()?,
                b"signal" => s.signal = vs.parse().ok()?,
                b"security" => s.security = Security::from_name(vs)?,
                b"error" => s.error = Error::from_name(vs)?,
                b"scans" => s.scans = vs.parse().ok()?,
                _ => {}
            }
        }
        saw_state.then_some(s)
    }
}

fn parse_ssid(v: &[u8]) -> Option<Ssid> {
    let mut raw = [0u8; crate::SSID_MAX];
    if v.len() > 2 * crate::SSID_MAX {
        return None;
    }
    let n = hex::decode(v, &mut raw)?;
    Ssid::new(&raw[..n])
}

fn parse_bss(rest: &[u8]) -> Option<Bss> {
    let mut b = Bss::EMPTY;
    for field in rest.split(|&c| c == b' ').filter(|f| !f.is_empty()) {
        let eq = field.iter().position(|&c| c == b'=')?;
        let (k, v) = (&field[..eq], &field[eq + 1..]);
        let vs = core::str::from_utf8(v).ok()?;
        match k {
            b"ssid" => b.ssid = parse_ssid(v)?,
            b"bssid" => b.bssid = hex::decode_mac(v)?,
            b"chan" => b.chan = vs.parse().ok()?,
            b"signal" => b.signal = vs.parse().ok()?,
            b"security" => b.security = Security::from_name(vs)?,
            _ => {}
        }
    }
    Some(b)
}

/// A no-allocation text writer into a caller buffer.
pub struct Writer<'a> {
    out: &'a mut [u8],
    n: usize,
}

impl<'a> Writer<'a> {
    pub fn new(out: &'a mut [u8]) -> Self {
        Self { out, n: 0 }
    }

    #[must_use]
    pub const fn len(&self) -> usize {
        self.n
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.n == 0
    }

    pub fn str(&mut self, s: &str) -> Option<()> {
        let dst = self.out.get_mut(self.n..self.n + s.len())?;
        dst.copy_from_slice(s.as_bytes());
        self.n += s.len();
        Some(())
    }

    pub fn hex(&mut self, b: &[u8]) -> Option<()> {
        let n = hex::encode(b, self.out.get_mut(self.n..)?)?;
        self.n += n;
        Some(())
    }

    pub fn mac(&mut self, m: &Bssid) -> Option<()> {
        let n = hex::encode_mac(m, self.out.get_mut(self.n..)?)?;
        self.n += n;
        Some(())
    }

    pub fn num(&mut self, v: i64) -> Option<()> {
        let mut buf = [0u8; 20];
        let mut i = buf.len();
        let neg = v < 0;
        let mut u = v.unsigned_abs();
        loop {
            i -= 1;
            buf[i] = b'0' + (u % 10) as u8;
            u /= 10;
            if u == 0 {
                break;
            }
        }
        if neg {
            self.str("-")?;
        }
        self.str(core::str::from_utf8(&buf[i..]).ok()?)
    }

    fn kv_str(&mut self, k: &str, v: &str) -> Option<()> {
        self.str(k)?;
        self.str("=")?;
        self.str(v)?;
        self.str("\n")
    }

    fn kv_num(&mut self, k: &str, v: i64) -> Option<()> {
        self.str(k)?;
        self.str("=")?;
        self.num(v)?;
        self.str("\n")
    }
}
