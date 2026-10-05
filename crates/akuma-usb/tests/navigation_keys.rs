//! Arrow/navigation keys, Backspace, and software key repeat.
//!
//! A USB boot keyboard sends the *set of keys currently down* and never repeats,
//! so everything a person expects from holding a key lives in the decoder.

use akuma_usb::hid::{BootKeyboardDecoder, BootReport};

const UP: u8 = 0x52;
const DOWN: u8 = 0x51;
const LEFT: u8 = 0x50;
const RIGHT: u8 = 0x4F;
const DELETE: u8 = 0x4C;
const BACKSPACE: u8 = 0x2A;
const A: u8 = 0x04;
const LSHIFT: u8 = 1 << 1;

fn report(mods: u8, keys: &[u8]) -> BootReport {
    let mut k = [0u8; 6];
    k[..keys.len()].copy_from_slice(keys);
    BootReport { modifiers: mods, keys: k }
}

fn feed(d: &mut BootKeyboardDecoder, mods: u8, keys: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    d.feed(&report(mods, keys), |b| out.push(b));
    out
}

#[test]
fn arrows_are_the_linux_console_sequences() {
    for (usage, want) in [(UP, "\x1b[A"), (DOWN, "\x1b[B"), (RIGHT, "\x1b[C"), (LEFT, "\x1b[D")] {
        let mut d = BootKeyboardDecoder::new();
        assert_eq!(feed(&mut d, 0, &[usage]), want.as_bytes(), "usage {usage:#04x}");
    }
}

#[test]
fn navigation_keys_have_their_sequences() {
    for (usage, want) in [
        (0x4A, "\x1b[H"),  // Home
        (0x4D, "\x1b[F"),  // End
        (0x49, "\x1b[2~"), // Insert
        (DELETE, "\x1b[3~"),
        (0x4B, "\x1b[5~"), // PageUp
        (0x4E, "\x1b[6~"), // PageDown
    ] {
        let mut d = BootKeyboardDecoder::new();
        assert_eq!(feed(&mut d, 0, &[usage]), want.as_bytes(), "usage {usage:#04x}");
    }
}

#[test]
fn backspace_is_del_not_ctrl_h() {
    // VERASE is 0x7f; 0x08 only erases in a raw-mode line editor.
    let mut d = BootKeyboardDecoder::new();
    assert_eq!(feed(&mut d, 0, &[BACKSPACE]), [0x7f]);
    let mut d = BootKeyboardDecoder::new();
    assert_eq!(feed(&mut d, LSHIFT, &[BACKSPACE]), [0x7f]);
}

#[test]
fn a_held_arrow_emits_once_until_asked_to_repeat() {
    let mut d = BootKeyboardDecoder::new();
    assert_eq!(feed(&mut d, 0, &[LEFT]), b"\x1b[D");
    // The keyboard re-sends the same report while the key stays down.
    assert!(feed(&mut d, 0, &[LEFT]).is_empty(), "no edge, no output");
    assert!(d.held());
    let mut rep = Vec::new();
    d.repeat(|b| rep.push(b));
    assert_eq!(rep, b"\x1b[D", "repeat re-emits the whole sequence");
}

#[test]
fn releasing_the_key_stops_the_repeat() {
    let mut d = BootKeyboardDecoder::new();
    feed(&mut d, 0, &[A]);
    assert!(d.held());
    feed(&mut d, 0, &[]);
    assert!(!d.held());
    let mut rep = Vec::new();
    d.repeat(|b| rep.push(b));
    assert_eq!(rep, Vec::<u8>::new());
}

#[test]
fn repeat_follows_the_newest_key_and_stops_when_it_is_released() {
    let mut d = BootKeyboardDecoder::new();
    feed(&mut d, 0, &[A]);
    feed(&mut d, 0, &[A, DOWN]);
    let mut rep = Vec::new();
    d.repeat(|b| rep.push(b));
    assert_eq!(rep, b"\x1b[B", "the most recent press repeats");
    // Releasing the repeating key stops the repeat, even with `a` still down —
    // what every desktop does.
    feed(&mut d, 0, &[A]);
    assert!(!d.held());
}

#[test]
fn a_modifier_alone_never_repeats() {
    let mut d = BootKeyboardDecoder::new();
    assert_eq!(feed(&mut d, LSHIFT, &[]), Vec::<u8>::new());
    assert!(!d.held());
}

#[test]
fn repeat_uses_the_modifiers_held_now() {
    let mut d = BootKeyboardDecoder::new();
    feed(&mut d, 0, &[A]);
    feed(&mut d, LSHIFT, &[A]); // shift pressed while a is held
    let mut rep = Vec::new();
    d.repeat(|b| rep.push(b));
    assert_eq!(rep, b"A");
}

#[test]
fn every_new_press_bumps_the_sequence_counter() {
    let mut d = BootKeyboardDecoder::new();
    let s0 = d.press_seq();
    feed(&mut d, 0, &[A]);
    let s1 = d.press_seq();
    assert_ne!(s0, s1);
    feed(&mut d, 0, &[A]); // same report again: not a new press
    assert_eq!(d.press_seq(), s1);
    feed(&mut d, 0, &[]);
    feed(&mut d, 0, &[A]);
    assert_ne!(d.press_seq(), s1, "release then press is a new press");
}

#[test]
fn ordinary_typing_is_unchanged() {
    let mut d = BootKeyboardDecoder::new();
    assert_eq!(feed(&mut d, 0, &[A]), b"a");
    feed(&mut d, 0, &[]);
    assert_eq!(feed(&mut d, LSHIFT, &[A]), b"A");
}

const S: u8 = 0x16;
const LALT: u8 = 1 << 2;
const RALT: u8 = 1 << 6;
const LCTRL: u8 = 1 << 0;

/// "Meta sends escape": Alt+key is ESC then the key, for either Alt.
#[test]
fn alt_prefixes_the_key_with_escape() {
    for alt in [LALT, RALT] {
        let mut d = BootKeyboardDecoder::new();
        assert_eq!(feed(&mut d, alt, &[S]), b"\x1bs", "alt mods {alt:#04x}");
    }
    let mut d = BootKeyboardDecoder::new();
    assert_eq!(feed(&mut d, LALT | LSHIFT, &[S]), b"\x1bS", "Alt+Shift+S");
    let mut d = BootKeyboardDecoder::new();
    assert_eq!(feed(&mut d, LALT | LCTRL, &[S]), b"\x1b\x13", "Ctrl+Alt+S is ESC ^S");
}

#[test]
fn alt_alone_and_alt_with_arrows_add_nothing() {
    let mut d = BootKeyboardDecoder::new();
    assert!(feed(&mut d, LALT, &[]).is_empty(), "a modifier alone emits nothing");
    let mut d = BootKeyboardDecoder::new();
    assert_eq!(feed(&mut d, LALT, &[UP]), b"\x1b[A");
}

#[test]
fn a_held_alt_key_repeats_with_its_escape() {
    let mut d = BootKeyboardDecoder::new();
    assert_eq!(feed(&mut d, LALT, &[A]), b"\x1ba");
    let mut rep = Vec::new();
    d.repeat(|b| rep.push(b));
    assert_eq!(rep, b"\x1ba");
}
