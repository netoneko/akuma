//! `TerminalState` (console / ssh sessions) and `PtyPair` (Unix98 ptys), fed the
//! same keystrokes.
//!
//! Until 2026-10-19 these were two engines and this file pinned eleven
//! observable differences (`git log -- crates/akuma-tty/src/differential.rs`
//! has them). Both now run [`crate::ld::LineDiscipline`], so the contract is the
//! opposite one: **every input gives identical echo and identical reads**.
//! What remains different is not the discipline: `translate_output` (stateless,
//! ignores `OPOST`) and *when* the discipline runs (lazily on `read` for a
//! session, eagerly on write for a pty).
//!
//! Both are driven on [`Termios::initial`] (Linux's `tty_std_termios`).

extern crate std;

use std::vec::Vec;

use crate::pty::{MasterRead, PtyPair, SlaveRead};
use crate::session::{CanonRead, TerminalState};
use crate::termios::{Termios, flags};

/// What a terminal did with some typed input.
#[derive(Debug, PartialEq, Eq)]
struct Outcome {
    /// Bytes echoed to the screen.
    echo: Vec<u8>,
    /// Bytes a canonical read returned, one entry per `read` call.
    reads: Vec<Vec<u8>>,
    /// The reader saw end-of-file (a zero-length read) before running dry.
    eof: bool,
}

fn session(t: &Termios, input: &[u8]) -> Outcome {
    let mut ts = TerminalState::with_termios(t);
    let mut echo = Vec::new();
    for chunk in input.chunks(TerminalState::INGEST_CHUNK) {
        ts.ingest(chunk, |e| echo.extend_from_slice(e)).unwrap();
    }
    let mut reads = Vec::new();
    let mut eof = false;
    let mut buf = std::vec![0u8; 8192];
    loop {
        match ts.read_line(&mut buf) {
            CanonRead::Data(n) => reads.push(buf[..n].to_vec()),
            CanonRead::Eof => {
                eof = true;
                break;
            }
            CanonRead::Empty => break,
        }
    }
    Outcome { echo, reads, eof }
}

fn pty(t: &Termios, input: &[u8]) -> Outcome {
    let mut p: PtyPair<u32> = PtyPair::try_new().unwrap();
    p.set_locked(false);
    p.slave_open().unwrap();
    p.set_termios(*t);
    p.master_write(input).unwrap();
    let mut echo = Vec::new();
    let mut buf = std::vec![0u8; 8192];
    if let MasterRead::Data(n) = p.master_read(&mut buf) {
        echo.extend_from_slice(&buf[..n]);
    }
    let mut reads = Vec::new();
    let mut eof = false;
    loop {
        match p.slave_read(&mut buf, false) {
            SlaveRead::Data(0) => {
                eof = true;
                break;
            }
            SlaveRead::Data(n) => reads.push(buf[..n].to_vec()),
            _ => break,
        }
    }
    Outcome { echo, reads, eof }
}

fn both(input: &[u8]) -> (Outcome, Outcome) {
    let t = Termios::initial();
    (session(&t, input), pty(&t, input))
}

fn agrees(input: &[u8]) {
    let (s, p) = both(input);
    assert_eq!(s, p, "input {input:?}");
}

#[test]
fn agrees_on_plain_and_edited_lines() {
    for input in [
        &b"hello\n"[..],
        b"one\ntwo\n",
        b"partial",
        b"ls\r",
        b"helo\x7flo\n",
        b"\x7f\x7f\x7fx\n",
        b"discard me\x15keep\n",
        b"ab\x08c\n",
    ] {
        agrees(input);
    }
}

#[test]
fn agrees_on_eof() {
    for input in [&b"\x04"[..], b"abc\x04", b"abc\x04def\n", b"a\n\x04"] {
        agrees(input);
    }
}

#[test]
fn agrees_on_control_characters() {
    for input in [
        &b"\x01\n"[..],           // ^A echoes as caret notation
        b"foo bar\x17\n",         // word erase
        b"\x16\x03\n",           // literal-next
        b"a\x1b\x1a\x1cb\n",    // ESC, SUSP, QUIT: signals + flush
        b"abc\x1c",
    ] {
        agrees(input);
    }
}

#[test]
fn agrees_on_flag_variants() {
    let mut cases: Vec<(Termios, &[u8])> = Vec::new();
    let mut t = Termios::initial();
    t.lflag &= !flags::ECHOE;
    cases.push((t, b"abc\x15"));
    let mut t = Termios::initial();
    t.lflag &= !flags::ECHO;
    cases.push((t, b"secret\n"));
    let mut t = Termios::initial();
    t.lflag &= !flags::ECHO;
    t.lflag |= flags::ECHONL;
    cases.push((t, b"pw\n"));
    let mut t = Termios::initial();
    t.oflag &= !flags::ONLCR;
    cases.push((t, b"x\n"));
    let mut t = Termios::initial();
    t.iflag |= flags::IGNCR;
    cases.push((t, b"a\r\n"));
    let mut t = Termios::initial();
    t.iflag |= flags::INLCR;
    t.iflag &= !flags::ICRNL;
    cases.push((t, b"a\nb\r"));
    let mut t = Termios::initial();
    t.cc[crate::termios::cc::VERASE] = 0;
    cases.push((t, b"a\0b\n"));
    let mut t = Termios::initial();
    t.lflag &= !flags::ISIG;
    cases.push((t, b"a\x03b\x1cc\n"));
    for (t, input) in cases {
        assert_eq!(session(&t, input), pty(&t, input), "termios {t:?} input {input:?}");
    }
}

#[test]
fn agrees_on_one_line_per_read() {
    let (s, p) = both(b"one\ntwo\n");
    assert_eq!(s.reads, std::vec![b"one\n".to_vec(), b"two\n".to_vec()]);
    assert_eq!(s, p);
}

#[test]
fn agrees_on_the_line_cap() {
    let mut input = std::vec![b'x'; 10_000];
    input.push(b'\n');
    let (s, p) = both(&input);
    assert_eq!(s.reads.len(), 1);
    assert_eq!(s.reads[0].len(), crate::termios::MAX_CANON + 1);
    assert_eq!(s, p);
}

// ---- what still differs (and is not the discipline) ------------------------------

#[test]
fn session_output_translation_ignores_opost() {
    // `translate_output` checks only ONLCR; `PtyPair` honours OPOST first.
    // Stateless, so not an engine difference — left alone because raw-mode
    // programs on the console may rely on it.
    let mut t = Termios::initial();
    t.oflag &= !flags::OPOST;
    let ts = TerminalState::with_termios(&t);
    assert_eq!(ts.translate_output(b"a\n"), b"a\r\n", "session: OPOST ignored");
    let mut p: PtyPair<u32> = PtyPair::try_new().unwrap();
    p.set_locked(false);
    p.slave_open().unwrap();
    p.set_termios(t);
    p.slave_write(b"a\n").unwrap();
    let mut buf = [0u8; 8];
    let MasterRead::Data(n) = p.master_read(&mut buf) else { panic!() };
    assert_eq!(&buf[..n], b"a\n", "pty: OPOST off, no translation");
}
