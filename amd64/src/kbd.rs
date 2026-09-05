//! i8042 PS/2 keyboard, polled.
//!
//! The reference machine's keyboard is USB and this kernel has no USB stack —
//! and yet the keyboard's LEDs are lit when Akuma runs, because the *firmware*
//! brought USB up. PC firmware has, since the first USB keyboards, presented
//! them to a USB-unaware OS through the one interface every PC OS knows: the
//! i8042 keyboard controller at ports `0x60`/`0x64`, with an SMM handler
//! translating HID reports into PS/2 scancodes for as long as nobody takes the
//! USB controllers away from it. Akuma never touches USB, so the emulation
//! stays live for the whole boot. That is what this driver reads.
//!
//! **Polled, like the UART.** No IRQ 1, no IOAPIC: `has_byte` and `getb` are
//! called from the console read and `poll(2)` paths, which already spin (and
//! yield) waiting for input. Scancode **set 1** — the controller's translation
//! mode, which is what firmware emulation and every PC's power-on state give.
//!
//! What it does not do: arrow keys and function keys (dropped), key repeat
//! (the keyboard does that itself), LEDs, any command to the keyboard beyond
//! draining what it has already sent. Enough for a shell.
//!
//! # Absent controller
//!
//! An absent x86 I/O port reads `0xFF` — the same fact `serial` learned the hard
//! way. `0xFF` as an i8042 status byte would say "output buffer full, from the
//! mouse", so an unguarded reader would spin swallowing phantom mouse bytes.
//! [`init`] refuses a status of `0xFF` and everything else is gated on that.
//! QEMU `microvm` has no i8042 at all (absent, correctly); Firecracker's is a
//! reset-only stub whose buffer never fills; QEMU `pc` and real firmware have
//! the real thing.

use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU16, Ordering};

use crate::port::{inb, outb};

const DATA: u16 = 0x60;
const STATUS: u16 = 0x64;

/// Status bit 0: a byte is waiting in the output buffer.
const STS_OUTPUT_FULL: u8 = 1 << 0;
/// Status bit 1: the input buffer is still holding our last command.
const STS_INPUT_FULL: u8 = 1 << 1;
/// Status bit 5: the waiting byte came from the second (mouse) port.
const STS_FROM_AUX: u8 = 1 << 5;

/// Controller command: enable the first (keyboard) port.
const CMD_ENABLE_KBD: u8 = 0xAE;

/// Modifier state, one bit each.
const MOD_SHIFT: u8 = 1 << 0;
const MOD_CTRL: u8 = 1 << 1;
const MOD_CAPS: u8 = 1 << 2;
/// The last scancode was the `0xE0` extended prefix.
const MOD_E0: u8 = 1 << 3;

static PRESENT: AtomicBool = AtomicBool::new(false);
static MODS: AtomicU8 = AtomicU8::new(0);
/// A decoded byte that [`has_byte`] pulled off the controller and [`getb`]
/// has not returned yet: `0x100 | byte`, or 0 for none. `poll(2)` must be
/// able to say "readable" without consuming the key.
static PENDING: AtomicU16 = AtomicU16::new(0);

/// Set-1 make codes to ASCII, unshifted. 0 = no character (a modifier, a
/// function key, or nothing).
const PLAIN: [u8; 0x59] = [
    0, 0x1B, b'1', b'2', b'3', b'4', b'5', b'6', b'7', b'8', b'9', b'0', b'-', b'=', 0x08, b'\t',
    b'q', b'w', b'e', b'r', b't', b'y', b'u', b'i', b'o', b'p', b'[', b']', b'\r', 0, b'a', b's',
    b'd', b'f', b'g', b'h', b'j', b'k', b'l', b';', b'\'', b'`', 0, b'\\', b'z', b'x', b'c', b'v',
    b'b', b'n', b'm', b',', b'.', b'/', 0, b'*', 0, b' ', 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, b'7', b'8', b'9', b'-', b'4', b'5', b'6', b'+', b'1',
    b'2', b'3', b'0', b'.', 0, 0, 0, 0, 0,
];

/// The same keys with Shift held.
const SHIFTED: [u8; 0x59] = [
    0, 0x1B, b'!', b'@', b'#', b'$', b'%', b'^', b'&', b'*', b'(', b')', b'_', b'+', 0x08, b'\t',
    b'Q', b'W', b'E', b'R', b'T', b'Y', b'U', b'I', b'O', b'P', b'{', b'}', b'\r', 0, b'A', b'S',
    b'D', b'F', b'G', b'H', b'J', b'K', b'L', b':', b'"', b'~', 0, b'|', b'Z', b'X', b'C', b'V',
    b'B', b'N', b'M', b'<', b'>', b'?', 0, b'*', 0, b' ', 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, b'7', b'8', b'9', b'-', b'4', b'5', b'6', b'+', b'1',
    b'2', b'3', b'0', b'.', 0, 0, 0, 0, 0,
];

const SC_LSHIFT: u8 = 0x2A;
const SC_RSHIFT: u8 = 0x36;
const SC_CTRL: u8 = 0x1D;
const SC_CAPS: u8 = 0x3A;
const SC_ENTER: u8 = 0x1C;
const SC_KP_SLASH: u8 = 0x35;
const SC_E0: u8 = 0xE0;
const SC_BREAK: u8 = 0x80;

fn status() -> u8 {
    // SAFETY: the i8042 status port is fixed by the PC architecture; reading it
    // has no side effect.
    unsafe { inb(STATUS) }
}

/// Wait, bounded, for the controller to accept a command.
fn wait_input_empty() -> bool {
    for _ in 0..100_000 {
        if status() & STS_INPUT_FULL == 0 {
            return true;
        }
        core::hint::spin_loop();
    }
    false
}

/// Probe for a controller, drain whatever it holds, enable the keyboard port.
/// Returns whether one answered.
pub fn init() -> bool {
    let st = status();
    // `0xFF` is an empty bus, not a status. Every real controller has at
    // least one of bits 2 (system flag) or 3 (command/data) meaningful and the
    // top bits clear; `0xFF` never is.
    if st == 0xFF {
        PRESENT.store(false, Ordering::Release);
        return false;
    }
    // Drain: firmware may have left keystrokes (or its own replies) queued.
    for _ in 0..32 {
        if status() & STS_OUTPUT_FULL == 0 {
            break;
        }
        // SAFETY: reading the data port consumes one queued byte.
        unsafe { inb(DATA) };
    }
    if wait_input_empty() {
        // SAFETY: an architectural controller command with no argument; on a
        // controller that already has the port enabled it is a no-op.
        unsafe { outb(STATUS, CMD_ENABLE_KBD) };
    }
    PRESENT.store(true, Ordering::Release);
    true
}

/// Did [`init`] find a controller?
#[must_use]
pub fn present() -> bool {
    PRESENT.load(Ordering::Relaxed)
}

/// Turn one scancode into a byte, updating modifier state. `None` for a
/// modifier, a break code, an ignored key, or the extended prefix.
fn decode(sc: u8) -> Option<u8> {
    let mods = MODS.load(Ordering::Relaxed);
    if sc == SC_E0 {
        MODS.store(mods | MOD_E0, Ordering::Relaxed);
        return None;
    }
    let extended = mods & MOD_E0 != 0;
    if extended {
        MODS.store(mods & !MOD_E0, Ordering::Relaxed);
    }
    let released = sc & SC_BREAK != 0;
    let code = sc & !SC_BREAK;

    // Modifiers, on both the plain and the extended (right-hand) codes.
    match code {
        SC_LSHIFT | SC_RSHIFT => {
            let m = if released { mods & !MOD_SHIFT } else { mods | MOD_SHIFT };
            MODS.store(m & !MOD_E0, Ordering::Relaxed);
            return None;
        }
        SC_CTRL => {
            let m = if released { mods & !MOD_CTRL } else { mods | MOD_CTRL };
            MODS.store(m & !MOD_E0, Ordering::Relaxed);
            return None;
        }
        SC_CAPS => {
            if !released {
                MODS.store((mods ^ MOD_CAPS) & !MOD_E0, Ordering::Relaxed);
            }
            return None;
        }
        _ => {}
    }
    if released {
        return None;
    }
    if extended {
        // Only two extended keys produce a character worth having: keypad
        // Enter and keypad `/`. Arrows, Home/End, Delete and the like are
        // dropped rather than turned into escape sequences.
        return match code {
            SC_ENTER => Some(b'\r'),
            SC_KP_SLASH => Some(b'/'),
            _ => None,
        };
    }
    let idx = code as usize;
    if idx >= PLAIN.len() {
        return None;
    }
    let shift = mods & MOD_SHIFT != 0;
    let mut c = if shift { SHIFTED[idx] } else { PLAIN[idx] };
    if c == 0 {
        return None;
    }
    // Caps Lock inverts the case of letters only.
    if mods & MOD_CAPS != 0 && c.is_ascii_alphabetic() {
        c ^= 0x20;
    }
    // Ctrl-letter is the control character, as a terminal sends it: Ctrl-D
    // is 0x04 (EOF to the line discipline), Ctrl-C 0x03.
    if mods & MOD_CTRL != 0 && c.is_ascii_alphabetic() {
        c = c.to_ascii_lowercase() - b'a' + 1;
    }
    Some(c)
}

/// Pull scancodes off the controller until one decodes to a byte, or the
/// buffer is empty.
fn pump() -> Option<u8> {
    for _ in 0..64 {
        let st = status();
        if st & STS_OUTPUT_FULL == 0 {
            return None;
        }
        // SAFETY: the output buffer is full; reading the data port takes the
        // byte and clears the flag.
        let sc = unsafe { inb(DATA) };
        if st & STS_FROM_AUX != 0 {
            continue; // the mouse; not ours
        }
        if let Some(c) = decode(sc) {
            return Some(c);
        }
    }
    None
}

/// Take a decoded key, or `None` if none is waiting.
#[must_use]
pub fn getb() -> Option<u8> {
    if !present() {
        return None;
    }
    let pending = PENDING.swap(0, Ordering::Relaxed);
    if pending != 0 {
        return Some(pending as u8);
    }
    pump()
}

/// Is a key waiting? Non-destructive: a key decoded here is kept for the next
/// [`getb`].
#[must_use]
pub fn has_byte() -> bool {
    if !present() {
        return false;
    }
    if PENDING.load(Ordering::Relaxed) != 0 {
        return true;
    }
    match pump() {
        Some(c) => {
            PENDING.store(0x100 | u16::from(c), Ordering::Relaxed);
            true
        }
        None => false,
    }
}
