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
///
/// `energy_mwh` is the running integrated total from
/// [`crate::ledger::Ledger`] when a battery is being sampled — absent with the
/// source, so a reader can tell "no energy counted" from "no meter here".
pub fn render(
    out: &mut [u8],
    source: Source,
    raw: Option<&[u8; BLOCK_LEN]>,
    energy_mwh: Option<u64>,
) -> usize {
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
    // Energy from the pack, integrated between reads (`ledger`). Shown whenever
    // a meter exists — on AC at BTST 0 the pack's own power is near zero, and
    // the number honestly says so — because "how much came *out of the pack*"
    // is exactly the question a battery-life comparison asks.
    if let Some(mwh) = energy_mwh {
        let _ = writeln!(w, "energy_mwh={mwh}");
    }
    let _ = writeln!(w, "btst={}", r.btst);
    let _ = write!(w, "ec_raw=");
    for b in raw {
        let _ = write!(w, "{b:02x}");
    }
    let _ = writeln!(w);
    w.pos
}

/// Append the kernel's own power facts to a `/proc/power` buffer.
///
/// They follow [`render`]'s battery lines: `idle_ms=` (console input
/// inactivity, `amd64/src/input.rs`) and `dim=` (the global console dim factor
/// the power daemon drives through `/dev/power`).
///
/// These are facts about the *machine*, not the battery, which is why they are
/// a second function the glue appends rather than parameters threading through
/// [`render`]: a source with no EC still has an idle keyboard and a dim level.
/// Always both lines, never absent — "the daemon has not dimmed" (`dim=100`)
/// must be distinguishable from "a kernel too old to say" (no line), the same
/// reading rule `source=none` established.
pub fn render_kernel_facts(out: &mut [u8], idle_ms: u64, dim_pct: u8) -> usize {
    let mut w = Buf { buf: out, pos: 0 };
    let _ = writeln!(w, "idle_ms={idle_ms}");
    let _ = writeln!(w, "dim={dim_pct}");
    w.pos
}
