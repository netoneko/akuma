use alloc::collections::VecDeque;
use crate::std_compat::sync::Mutex;
use alloc::sync::Arc;
use core::task::Waker;

/// Mode flags for terminal attributes (similar to termios c_lflag)
pub mod mode_flags {
    /// Enable raw mode (disable canonical, echo, ISIG)
    pub const RAW_MODE_ENABLE: u64 = 0x01;
    /// Disable raw mode (restore canonical, echo, ISIG)
    pub const RAW_MODE_DISABLE: u64 = 0x02;
    
    // Linux-compatible constants for ioctl
    // iflag
    pub const ICRNL: u32  = 0x00000100;
    pub const IXON: u32   = 0x00000400;
    
    // oflag
    pub const OPOST: u32  = 0x00000001;
    pub const ONLCR: u32  = 0x00000004;

    // lflag
    pub const ISIG: u32   = 0x00000001;
    pub const ICANON: u32 = 0x00000002;
    pub const ECHO: u32   = 0x00000008;
    pub const ECHOE: u32  = 0x00000010;
    pub const ECHOK: u32  = 0x00000020;
    pub const ECHONL: u32 = 0x00000040;
}

/// c_cc indices (Linux-compatible)
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
    pub input_buffer: Mutex<VecDeque<u8>>,
    /// Waker for tasks waiting on input
    pub input_waker: Mutex<Option<core::task::Waker>>,

    /// Canonical mode line buffer (current line being edited)
    pub canon_buffer: alloc::vec::Vec<u8>,
    /// Canonical mode ready buffer (completed lines awaiting read)
    pub canon_ready: VecDeque<u8>,
}

impl Default for TerminalState {
    fn default() -> Self {
        let mut cc = [0u8; 20];
        cc[cc_index::VMIN] = 1;
        cc[cc_index::VTIME] = 0;
        cc[cc_index::VERASE] = 0x7F;
        cc[cc_index::VEOF] = 0x04; // Ctrl+D
        cc[cc_index::VINTR] = 0x03; // Ctrl+C
        cc[cc_index::VQUIT] = 0x1C; // Ctrl+backslash
        cc[cc_index::VKILL] = 0x15; // Ctrl+U
        
        TerminalState {
            mode_flags: 0,
            iflag: mode_flags::ICRNL | mode_flags::IXON,
            oflag: mode_flags::OPOST | mode_flags::ONLCR, // Enable CRLF translation by default
            cflag: 0,
            lflag: mode_flags::ICANON | mode_flags::ECHO | mode_flags::ISIG | mode_flags::ECHOE | mode_flags::ECHOK,
            cc,
            foreground_pgid: 1,
            term_width: 80,
            term_height: 24,
            cursor_col: 0,
            cursor_row: 0,
            cursor_hidden: false,
            input_buffer: Mutex::new(VecDeque::new()),
            input_waker: Mutex::new(None),
            canon_buffer: alloc::vec::Vec::new(),
            canon_ready: VecDeque::new(),
        }
    }
}

impl TerminalState {
    /// Pushes data into the input buffer.
    pub fn push_input(&self, data: &[u8]) {
        let mut buffer = self.input_buffer.lock();
        for &byte in data {
            buffer.push_back(byte);
        }
        // Wake up any task waiting for input
        if let Some(waker) = self.input_waker.lock().take() {
            waker.wake();
        }
    }

    /// Tries to read data from the input buffer.
    /// Returns number of bytes read.
    pub fn read_input(&self, buf: &mut [u8]) -> usize {
        let mut buffer = self.input_buffer.lock();
        let mut bytes_read = 0;
        for i in 0..buf.len() {
            if let Some(byte) = buffer.pop_front() {
                buf[i] = byte;
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
}
