#![cfg_attr(not(test), no_std)]

extern crate alloc;

use alloc::collections::VecDeque;
use spinning_top::Spinlock;

/// Mode flags for terminal attributes (similar to termios `c_lflag`)
pub mod mode_flags {
    /// Enable raw mode (disable canonical, echo, ISIG)
    pub const RAW_MODE_ENABLE: u64 = 0x01;
    /// Disable raw mode (restore canonical, echo, ISIG)
    pub const RAW_MODE_DISABLE: u64 = 0x02;

    // Linux-compatible constants for ioctl
    // iflag
    pub const ICRNL: u32  = 0x0000_0100;
    pub const IXON: u32   = 0x0000_0400;

    // oflag
    pub const OPOST: u32  = 0x0000_0001;
    pub const ONLCR: u32  = 0x0000_0004;

    // lflag
    pub const ISIG: u32   = 0x0000_0001;
    pub const ICANON: u32 = 0x0000_0002;
    pub const ECHO: u32   = 0x0000_0008;
    pub const ECHOE: u32  = 0x0000_0010;
    pub const ECHOK: u32  = 0x0000_0020;
    pub const ECHONL: u32 = 0x0000_0040;
}

/// `c_cc` indices (Linux-compatible)
pub mod cc_index {
    pub const VINTR: usize  = 0;
    pub const VQUIT: usize  = 1;
    pub const VERASE: usize = 2;
    pub const VKILL: usize  = 3;
    pub const VEOF: usize   = 4;
    pub const VTIME: usize  = 5;
    pub const VMIN: usize   = 6;
    pub const VEOL: usize   = 11;
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
    /// Input buffer for events (e.g., key presses)
    pub input_buffer: Spinlock<VecDeque<u8>>,
    /// Waker for tasks waiting on input
    pub input_waker: Spinlock<Option<core::task::Waker>>,

    /// Canonical mode line buffer (current line being edited)
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
            input_buffer: Spinlock::new(VecDeque::new()),
            input_waker: Spinlock::new(None),
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
    /// Pushes data into the input buffer.
    pub fn push_input(&self, data: &[u8]) {
        let mut buffer = self.input_buffer.lock();
        for &byte in data {
            buffer.push_back(byte);
        }
        drop(buffer);
        let waker = self.input_waker.lock().take();
        if let Some(w) = waker {
            w.wake();
        }
    }

    /// Tries to read data from the input buffer.
    /// Returns number of bytes read.
    pub fn read_input(&self, buf: &mut [u8]) -> usize {
        let mut buffer = self.input_buffer.lock();
        let mut bytes_read = 0;
        for slot in buf.iter_mut() {
            if let Some(byte) = buffer.pop_front() {
                *slot = byte;
                bytes_read += 1;
            } else {
                break;
            }
        }
        bytes_read
    }

    /// Sets a waker to be notified when input is available.
    pub fn set_input_waker(&self, waker: core::task::Waker) {
        *self.input_waker.lock() = Some(waker);
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
                    let line: alloc::vec::Vec<u8> = self.canon_buffer.drain(..).collect();
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
                let line: alloc::vec::Vec<u8> = self.canon_buffer.drain(..).collect();
                self.canon_ready.extend(line);
            } else {
                self.canon_buffer.push(byte);
                if echo_on {
                    echo.push(byte);
                }
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
            let line: alloc::vec::Vec<u8> = self.canon_buffer.drain(..).collect();
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

    /// Enter raw mode: save current flags and disable ICRNL, OPOST, ECHO, ICANON.
    #[allow(clippy::missing_const_for_fn)]
    pub fn enter_raw_mode(&mut self) {
        self.saved_iflag = Some(self.iflag);
        self.saved_oflag = Some(self.oflag);
        self.saved_lflag = Some(self.lflag);
        self.iflag &= !(mode_flags::ICRNL | mode_flags::ECHONL);
        self.oflag &= !mode_flags::OPOST;
        self.lflag &= !(mode_flags::ECHO | mode_flags::ICANON);
    }

    /// Exit raw mode: restore previously saved flags, or fall back to sane defaults.
    #[allow(clippy::missing_const_for_fn)]
    pub fn exit_raw_mode(&mut self) {
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
}
