//! The text `/proc/power` serves: `key=value` lines, one per fact.
//!
//! Plain `key=value` rather than a table because the readers are a status bar
//! and a person with `cat`, and both want to ask for one field by name. Units
//! are in the key (`_mv`, `_ma`, `_mw`, `_mwh`) so no reader needs a manual.
//! The shape follows Linux's `power_supply` uevent on purpose.

use core::fmt::Write;

use crate::ec::{BLOCK_LEN, Reading};

/// Where the numbers came from.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Source {
    /// No window found: a desktop, or a VM. `ac` and the battery are unknown.
    None,
    /// The EC memory window.
    Ec,
    /// The built-in simulator (`powersim`).
    Sim,
}

impl Source {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Ec => "ec",
            Self::Sim => "sim",
        }
    }
}

struct Buf<'a> {
    buf: &'a mut [u8],
    pos: usize,
}

impl Write for Buf<'_> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let n = s.len().min(self.buf.len() - self.pos);
        self.buf[self.pos..self.pos + n].copy_from_slice(&s.as_bytes()[..n]);
        self.pos += n;
        Ok(())
    }
}

/// Render into `out`, returning bytes written. `raw` is the undecoded window,
/// printed as `ec_raw=` so a reader can check the decode against another OS
/// without a rebuild.
pub fn render(out: &mut [u8], source: Source, raw: Option<&[u8; BLOCK_LEN]>) -> usize {
    let mut w = Buf { buf: out, pos: 0 };
    let _ = writeln!(w, "source={}", source.as_str());
    let Some(raw) = raw else {
        let _ = writeln!(w, "ac=unknown\nbattery=0");
        return w.pos;
    };
    let r = Reading::decode(raw);
    let _ = writeln!(w, "ac={}", u8::from(r.ac));
    let _ = writeln!(w, "battery={}", u8::from(r.battery_present));
    let valid = r.validate();
    let _ = writeln!(w, "valid={}", u8::from(valid));
    if valid {
        let _ = writeln!(w, "status={}", r.state.as_str());
        let _ = writeln!(w, "percent={}", r.percent());
        let _ = writeln!(w, "voltage_mv={}", r.voltage_mv);
        let _ = writeln!(w, "current_ma={}", r.current_ma);
        let _ = writeln!(w, "power_mw={}", r.power_mw());
        let _ = writeln!(w, "energy_now_mwh={}", r.remaining_mwh);
        let _ = writeln!(w, "energy_full_mwh={}", r.full_mwh);
        let _ = writeln!(w, "energy_design_mwh={}", r.design_mwh);
        if let Some(m) = r.minutes() {
            let _ = writeln!(w, "minutes={m}");
        }
    }
    let _ = writeln!(w, "btst={}", r.btst);
    let _ = write!(w, "ec_raw=");
    for b in raw {
        let _ = write!(w, "{b:02x}");
    }
    let _ = writeln!(w);
    w.pos
}
