use alloc::collections::{BTreeMap, VecDeque};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use spinning_top::Spinlock;

use crate::runtime::{config, with_irqs_disabled};
use akuma_terminal as terminal;

/// Channel for streaming process output between threads
///
/// Used to pass output from a process running on a user thread
/// to the async shell that spawned it.
pub struct ProcessChannel {
    /// Output buffer (spinlock-protected for thread safety)
    buffer: Spinlock<VecDeque<u8>>,
    /// Stdin buffer for interactive input (SSH -> process)
    stdin_buffer: Spinlock<VecDeque<u8>>,
    /// Exit code (set when process exits)
    exit_code: AtomicI32,
    /// Whether the process has exited
    exited: AtomicBool,
    /// Interrupt signal (set by Ctrl+C, checked by process)
    interrupted: AtomicBool,
    /// Raw mode flag (true if terminal is in raw mode, false for cooked)
    raw_mode: AtomicBool,
    /// Stdin closed flag (true if no more data will be written to stdin)
    stdin_closed: AtomicBool,
    /// Thread ID of a blocking reader waiting for output
    reader_thread: Spinlock<Option<usize>>,
}

/// Maximum size for process channel buffers to prevent memory exhaustion (1 MB)
const MAX_BUFFER_SIZE: usize = 1024 * 1024;

impl ProcessChannel {
    /// Create a new empty process channel
    pub fn new() -> Self {
        Self {
            buffer: Spinlock::new(VecDeque::new()),
            stdin_buffer: Spinlock::new(VecDeque::new()),
            exit_code: AtomicI32::new(0),
            exited: AtomicBool::new(false),
            interrupted: AtomicBool::new(false),
            raw_mode: AtomicBool::new(false),
            stdin_closed: AtomicBool::new(false),
            reader_thread: Spinlock::new(None),
        }
    }

    /// Mark stdin as closed (no more data will be arriving)
    pub fn close_stdin(&self) {
        self.stdin_closed.store(true, Ordering::Release);
    }

    /// Check if stdin is closed
    pub fn is_stdin_closed(&self) -> bool {
        self.stdin_closed.load(Ordering::Acquire)
    }

    /// Write data to the channel buffer (stdout from process)
    pub fn write(&self, data: &[u8]) {
        if data.is_empty() { return; }

        // Copy data from userspace BEFORE the critical section
        // to prevent page faults while holding a spinlock.
        let mut kernel_copy = Vec::with_capacity(data.len());
        kernel_copy.extend_from_slice(data);

        if config().syscall_debug_info_enabled {
            let sn_len = kernel_copy.len().min(32);
            let mut snippet = [0u8; 32];
            let n = sn_len.min(snippet.len());
            snippet[..n].copy_from_slice(&kernel_copy[..n]);
            for byte in &mut snippet[..n] {
                if *byte < 32 || *byte > 126 { *byte = b'.'; }
            }
            let snippet_str = core::str::from_utf8(&snippet[..n]).unwrap_or("...");
            log::debug!("[ProcessChannel] Write {} bytes to stdout \"{}\"", kernel_copy.len(), snippet_str);
        }

        // CRITICAL: Disable IRQs while holding the lock!
        with_irqs_disabled(|| {
            let mut buf = self.buffer.lock();
            
            // Check for buffer overflow
            if buf.len() + kernel_copy.len() > MAX_BUFFER_SIZE {
                // If the write itself is larger than the buffer, truncate it
                let data_to_write = if kernel_copy.len() > MAX_BUFFER_SIZE {
                    &kernel_copy[kernel_copy.len() - MAX_BUFFER_SIZE..]
                } else {
                    &kernel_copy
                };
                
                // Remove old data to make room
                let current_len = buf.len();
                let overflow = (current_len + data_to_write.len()).saturating_sub(MAX_BUFFER_SIZE);
                if overflow > 0 {
                    buf.drain(..overflow.min(current_len));
                }
                buf.extend(data_to_write);
            } else {
                buf.extend(&kernel_copy);
            }
        });

        // Wake any blocking reader waiting for this data
        if let Some(tid) = self.reader_thread.lock().take() {
            crate::threading::get_waker_for_thread(tid).wake();
        }
    }

    /// Read available data from the channel (non-blocking)
    /// Returns None if no data is available
    pub fn try_read(&self) -> Option<Vec<u8>> {
        with_irqs_disabled(|| {
            let mut buf = self.buffer.lock();
            if buf.is_empty() {
                None
            } else {
                Some(buf.drain(..).collect())
            }
        })
    }

    /// Read available data from the channel into a buffer
    /// Returns number of bytes read
    pub fn read(&self, buf: &mut [u8]) -> usize {
        let n = with_irqs_disabled(|| {
            let mut buffer = self.buffer.lock();
            let to_read = buf.len().min(buffer.len());
            for (i, byte) in buffer.drain(..to_read).enumerate() {
                buf[i] = byte;
            }
            if to_read > 0 && config().syscall_debug_info_enabled {
                log::debug!("[ProcessChannel] Read {} bytes from stdout", to_read);
            }
            to_read
        });

        if n == 0 {
            // Register current thread as reader so it can be woken by next write
            *self.reader_thread.lock() = Some(crate::threading::current_thread_id());
        }
        n
    }

    pub fn has_stdout_data(&self) -> bool {
        !self.buffer.lock().is_empty()
    }

    /// Read all remaining data from the channel
    pub fn read_all(&self) -> Vec<u8> {
        with_irqs_disabled(|| {
            let mut buf = self.buffer.lock();
            buf.drain(..).collect()
        })
    }

    /// Write data to stdin buffer (SSH -> process)
    pub fn write_stdin(&self, data: &[u8]) {
        with_irqs_disabled(|| {
            let mut buf = self.stdin_buffer.lock();
            
            // Check for buffer overflow
            if buf.len() + data.len() > MAX_BUFFER_SIZE {
                let data_to_write = if data.len() > MAX_BUFFER_SIZE {
                    &data[data.len() - MAX_BUFFER_SIZE..]
                } else {
                    data
                };
                
                let current_len = buf.len();
                let overflow = (current_len + data_to_write.len()).saturating_sub(MAX_BUFFER_SIZE);
                if overflow > 0 {
                    buf.drain(..overflow.min(current_len));
                }
                buf.extend(data_to_write);
            } else {
                buf.extend(data);
            }
        })
    }

    /// Read from stdin buffer (process reads from SSH input)
    /// Returns number of bytes read into buf
    pub fn read_stdin(&self, buf: &mut [u8]) -> usize {
        with_irqs_disabled(|| {
            let mut stdin = self.stdin_buffer.lock();
            let to_read = buf.len().min(stdin.len());
            for (i, byte) in stdin.drain(..to_read).enumerate() {
                buf[i] = byte;
            }
            to_read
        })
    }

    /// Check if stdin has data available
    pub fn has_stdin_data(&self) -> bool {
        with_irqs_disabled(|| {
            !self.stdin_buffer.lock().is_empty()
        })
    }

    /// Return the number of bytes available in the stdin buffer
    pub fn stdin_bytes_available(&self) -> usize {
        with_irqs_disabled(|| {
            self.stdin_buffer.lock().len()
        })
    }

    /// Clear all pending data from the stdin buffer
    pub fn flush_stdin(&self) {
        with_irqs_disabled(|| {
            self.stdin_buffer.lock().clear();
        })
    }

    /// Mark the process as exited with the given exit code
    pub fn set_exited(&self, code: i32) {
        self.exit_code.store(code, Ordering::Release);
        self.exited.store(true, Ordering::Release);
    }

    /// Check if the process has exited
    pub fn has_exited(&self) -> bool {
        self.exited.load(Ordering::Acquire)
    }

    /// Get the exit code (only valid after has_exited() returns true)
    pub fn exit_code(&self) -> i32 {
        self.exit_code.load(Ordering::Acquire)
    }

    /// Set the interrupt flag (called when Ctrl+C is pressed)
    pub fn set_interrupted(&self) {
        self.interrupted.store(true, Ordering::Release);
    }

    /// Check if the process has been interrupted
    pub fn is_interrupted(&self) -> bool {
        self.interrupted.load(Ordering::Acquire)
    }

    /// Clear the interrupt flag
    pub fn clear_interrupted(&self) {
        self.interrupted.store(false, Ordering::Release);
    }

    /// Set the raw mode flag
    pub fn set_raw_mode(&self, enabled: bool) {
        self.raw_mode.store(enabled, Ordering::Release);
    }

    /// Check if raw mode is enabled
    pub fn is_raw_mode(&self) -> bool {
        self.raw_mode.load(Ordering::Acquire)
    }
}

impl Default for ProcessChannel {
    fn default() -> Self {
        Self::new()
    }
}

/// Global registry mapping thread IDs to their process channels
static PROCESS_CHANNELS: Spinlock<BTreeMap<usize, Arc<ProcessChannel>>> =
    Spinlock::new(BTreeMap::new());

/// Global registry mapping thread IDs to their shared terminal states
static TERMINAL_STATES: Spinlock<BTreeMap<usize, Arc<Spinlock<terminal::TerminalState>>>> =
    Spinlock::new(BTreeMap::new());

/// Register a process channel for a thread
pub fn register_channel(thread_id: usize, channel: Arc<ProcessChannel>) {
    with_irqs_disabled(|| {
        PROCESS_CHANNELS.lock().insert(thread_id, channel);
    })
}

/// Register a terminal state for a thread
pub fn register_terminal_state(thread_id: usize, state: Arc<Spinlock<terminal::TerminalState>>) {
    with_irqs_disabled(|| {
        TERMINAL_STATES.lock().insert(thread_id, state);
    })
}

/// Register a process channel for a system thread (one that doesn't have a Process struct)
pub fn register_system_thread_channel(thread_id: usize, channel: Arc<ProcessChannel>) {
    with_irqs_disabled(|| {
        PROCESS_CHANNELS.lock().insert(thread_id, channel);
    });
}

/// Get the process channel for a thread (if any)
pub fn get_channel(thread_id: usize) -> Option<Arc<ProcessChannel>> {
    with_irqs_disabled(|| {
        PROCESS_CHANNELS.lock().get(&thread_id).cloned()
    })
}

/// Get the terminal state for a thread (if any)
pub fn get_terminal_state(thread_id: usize) -> Option<Arc<Spinlock<terminal::TerminalState>>> {
    with_irqs_disabled(|| {
        TERMINAL_STATES.lock().get(&thread_id).cloned()
    })
}

/// Remove and return the process channel for a thread
pub fn remove_channel(thread_id: usize) -> Option<Arc<ProcessChannel>> {
    with_irqs_disabled(|| {
        PROCESS_CHANNELS.lock().remove(&thread_id)
    })
}

/// Remove and return the terminal state for a thread
pub fn remove_terminal_state(thread_id: usize) -> Option<Arc<Spinlock<terminal::TerminalState>>> {
    with_irqs_disabled(|| {
        TERMINAL_STATES.lock().remove(&thread_id)
    })
}
