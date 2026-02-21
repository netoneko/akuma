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
    pub const ICANON: u32 = 0x00000002;
    pub const ECHO: u32   = 0x00000008;
    pub const OPOST: u32  = 0x00000001;
    pub const ONLCR: u32  = 0x00000004;
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
}

impl Default for TerminalState {
    fn default() -> Self {
        let mut cc = [0u8; 20];
        cc[6] = 1; // VMIN = 1
        
        TerminalState {
            mode_flags: 0,
            iflag: 0,
            oflag: mode_flags::OPOST | mode_flags::ONLCR, // Enable CRLF translation by default
            cflag: 0,
            lflag: mode_flags::ICANON | mode_flags::ECHO,
            cc,
            cursor_col: 0,
            cursor_row: 0,
            cursor_hidden: false,
            input_buffer: Mutex::new(VecDeque::new()),
            input_waker: Mutex::new(None),
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
