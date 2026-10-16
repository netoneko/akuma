//! Windows Precision Touchpad (PTP) report decoding and the pointer wire
//! format: the layer between `akuma-hid`'s descriptor walk and the
//! [`crate::Clickpad`] policy.
//!
//! [`PtpLayout::from_descriptor`] finds the touch report in a device's report
//! descriptor (the first report id that carries a *Tip Switch*), records where
//! each contact's tip/id/X/Y sit and where the contact count and the clickpad
//! button sit. [`PtpDecoder`] turns payloads into a stable-slot
//! [`crate::Frame`]: a finger keeps its slot for as long as its contact id
//! lives, because the report's array order is **not** stable — fingers lift
//! from the middle and the rest shift down, which would otherwise read as
//! two-finger scroll jumps.
//!
//! Measured layout of the IdeaPad 5 2-in-1 16AHP9's Goodix `GXTP5100`: report
//! 4, 29 payload bytes, 5 contacts × 40 bits, then scan time, count, button.
//! No allocation; every table is a fixed array.

use akuma_hid::{Filter, HIO_CONST, HidLocation, HidParser, ItemKind, get_bits, get_ubits};

use crate::{Button, Contact, Frame, Geometry, GeometryError, MAX_CONTACTS, PointerEvent};

const PAGE_GENERIC: u32 = 0x01 << 16;
const PAGE_BUTTON: u32 = 0x09 << 16;
const PAGE_DIGITIZER: u32 = 0x0D << 16;
const USAGE_X: u32 = PAGE_GENERIC | 0x30;
const USAGE_Y: u32 = PAGE_GENERIC | 0x31;
const USAGE_BUTTON1: u32 = PAGE_BUTTON | 0x01;
const USAGE_TIP: u32 = PAGE_DIGITIZER | 0x42;
const USAGE_CONTACT_ID: u32 = PAGE_DIGITIZER | 0x51;
const USAGE_CONTACT_COUNT: u32 = PAGE_DIGITIZER | 0x54;

/// Where one contact's fields sit in the payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SlotFields {
    tip: HidLocation,
    id: Option<HidLocation>,
    x: HidLocation,
    y: HidLocation,
}

/// The touch report's shape, from the descriptor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PtpLayout {
    /// The report id (the first byte of every report of this kind).
    pub report_id: u8,
    /// Payload bytes after the id.
    pub payload_len: usize,
    /// Contacts one report carries.
    pub contacts: usize,
    /// Sensor range, from the X/Y logical limits.
    pub x_min: i32,
    pub x_max: i32,
    pub y_min: i32,
    pub y_max: i32,
    slots: [Option<SlotFields>; MAX_CONTACTS],
    count: Option<HidLocation>,
    button: Option<HidLocation>,
}

impl PtpLayout {
    /// Find the touch report. `None` when the descriptor has no Tip Switch
    /// with X and Y beside it (a mouse, a keyboard, a sensor hub).
    #[must_use]
    pub fn from_descriptor(desc: &[u8]) -> Option<Self> {
        let mut first = HidParser::new(desc, Filter::Kind(ItemKind::Input));
        let id = loop {
            let i = first.next_item()?;
            if i.kind == ItemKind::Input && i.usage == USAGE_TIP && i.flags & HIO_CONST == 0 {
                break i.report_id;
            }
        };

        let mut slots = [None::<SlotFields>; MAX_CONTACTS];
        let mut tip = [None::<HidLocation>; MAX_CONTACTS];
        let mut cid = [None::<HidLocation>; MAX_CONTACTS];
        let mut xs = [None::<HidLocation>; MAX_CONTACTS];
        let mut ys = [None::<HidLocation>; MAX_CONTACTS];
        let (mut nt, mut ni, mut nx, mut ny) = (0, 0, 0, 0);
        let (mut count, mut button) = (None, None);
        let (mut x_min, mut x_max, mut y_min, mut y_max) = (0, 0, 0, 0);

        let mut p = HidParser::new(desc, Filter::Kind(ItemKind::Input));
        while let Some(i) = p.next_item() {
            // Constant (padding) items keep the *previous* item's usage, as in
            // the C parser they come from; they are not fields.
            if i.kind != ItemKind::Input || i.report_id != id || i.flags & HIO_CONST != 0 {
                continue;
            }
            match i.usage {
                USAGE_TIP if nt < MAX_CONTACTS => {
                    tip[nt] = Some(i.loc);
                    nt += 1;
                }
                USAGE_CONTACT_ID if ni < MAX_CONTACTS => {
                    cid[ni] = Some(i.loc);
                    ni += 1;
                }
                USAGE_X if nx < MAX_CONTACTS => {
                    if nx == 0 {
                        x_min = i.logical_minimum;
                        x_max = i.logical_maximum;
                    }
                    xs[nx] = Some(i.loc);
                    nx += 1;
                }
                USAGE_Y if ny < MAX_CONTACTS => {
                    if ny == 0 {
                        y_min = i.logical_minimum;
                        y_max = i.logical_maximum;
                    }
                    ys[ny] = Some(i.loc);
                    ny += 1;
                }
                USAGE_CONTACT_COUNT => count = count.or(Some(i.loc)),
                USAGE_BUTTON1 => button = button.or(Some(i.loc)),
                _ => {}
            }
        }
        let contacts = nt.min(nx).min(ny);
        if contacts == 0 || x_min >= x_max || y_min >= y_max {
            return None;
        }
        for (s, slot) in slots.iter_mut().enumerate().take(contacts) {
            *slot = Some(SlotFields {
                tip: tip[s]?,
                id: if s < ni { cid[s] } else { None },
                x: xs[s]?,
                y: ys[s]?,
            });
        }
        let payload_len = akuma_hid::report_size(desc, ItemKind::Input, id)?;
        Some(Self {
            report_id: id,
            payload_len,
            contacts,
            x_min,
            x_max,
            y_min,
            y_max,
            slots,
            count,
            button,
        })
    }

    /// The pointer mapping onto a `width × height` panel.
    ///
    /// # Errors
    /// [`GeometryError::NoPixels`] for a zero-sized panel.
    pub fn geometry(&self, width_px: u32, height_px: u32) -> Result<Geometry, GeometryError> {
        Geometry::new(self.x_min, self.x_max, self.y_min, self.y_max, width_px, height_px)
    }
}

/// Stateful decoder: keeps each finger in a stable slot across reports.
#[derive(Clone, Copy, Debug)]
pub struct PtpDecoder {
    layout: PtpLayout,
    ids: [Option<u32>; MAX_CONTACTS],
    contacts: [Contact; MAX_CONTACTS],
    button: bool,
}

impl PtpDecoder {
    #[must_use]
    pub const fn new(layout: PtpLayout) -> Self {
        Self {
            layout,
            ids: [None; MAX_CONTACTS],
            contacts: [Contact::UP; MAX_CONTACTS],
            button: false,
        }
    }

    #[must_use]
    pub const fn layout(&self) -> &PtpLayout {
        &self.layout
    }

    /// The decoded state as the policy's input.
    #[must_use]
    pub fn frame(&self) -> Frame<'_> {
        Frame {
            contacts: &self.contacts,
            button: self.button,
        }
    }

    fn coord(loc: &HidLocation, signed: bool, buf: &[u8]) -> i32 {
        if signed {
            get_bits(buf, loc)
        } else {
            i32::try_from(get_ubits(buf, loc)).unwrap_or(i32::MAX)
        }
    }

    /// Decode one report **payload** (the bytes after the report id).
    /// Returns `false`, leaving the state untouched, when the payload is
    /// shorter than the layout needs.
    pub fn decode(&mut self, payload: &[u8]) -> bool {
        if payload.len() < self.layout.payload_len {
            return false;
        }
        let l = self.layout;
        let valid = match l.count {
            Some(loc) => (get_ubits(payload, &loc) as usize).min(l.contacts),
            None => l.contacts,
        };
        let mut seen = [false; MAX_CONTACTS];
        for (i, f) in l.slots.iter().enumerate().take(valid) {
            let Some(f) = f else { continue };
            let tip = get_ubits(payload, &f.tip) != 0;
            let id = f.id.map_or(i as u32, |loc| get_ubits(payload, &loc));
            let known = self.ids.iter().position(|s| *s == Some(id));
            if tip {
                let Some(s) = known.or_else(|| self.ids.iter().position(Option::is_none)) else {
                    continue;
                };
                self.ids[s] = Some(id);
                self.contacts[s] = Contact {
                    down: true,
                    x: Self::coord(&f.x, l.x_min < 0, payload),
                    y: Self::coord(&f.y, l.y_min < 0, payload),
                };
                seen[s] = true;
            } else if let Some(s) = known {
                self.contacts[s].down = false;
                self.ids[s] = None;
                seen[s] = true;
            }
        }
        // A finger the report no longer lists has lifted.
        for s in 0..MAX_CONTACTS {
            if !seen[s] {
                self.contacts[s].down = false;
                self.ids[s] = None;
            }
        }
        self.button = l.button.is_some_and(|loc| get_ubits(payload, &loc) != 0);
        true
    }
}

/// Bytes per record in the pointer node's stream.
pub const EVENT_BYTES: usize = 8;

/// Record kinds (byte 0).
pub mod kind {
    pub const MOVE: u8 = 1;
    pub const BUTTON: u8 = 2;
    pub const WHEEL: u8 = 3;
    /// The pointer stroke ended; the next move starts a new one.
    pub const LIFT: u8 = 4;
}

/// Encode one event as an [`EVENT_BYTES`] record, the wire format of
/// `/dev/input/mouse0`:
///
/// | kind | byte 1 | byte 2 | bytes 4–5 | bytes 6–7 |
/// |---|---|---|---|---|
/// | `MOVE` | 0 | 0 | x, u16 LE (panel px) | y, u16 LE |
/// | `BUTTON` | 0 left, 1 right, 2 middle | 1 down, 0 up | 0 | 0 |
/// | `WHEEL` | 0 | 0 | dx, i16 LE | dy, i16 LE |
///
/// Byte 3 is reserved zero. Values saturate at the field width.
#[must_use]
pub fn encode(ev: &PointerEvent) -> [u8; EVENT_BYTES] {
    let mut r = [0u8; EVENT_BYTES];
    let put16 = |r: &mut [u8; EVENT_BYTES], at: usize, v: u16| {
        r[at..at + 2].copy_from_slice(&v.to_le_bytes());
    };
    match *ev {
        PointerEvent::Move { x, y } => {
            r[0] = kind::MOVE;
            put16(&mut r, 4, u16::try_from(x).unwrap_or(u16::MAX));
            put16(&mut r, 6, u16::try_from(y).unwrap_or(u16::MAX));
        }
        PointerEvent::Button { button, down } => {
            r[0] = kind::BUTTON;
            r[1] = match button {
                Button::Left => 0,
                Button::Right => 1,
                Button::Middle => 2,
            };
            r[2] = u8::from(down);
        }
        PointerEvent::Lift => {
            r[0] = kind::LIFT;
        }
        PointerEvent::Wheel { dx, dy } => {
            r[0] = kind::WHEEL;
            let sat = |v: i32| v.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16;
            put16(&mut r, 4, sat(dx) as u16);
            put16(&mut r, 6, sat(dy) as u16);
        }
    }
    r
}

/// A command written to `/dev/input/mouse0`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ctl {
    /// `natural`, `natural=1`, `natural off`, ...: `None` toggles.
    Natural(Option<bool>),
}

/// Parse one line of the control language: `natural` (toggle), or
/// `natural=<v>` / `natural <v>` with `v` one of `1 0 on off true false yes no`.
/// Anything else is `None` — the whole write is then refused.
#[must_use]
pub fn parse_ctl(line: &[u8]) -> Option<Ctl> {
    let line = line.trim_ascii();
    let (key, val) = match line.iter().position(|&c| c == b'=' || c.is_ascii_whitespace()) {
        Some(i) => {
            // `key=v`, `key v` and `key = v` are all the same line.
            let rest = line[i..].trim_ascii();
            (&line[..i], rest.strip_prefix(b"=").unwrap_or(rest).trim_ascii())
        }
        None => (line, &b""[..]),
    };
    if key != b"natural" {
        return None;
    }
    match val {
        b"" => Some(Ctl::Natural(None)),
        b"1" | b"on" | b"true" | b"yes" => Some(Ctl::Natural(Some(true))),
        b"0" | b"off" | b"false" | b"no" => Some(Ctl::Natural(Some(false))),
        _ => None,
    }
}

#[cfg(test)]
mod ctl_tests {
    use super::*;

    #[test]
    fn natural_forms() {
        assert_eq!(parse_ctl(b"natural"), Some(Ctl::Natural(None)));
        assert_eq!(parse_ctl(b"natural\n"), Some(Ctl::Natural(None)));
        assert_eq!(parse_ctl(b"natural=1"), Some(Ctl::Natural(Some(true))));
        assert_eq!(parse_ctl(b"natural 0"), Some(Ctl::Natural(Some(false))));
        assert_eq!(parse_ctl(b"  natural = off "), Some(Ctl::Natural(Some(false))));
        assert_eq!(parse_ctl(b"natural=yes"), Some(Ctl::Natural(Some(true))));
    }

    #[test]
    fn anything_else_is_refused() {
        assert_eq!(parse_ctl(b""), None);
        assert_eq!(parse_ctl(b"natural=maybe"), None);
        assert_eq!(parse_ctl(b"scroll=1"), None);
        assert_eq!(parse_ctl(b"naturalx"), None);
    }
}
