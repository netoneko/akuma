//! The console / ssh-session terminal: [`TerminalState`].
//!
//! The termios words, window size and foreground group of one session's
//! terminal, plus a canonical-mode line discipline that is **the same engine a
//! pty runs** ([`crate::ld::LineDiscipline`]), allocated on first use.
//!
//! Where it sits: input arrives through `ProcessChannel::write_stdin` (and the
//! amd64 console pump) as raw bytes and is run through the discipline *when a
//! reader asks for it* — [`TerminalState::ingest`] on the bytes just read,
//! [`TerminalState::read_line`] for what is ready. That laziness is why `^C`
//! (`VINTR`) is decided one layer up, in `akuma-exec::write_to_process_stdin`:
//! a program that is not reading must still be interruptible.

extern crate alloc;

use crate::cc_index;
use crate::ld::{LineDiscipline, Signals};
use crate::mode_flags;
use crate::pty::SlaveRead;
pub use crate::termios::MAX_CANON;
use crate::termios::{Termios, Transition, cc::NCCS};

/// Why [`TerminalState::ingest`] could not run: the discipline's buffers could
/// not be allocated (first canonical read under memory pressure).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NoMemory;

/// What [`TerminalState::ingest`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ingest {
    /// Bytes of the offered chunk the discipline consumed. Fewer than offered
    /// only if its queue filled, which `ingest`'s caller prevents by offering
    /// chunks of at most [`TerminalState::INGEST_CHUNK`] and never ingesting
    /// while a completed line is waiting.
    pub consumed: usize,
    /// Signals the discipline raised (`ISIG`): for the foreground group, to be
    /// delivered by the caller after it drops the terminal-state guard.
    pub signals: Signals,
}

/// What a canonical read found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanonRead {
    /// This many bytes of one line (0 never: that is [`CanonRead::Eof`]).
    Data(usize),
    /// `^D` on an empty line — a zero-length read.
    Eof,
    /// No complete line yet.
    Empty,
}

/// Represents the state of a virtual terminal for an SSH session.
#[derive(Debug)]
pub struct TerminalState {
    /// Current terminal mode flags (custom Akuma flags)
    pub mode_flags: u64,

    // Linux-compatible termios flags
    pub iflag: u32,
    pub oflag: u32,
    pub cflag: u32,
    pub lflag: u32,
    pub cc: [u8; 20],

    /// Current foreground process group ID (`TIOCSPGRP`).
    pub foreground_pgid: u32,
    /// The group of the session's own shell (what `foreground_pgid` started as,
    /// see [`Self::set_session_leader`]). A terminal signal aimed at *this* group
    /// spares its leader — the shell — and one aimed at any other group (a job a
    /// job-control shell `TIOCSPGRP`'d to) reaches the leader too, because there
    /// the leader is the job (`sleep` made its own group). Console default 1.
    pub session_pgid: u32,

    /// Terminal dimensions
    pub term_width: u16,
    pub term_height: u16,

    /// Current cursor column (0-indexed)
    pub cursor_col: usize,
    /// Current cursor row (0-indexed)
    pub cursor_row: usize,
    /// Is the cursor hidden?
    pub cursor_hidden: bool,
    /// Waker for a reader parked on this terminal's input.
    ///
    /// Input itself does **not** live here: it arrives through
    /// `ProcessChannel::write_stdin` (and the amd64 console pump); the canonical
    /// line discipline ([`Self::ld`]) is applied on read. A second byte queue on
    /// this struct (`input_buffer`) was removed 2026-10-17 as an unwired
    /// duplicate view of stdin (`docs/archive/POST_AMD64_CODE_AUDIT.md` §1.1).
    ///
    /// Plain field since 2026-10-19: it was a second `Spinlock` nested inside
    /// the `Arc<Spinlock<TerminalState>>` every consumer already held, which
    /// bought nothing and made this crate depend on `spinning_top`. Callers take
    /// the outer guard with `akuma_exec::sync::lock_bounded` (one `try_lock`
    /// attempt at a time, never across an unbounded wait —
    /// `docs/archive/TERM_POLL_INPUT_PREEMPTION_FIX.md`) and use
    /// [`Self::set_input_waker`] / [`Self::take_input_waker`].
    input_waker: Option<core::task::Waker>,

    /// The canonical-mode discipline's buffers (~28 KiB), allocated by the first
    /// [`Self::ingest`]: every process owns a `TerminalState`, and almost none
    /// of them ever read a canonical terminal.
    ld: Option<LineDiscipline>,

    /// Flags saved before the last `RAW_MODE_ENABLE` call, for exact restoration.
    pub saved_iflag: Option<u32>,
    pub saved_oflag: Option<u32>,
    pub saved_lflag: Option<u32>,
}

impl Default for TerminalState {
    /// [`Termios::console_default`] (= a pty's start state) on both kernels
    /// since 2026-10-19; see its doc for what each used to be.
    fn default() -> Self {
        let t = Termios::console_default();
        let mut cc = [0u8; 20];
        cc[..NCCS].copy_from_slice(&t.cc);
        Self {
            mode_flags: 0,
            iflag: t.iflag,
            oflag: t.oflag,
            cflag: t.cflag,
            lflag: t.lflag,
            cc,
            foreground_pgid: 1,
            session_pgid: 1,
            term_width: 80,
            term_height: 24,
            cursor_col: 0,
            cursor_row: 0,
            cursor_hidden: false,
            input_waker: None,
            ld: None,
            saved_iflag: None,
            saved_oflag: None,
            saved_lflag: None,
        }
    }
}

impl TerminalState {
    /// Largest chunk to offer [`Self::ingest`]. The discipline's queue is
    /// 8 KiB; ingesting only when no completed line is waiting means the queue
    /// is empty, so a chunk this size always fits.
    pub const INGEST_CHUNK: usize = 4096;

    /// The session's shell is `pid`: its group is both the session group and the
    /// initial foreground group.
    pub fn set_session_leader(&mut self, pid: u32) {
        self.session_pgid = pid;
        self.foreground_pgid = pid;
    }

    /// Does a terminal signal to the foreground group spare the group leader?
    /// Only while the foreground group is the session shell's own.
    #[must_use]
    pub const fn spares_leader(&self) -> bool {
        self.foreground_pgid == self.session_pgid
    }

    /// Sets a waker to be notified when input is available.
    pub fn set_input_waker(&mut self, waker: core::task::Waker) {
        self.input_waker = Some(waker);
    }

    /// Take the parked reader's waker, if any. The caller fires it **after**
    /// dropping the terminal-state guard.
    pub fn take_input_waker(&mut self) -> Option<core::task::Waker> {
        self.input_waker.take()
    }

    #[must_use]
    pub const fn is_canonical(&self) -> bool {
        (self.lflag & mode_flags::ICANON) != 0
    }

    #[must_use]
    pub const fn needs_onlcr(&self) -> bool {
        (self.oflag & mode_flags::ONLCR) != 0
    }

    /// Apply ICRNL mapping in-place: CR -> NL. For the **raw** read path; the
    /// canonical discipline maps CR/NL itself, per byte.
    pub fn map_cr_to_nl(&self, data: &mut [u8]) {
        if (self.iflag & mode_flags::ICRNL) != 0 {
            for byte in data.iter_mut() {
                if *byte == b'\r' {
                    *byte = b'\n';
                }
            }
        }
    }

    /// Run `data` through the canonical discipline: line editing, echo (handed
    /// to `echo` in chunks, no allocation), `ISIG` keys, `^D`.
    ///
    /// The caller (glue's `read(stdin)`) offers at most [`Self::INGEST_CHUNK`]
    /// bytes and only when [`Self::has_line`] is false, then calls
    /// [`Self::read_line`].
    pub fn ingest(&mut self, data: &[u8], mut echo: impl FnMut(&[u8])) -> Result<Ingest, NoMemory> {
        let t = self.termios();
        if self.ld.is_none() {
            self.ld = Some(LineDiscipline::try_new().ok_or(NoMemory)?);
        }
        let ld = self.ld.as_mut().ok_or(NoMemory)?;
        let mut signals = Signals::default();
        let consumed = ld.receive_all(&t, data, &mut signals);
        let mut buf = [0u8; 256];
        loop {
            let n = ld.read_output(&mut buf);
            if n == 0 {
                break;
            }
            echo(&buf[..n]);
        }
        Ok(Ingest { consumed, signals })
    }

    /// Take one completed line (at most `out.len()` bytes of it).
    pub fn read_line(&mut self, out: &mut [u8]) -> CanonRead {
        let t = self.termios();
        let Some(ld) = self.ld.as_mut() else { return CanonRead::Empty };
        match ld.read_input(&t, out, false) {
            SlaveRead::Data(0) if out.is_empty() => CanonRead::Empty,
            SlaveRead::Data(0) => CanonRead::Eof,
            SlaveRead::Data(n) => CanonRead::Data(n),
            _ => CanonRead::Empty,
        }
    }

    /// A completed line is waiting to be read.
    #[must_use]
    pub fn has_line(&self) -> bool {
        self.ld.as_ref().is_some_and(LineDiscipline::has_line)
    }

    /// Text is being edited (no terminator yet).
    #[must_use]
    pub fn has_partial_line(&self) -> bool {
        self.ld.as_ref().is_some_and(LineDiscipline::has_partial_line)
    }

    /// Make a half-edited line readable as it stands (the channel closed, so no
    /// terminator is coming).
    pub fn flush_partial_line(&mut self) {
        if let Some(ld) = self.ld.as_mut() {
            ld.flush_line();
        }
    }

    /// Discard unread input (`TCSETSF`, `TCSAFLUSH`): the discipline's queue and
    /// edit buffer. The channel's raw bytes are the caller's (`flush_stdin`).
    pub fn flush_input(&mut self) {
        if let Some(ld) = self.ld.as_mut() {
            ld.flush(true, false);
        }
    }

    /// Generate echo bytes for non-canonical mode input.
    /// Returns `None` if echo is disabled.
    #[must_use]
    pub fn echo_noncanon(&self, data: &[u8]) -> Option<alloc::vec::Vec<u8>> {
        if (self.lflag & mode_flags::ECHO) == 0 {
            return None;
        }
        if self.needs_onlcr() {
            let mut buf = alloc::vec::Vec::with_capacity(data.len() * 2);
            for &byte in data {
                if byte == b'\n' {
                    buf.extend_from_slice(b"\r\n");
                } else {
                    buf.push(byte);
                }
            }
            Some(buf)
        } else {
            Some(data.to_vec())
        }
    }

    /// Apply ONLCR translation to output data (`\n` -> `\r\n`).
    /// If ONLCR is not set, returns a copy unchanged. (Pinned: `OPOST` is not
    /// consulted — `differential::diverges_output_translation_ignores_opost`.)
    #[must_use]
    pub fn translate_output(&self, data: &[u8]) -> alloc::vec::Vec<u8> {
        if !self.needs_onlcr() {
            return data.to_vec();
        }
        let mut result = alloc::vec::Vec::with_capacity(data.len() + 8);
        for &byte in data {
            if byte == b'\n' {
                result.extend_from_slice(b"\r\n");
            } else {
                result.push(byte);
            }
        }
        result
    }

    /// `TCSETS`/`TCSETSW`/`TCSETSF`: replace the four flag words and the 19
    /// kernel-visible control characters. `cc[19]` has no counterpart in the
    /// kernel layout and keeps its value. **The one writer of the termios words
    /// on a session terminal** besides [`Self::apply_mode_request`].
    pub fn set_termios(&mut self, t: &Termios) -> Transition {
        let was = self.is_canonical();
        self.iflag = t.iflag;
        self.oflag = t.oflag;
        self.cflag = t.cflag;
        self.lflag = t.lflag;
        self.cc[..NCCS].copy_from_slice(&t.cc);
        Transition::between(was, self.is_canonical())
    }

    /// The `TCGETS` image: four flag words, `c_line` 0, 19 control characters.
    #[must_use]
    pub fn termios(&self) -> Termios {
        let mut cc = [0u8; NCCS];
        cc.copy_from_slice(&self.cc[..NCCS]);
        Termios { iflag: self.iflag, oflag: self.oflag, cflag: self.cflag, lflag: self.lflag, line: 0, cc }
    }

    /// A state that starts at `t` instead of [`Default`]'s flags (the amd64
    /// console reports `Termios::console_default()` from `TCGETS`).
    #[must_use]
    pub fn with_termios(t: &Termios) -> Self {
        let mut ts = Self::default();
        let _ = ts.set_termios(t);
        ts
    }

    /// Akuma's `SET_TERMINAL_ATTRIBUTES`: record `mode_flags` and enter
    /// (`RAW_MODE_ENABLE`) or leave raw mode. The other writer of the termios
    /// words; see [`Self::set_termios`].
    pub fn apply_mode_request(&mut self, request: u64) -> Transition {
        let was = self.is_canonical();
        self.mode_flags = request;
        if request & mode_flags::RAW_MODE_ENABLE != 0 {
            self.enter_raw_mode();
        } else {
            self.exit_raw_mode();
        }
        Transition::between(was, self.is_canonical())
    }

    /// The interrupt character while `ISIG` is on: a byte equal to it is not
    /// data, it raises `SIGINT` on the foreground group. `None` with `ISIG`
    /// clear. **Pinned:** a `VINTR` of 0 ("disabled" on Linux) is returned as 0
    /// and matches NUL bytes; `PtyPair` does honour `_POSIX_VDISABLE`.
    #[must_use]
    pub const fn intr_char(&self) -> Option<u8> {
        if self.lflag & mode_flags::ISIG != 0 { Some(self.cc[cc_index::VINTR]) } else { None }
    }

    /// Non-canonical `VMIN == 0` read: `Some(VTIME)` — 0 is "poll, return 0 at
    /// once", otherwise a wait of at most that many tenths of a second. `None`
    /// when the terminal is canonical or `VMIN > 0`.
    #[must_use]
    pub const fn raw_vmin0(&self) -> Option<u8> {
        if !self.is_canonical() && self.cc[cc_index::VMIN] == 0 { Some(self.cc[cc_index::VTIME]) } else { None }
    }

    /// Enter raw mode: save current flags and disable ICRNL, OPOST, ECHO, ICANON.
    #[allow(clippy::missing_const_for_fn)]
    fn enter_raw_mode(&mut self) {
        self.saved_iflag = Some(self.iflag);
        self.saved_oflag = Some(self.oflag);
        self.saved_lflag = Some(self.lflag);
        self.iflag &= !(mode_flags::ICRNL | mode_flags::ECHONL);
        self.oflag &= !mode_flags::OPOST;
        self.lflag &= !(mode_flags::ECHO | mode_flags::ICANON);
    }

    /// Exit raw mode: restore previously saved flags, or fall back to sane defaults.
    #[allow(clippy::missing_const_for_fn)]
    fn exit_raw_mode(&mut self) {
        if let Some(saved) = self.saved_iflag.take() {
            self.iflag = saved;
        }
        if let Some(saved) = self.saved_oflag.take() {
            self.oflag = saved;
        }
        if let Some(saved) = self.saved_lflag.take() {
            self.lflag = saved;
        } else {
            self.oflag |= mode_flags::OPOST | mode_flags::ONLCR;
            self.lflag |= mode_flags::ECHO | mode_flags::ICANON;
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::vec::Vec;

    fn default_ts() -> TerminalState {
        TerminalState::default()
    }

    /// Type `input` at the terminal; returns what was echoed.
    fn type_in(ts: &mut TerminalState, input: &[u8]) -> Vec<u8> {
        let mut echo = Vec::new();
        let mut data = input.to_vec();
        ts.map_cr_to_nl(&mut data);
        for chunk in data.chunks(TerminalState::INGEST_CHUNK) {
            ts.ingest(chunk, |e| echo.extend_from_slice(e)).unwrap();
        }
        echo
    }

    /// Every line a program doing `read(0, buf, 4096)` in a loop would get,
    /// ending at "nothing ready" or EOF.
    fn reads(ts: &mut TerminalState) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            match ts.read_line(&mut buf) {
                CanonRead::Data(n) => out.push(buf[..n].to_vec()),
                CanonRead::Eof => {
                    out.push(Vec::new());
                    break;
                }
                CanonRead::Empty => break,
            }
        }
        out
    }

    // ---- the canonical discipline (now `ld::LineDiscipline`) -------------------

    #[test]
    fn canon_simple_line() {
        let mut ts = default_ts();
        assert_eq!(type_in(&mut ts, b"hello\n"), b"hello\r\n");
        assert_eq!(reads(&mut ts), [b"hello\n".to_vec()]);
    }

    #[test]
    fn engine_is_allocated_lazily() {
        let mut ts = default_ts();
        assert!(ts.ld.is_none(), "a terminal nobody reads costs nothing");
        assert!(!ts.has_line() && !ts.has_partial_line());
        assert_eq!(ts.read_line(&mut [0u8; 8]), CanonRead::Empty);
        type_in(&mut ts, b"x");
        assert!(ts.ld.is_some());
    }

    #[test]
    fn canon_erase_backspace() {
        let mut ts = default_ts();
        type_in(&mut ts, b"helo");
        assert_eq!(type_in(&mut ts, &[0x7F]), b"\x08 \x08");
        type_in(&mut ts, b"lo\n");
        assert_eq!(reads(&mut ts), [b"hello\n".to_vec()]);
    }

    #[test]
    fn canon_erase_on_empty_buffer() {
        let mut ts = default_ts();
        assert!(type_in(&mut ts, &[0x7F]).is_empty());
        assert!(!ts.has_line() && !ts.has_partial_line());
    }

    #[test]
    fn canon_kill_rubs_the_line_out() {
        let mut ts = default_ts();
        type_in(&mut ts, b"hello");
        assert_eq!(type_in(&mut ts, &[0x15]).len(), 5 * 3); // 5 * "\x08 \x08"
        assert!(!ts.has_partial_line());
    }

    #[test]
    fn canon_eof_on_empty_is_a_zero_length_read() {
        let mut ts = default_ts();
        type_in(&mut ts, &[0x04]);
        assert_eq!(ts.read_line(&mut [0u8; 8]), CanonRead::Eof);
    }

    #[test]
    fn canon_eof_with_data_ends_the_line_without_eof() {
        let mut ts = default_ts();
        type_in(&mut ts, b"hi\x04");
        assert_eq!(reads(&mut ts), [b"hi".to_vec()], "the data; the next read would block");
    }

    #[test]
    fn canon_newline_echo_without_onlcr() {
        let mut ts = default_ts();
        ts.oflag &= !mode_flags::ONLCR;
        assert_eq!(type_in(&mut ts, b"x\n"), b"x\n");
    }

    #[test]
    fn canon_no_echo() {
        let mut ts = default_ts();
        ts.lflag &= !mode_flags::ECHO;
        assert!(type_in(&mut ts, b"secret\n").is_empty());
        assert_eq!(reads(&mut ts), [b"secret\n".to_vec()]);
    }

    #[test]
    fn canon_echonl_echoes_newline_only() {
        let mut ts = default_ts();
        ts.lflag &= !mode_flags::ECHO;
        ts.lflag |= mode_flags::ECHONL;
        assert_eq!(type_in(&mut ts, b"pw\n"), b"\r\n");
    }

    #[test]
    fn one_line_per_read() {
        // Linux: a canonical read ends at the end of the line. (The old
        // `drain_canon_ready` returned every completed line at once.)
        let mut ts = default_ts();
        type_in(&mut ts, b"line1\nline2\n");
        assert!(ts.has_line());
        assert_eq!(reads(&mut ts), [b"line1\n".to_vec(), b"line2\n".to_vec()]);
    }

    #[test]
    fn a_short_read_leaves_the_rest_of_the_line() {
        let mut ts = default_ts();
        type_in(&mut ts, b"hello world\n");
        let mut five = [0u8; 5];
        assert_eq!(ts.read_line(&mut five), CanonRead::Data(5));
        assert_eq!(&five, b"hello");
        assert_eq!(reads(&mut ts), [b" world\n".to_vec()]);
    }

    #[test]
    fn partial_line_is_capped_at_max_canon() {
        // Without the cap a peer that writes to a tty in canonical mode and never
        // sends `\n` grows kernel heap without bound.
        let mut ts = default_ts();
        type_in(&mut ts, &[b'x'; 10_000]);
        assert!(ts.has_partial_line() && !ts.has_line());
    }

    #[test]
    fn capped_line_can_still_be_terminated() {
        let mut ts = default_ts();
        type_in(&mut ts, &[b'x'; 10_000]);
        type_in(&mut ts, b"\n");
        let got = reads(&mut ts);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].len(), MAX_CANON + 1, "truncated line plus its \\n");
        assert_eq!(*got[0].last().unwrap(), b'\n');
        assert!(!ts.has_partial_line());
    }

    #[test]
    fn closing_the_channel_flushes_the_partial_line() {
        let mut ts = default_ts();
        type_in(&mut ts, b"partial");
        assert!(!ts.has_line());
        ts.flush_partial_line();
        assert_eq!(reads(&mut ts), [b"partial".to_vec()]);
    }

    #[test]
    fn flush_input_discards_ready_and_partial() {
        let mut ts = default_ts();
        type_in(&mut ts, b"one\ntwo");
        ts.flush_input();
        assert!(!ts.has_line() && !ts.has_partial_line());
    }

    #[test]
    fn chunk_of_ingest_chunk_lines_always_fits() {
        let mut ts = default_ts();
        let input: Vec<u8> = b"ab\n".iter().cycle().take(TerminalState::INGEST_CHUNK).copied().collect();
        let mut consumed = 0;
        let r = ts.ingest(&input, |_| {}).unwrap();
        consumed += r.consumed;
        assert_eq!(consumed, input.len());
        // 1365 whole `ab\n` lines; the last byte is the start of a 1366th.
        assert_eq!(reads(&mut ts).len(), TerminalState::INGEST_CHUNK / 3);
        assert!(ts.has_partial_line());
    }

    #[test]
    fn isig_quit_is_consumed_and_reported() {
        // Behaviour change 2026-10-19: `^\` used to be queued as data on a session
        // terminal. The discipline raises SIGQUIT, flushes what was typed first
        // (no NOFLSH) and echoes `^\\`.
        let mut ts = default_ts();
        type_in(&mut ts, b"abc");
        let mut echo = Vec::new();
        let r = ts.ingest(&[0x1C], |e| echo.extend_from_slice(e)).unwrap();
        assert!(r.signals.contains(crate::termios::sig::SIGQUIT));
        assert_eq!(echo, b"^\\");
        assert!(!ts.has_partial_line(), "flushed");
    }

    #[test]
    fn word_erase_and_literal_next_now_work() {
        let mut ts = default_ts();
        ts.cc[crate::termios::cc::VWERASE] = 0x17;
        ts.cc[crate::termios::cc::VLNEXT] = 0x16;
        type_in(&mut ts, b"foo bar\x17\n");
        assert_eq!(reads(&mut ts), [b"foo \n".to_vec()]);
    }

    #[test]
    fn ctrl_chars_echo_as_caret_notation() {
        let mut ts = default_ts();
        assert_eq!(type_in(&mut ts, &[0x01]), b"^A");
    }

    #[test]
    fn backspace_0x08_is_data_unless_verase() {
        // The old discipline hard-coded ^H as erase.
        let mut ts = default_ts();
        type_in(&mut ts, b"ab\x08c\n");
        assert_eq!(reads(&mut ts), [b"ab\x08c\n".to_vec()]);
    }

    #[test]
    fn mode_change_keeps_a_half_typed_line() {
        // The incident shape: input sits in the edit buffer (not ready), the
        // terminal goes raw and back, and the buffer is *still there* — only a
        // line terminator releases it. Nothing in a mode change flushes it.
        let mut ts = default_ts();
        type_in(&mut ts, b"abc");
        let _ = ts.apply_mode_request(mode_flags::RAW_MODE_ENABLE);
        let _ = ts.apply_mode_request(mode_flags::RAW_MODE_DISABLE);
        assert!(ts.has_partial_line() && !ts.has_line(), "nothing readable until a newline");
        type_in(&mut ts, b"\n");
        assert_eq!(reads(&mut ts), [b"abc\n".to_vec()]);
    }

    // ---- unchanged from before the engine swap ---------------------------------

    #[test]
    fn default_is_canonical_with_echo() {
        let ts = default_ts();
        assert!(ts.is_canonical());
        assert!((ts.lflag & mode_flags::ECHO) != 0);
        assert!(ts.needs_onlcr());
    }
    #[test]
    fn icrnl_maps_cr_to_nl() {
        let ts = default_ts();
        let mut buf = *b"abc\rdef\r";
        ts.map_cr_to_nl(&mut buf);
        assert_eq!(&buf, b"abc\ndef\n");
    }
    #[test]
    fn icrnl_disabled_leaves_cr() {
        let mut ts = default_ts();
        ts.iflag &= !mode_flags::ICRNL;
        let mut buf = *b"abc\rdef\r";
        ts.map_cr_to_nl(&mut buf);
        assert_eq!(&buf, b"abc\rdef\r");
    }
    #[test]
    fn noncanon_echo_with_onlcr() {
        let mut ts = default_ts();
        ts.lflag &= !mode_flags::ICANON;
        let echo = ts.echo_noncanon(b"a\nb").unwrap();
        assert_eq!(echo, b"a\r\nb");
    }
    #[test]
    fn noncanon_echo_disabled() {
        let mut ts = default_ts();
        ts.lflag &= !(mode_flags::ICANON | mode_flags::ECHO);
        assert!(ts.echo_noncanon(b"abc").is_none());
    }
    #[test]
    fn output_onlcr_translation() {
        let ts = default_ts();
        let out = ts.translate_output(b"line1\nline2\n");
        assert_eq!(out, b"line1\r\nline2\r\n");
    }
    #[test]
    fn output_no_onlcr() {
        let mut ts = default_ts();
        ts.oflag &= !mode_flags::ONLCR;
        let out = ts.translate_output(b"line1\nline2\n");
        assert_eq!(out, b"line1\nline2\n");
    }
    #[test]
    fn enter_exit_raw_mode_roundtrip() {
        let mut ts = default_ts();
        let orig_iflag = ts.iflag;
        let orig_oflag = ts.oflag;
        let orig_lflag = ts.lflag;

        ts.enter_raw_mode();
        assert!(!ts.is_canonical());
        assert!((ts.lflag & mode_flags::ECHO) == 0);
        assert!((ts.oflag & mode_flags::OPOST) == 0);

        ts.exit_raw_mode();
        assert_eq!(ts.iflag, orig_iflag);
        assert_eq!(ts.oflag, orig_oflag);
        assert_eq!(ts.lflag, orig_lflag);
    }
    #[test]
    fn exit_raw_mode_without_enter_falls_back_to_defaults() {
        let mut ts = default_ts();
        ts.lflag = 0;
        ts.oflag = 0;
        ts.exit_raw_mode();
        assert!(ts.is_canonical());
        assert!((ts.lflag & mode_flags::ECHO) != 0);
        assert!(ts.needs_onlcr());
    }
    #[test]
    fn bare_cr_is_data_until_icrnl_maps_it() {
        // A raw-mode TUI's Enter is a bare `\r`. Cooked, ICRNL turns it into the
        // `\n` that releases the line; raw (ICRNL off) it stays `\r` and never
        // does — the stuck-input symptom when something re-cooks the terminal.
        let mut ts = default_ts();
        ts.enter_raw_mode();
        let mut b = *b"\r";
        ts.map_cr_to_nl(&mut b);
        assert_eq!(&b, b"\r");
        ts.exit_raw_mode();
        ts.map_cr_to_nl(&mut b);
        assert_eq!(&b, b"\n");
    }
    #[test]
    fn double_enter_raw_loses_the_cooked_flags() {
        // PINNED DIVERGENCE: the second `enter_raw_mode` saves the already-raw
        // flags over the cooked ones, so the following exit "restores" raw.
        let mut ts = default_ts();
        ts.enter_raw_mode();
        ts.enter_raw_mode();
        ts.exit_raw_mode();
        assert!(!ts.is_canonical());
    }
    #[test]
    fn second_exit_raw_mode_falls_back_to_defaults() {
        // The saved flags are consumed by the first exit; a second one (a
        // program that cooks twice) ORs the sane defaults back in.
        let mut ts = default_ts();
        ts.enter_raw_mode();
        ts.exit_raw_mode();
        ts.lflag = 0;
        ts.oflag = 0;
        ts.exit_raw_mode();
        assert!(ts.is_canonical());
        assert!(ts.needs_onlcr());
    }
    #[test]
    fn enter_raw_mode_clears_exactly_four_bits() {
        let mut ts = default_ts();
        let (i0, o0, l0) = (ts.iflag, ts.oflag, ts.lflag);
        ts.enter_raw_mode();
        assert_eq!(ts.iflag, i0 & !(mode_flags::ICRNL | mode_flags::ECHONL));
        assert_eq!(ts.oflag, o0 & !mode_flags::OPOST);
        assert_eq!(ts.lflag, l0 & !(mode_flags::ECHO | mode_flags::ICANON));
        // ISIG is deliberately left on: `^C` still raises SIGINT in raw mode.
        assert_ne!(ts.lflag & mode_flags::ISIG, 0);
    }
    #[test]
    fn default_control_characters() {
        let ts = default_ts();
        assert_eq!(ts.cc[cc_index::VINTR], 0x03);
        assert_eq!(ts.cc[cc_index::VQUIT], 0x1C);
        assert_eq!(ts.cc[cc_index::VERASE], 0x7F);
        assert_eq!(ts.cc[cc_index::VKILL], 0x15);
        assert_eq!(ts.cc[cc_index::VEOF], 0x04);
        assert_eq!((ts.cc[cc_index::VMIN], ts.cc[cc_index::VTIME]), (1, 0));
        assert_eq!((ts.iflag, ts.oflag, ts.cflag, ts.lflag), (0x4500, 0x5, 0xBF, 0x8A3B));
    }

    // ---- the funnel: set_termios / apply_mode_request / intr_char / raw_vmin0
    #[test]
    fn set_termios_reports_the_cooked_edge() {
        let mut ts = default_ts();
        let mut raw = ts.termios();
        raw.lflag &= !(mode_flags::ICANON | mode_flags::ECHO);
        assert_eq!(ts.set_termios(&raw), Transition::Raw);
        assert!(!ts.is_canonical());
        assert_eq!(ts.set_termios(&raw), Transition::Same);
        let mut cooked = raw;
        cooked.lflag |= mode_flags::ICANON;
        assert!(ts.set_termios(&cooked).is_cooked());
    }
    #[test]
    fn set_termios_keeps_cc19_and_roundtrips() {
        let mut ts = default_ts();
        ts.cc[19] = 0xAB;
        let mut t = ts.termios();
        t.cc[cc_index::VMIN] = 0;
        t.cc[cc_index::VTIME] = 5;
        let _ = ts.set_termios(&t);
        assert_eq!(ts.cc[19], 0xAB);
        assert_eq!(ts.termios(), t);
    }
    #[test]
    fn apply_mode_request_matches_enter_exit() {
        let mut ts = default_ts();
        assert_eq!(ts.apply_mode_request(mode_flags::RAW_MODE_ENABLE), Transition::Raw);
        assert_eq!(ts.mode_flags, mode_flags::RAW_MODE_ENABLE);
        assert!(ts.apply_mode_request(mode_flags::RAW_MODE_DISABLE).is_cooked());
        assert_eq!(ts.apply_mode_request(mode_flags::RAW_MODE_DISABLE), Transition::Same);
    }
    #[test]
    fn tcsets_that_recooks_is_a_cooked_transition_even_after_set_raw() {
        // The incident shape: raw via SET_TERMINAL_ATTRIBUTES, then a helper's
        // TCSETS with ICANON set. Both go through the same bit.
        let mut ts = default_ts();
        let _ = ts.apply_mode_request(mode_flags::RAW_MODE_ENABLE);
        let mut t = ts.termios();
        t.lflag |= mode_flags::ICANON;
        assert!(ts.set_termios(&t).is_cooked());
    }
    #[test]
    fn intr_char_follows_isig_and_vintr() {
        let mut ts = default_ts();
        assert_eq!(ts.intr_char(), Some(0x03));
        ts.cc[cc_index::VINTR] = 0x1F;
        assert_eq!(ts.intr_char(), Some(0x1F));
        ts.lflag &= !mode_flags::ISIG;
        assert_eq!(ts.intr_char(), None);
        // Raw mode leaves ISIG alone, so ^C still interrupts.
        let mut ts = default_ts();
        let _ = ts.apply_mode_request(mode_flags::RAW_MODE_ENABLE);
        assert_eq!(ts.intr_char(), Some(0x03));
    }
    #[test]
    fn intr_char_zero_is_not_treated_as_disabled() {
        // PINNED DIVERGENCE (PtyPair honours _POSIX_VDISABLE; this does not).
        let mut ts = default_ts();
        ts.cc[cc_index::VINTR] = 0;
        assert_eq!(ts.intr_char(), Some(0));
    }
    #[test]
    fn raw_vmin0_only_when_raw_and_vmin_zero() {
        let mut ts = default_ts();
        assert_eq!(ts.raw_vmin0(), None);
        ts.cc[cc_index::VMIN] = 0;
        ts.cc[cc_index::VTIME] = 7;
        assert_eq!(ts.raw_vmin0(), None, "canonical: VMIN is ignored");
        ts.lflag &= !mode_flags::ICANON;
        assert_eq!(ts.raw_vmin0(), Some(7));
        ts.cc[cc_index::VMIN] = 1;
        assert_eq!(ts.raw_vmin0(), None);
    }
    #[test]
    fn with_termios_starts_at_the_console_default() {
        let ts = TerminalState::with_termios(&Termios::console_default());
        assert_eq!((ts.iflag, ts.oflag, ts.cflag, ts.lflag), (0x4500, 0x5, 0xBF, 0x8A3B));
        assert_eq!(ts.cc[10], 0x1A);
        assert!(ts.is_canonical());
    }
    #[test]
    fn terminal_signals_spare_only_the_shells_own_group_leader() {
        let mut ts = default_ts();
        ts.set_session_leader(40);
        assert_eq!((ts.session_pgid, ts.foreground_pgid), (40, 40));
        assert!(ts.spares_leader(), "^C at the prompt must not kill the shell");
        ts.foreground_pgid = 57; // a job-control shell moved `sleep` (pid 57, its own group) to the front
        assert!(!ts.spares_leader(), "the leader of the job's group is the job");
        ts.foreground_pgid = 40;
        assert!(ts.spares_leader());
        assert!(default_ts().spares_leader(), "console default: group 1 is init");
    }

    #[test]
    fn input_waker_is_taken_once() {
        let mut ts = default_ts();
        assert!(ts.take_input_waker().is_none());
        ts.set_input_waker(core::task::Waker::noop().clone());
        assert!(ts.take_input_waker().is_some());
        assert!(ts.take_input_waker().is_none());
    }
}
