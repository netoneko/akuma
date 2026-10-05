//! HID Keyboard/Keypad usage codes (usage page `0x07`) to ASCII.
//!
//! This is the USB counterpart of `amd64/src/kbd.rs`'s `PLAIN`/`SHIFTED`
//! set-1 scancode tables, and it makes the same choices on purpose so a shell
//! behaves identically whichever keyboard the bytes came from:
//!
//! * Caps Lock inverts the case of letters only (applied after the shift
//!   table, so Shift+CapsLock on a letter is lowercase).
//! * Ctrl+letter is the control character a terminal sends — Ctrl-C is `0x03`,
//!   Ctrl-D `0x04` (EOF to the line discipline).
//! * Backspace is `0x7F` (DEL), what a terminal sends and the line discipline's
//!   `VERASE` — it was `0x08` until 2026-10-01, which a raw-mode line editor
//!   accepts and a cooked-mode `read(0)` does not.
//! * Arrow and navigation keys are the escape sequences a Linux console / xterm
//!   sends ([`usage_to_sequence`]), so a shell's line editor gets history and
//!   cursor movement. Function keys and the modifiers produce nothing.
//!
//! The keypad rows assume Num Lock is on (digits, not arrows) — this target
//! has no Num Lock LED and no reason to track the state.

/// `bModifier` bits in a boot-keyboard report (HID 1.11 §8.3).
pub const MOD_LCTRL: u8 = 1 << 0;
pub const MOD_LSHIFT: u8 = 1 << 1;
pub const MOD_LALT: u8 = 1 << 2;
pub const MOD_LGUI: u8 = 1 << 3;
pub const MOD_RCTRL: u8 = 1 << 4;
pub const MOD_RSHIFT: u8 = 1 << 5;
pub const MOD_RALT: u8 = 1 << 6;
pub const MOD_RGUI: u8 = 1 << 7;

/// Either Shift held.
pub const MOD_SHIFT: u8 = MOD_LSHIFT | MOD_RSHIFT;
/// Either Ctrl held.
pub const MOD_CTRL: u8 = MOD_LCTRL | MOD_RCTRL;
/// Either Alt held.
pub const MOD_ALT: u8 = MOD_LALT | MOD_RALT;

/// Usage code for Caps Lock — a toggle the decoder tracks itself.
pub const USAGE_CAPS_LOCK: u8 = 0x39;
/// Usage code the keyboard fills every key slot with when more keys are held
/// than the report can carry (HID 1.11 §10.2). Not a real key.
pub const USAGE_ERROR_ROLL_OVER: u8 = 0x01;

/// The number of usage codes the tables cover; codes at or above this produce
/// no byte.
pub const TABLE_LEN: usize = 0x68;

/// Usage -> byte, no modifier held. `0` means "no character".
static PLAIN: [u8; TABLE_LEN] = [
    // 0x00
    0, 0, 0, 0, b'a', b'b', b'c', b'd', b'e', b'f', b'g', b'h', b'i', b'j', b'k', b'l',
    // 0x10
    b'm', b'n', b'o', b'p', b'q', b'r', b's', b't', b'u', b'v', b'w', b'x', b'y', b'z', b'1', b'2',
    // 0x20
    b'3', b'4', b'5', b'6', b'7', b'8', b'9', b'0', b'\r', 0x1B, 0x7F, b'\t', b' ', b'-', b'=', b'[',
    // 0x30
    b']', b'\\', b'\\', b';', b'\'', b'`', b',', b'.', b'/', 0, 0, 0, 0, 0, 0, 0,
    // 0x40
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    // 0x50
    0, 0, 0, 0, b'/', b'*', b'-', b'+', b'\r', b'1', b'2', b'3', b'4', b'5', b'6', b'7',
    // 0x60
    b'8', b'9', b'0', b'.', b'\\', 0, 0, 0,
];

/// Usage -> byte with Shift held.
static SHIFTED: [u8; TABLE_LEN] = [
    // 0x00
    0, 0, 0, 0, b'A', b'B', b'C', b'D', b'E', b'F', b'G', b'H', b'I', b'J', b'K', b'L',
    // 0x10
    b'M', b'N', b'O', b'P', b'Q', b'R', b'S', b'T', b'U', b'V', b'W', b'X', b'Y', b'Z', b'!', b'@',
    // 0x20
    b'#', b'$', b'%', b'^', b'&', b'*', b'(', b')', b'\r', 0x1B, 0x7F, b'\t', b' ', b'_', b'+', b'{',
    // 0x30
    b'}', b'|', b'|', b':', b'"', b'~', b'<', b'>', b'?', 0, 0, 0, 0, 0, 0, 0,
    // 0x40
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    // 0x50
    0, 0, 0, 0, b'/', b'*', b'-', b'+', b'\r', b'1', b'2', b'3', b'4', b'5', b'6', b'7',
    // 0x60
    b'8', b'9', b'0', b'.', b'|', 0, 0, 0,
];

/// The escape sequence a navigation key sends, or `None` for any other key.
///
/// The Linux console / xterm "normal cursor key" forms, which busybox's line
/// editor and every full-screen program understand: arrows `ESC [ A..D`,
/// Home/End `ESC [ H`/`F`, and `ESC [ n ~` for Insert, Delete, PageUp, PageDown.
/// Modified forms (Ctrl-Left and friends) are not generated.
#[must_use]
pub fn usage_to_sequence(usage: u8) -> Option<&'static [u8]> {
    Some(match usage {
        0x4F => b"\x1b[C", // Right
        0x50 => b"\x1b[D", // Left
        0x51 => b"\x1b[B", // Down
        0x52 => b"\x1b[A", // Up
        0x4A => b"\x1b[H", // Home
        0x4D => b"\x1b[F", // End
        0x49 => b"\x1b[2~", // Insert
        0x4C => b"\x1b[3~", // Delete
        0x4B => b"\x1b[5~", // PageUp
        0x4E => b"\x1b[6~", // PageDown
        _ => return None,
    })
}

/// One key-usage code to the byte a terminal would receive, or `None` for a
/// key that produces no text (a modifier, a function key, an unmapped code).
///
/// `caps` is the decoder's tracked Caps Lock state; `shift` and `ctrl` come
/// from the report's modifier byte.
#[must_use]
pub fn usage_to_ascii(usage: u8, shift: bool, ctrl: bool, caps: bool) -> Option<u8> {
    let idx = usage as usize;
    let mut c = if shift {
        *SHIFTED.get(idx)?
    } else {
        *PLAIN.get(idx)?
    };
    if c == 0 {
        return None;
    }
    // Caps Lock inverts letter case, after the shift table — matches kbd.rs.
    if caps && c.is_ascii_alphabetic() {
        c ^= 0x20;
    }
    // Ctrl+letter -> the control character (Ctrl-A = 0x01 .. Ctrl-Z = 0x1A).
    if ctrl && c.is_ascii_alphabetic() {
        c = c.to_ascii_lowercase() - b'a' + 1;
    }
    Some(c)
}
