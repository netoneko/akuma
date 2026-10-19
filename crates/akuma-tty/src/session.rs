//! The console / ssh-session line discipline: [`TerminalState`].

extern crate alloc;

use alloc::collections::VecDeque;

use crate::cc_index;
use crate::mode_flags;
pub use crate::termios::MAX_CANON;
use crate::termios::{Termios, Transition, cc::NCCS};

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

    /// Current foreground process group ID
    pub foreground_pgid: u32,

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
    /// `ProcessChannel::write_stdin` (and the amd64 console pump), and the
    /// canonical line discipline below (`canon_buffer`/`canon_ready`) is applied
    /// on read. A second byte queue on this struct (`input_buffer`, with
    /// `push_input`/`read_input`) was removed 2026-10-17 as an unwired duplicate
    /// view of stdin (`docs/archive/POST_AMD64_CODE_AUDIT.md` §1.1).
    ///
    /// Plain field since 2026-10-19: it was a second `Spinlock` nested inside
    /// the `Arc<Spinlock<TerminalState>>` every consumer already held, which
    /// bought nothing and made this crate depend on `spinning_top`. Callers take
    /// the outer guard with `akuma_exec::sync::lock_bounded` (one `try_lock`
    /// attempt at a time, never across an unbounded wait —
    /// `docs/archive/TERM_POLL_INPUT_PREEMPTION_FIX.md`) and use
    /// [`Self::set_input_waker`] / [`Self::take_input_waker`].
    input_waker: Option<core::task::Waker>,

    /// Canonical mode line buffer (current line being edited).
    ///
    /// Bounded by [`MAX_CANON`]: this grows one byte per keystroke and is only
    /// drained by a line terminator, so without a cap a peer that writes to a
    /// tty in canonical mode and never sends `\n` grows kernel heap without
    /// limit. Linux's N_TTY has the same ceiling for the same reason.
    pub canon_buffer: alloc::vec::Vec<u8>,
    /// Canonical mode ready buffer (completed lines awaiting read)
    pub canon_ready: VecDeque<u8>,

    /// Flags saved before the last `RAW_MODE_ENABLE` call, for exact restoration.
    pub saved_iflag: Option<u32>,
    pub saved_oflag: Option<u32>,
    pub saved_lflag: Option<u32>,
}

impl Default for TerminalState {
    fn default() -> Self {
        let mut cc = [0u8; 20];
        cc[cc_index::VMIN] = 1;
        cc[cc_index::VTIME] = 0;
        cc[cc_index::VERASE] = 0x7F;
        cc[cc_index::VEOF] = 0x04;
        cc[cc_index::VINTR] = 0x03;
        cc[cc_index::VQUIT] = 0x1C;
        cc[cc_index::VKILL] = 0x15;

        Self {
            mode_flags: 0,
            iflag: mode_flags::ICRNL | mode_flags::IXON,
            oflag: mode_flags::OPOST | mode_flags::ONLCR,
            cflag: 0,
            lflag: mode_flags::ICANON | mode_flags::ECHO | mode_flags::ISIG | mode_flags::ECHOE | mode_flags::ECHOK,
            cc,
            foreground_pgid: 1,
            term_width: 80,
            term_height: 24,
            cursor_col: 0,
            cursor_row: 0,
            cursor_hidden: false,
            input_waker: None,
            canon_buffer: alloc::vec::Vec::new(),
            canon_ready: VecDeque::new(),
            saved_iflag: None,
            saved_oflag: None,
            saved_lflag: None,
        }
    }
}

/// Result of processing input through the canonical-mode line discipline.
pub struct ProcessedInput {
    /// Bytes to echo back to the terminal.
    pub echo: alloc::vec::Vec<u8>,
    /// True if EOF was signaled (Ctrl+D on empty canonical buffer).
    pub eof: bool,
}

impl TerminalState {
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

    /// Apply ICRNL mapping in-place: CR -> NL.
    pub fn map_cr_to_nl(&self, data: &mut [u8]) {
        if (self.iflag & mode_flags::ICRNL) != 0 {
            for byte in data.iter_mut() {
                if *byte == b'\r' {
                    *byte = b'\n';
                }
            }
        }
    }

    /// Process input bytes in canonical mode.
    ///
    /// Handles line editing (erase, kill), line completion (newline),
    /// and EOF (Ctrl+D). Returns echo bytes the caller should write
    /// back to the terminal, and an EOF flag.
    pub fn process_canon_input(&mut self, data: &[u8]) -> ProcessedInput {
        let echo_on = (self.lflag & mode_flags::ECHO) != 0;
        let echoe = (self.lflag & mode_flags::ECHOE) != 0;
        let echonl = (self.lflag & mode_flags::ECHONL) != 0;
        let onlcr = self.needs_onlcr();
        let verase = self.cc[cc_index::VERASE];
        let veof = self.cc[cc_index::VEOF];
        let vkill = self.cc[cc_index::VKILL];

        let mut echo = alloc::vec::Vec::new();
        let mut eof = false;

        for &byte in data {
            if byte == verase || byte == 0x08 {
                if self.canon_buffer.pop().is_some() && echoe {
                    echo.extend_from_slice(b"\x08 \x08");
                }
            } else if byte == vkill && vkill != 0 {
                let erased = self.canon_buffer.len();
                self.canon_buffer.clear();
                if echoe {
                    for _ in 0..erased {
                        echo.extend_from_slice(b"\x08 \x08");
                    }
                }
            } else if byte == veof && veof != 0 {
                if self.canon_buffer.is_empty() {
                    eof = true;
                } else {
                    let line: alloc::vec::Vec<u8> = core::mem::take(&mut self.canon_buffer);
                    self.canon_ready.extend(line);
                }
            } else if byte == b'\n' {
                self.canon_buffer.push(byte);
                if echo_on || echonl {
                    if onlcr {
                        echo.extend_from_slice(b"\r\n");
                    } else {
                        echo.push(b'\n');
                    }
                }
                let line: alloc::vec::Vec<u8> = core::mem::take(&mut self.canon_buffer);
                self.canon_ready.extend(line);
            } else if self.canon_buffer.len() < MAX_CANON {
                self.canon_buffer.push(byte);
                if echo_on {
                    echo.push(byte);
                }
            } else {
                // Line is at MAX_CANON with no terminator yet: drop the byte,
                // and do NOT echo it — echoing input that was discarded would
                // desynchronise the user's view of the line from its contents.
                // The `\n`/VEOF branches above deliberately stay uncapped so a
                // full line can always still be terminated and drained; capping
                // those would wedge the line instead of truncating it.
            }
        }

        ProcessedInput { echo, eof }
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

    /// Drain up to `max` bytes from the canonical ready buffer.
    pub fn drain_canon_ready(&mut self, max: usize) -> alloc::vec::Vec<u8> {
        let to_read = max.min(self.canon_ready.len());
        let mut result = alloc::vec::Vec::with_capacity(to_read);
        for _ in 0..to_read {
            result.push(self.canon_ready.pop_front().unwrap());
        }
        result
    }

    /// Move whatever is in the canonical edit buffer into `canon_ready`.
    /// Used when the channel closes so partial input isn't lost.
    pub fn flush_canon_buffer(&mut self) {
        if !self.canon_buffer.is_empty() {
            let line: alloc::vec::Vec<u8> = core::mem::take(&mut self.canon_buffer);
            self.canon_ready.extend(line);
        }
    }

    /// Apply ONLCR translation to output data (`\n` -> `\r\n`).
    /// If ONLCR is not set, returns a copy unchanged.
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

    fn default_ts() -> TerminalState {
        TerminalState::default()
    }

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
    fn canon_simple_line() {
        let mut ts = default_ts();
        let result = ts.process_canon_input(b"hello\n");
        assert_eq!(result.echo, b"hello\r\n");
        assert!(!result.eof);
        let ready = ts.drain_canon_ready(64);
        assert_eq!(ready, b"hello\n");
    }

    #[test]
    fn canon_erase_backspace() {
        let mut ts = default_ts();
        ts.process_canon_input(b"helo");
        let result = ts.process_canon_input(&[0x7F]); // VERASE
        assert_eq!(result.echo, b"\x08 \x08");
        ts.process_canon_input(b"lo\n");
        let ready = ts.drain_canon_ready(64);
        assert_eq!(ready, b"hello\n");
    }

    #[test]
    fn canon_erase_on_empty_buffer() {
        let mut ts = default_ts();
        let result = ts.process_canon_input(&[0x7F]);
        assert!(result.echo.is_empty());
        assert!(ts.canon_ready.is_empty());
    }

    #[test]
    fn canon_kill_erases_line() {
        let mut ts = default_ts();
        ts.process_canon_input(b"hello");
        let result = ts.process_canon_input(&[0x15]); // Ctrl+U = VKILL
        assert_eq!(result.echo.len(), 5 * 3); // 5 * "\x08 \x08"
        assert!(ts.canon_buffer.is_empty());
    }

    #[test]
    fn canon_eof_on_empty_signals_eof() {
        let mut ts = default_ts();
        let result = ts.process_canon_input(&[0x04]); // Ctrl+D
        assert!(result.eof);
        assert!(ts.canon_ready.is_empty());
    }

    #[test]
    fn canon_eof_with_data_flushes_line() {
        let mut ts = default_ts();
        ts.process_canon_input(b"hi");
        let result = ts.process_canon_input(&[0x04]); // Ctrl+D
        assert!(!result.eof);
        let ready = ts.drain_canon_ready(64);
        assert_eq!(ready, b"hi");
    }

    #[test]
    fn canon_newline_echo_without_onlcr() {
        let mut ts = default_ts();
        ts.oflag &= !mode_flags::ONLCR;
        let result = ts.process_canon_input(b"x\n");
        assert_eq!(result.echo, b"x\n");
    }

    #[test]
    fn canon_no_echo() {
        let mut ts = default_ts();
        ts.lflag &= !mode_flags::ECHO;
        let result = ts.process_canon_input(b"secret\n");
        assert!(result.echo.is_empty());
        let ready = ts.drain_canon_ready(64);
        assert_eq!(ready, b"secret\n");
    }

    #[test]
    fn canon_echonl_echoes_newline_only() {
        let mut ts = default_ts();
        ts.lflag &= !mode_flags::ECHO;
        ts.lflag |= mode_flags::ECHONL;
        let result = ts.process_canon_input(b"pw\n");
        assert_eq!(result.echo, b"\r\n"); // only newline echoed
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
    fn drain_canon_ready_partial() {
        let mut ts = default_ts();
        ts.process_canon_input(b"hello world\n");
        let partial = ts.drain_canon_ready(5);
        assert_eq!(partial, b"hello");
        let rest = ts.drain_canon_ready(64);
        assert_eq!(rest, b" world\n");
    }

    #[test]
    fn canon_buffer_is_capped_at_max_canon() {
        // Without the MAX_CANON cap this asserts 10_000 == 4095 and fails:
        // the buffer grew one byte per keystroke with nothing to stop it, so a
        // peer writing to a tty in canonical mode and never sending `\n` grew
        // kernel heap without bound.
        let mut ts = default_ts();
        ts.process_canon_input(&[b'x'; 10_000]);
        assert_eq!(ts.canon_buffer.len(), MAX_CANON);
        assert!(ts.canon_ready.is_empty(), "no terminator seen, nothing is ready");
    }

    #[test]
    fn capped_line_can_still_be_terminated() {
        // The cap must truncate the line, not wedge it: `\n` is still accepted
        // on a full buffer and still completes the line.
        let mut ts = default_ts();
        ts.process_canon_input(&[b'x'; 10_000]);
        ts.process_canon_input(b"\n");
        assert!(ts.canon_buffer.is_empty(), "terminator must drain the line");
        let ready = ts.drain_canon_ready(usize::MAX);
        assert_eq!(ready.len(), MAX_CANON + 1, "truncated line plus its \\n");
        assert_eq!(*ready.last().unwrap(), b'\n');
    }

    #[test]
    fn flush_canon_buffer_moves_to_ready() {
        let mut ts = default_ts();
        ts.process_canon_input(b"partial");
        assert!(ts.canon_ready.is_empty());
        ts.flush_canon_buffer();
        let ready = ts.drain_canon_ready(64);
        assert_eq!(ready, b"partial");
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
    fn multiple_lines_buffered() {
        let mut ts = default_ts();
        ts.process_canon_input(b"line1\nline2\n");
        let first = ts.drain_canon_ready(6);
        assert_eq!(first, b"line1\n");
        let second = ts.drain_canon_ready(64);
        assert_eq!(second, b"line2\n");
    }

    // ---- characterization: raw <-> canonical (2026-10-19, before the
    // `Transition` refactor). Each pins what the code did at that point.

    #[test]
    fn mode_change_keeps_a_half_typed_line() {
        // The incident shape: input sits in `canon_buffer` (not ready), the
        // terminal goes raw and back, and the buffer is *still there* — only a
        // line terminator releases it. Nothing in a mode change flushes it.
        let mut ts = default_ts();
        ts.process_canon_input(b"abc");
        ts.enter_raw_mode();
        ts.exit_raw_mode();
        assert_eq!(ts.canon_buffer, b"abc");
        assert!(ts.canon_ready.is_empty(), "nothing readable until a newline");
        ts.process_canon_input(b"\n");
        assert_eq!(ts.drain_canon_ready(64), b"abc\n");
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
        assert_eq!((ts.iflag, ts.oflag, ts.cflag, ts.lflag), (0x500, 0x5, 0, 0x3B));
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
        assert_eq!((ts.iflag, ts.oflag, ts.cflag, ts.lflag), (0x500, 0x5, 0xBF, 0x8A3B));
        assert_eq!(ts.cc[10], 0x1A);
        assert!(ts.is_canonical());
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
