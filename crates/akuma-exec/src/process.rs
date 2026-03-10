//! Process Management
//!
//! Manages user processes including creation, execution, and termination.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::string::ToString;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};

/// Default environment variables for new processes when none are provided.
pub const DEFAULT_ENV: &[&str] = &[
    "PATH=/usr/bin:/bin",
    "HOME=/",
    "TERM=xterm",
];

/// A future that yields once then completes
/// This allows proper async yielding in poll_fn contexts
pub struct YieldOnce(bool);

impl YieldOnce {
    pub fn new() -> Self {
        YieldOnce(false)
    }
}

impl Future for YieldOnce {
    type Output = ();
    
    fn poll(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<()> {
        if self.0 {
            Poll::Ready(())
        } else {
            self.0 = true;
            Poll::Pending
        }
    }
}
use spinning_top::Spinlock;

use crate::elf_loader::{self, ElfError};
use crate::mmu::{self, UserAddressSpace};
use crate::runtime::{PhysFrame, FrameSource, runtime, config, with_irqs_disabled};
use akuma_terminal as terminal;

/// Fixed address for process info page (read-only from userspace)
///
/// This page is mapped read-only for the user process but the kernel
/// writes to it before entering userspace. The kernel can read from
/// this address during syscalls to identify which process is calling.
///
/// WARNING: This struct currently uses only ~8 bytes but we reserve 1KB (1024 bytes).
/// If ProcessInfo grows beyond 1KB, it will overflow into unmapped memory!
pub const PROCESS_INFO_ADDR: usize = 0x1000;

/// Maximum size of argument data in ProcessInfo
pub const ARGV_DATA_SIZE: usize = 744;

/// Maximum size of cwd data in ProcessInfo
pub const CWD_DATA_SIZE: usize = 256;

/// Process info structure shared between kernel and userspace
///
/// The kernel writes this, userspace reads it (read-only mapping).
///
/// Layout must match libakuma exactly.
#[repr(C)]
pub struct ProcessInfo {
    /// Process ID
    pub pid: u32,
    /// Parent process ID
    pub ppid: u32,
    /// Box ID
    pub box_id: u64,
    /// Reserved
    pub _reserved: [u8; 1008],
}

impl ProcessInfo {
    /// Create a new ProcessInfo
    pub const fn new(pid: u32, ppid: u32, box_id: u64) -> Self {
        Self {
            pid,
            ppid,
            box_id,
            _reserved: [0u8; 1008],
        }
    }
}

// Compile-time check that ProcessInfo fits in 1KB
const _: () = assert!(core::mem::size_of::<ProcessInfo>() == 1024);

/// Process ID type
pub type Pid = u32;

static PROCESS_SYSCALL_STATS_ENABLED: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

pub fn enable_process_syscall_stats(enabled: bool) {
    PROCESS_SYSCALL_STATS_ENABLED.store(enabled, Ordering::Relaxed);
}

fn process_syscall_stats_enabled() -> bool {
    PROCESS_SYSCALL_STATS_ENABLED.load(Ordering::Relaxed)
}

// Box registry re-exports (implementation in box_registry.rs)
pub use crate::box_registry::{
    BoxInfo, register_box, unregister_box, list_boxes,
    find_box_by_name, get_box_name, get_box_info, find_primary_box,
    init_box_registry,
};

/// Write data to a process's stdin (handling both legacy buffer and ProcessChannel)
pub fn write_to_process_stdin(pid: Pid, data: &[u8]) -> Result<(), &'static str> {
    let proc = lookup_process(pid).ok_or("Process not found")?;
    
    // If this process has delegated its I/O to another PID (reattach), forward it
    if let Some(target_pid) = proc.delegate_pid {
        // Use with_irqs_disabled or release lock before recursing if needed, 
        // but lookup_process handles its own locking.
        return write_to_process_stdin(target_pid, data);
    }

    // 1. Write to the legacy StdioBuffer (for procfs visibility)
    proc.stdin.lock().write_with_limit(data, config().proc_stdin_max_size);
    
    // 2. If the process has a ProcessChannel, write to it so the process actually 
    // receives the input in sys_read/sys_poll_input_event.
    if let Some(ref channel) = proc.channel {
        channel.write_stdin(data);
        
        // 3. Wake up the process if it's waiting for input in sys_poll_input_event
        crate::threading::disable_preemption();
        if let Some(waker) = proc.terminal_state.lock().input_waker.lock().take() {
            if config().syscall_debug_info_enabled {
                log::debug!("[Process] Waking PID {}", pid);
            }
            waker.wake();
            // Ensure scheduler runs to pick up the newly ready process
            (runtime().trigger_sgi)(0);
        } else {
            // Even if no waker is registered, we should still trigger SGI
            // to ensure the process gets a chance to poll soon.
            (runtime().trigger_sgi)(0);
        }
        crate::threading::enable_preemption();
    }
    
    Ok(())
}

/// Reattach I/O from a caller process (or kernel) to a target PID
pub fn reattach_process_ext(caller_pid: Option<Pid>, target_pid: Pid) -> Result<(), &'static str> {
    // 1. Validate hierarchy permissions
    let (caller_box_id, channel) = if let Some(pid) = caller_pid {
        let caller = lookup_process(pid).ok_or("Caller not found")?;
        (caller.box_id, caller.channel.clone())
    } else {
        // Kernel caller (e.g. built-in SSH shell)
        // System threads use thread-ID based channel lookup
        let tid = crate::threading::current_thread_id();
        let ch = get_channel(tid).ok_or("Kernel thread has no channel")?;
        (0, Some(ch)) // Kernel is Box 0
    };

    let target_box_id = {
        let target = lookup_process(target_pid).ok_or("Target not found")?;
        target.box_id
    };

    let mut allowed = false;
    if caller_box_id == 0 {
        allowed = true; // Host/Kernel can reattach anything
    } else if target_box_id == caller_box_id {
        allowed = true; // Same box
    } else if let Some(pid) = caller_pid {
        // Check if caller created the target's box (child box)
        if let Some(info) = get_box_info(target_box_id) {
            if info.creator_pid == pid {
                allowed = true;
            }
        }
    }

    if !allowed {
        return Err("Permission denied: cannot reattach process outside hierarchy");
    }

    // 2. Perform the delegation
    if let Some(pid) = caller_pid {
        let caller = lookup_process(pid).ok_or("Caller not found")?;
        caller.delegate_pid = Some(target_pid);
    } else {
        // For kernel caller, we don't have a 'Process' struct to set delegate_pid,
        // but we still want to link the channel to the target.
    }

    // Target process now uses caller's output channel
    {
        let target = lookup_process(target_pid).ok_or("Target not found")?;
        target.channel = channel;
    }

    if config().syscall_debug_info_enabled {
        log::debug!("[Process] Reattached (caller={:?}) -> PID {}", caller_pid, target_pid);
    }

    Ok(())
}

/// Reattach I/O from the current process to a target PID
pub fn reattach_process(target_pid: Pid) -> Result<(), &'static str> {
    let caller_pid = read_current_pid(); // Can be None for kernel threads
    reattach_process_ext(caller_pid, target_pid)
}

/// Kill all processes in a box and unregister it
pub fn kill_box(box_id: u64) -> Result<(), &'static str> {
    if box_id == 0 {
        return Err("Cannot kill Box 0 (Host)");
    }

    // 1. Get list of PIDs in this box
    let pids: Vec<Pid> = with_irqs_disabled(|| {
        let table = PROCESS_TABLE.lock();
        table.iter()
            .filter(|(_, proc)| proc.box_id == box_id)
            .map(|(&pid, _)| pid)
            .collect()
    });

    // 2. Kill each process
    for pid in pids {
        // kill_process handles unregistering and thread termination
        let _ = kill_process(pid);
    }

    // 3. Unregister the box from the global registry
    unregister_box(box_id);

    Ok(())
}

// ============================================================================
// Stdio Buffer (thread-safe stdin/stdout with size limits)
// ============================================================================

/// Thread-safe stdio buffer with size limits to prevent OOM
///
/// Used for both stdin and stdout. Size limits use "last write wins" policy:
/// when a write would exceed the limit, the buffer is cleared before writing.
#[derive(Clone)]
pub struct StdioBuffer {
    /// The actual data buffer
    pub data: Vec<u8>,
    /// Read position (only meaningful for stdin)
    pub pos: usize,
}

impl StdioBuffer {
    /// Create a new empty buffer
    pub fn new() -> Self {
        Self {
            data: Vec::new(),
            pos: 0,
        }
    }

    /// Get the data length
    pub fn len(&self) -> usize {
        self.data.len()
    }

    /// Check if buffer is empty
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// Clear the buffer and reset position
    pub fn clear(&mut self) {
        self.data.clear();
        self.pos = 0;
    }

    /// Write data with size limit ("last write wins" policy)
    ///
    /// If adding data would exceed max_size, clears buffer first.
    /// A single write larger than max_size is still accepted in full.
    pub fn write_with_limit(&mut self, data: &[u8], max_size: usize) {
        if self.data.len() + data.len() > max_size {
            self.data.clear();
        }
        self.data.extend_from_slice(data);
    }

    /// Set data (replaces existing, with size limit)
    pub fn set_with_limit(&mut self, data: &[u8], max_size: usize) {
        self.data.clear();
        self.pos = 0;
        if data.len() <= max_size {
            self.data.extend_from_slice(data);
        } else {
            // Data exceeds limit - keep last max_size bytes
            self.data.extend_from_slice(&data[data.len() - max_size..]);
        }
    }

    /// Read from buffer (advances position)
    pub fn read(&mut self, buf: &mut [u8]) -> usize {
        let remaining = &self.data[self.pos..];
        let to_read = buf.len().min(remaining.len());
        buf[..to_read].copy_from_slice(&remaining[..to_read]);
        self.pos += to_read;
        to_read
    }

    /// Clone the data (for procfs reads)
    pub fn clone_data(&self) -> Vec<u8> {
        self.data.clone()
    }
}

impl Default for StdioBuffer {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// File Descriptor Table
// ============================================================================

/// File descriptor types for the per-process FD table
#[derive(Debug, Clone)]
pub enum FileDescriptor {
    /// Standard input (fd 0)
    Stdin,
    /// Standard output (fd 1)
    Stdout,
    /// Standard error (fd 2)
    Stderr,
    /// Socket file descriptor - index into global socket table
    Socket(usize),
    /// File file descriptor
    File(KernelFile),
    /// Child process stdout - PID of the child process
    /// Used by parent to read child's stdout via ProcessChannel
    ChildStdout(Pid),
    /// Read end of a kernel pipe (pipe_id into global PIPES table)
    PipeRead(u32),
    /// Write end of a kernel pipe (pipe_id into global PIPES table)
    PipeWrite(u32),
    /// Event file descriptor (eventfd_id into global EVENTFDS table)
    EventFd(u32),
    /// /dev/null — reads return EOF, writes are discarded
    DevNull,
    /// /dev/urandom — reads return random bytes
    DevUrandom,
    /// timerfd — reads return 8-byte expiration count
    TimerFd(u32),
    /// epoll instance (epoll_id into global EPOLL_TABLE)
    EpollFd(u32),
}

/// Kernel file handle for open files
#[derive(Debug, Clone)]
pub struct KernelFile {
    /// Path to the file
    pub path: String,
    /// Current read/write position
    pub position: usize,
    /// Open flags (O_RDONLY, O_WRONLY, O_RDWR, etc.)
    pub flags: u32,
}

impl KernelFile {
    /// Create a new kernel file handle
    pub fn new(path: String, flags: u32) -> Self {
        Self {
            path,
            position: 0,
            flags,
        }
    }
}

/// File open flags (Linux compatible)
pub mod open_flags {
    pub const O_RDONLY: u32 = 0;
    pub const O_WRONLY: u32 = 1;
    pub const O_RDWR: u32 = 2;
    pub const O_CREAT: u32 = 0o100;
    pub const O_TRUNC: u32 = 0o1000;
    pub const O_APPEND: u32 = 0o2000;
    pub const O_CLOEXEC: u32 = 0o2000000;
}

/// Next available PID
static NEXT_PID: AtomicU32 = AtomicU32::new(1);

/// Process table: maps PID to owned Process
///
/// Processes are stored here when created and removed when they exit.
/// Syscall handlers use read_current_pid() + lookup_process() to find
/// the calling process.
///
/// IMPORTANT: The table owns the Process via Box. When unregister_process
/// is called, the Box<Process> is returned and dropped, which triggers
/// UserAddressSpace::drop() to free all physical pages. This prevents
/// memory leaks when processes exit.
static PROCESS_TABLE: Spinlock<alloc::collections::BTreeMap<Pid, Box<Process>>> =
    Spinlock::new(alloc::collections::BTreeMap::new());

/// Maps kernel thread IDs to PIDs for CLONE_THREAD children.
/// Needed because thread clones share the parent's ProcessInfo page, so
/// read_current_pid() would return the parent's PID.
static THREAD_PID_MAP: Spinlock<alloc::collections::BTreeMap<usize, Pid>> =
    Spinlock::new(alloc::collections::BTreeMap::new());

/// Source of data for a lazy region page.
#[derive(Clone)]
pub enum LazySource {
    /// Zero-filled on demand (anonymous mapping).
    Zero,
    /// Backed by file data; pages beyond `filesz` are zero-filled (BSS).
    File {
        path: String,
        inode: u32,
        file_offset: usize,
        filesz: usize,
        segment_va: usize,
    },
}

/// A lazily-backed virtual memory region.
#[derive(Clone)]
pub struct LazyRegion {
    pub start_va: usize,
    pub size: usize,
    pub flags: u64,
    pub source: LazySource,
}

/// Global lazy region table, keyed by PID.
/// Stored separately from Process to avoid aliasing/corruption issues
/// with &mut Process references from current_process().
pub static LAZY_REGION_TABLE: Spinlock<alloc::collections::BTreeMap<Pid, Vec<LazyRegion>>> =
    Spinlock::new(alloc::collections::BTreeMap::new());

/// Register a process in the table (takes ownership)
pub fn register_process(pid: Pid, proc: Box<Process>) {
    with_irqs_disabled(|| {
        PROCESS_TABLE.lock().insert(pid, proc);
    })
}

/// Unregister a process from the table
///
/// Returns the owned Process so it can be dropped, freeing all memory
/// including the UserAddressSpace and all its physical pages.
pub fn unregister_process(pid: Pid) -> Option<Box<Process>> {
    with_irqs_disabled(|| {
        PROCESS_TABLE.lock().remove(&pid)
    })
}

pub fn register_thread_pid(tid: usize, pid: Pid) {
    with_irqs_disabled(|| {
        THREAD_PID_MAP.lock().insert(tid, pid);
    });
}

pub fn unregister_thread_pid(tid: usize) {
    with_irqs_disabled(|| {
        THREAD_PID_MAP.lock().remove(&tid);
    });
}

// ============================================================================
// Process Channel - Inter-thread communication for process I/O
// ============================================================================

use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::format;
use core::sync::atomic::{AtomicBool, AtomicI32};

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
        })
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
        with_irqs_disabled(|| {
            let mut buffer = self.buffer.lock();
            let to_read = buf.len().min(buffer.len());
            for (i, byte) in buffer.drain(..to_read).enumerate() {
                buf[i] = byte;
            }
            if to_read > 0 && config().syscall_debug_info_enabled {
                log::debug!("[ProcessChannel] Read {} bytes from stdout", to_read);
            }
            to_read
        })
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
static PROCESS_CHANNELS: Spinlock<alloc::collections::BTreeMap<usize, Arc<ProcessChannel>>> =
    Spinlock::new(alloc::collections::BTreeMap::new());

/// Global registry mapping thread IDs to their shared terminal states
static TERMINAL_STATES: Spinlock<alloc::collections::BTreeMap<usize, Arc<Spinlock<terminal::TerminalState>>>> =
    Spinlock::new(alloc::collections::BTreeMap::new());

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

// ============================================================================
// Child Process Registry (for userspace process management)
// ============================================================================

/// Registry mapping child PIDs to (ProcessChannel, parent_pid)
/// Used by parent processes to read child stdout via ChildStdout FD
/// and by wait4(-1) to find children of a specific parent.
static CHILD_CHANNELS: Spinlock<alloc::collections::BTreeMap<Pid, (Arc<ProcessChannel>, Pid)>> =
    Spinlock::new(alloc::collections::BTreeMap::new());

/// Register a child process channel (called when spawning via syscall)
pub fn register_child_channel(child_pid: Pid, channel: Arc<ProcessChannel>, parent_pid: Pid) {
    with_irqs_disabled(|| {
        CHILD_CHANNELS.lock().insert(child_pid, (channel, parent_pid));
    })
}

/// Get a child process channel by PID
pub fn get_child_channel(child_pid: Pid) -> Option<Arc<ProcessChannel>> {
    with_irqs_disabled(|| {
        CHILD_CHANNELS.lock().get(&child_pid).map(|(ch, _)| ch.clone())
    })
}

/// Remove a child process channel (called when child exits or parent closes FD)
pub fn remove_child_channel(child_pid: Pid) -> Option<Arc<ProcessChannel>> {
    with_irqs_disabled(|| {
        CHILD_CHANNELS.lock().remove(&child_pid).map(|(ch, _)| ch)
    })
}

/// Find any exited child of the given parent. Returns (child_pid, channel).
pub fn find_exited_child(parent_pid: Pid) -> Option<(Pid, Arc<ProcessChannel>)> {
    with_irqs_disabled(|| {
        let channels = CHILD_CHANNELS.lock();
        for (&child_pid, (ch, ppid)) in channels.iter() {
            if *ppid == parent_pid && ch.has_exited() {
                return Some((child_pid, ch.clone()));
            }
        }
        None
    })
}

/// Check if the given parent has any children registered.
pub fn has_children(parent_pid: Pid) -> bool {
    with_irqs_disabled(|| {
        CHILD_CHANNELS.lock().values().any(|(_, ppid)| *ppid == parent_pid)
    })
}

/// Get channel for the current thread (used by syscall handlers)
pub fn current_channel() -> Option<Arc<ProcessChannel>> {
    if let Some(proc) = current_process() {
        if let Some(ref ch) = proc.channel {
            return Some(ch.clone());
        }
    }
    
    // Fallback to thread-ID based lookup for legacy system threads
    let thread_id = crate::threading::current_thread_id();
    get_channel(thread_id)
}

/// Check if the current process has been interrupted (Ctrl+C)
///
/// Called by syscall handlers to detect interrupt signal.
/// Returns true if the process should terminate.
pub fn is_current_interrupted() -> bool {
    current_channel()
        .map(|ch| ch.is_interrupted())
        .unwrap_or(false)
}

/// Interrupt a process by thread ID
///
/// Used by the SSH shell to send Ctrl+C signal to a running process.
pub fn interrupt_thread(thread_id: usize) {
    if let Some(channel) = get_channel(thread_id) {
        channel.set_interrupted();
    }
}

/// Read the current process PID from the process info page
///
/// During a syscall, TTBR0 is still set to the user's page tables,
/// so reading from PROCESS_INFO_ADDR gives us the calling process's PID.
/// This prevents PID spoofing since the page is read-only for userspace.
///
/// Returns None if TTBR0 points to boot page tables (no user process context).
pub fn read_current_pid() -> Option<Pid> {
    // CRITICAL: Check TTBR0 before reading from user address space!
    //
    // PROCESS_INFO_ADDR (0x1000) is only mapped in USER page tables.
    // With boot TTBR0, address 0x1000 is in the device memory region (0x0-0x40000000)
    // and reading from it returns garbage, causing FAR=0x5 crashes.
    let ttbr0: u64;
    unsafe {
        core::arch::asm!("mrs {}, ttbr0_el1", out(reg) ttbr0);
    }
    
    // Compare against actual boot TTBR0, not a range check.
    // User page tables are allocated from the same physical memory pool,
    // so they can have addresses in the same range as boot tables.
    let boot_ttbr0 = crate::mmu::get_boot_ttbr0();
    let ttbr0_addr = ttbr0 & 0x0000_FFFF_FFFF_FFFF; // Mask off ASID bits
    if ttbr0_addr == boot_ttbr0 {
        return None; // Boot TTBR0 - no user process context
    }
    
    // Read from the fixed address in the current address space
    // SAFETY: TTBR0 is user page tables, so PROCESS_INFO_ADDR is mapped
    let pid = unsafe { (*(PROCESS_INFO_ADDR as *const ProcessInfo)).pid };
    if pid == 0 { None } else { Some(pid) }
}

/// Look up a process by PID
///
/// Returns a mutable reference to the process if found.
/// SAFETY: The caller must ensure no other code is mutating the process.
pub fn lookup_process(pid: Pid) -> Option<&'static mut Process> {
    with_irqs_disabled(|| {
        let mut table = PROCESS_TABLE.lock();
        table.get_mut(&pid).map(|boxed| {
            // SAFETY: We return a 'static reference because:
            // 1. The Process is heap-allocated via Box and won't move
            // 2. The process remains in the table until unregister_process
            // 3. Callers must not hold reference across unregister_process
            unsafe { &mut *(&mut **boxed as *mut Process) }
        })
    })
}

/// Get the current process (for syscall handlers)
///
/// For CLONE_THREAD children, uses the thread-to-PID map since they share
/// the parent's ProcessInfo page. Otherwise reads PID from the process info page.
pub fn current_process() -> Option<&'static mut Process> {
    let tid = crate::threading::current_thread_id();
    let thread_pid = with_irqs_disabled(|| {
        THREAD_PID_MAP.lock().get(&tid).copied()
    });
    if let Some(pid) = thread_pid {
        return lookup_process(pid);
    }
    let pid = read_current_pid()?;
    lookup_process(pid)
}

/// Get the current process's TerminalState (for syscall handlers)
///
/// Returns a mutable reference to the TerminalState if found.
pub fn current_terminal_state() -> Option<Arc<Spinlock<terminal::TerminalState>>> {
    // 1. Try thread-ID based lookup (for system threads or overridden processes)
    let tid = crate::threading::current_thread_id();
    if let Some(state) = get_terminal_state(tid) {
        return Some(state);
    }

    // 2. Fallback to process table
    current_process().map(|p| p.terminal_state.clone())
}

/// Allocate mmap region for current process
/// Returns the address or 0 on failure
pub fn alloc_mmap(size: usize) -> usize {
    // Use address-space owner so CLONE_VM threads share allocation state.
    let pid = read_current_pid().unwrap_or(0);
    let proc = match lookup_process(pid) {
        Some(p) => p,
        None => {
            (runtime().print_str)("[mmap] ERROR: No current process\n");
            return 0;
        }
    };

    // Use per-process memory tracking
    match proc.memory.alloc_mmap(size) {
        Some(addr) => addr,
        None => {
            log::debug!("[mmap] REJECT: pid={} size=0x{:x} next=0x{:x} limit=0x{:x}",
                proc.pid, size, proc.memory.next_mmap, proc.memory.mmap_limit);
            0
        }
    }
}

/// Record a new mmap region for the current process
///
/// Called by sys_mmap after allocating frames.
/// The frames Vec should contain all physical frames for this region.
pub fn record_mmap_region(start_va: usize, frames: Vec<PhysFrame>) {
    let pid = read_current_pid().unwrap_or(0);
    if let Some(proc) = lookup_process(pid) {
        proc.mmap_regions.push((start_va, frames));
    }
}

/// Record a lazy mmap region — VA reserved, no physical pages.
/// `page_flags` = 0 for PROT_NONE (needs mprotect), non-zero for demand-paged.
pub fn record_lazy_region(start_va: usize, size: usize, page_flags: u64) {
    let pid = read_current_pid().unwrap_or(0);
    if let Some(proc) = lookup_process(pid) {
        proc.lazy_regions.push(LazyRegion { start_va, size, flags: page_flags, source: LazySource::Zero });
    }
}

/// Check if a virtual address falls within any lazy region of the current process.
/// Returns `(flags, source, region_start, region_size)` if found.
/// The source is cloned so the caller can release the table lock before performing I/O.
pub fn lazy_region_lookup(va: usize) -> Option<(u64, LazySource, usize, usize)> {
    let pid = read_current_pid()?;
    with_irqs_disabled(|| {
        let table = LAZY_REGION_TABLE.lock();
        if let Some(regions) = table.get(&pid) {
            for r in regions {
                if va >= r.start_va && va < r.start_va + r.size {
                    return Some((r.flags, r.source.clone(), r.start_va, r.size));
                }
            }
        }
        None
    })
}

/// Like lazy_region_lookup but takes an explicit PID (for tests and non-current-process use).
pub fn lazy_region_count_for_pid(pid: Pid) -> usize {
    with_irqs_disabled(|| {
        let table = LAZY_REGION_TABLE.lock();
        table.get(&pid).map_or(0, |r| r.len())
    })
}

pub fn lazy_region_lookup_for_pid(pid: Pid, va: usize) -> Option<(u64, LazySource, usize, usize)> {
    with_irqs_disabled(|| {
        let table = LAZY_REGION_TABLE.lock();
        if let Some(regions) = table.get(&pid) {
            for r in regions {
                if va >= r.start_va && va < r.start_va + r.size {
                    return Some((r.flags, r.source.clone(), r.start_va, r.size));
                }
            }
        }
        None
    })
}

/// Stack-local writer for visible kernel output without heap allocation.
struct LazyDebugWriter<const N: usize> {
    buf: [u8; N],
    pos: usize,
}
impl<const N: usize> LazyDebugWriter<N> {
    const fn new() -> Self { Self { buf: [0; N], pos: 0 } }
    fn flush(&mut self) {
        if let Ok(s) = core::str::from_utf8(&self.buf[..self.pos]) {
            (runtime().print_str)(s);
        }
        self.pos = 0;
    }
}
impl<const N: usize> core::fmt::Write for LazyDebugWriter<N> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let bytes = s.as_bytes();
        let remaining = N - self.pos;
        let len = core::cmp::min(bytes.len(), remaining);
        self.buf[self.pos..self.pos + len].copy_from_slice(&bytes[..len]);
        self.pos += len;
        Ok(())
    }
}

pub fn lazy_region_debug(va: usize) {
    let pid = read_current_pid().unwrap_or(0);
    with_irqs_disabled(|| {
        use core::fmt::Write;
        let table = LAZY_REGION_TABLE.lock();
        if let Some(regions) = table.get(&pid) {
            let mut w = LazyDebugWriter::<256>::new();
            let _ = write!(w, "[DP] lazy miss: pid={} va={:#x} regions={} [", pid, va, regions.len());
            for (i, r) in regions.iter().enumerate().take(8) {
                if i > 0 { let _ = w.write_str(","); }
                let _ = write!(w, "{:#x}+{:#x}", r.start_va, r.size);
            }
            let _ = w.write_str("]\n");
            w.flush();
        } else {
            let mut w = LazyDebugWriter::<128>::new();
            let _ = writeln!(w, "[DP] lazy miss: pid={} va={:#x} no entry in table", pid, va);
            w.flush();
        }
    });
}

pub fn push_lazy_region(pid: Pid, start_va: usize, size: usize, page_flags: u64) -> usize {
    push_lazy_region_with_source(pid, start_va, size, page_flags, LazySource::Zero)
}

pub fn push_lazy_region_with_source(pid: Pid, start_va: usize, size: usize, page_flags: u64, source: LazySource) -> usize {
    let len = with_irqs_disabled(|| {
        let mut table = LAZY_REGION_TABLE.lock();
        let regions = table.entry(pid).or_insert_with(Vec::new);
        regions.push(LazyRegion { start_va, size, flags: page_flags, source });
        regions.len()
    });
    len
}

/// Update flags on all lazy regions that overlap [range_start, range_start+range_size).
/// Called by sys_mprotect so demand paging uses the correct permissions.
pub fn update_lazy_region_flags(pid: Pid, range_start: usize, range_size: usize, new_flags: u64) {
    let range_end = range_start + range_size;
    with_irqs_disabled(|| {
        let mut table = LAZY_REGION_TABLE.lock();
        if let Some(regions) = table.get_mut(&pid) {
            for r in regions.iter_mut() {
                let r_end = r.start_va + r.size;
                if r.start_va < range_end && r_end > range_start {
                    r.flags = new_flags;
                }
            }
        }
    });
}

pub fn remove_lazy_region(pid: Pid, start_va: usize) -> Option<LazyRegion> {
    let result = with_irqs_disabled(|| {
        let mut table = LAZY_REGION_TABLE.lock();
        if let Some(regions) = table.get_mut(&pid) {
            if let Some(idx) = regions.iter().position(|r| r.start_va == start_va) {
                let removed = regions.remove(idx);
                return Some((removed, regions.len()));
            }
        }
        None
    });
    if let Some((removed, _remaining)) = result {
        Some(removed)
    } else {
        None
    }
}

/// Handle munmap across all lazy regions overlapping [unmap_addr, unmap_addr+unmap_len).
/// Returns a Vec of (freed_start, freed_pages) for each affected region.
pub fn munmap_lazy_regions_in_range(pid: Pid, unmap_addr: usize, unmap_len: usize) -> alloc::vec::Vec<(usize, usize)> {
    let unmap_end = unmap_addr + unmap_len;
    let mut results = alloc::vec::Vec::new();

    loop {
        if let Some(result) = munmap_lazy_region_overlapping(pid, unmap_addr, unmap_end) {
            results.push(result);
        } else {
            break;
        }
    }
    results
}

/// Find and modify a single lazy region that overlaps with [range_start, range_end).
/// Uses overlap check rather than containment check, so it finds regions that
/// start within the range even if range_start is in a gap between regions.
fn munmap_lazy_region_overlapping(pid: Pid, range_start: usize, range_end: usize) -> Option<(usize, usize)> {
    let result = with_irqs_disabled(|| {
        let mut table = LAZY_REGION_TABLE.lock();
        let regions = table.get_mut(&pid)?;

        let idx = regions.iter().position(|r| {
            let reg_end = r.start_va + r.size;
            r.start_va < range_end && reg_end > range_start
        })?;

        let reg_start = regions[idx].start_va;
        let reg_size = regions[idx].size;
        let reg_end = reg_start + reg_size;

        let clip_start = if range_start > reg_start { range_start } else { reg_start };
        let clip_end = if range_end < reg_end { range_end } else { reg_end };

        if clip_start == reg_start && clip_end == reg_end {
            regions.remove(idx);
            Some(('F', reg_start, reg_size / 4096))
        } else if clip_start == reg_start {
            regions[idx].start_va = clip_end;
            regions[idx].size = reg_end - clip_end;
            let freed = (clip_end - clip_start) / 4096;
            Some(('P', clip_start, freed))
        } else if clip_end == reg_end {
            regions[idx].size = clip_start - reg_start;
            let freed = (reg_end - clip_start) / 4096;
            Some(('S', clip_start, freed))
        } else {
            let right = LazyRegion {
                start_va: clip_end,
                size: reg_end - clip_end,
                flags: regions[idx].flags,
                source: regions[idx].source.clone(),
            };
            regions[idx].size = clip_start - reg_start;
            regions.push(right);
            let freed = (clip_end - clip_start) / 4096;
            Some(('M', clip_start, freed))
        }
    });

    if let Some((op, freed_start, freed_pages)) = result {
        log::debug!("[LR{}] pid={} munmap {:#x}+{:#x} ({} pages)",
            op as char, pid, freed_start, freed_pages * 4096, freed_pages);
        Some((freed_start, freed_pages))
    } else {
        None
    }
}

pub fn clear_lazy_regions(pid: Pid) {
    let count = with_irqs_disabled(|| {
        let mut table = LAZY_REGION_TABLE.lock();
        let count = table.get(&pid).map_or(0, |r| r.len());
        table.remove(&pid);
        count
    });
    if count > 0 {
        log::debug!("[LR!] clear pid={} ({} regions)", pid, count);
    }
}

pub fn clone_lazy_regions(from_pid: Pid, to_pid: Pid) {
    with_irqs_disabled(|| {
        let mut table = LAZY_REGION_TABLE.lock();
        if let Some(regions) = table.get(&from_pid) {
            let cloned = regions.clone();
            let len = cloned.len();
            table.insert(to_pid, cloned);
            log::debug!("[LR] clone pid={}->{} ({} regions)", from_pid, to_pid, len);
        }
    });
}

/// Check if a virtual address falls within any lazy region.
pub fn is_in_lazy_region(va: usize) -> bool {
    lazy_region_lookup(va).is_some()
}

/// Remove and return mmap region starting at the given VA
///
/// Called by sys_munmap to find frames to free.
/// Returns None if no region starts at this VA.
pub fn remove_mmap_region(start_va: usize) -> Option<Vec<PhysFrame>> {
    let pid = read_current_pid().unwrap_or(0);
    let proc = lookup_process(pid)?;
    
    // Find the region
    let idx = proc.mmap_regions.iter().position(|(va, _)| *va == start_va)?;
    
    // Remove and return the frames
    let (va, frames) = proc.mmap_regions.remove(idx);
    
    // RECLAIM: Add the freed range to free_regions
    let size = frames.len() * 4096; // config::PAGE_SIZE
    proc.memory.free_regions.push((va, size));
    
    Some(frames)
}

/// Get stack bounds for current process
pub fn get_stack_bounds() -> (usize, usize) {
    match current_process() {
        Some(p) => (p.memory.stack_bottom, p.memory.stack_top),
        None => (0, 0),
    }
}

/// Process info for display (used by ps command)
#[derive(Debug, Clone)]
pub struct ProcessInfo2 {
    pub pid: Pid,
    pub ppid: Pid,
    pub box_id: u64,
    pub name: String,
    pub state: &'static str,
    pub last_syscall: u64,
}

/// List all running processes
///
/// Returns a vector of process info for display.
pub fn list_processes() -> Vec<ProcessInfo2> {
    // Take a quick snapshot while holding lock with IRQs disabled
    // to prevent deadlock if timer fires while holding PROCESS_TABLE lock.
    // We collect data into a local Vec while locked, then return it.
    with_irqs_disabled(|| {
        let table = PROCESS_TABLE.lock();
        let mut result = Vec::new();

        for (&pid, proc) in table.iter() {
            let state = match proc.state {
                ProcessState::Ready => "ready",
                ProcessState::Running => "running",
                ProcessState::Blocked => "blocked",
                ProcessState::Zombie(_) => "zombie",
            };
            result.push(ProcessInfo2 {
                pid,
                ppid: proc.parent_pid,
                box_id: proc.box_id,
                name: proc.name.clone(),
                state,
                last_syscall: proc.last_syscall.load(core::sync::atomic::Ordering::Relaxed),
            });
        }

        result
    })
}

/// Find a process PID by thread ID
///
/// Returns the PID of the process running on the given thread, if any.
pub fn find_pid_by_thread(thread_id: usize) -> Option<Pid> {
    with_irqs_disabled(|| {
        let table = PROCESS_TABLE.lock();
        for (&pid, proc) in table.iter() {
            if proc.thread_id == Some(thread_id) {
                return Some(pid);
            }
        }
        None
    })
}

/// Process state
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessState {
    /// Process is ready to run
    Ready,
    /// Process is currently running
    Running,
    /// Process is waiting for I/O
    Blocked,
    /// Process has terminated
    Zombie(i32), // Exit code
}

/// User context saved during kernel entry
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct UserContext {
    // General purpose registers
    pub x0: u64,
    pub x1: u64,
    pub x2: u64,
    pub x3: u64,
    pub x4: u64,
    pub x5: u64,
    pub x6: u64,
    pub x7: u64,
    pub x8: u64,
    pub x9: u64,
    pub x10: u64,
    pub x11: u64,
    pub x12: u64,
    pub x13: u64,
    pub x14: u64,
    pub x15: u64,
    pub x16: u64,
    pub x17: u64,
    pub x18: u64,
    pub x19: u64,
    pub x20: u64,
    pub x21: u64,
    pub x22: u64,
    pub x23: u64,
    pub x24: u64,
    pub x25: u64,
    pub x26: u64,
    pub x27: u64,
    pub x28: u64,
    pub x29: u64,  // Frame pointer
    pub x30: u64,  // Link register
    pub sp: u64,   // Stack pointer (SP_EL0)
    pub pc: u64,   // Program counter (ELR_EL1)
    pub spsr: u64, // Saved program status
    pub tpidr: u64, // Thread pointer for TLS
    pub ttbr0: u64, // User address space base
}

impl UserContext {
    pub fn new(entry_point: usize, stack_pointer: usize) -> Self {
        Self {
            x0: 0,
            x1: 0,
            x2: 0,
            x3: 0,
            x4: 0,
            x5: 0,
            x6: 0,
            x7: 0,
            x8: 0,
            x9: 0,
            x10: 0,
            x11: 0,
            x12: 0,
            x13: 0,
            x14: 0,
            x15: 0,
            x16: 0,
            x17: 0,
            x18: 0,
            x19: 0,
            x20: 0,
            x21: 0,
            x22: 0,
            x23: 0,
            x24: 0,
            x25: 0,
            x26: 0,
            x27: 0,
            x28: 0,
            x29: 0,
            x30: 0,
            sp: stack_pointer as u64,
            pc: entry_point as u64,
            spsr: 0, // EL0t, interrupts enabled
            tpidr: 0,
            ttbr0: 0,
        }
    }
    
    pub fn default() -> Self {
        Self::new(0, 0)
    }
}

// ============================================================================
// Signal Infrastructure
// ============================================================================

pub const MAX_SIGNALS: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SignalHandler {
    Default,
    Ignore,
    UserFn(usize),
}

#[derive(Debug, Clone, Copy)]
pub struct SignalAction {
    pub handler: SignalHandler,
    pub flags: u64,
    pub mask: u64,
    pub restorer: usize,
}

impl SignalAction {
    pub const fn default() -> Self {
        Self {
            handler: SignalHandler::Default,
            flags: 0,
            mask: 0,
            restorer: 0,
        }
    }
}

/// Memory regions for a process
#[derive(Debug, Clone)]
pub struct ProcessMemory {
    /// Code/data region end (start of heap)
    pub code_end: usize,
    /// Current program break (heap grows up from here)
    pub brk: usize,
    /// Stack bottom (lowest mapped stack address)
    pub stack_bottom: usize,
    /// Stack top (highest mapped stack address + 1)
    pub stack_top: usize,
    /// Next mmap address (mmap region between code_end and stack_bottom)
    pub next_mmap: usize,
    /// Mmap region limit (must stay below this)
    pub mmap_limit: usize,
    /// Freed virtual address regions for reclamation (start_va, size)
    pub free_regions: Vec<(usize, usize)>,
}

impl ProcessMemory {
    pub fn new(code_end: usize, stack_bottom: usize, stack_top: usize, mmap_floor: usize) -> Self {
        // Mmap region starts 256MB above code_end to leave room for heap
        // growth (brk grows upward from code_end). Must be above code_end
        // so PIE binaries loaded at 0x1000_0000 don't get their code pages
        // overwritten by mmap allocations.
        // Also must be above mmap_floor (e.g. above the interpreter region).
        let base = (code_end + 0x1000_0000) & !0xFFFF;
        let mmap_start = core::cmp::max(base, mmap_floor);
        let mmap_limit = stack_bottom.saturating_sub(0x10_0000); // 1MB buffer before stack

        Self {
            code_end,
            brk: code_end,
            stack_bottom,
            stack_top,
            next_mmap: mmap_start,
            mmap_limit,
            free_regions: Vec::new(),
        }
    }

    /// Check if an address range overlaps with stack
    pub fn overlaps_stack(&self, addr: usize, size: usize) -> bool {
        let end = addr.saturating_add(size);
        addr < self.stack_top && end > self.stack_bottom
    }

    /// Kernel identity-maps VA 0x40000000-0x4FFFFFFF (256MB RAM via 2MB blocks).
    /// User mmap regions must not overlap this range.
    const KERNEL_VA_START: usize = 0x4000_0000;
    const KERNEL_VA_END: usize   = 0x5000_0000;

    /// Allocate mmap region, returns None if would overlap stack
    pub fn alloc_mmap(&mut self, size: usize) -> Option<usize> {
        // 1. Try to find a hole in free_regions (first-fit)
        for i in 0..self.free_regions.len() {
            let (start, f_size) = self.free_regions[i];
            if f_size >= size {
                self.free_regions.remove(i);
                if f_size > size {
                    self.free_regions.push((start + size, f_size - size));
                }
                return Some(start);
            }
        }

        // 2. Fall back to bump allocator — skip the kernel identity-mapped VA range
        let mut addr = self.next_mmap;

        // If we're about to enter the kernel VA hole, jump past it
        if addr < Self::KERNEL_VA_END && addr + size > Self::KERNEL_VA_START {
            addr = Self::KERNEL_VA_END;
        }

        let end = addr.checked_add(size)?;

        if end > self.mmap_limit {
            return None;
        }

        self.next_mmap = end;
        Some(addr)
    }
}

/// A user process
pub struct Process {
    /// Process ID
    pub pid: Pid,
    /// Process group ID
    pub pgid: Pid,
    /// Process name (for debugging)
    pub name: String,
    /// Process state
    pub state: ProcessState,
    /// User address space
    pub address_space: UserAddressSpace,
    /// Saved user context
    pub context: UserContext,
    /// Parent process ID (0 for init)
    pub parent_pid: Pid,
    /// Current program break (heap end)
    pub brk: usize,
    /// Initial program break (start of heap, set from ELF loader)
    pub initial_brk: usize,
    /// Entry point address (start of execution)
    pub entry_point: usize,
    /// Memory regions tracking
    pub memory: ProcessMemory,
    /// Physical address of the process info page
    ///
    /// This page is mapped read-only at PROCESS_INFO_ADDR for the user.
    /// The kernel writes to it (via phys_to_virt) before entering userspace.
    pub process_info_phys: usize,

    // ========== Command line arguments ==========
    /// Command line arguments (stored as strings, serialized to ProcessInfo on execute)
    pub args: Vec<String>,
    
    // ========== Current working directory ==========
    /// Current working directory (inherited from parent or set explicitly)
    pub cwd: String,

    // ========== Per-process I/O (Spinlock-protected for thread safety) ==========
    /// Process stdin buffer with read position
    /// Protected by Spinlock to prevent races between procfs reads and process reads
    pub stdin: Spinlock<StdioBuffer>,
    /// Process stdout buffer
    /// Protected by Spinlock to prevent races between syscall writes and procfs reads
    pub stdout: Spinlock<StdioBuffer>,
    /// Process has exited
    pub exited: bool,
    /// Exit code (valid when exited=true)
    pub exit_code: i32,

    // ========== Dynamic page table tracking ==========
    /// Page table frames allocated during mmap (for cleanup on exit)
    /// These are allocated by map_user_page() and need to be freed separately
    /// from address_space.page_table_frames since they're created dynamically.
    pub dynamic_page_tables: Vec<PhysFrame>,

    // ========== Mmap region tracking ==========
    /// Tracks mmap'd regions: (start_va, Vec<PhysFrame>)
    /// Used by munmap to find and free the correct frames.
    pub mmap_regions: Vec<(usize, Vec<PhysFrame>)>,
    /// Lazy mmap regions. VA is reserved but physical pages are allocated
    /// on demand via page fault. flags=0 means PROT_NONE (needs mprotect
    /// before access); non-zero means demand-paged with those permissions
    /// on first touch.
    pub lazy_regions: Vec<LazyRegion>,

    // ========== File Descriptor Table ==========
    /// Per-process file descriptor table
    /// Maps FD numbers to FileDescriptor entries (sockets, files, etc.)
    pub fd_table: Spinlock<alloc::collections::BTreeMap<u32, FileDescriptor>>,
    /// FDs marked close-on-exec (closed during execve)
    pub cloexec_fds: Spinlock<alloc::collections::BTreeSet<u32>>,
    /// FDs marked non-blocking (O_NONBLOCK)
    pub nonblock_fds: Spinlock<alloc::collections::BTreeSet<u32>>,
    /// Next available file descriptor number
    pub next_fd: AtomicU32,

    // ========== Thread tracking ==========
    /// Thread ID running this process (set after spawn, used for kill)
    pub thread_id: Option<usize>,

    /// Spawner tracking (for procfs permissions)
    pub spawner_pid: Option<Pid>,
    // ========== Terminal State ==========
    pub terminal_state: Arc<Spinlock<terminal::TerminalState>>,

    // ========== Isolation Context ==========
    /// Box ID (0 = Host, >0 = Isolated Box)
    pub box_id: u64,
    /// Per-process namespace (mount + network isolation)
    pub namespace: Arc<akuma_isolation::Namespace>,

    /// I/O Channel for async/interactive communication
    pub channel: Option<Arc<ProcessChannel>>,

    /// PID to which this process has delegated its I/O (for reattach)
    pub delegate_pid: Option<Pid>,

    /// Address to clear and futex-wake on thread exit (CLONE_CHILD_CLEARTID)
    pub clear_child_tid: u64,

    /// Robust futex list head pointer (set by set_robust_list syscall)
    pub robust_list_head: u64,
    /// Robust futex list entry size (from set_robust_list len argument)
    pub robust_list_len: usize,

    /// Per-process signal action table (sigaction storage)
    pub signal_actions: [SignalAction; MAX_SIGNALS],

    /// Monotonic timestamp (us) when the process was created
    pub start_time_us: u64,

    /// Last syscall number (for debugging stuck processes)
    pub last_syscall: core::sync::atomic::AtomicU64,

    /// Per-process syscall stats (emitted on exit when enabled)
    pub syscall_stats: ProcessSyscallStats,
}

/// Per-process syscall counters, emitted on exit for performance profiling.
/// Indexed directly by syscall number for zero-overhead tracking.
pub struct ProcessSyscallStats {
    counts: [AtomicU64; Self::MAX_NR],
    times_us: [AtomicU64; Self::MAX_NR],
    pub pagefaults: AtomicU64,
    pub pagefault_pages: AtomicU64,
}

impl ProcessSyscallStats {
    const MAX_NR: usize = 512;

    pub const fn new() -> Self {
        Self {
            counts: [const { AtomicU64::new(0) }; Self::MAX_NR],
            times_us: [const { AtomicU64::new(0) }; Self::MAX_NR],
            pagefaults: AtomicU64::new(0),
            pagefault_pages: AtomicU64::new(0),
        }
    }

    pub fn inc(&self, nr: u64) {
        let idx = nr as usize;
        if idx < Self::MAX_NR {
            self.counts[idx].fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn add_time_us(&self, nr: u64, us: u64) {
        let idx = nr as usize;
        if idx < Self::MAX_NR {
            self.times_us[idx].fetch_add(us, Ordering::Relaxed);
        }
    }

    pub fn inc_pagefault(&self, pages: u64) {
        self.pagefaults.fetch_add(1, Ordering::Relaxed);
        self.pagefault_pages.fetch_add(pages, Ordering::Relaxed);
    }

    pub fn dump(&self, pid: Pid, name: &str, elapsed_us: u64) {
        use alloc::format;
        use alloc::vec::Vec;

        let mut total: u64 = 0;
        let mut total_time_us: u64 = 0;
        let mut entries: Vec<(usize, u64, u64)> = Vec::new();
        for i in 0..Self::MAX_NR {
            let c = self.counts[i].load(Ordering::Relaxed);
            if c > 0 {
                let t = self.times_us[i].load(Ordering::Relaxed);
                total += c;
                total_time_us += t;
                entries.push((i, c, t));
            }
        }
        if total == 0 { return; }

        // Sort by time spent (descending) — shows the slowest syscalls first
        entries.sort_by(|a, b| b.2.cmp(&a.2));

        let secs = elapsed_us / 1_000_000;
        let frac = (elapsed_us % 1_000_000) / 10_000;
        let rate = if elapsed_us > 0 { total * 1_000_000 / elapsed_us } else { 0 };
        let (pmm_total, _pmm_alloc, pmm_free) = (runtime().pmm_stats)();
        let pf = self.pagefaults.load(Ordering::Relaxed);
        let pf_pg = self.pagefault_pages.load(Ordering::Relaxed);

        let mut top = alloc::string::String::new();
        for (i, (nr, count, time)) in entries.iter().enumerate() {
            if i > 0 { top.push(' '); }
            let sname = syscall_name(*nr);
            let time_ms = *time / 1000;
            if sname.is_empty() {
                let _ = core::fmt::Write::write_fmt(&mut top, format_args!("nr{}={}({}ms)", nr, count, time_ms));
            } else {
                let _ = core::fmt::Write::write_fmt(&mut top, format_args!("{}={}({}ms)", sname, count, time_ms));
            }
            if i >= 9 { break; }
        }

        let total_time_ms = total_time_us / 1000;
        let msg = format!(
            "[PSTATS] PID {} ({}) {}.{:02}s: {} syscalls ({}/s) in_kernel={}ms pmm={}free/{}tot pgfault={}({}pg) | {}\n",
            pid, name, secs, frac, total, rate, total_time_ms,
            pmm_free, pmm_total, pf, pf_pg, top,
        );
        (runtime().print_str)(&msg);
    }
}

fn syscall_name(nr: usize) -> &'static str {
    match nr {
        0 => "io_setup", 29 => "ioctl", 46 => "ftruncate",
        48 => "faccessat", 56 => "openat", 57 => "close",
        59 => "pipe2", 61 => "getdents64", 62 => "lseek",
        63 => "read", 64 => "write", 65 => "readv",
        66 => "writev", 67 => "pread64", 68 => "pwrite64",
        72 => "pselect6", 73 => "ppoll",
        78 => "readlinkat", 79 => "fstatat", 80 => "fstat",
        93 => "exit", 94 => "exit_group",
        96 => "set_tid_address", 98 => "futex",
        99 => "set_robust_list",
        113 => "clock_gettime", 115 => "clock_nanosleep",
        124 => "sched_yield",
        130 => "tkill", 131 => "tgkill",
        134 => "rt_sigaction", 135 => "rt_sigprocmask",
        160 => "uname", 167 => "prctl",
        172 => "getpid", 174 => "getuid", 175 => "geteuid",
        176 => "getgid", 177 => "getegid", 178 => "gettid",
        198 => "socket", 200 => "bind", 201 => "listen",
        202 => "accept", 203 => "connect",
        204 => "getsockname", 205 => "getpeername",
        206 => "sendto", 207 => "recvfrom",
        208 => "setsockopt", 209 => "getsockopt",
        210 => "shutdown",
        214 => "brk",
        215 => "munmap", 216 => "mremap", 222 => "mmap",
        226 => "mprotect", 233 => "madvise",
        220 => "clone", 221 => "execve",
        260 => "wait4",
        261 => "prlimit64",
        278 => "getrandom",
        281 => "memfd_create",
        282 => "membarrier",
        20 => "epoll_create1", 21 => "epoll_ctl", 22 => "epoll_pwait",
        25 => "fcntl",
        26 => "inotify_init1", 27 => "inotify_add_watch",
        35 => "unlinkat",
        85 => "timerfd_create", 86 => "timerfd_settime",
        19 => "eventfd2",
        435 => "clone3", 439 => "faccessat2",
        _ => "",
    }
}


fn compute_heap_lazy_size(brk: usize, memory: &ProcessMemory) -> usize {
    const MIN_HEAP: usize = 16 * 1024 * 1024;
    const RESERVE_PAGES: usize = 2048; // 8MB

    let (_, _, free) = (runtime().pmm_stats)();
    let phys_cap = free.saturating_sub(RESERVE_PAGES) * crate::mmu::PAGE_SIZE;
    let va_cap = memory.next_mmap.saturating_sub(brk);

    core::cmp::max(core::cmp::min(phys_cap, va_cap), MIN_HEAP)
}

impl Process {
    /// Create a new process from ELF data
    pub fn from_elf(name: &str, args: &[String], env: &[String], elf_data: &[u8], interp_prefix: Option<&str>) -> Result<Self, ElfError> {
        let (entry_point, mut address_space, stack_pointer, brk, stack_bottom, stack_top, mmap_floor, _deferred) =
            elf_loader::load_elf_with_stack(elf_data, args, env, config().user_stack_size, interp_prefix)?;

        let pid = NEXT_PID.fetch_add(1, Ordering::Relaxed);

        let process_info_frame = (runtime().alloc_page_zeroed)().ok_or(ElfError::OutOfMemory)?;
        (runtime().track_frame)(process_info_frame, FrameSource::UserData);

        address_space
            .map_page(
                PROCESS_INFO_ADDR,
                process_info_frame.addr,
                crate::mmu::user_flags::RO | crate::mmu::flags::UXN | crate::mmu::flags::PXN,
            )
            .map_err(|_| ElfError::MappingFailed("process info page"))?;

        address_space.track_user_frame(process_info_frame);

        let memory = ProcessMemory::new(brk, stack_bottom, stack_top, mmap_floor);

        log::debug!("[Process] PID {} memory: code_end=0x{:x}, stack=0x{:x}-0x{:x}, mmap=0x{:x}-0x{:x}",
            pid, brk, stack_bottom, stack_top, memory.next_mmap, memory.mmap_limit);

        // Initialize FD table with stdin/stdout/stderr pre-allocated
        let mut fd_map = alloc::collections::BTreeMap::new();
        fd_map.insert(0, FileDescriptor::Stdin);
        fd_map.insert(1, FileDescriptor::Stdout);
        fd_map.insert(2, FileDescriptor::Stderr);

        Ok(Self {
            pid,
            pgid: pid,
            name: String::from(name),
            state: ProcessState::Ready,
            address_space,
            context: UserContext::new(entry_point, stack_pointer),
            parent_pid: 0,
            brk,
            initial_brk: brk,
            entry_point,
            memory,
            process_info_phys: process_info_frame.addr,
            // Command line arguments - initialized empty
            args: Vec::new(),
            // Current working directory - defaults to root
            cwd: String::from("/"),
            // Per-process I/O - Spinlock-protected for thread safety
            stdin: Spinlock::new(StdioBuffer::new()),
            stdout: Spinlock::new(StdioBuffer::new()),
            exited: false,
            exit_code: 0,
            // Dynamic page tables - for mmap-allocated page tables
            dynamic_page_tables: Vec::new(),
            // Mmap regions - for tracking VA->frames mapping (used by munmap)
            mmap_regions: Vec::new(),
            lazy_regions: Vec::new(),
            // File descriptor table - stdin/stdout/stderr pre-allocated
            fd_table: Spinlock::new(fd_map),
            cloexec_fds: Spinlock::new(alloc::collections::BTreeSet::new()),
            nonblock_fds: Spinlock::new(alloc::collections::BTreeSet::new()),
            next_fd: AtomicU32::new(3), // Start after stdin/stdout/stderr
            // Thread ID - set when spawned
            thread_id: None,
            // Spawner PID - set when spawned by another process
            spawner_pid: None,
            // Terminal State - default for new processes
            terminal_state: Arc::new(Spinlock::new(terminal::TerminalState::default())),

            box_id: 0,
            namespace: akuma_isolation::global_namespace(),
            channel: None,
            delegate_pid: None,
            clear_child_tid: 0,
            robust_list_head: 0,
            robust_list_len: 0,
            signal_actions: [SignalAction::default(); MAX_SIGNALS],
            start_time_us: (runtime().uptime_us)(),
            last_syscall: core::sync::atomic::AtomicU64::new(0),
            syscall_stats: ProcessSyscallStats::new(),
})
    }

    /// Create a process from a large ELF file on disk, loading segments on demand.
    pub fn from_elf_path(name: &str, path: &str, file_size: usize, args: &[String], env: &[String], interp_prefix: Option<&str>) -> Result<Self, ElfError> {
        {
            let (allocated, heap_size) = (runtime().heap_stats)();
            log::debug!("[Process] heap before ELF load: {}MB / {}MB ({}%)",
                allocated / 1024 / 1024, heap_size / 1024 / 1024,
                if heap_size > 0 { allocated * 100 / heap_size } else { 0 });
        }
        let (entry_point, mut address_space, stack_pointer, brk, stack_bottom, stack_top, mmap_floor, deferred_segments) =
            elf_loader::load_elf_with_stack_from_path(path, file_size, args, env, config().user_stack_size, interp_prefix)?;

        let pid = NEXT_PID.fetch_add(1, Ordering::Relaxed);

        for seg in &deferred_segments {
            let source = match &seg.file_source {
                Some(fs) => LazySource::File {
                    path: fs.path.clone(),
                    inode: fs.inode,
                    file_offset: fs.file_offset,
                    filesz: fs.filesz,
                    segment_va: fs.segment_va,
                },
                None => LazySource::Zero,
            };
            push_lazy_region_with_source(pid, seg.start_va, seg.size, seg.page_flags, source);
        }

        let process_info_frame = (runtime().alloc_page_zeroed)().ok_or(ElfError::OutOfMemory)?;
        (runtime().track_frame)(process_info_frame, FrameSource::UserData);

        address_space
            .map_page(
                PROCESS_INFO_ADDR,
                process_info_frame.addr,
                crate::mmu::user_flags::RO | crate::mmu::flags::UXN | crate::mmu::flags::PXN,
            )
            .map_err(|_| ElfError::MappingFailed("process info page"))?;

        address_space.track_user_frame(process_info_frame);

        let memory = ProcessMemory::new(brk, stack_bottom, stack_top, mmap_floor);

        log::debug!("[Process] PID {} memory: code_end=0x{:x}, stack=0x{:x}-0x{:x}, mmap=0x{:x}-0x{:x}",
            pid, brk, stack_bottom, stack_top, memory.next_mmap, memory.mmap_limit);

        let mut fd_map = alloc::collections::BTreeMap::new();
        fd_map.insert(0, FileDescriptor::Stdin);
        fd_map.insert(1, FileDescriptor::Stdout);
        fd_map.insert(2, FileDescriptor::Stderr);

        let heap_lazy_size = compute_heap_lazy_size(brk, &memory);
        push_lazy_region(pid, brk, heap_lazy_size, crate::mmu::user_flags::RW_NO_EXEC);

        Ok(Self {
            pid,
            pgid: pid,
            name: String::from(name),
            state: ProcessState::Ready,
            address_space,
            context: UserContext::new(entry_point, stack_pointer),
            parent_pid: 0,
            brk,
            initial_brk: brk,
            entry_point,
            memory,
            process_info_phys: process_info_frame.addr,
            args: Vec::new(),
            cwd: String::from("/"),
            stdin: Spinlock::new(StdioBuffer::new()),
            stdout: Spinlock::new(StdioBuffer::new()),
            exited: false,
            exit_code: 0,
            dynamic_page_tables: Vec::new(),
            mmap_regions: Vec::new(),
            lazy_regions: Vec::new(),
            fd_table: Spinlock::new(fd_map),
            cloexec_fds: Spinlock::new(alloc::collections::BTreeSet::new()),
            nonblock_fds: Spinlock::new(alloc::collections::BTreeSet::new()),
            next_fd: AtomicU32::new(3),
            thread_id: None,
            spawner_pid: None,
            terminal_state: Arc::new(Spinlock::new(terminal::TerminalState::default())),
            box_id: 0,
            namespace: akuma_isolation::global_namespace(),
            channel: None,
            delegate_pid: None,
            clear_child_tid: 0,
            robust_list_head: 0,
            robust_list_len: 0,
            signal_actions: [SignalAction::default(); MAX_SIGNALS],
            start_time_us: (runtime().uptime_us)(),
            last_syscall: core::sync::atomic::AtomicU64::new(0),
            syscall_stats: ProcessSyscallStats::new(),
})
    }

    /// Replace current process image with a new ELF binary (execve core)
    pub fn replace_image(&mut self, elf_data: &[u8], args: &[String], env: &[String]) -> Result<(), &'static str> {
        let interp_prefix: Option<&str> = None;
        let (entry_point, mut address_space, sp, brk, stack_bottom, stack_top, mmap_floor, _deferred) =
            crate::elf_loader::load_elf_with_stack(elf_data, args, env, config().user_stack_size, interp_prefix)
            .map_err(|_| "Failed to load ELF")?;
            
        mmu::UserAddressSpace::deactivate();
        
        self.address_space = address_space;
        self.entry_point = entry_point;
        self.brk = brk;
        self.initial_brk = brk;
        self.memory = ProcessMemory::new(brk, stack_bottom, stack_top, mmap_floor);
        self.mmap_regions.clear();
        self.lazy_regions.clear();
        clear_lazy_regions(self.pid);
        self.dynamic_page_tables.clear();
        self.args = args.to_vec();
        self.clear_child_tid = 0;

        let heap_lazy_size = compute_heap_lazy_size(brk, &self.memory);
        push_lazy_region(self.pid, brk, heap_lazy_size, crate::mmu::user_flags::RW_NO_EXEC);
        
        if config().syscall_debug_info_enabled {
            log::debug!("[Process] PID {} replaced: entry=0x{:x}, brk=0x{:x}, stack=0x{:x}-0x{:x}, sp=0x{:x}",
                self.pid, entry_point, brk, stack_bottom, stack_top, sp);
        }

        // Update context for the next run
        self.context = UserContext::new(entry_point, sp);
        
        // Re-write process info page in the NEW address space
        let process_info_frame = (runtime().alloc_page_zeroed)().ok_or("OOM process info")?;
        (runtime().track_frame)(process_info_frame, FrameSource::UserData);
        
        self.address_space
            .map_page(
                PROCESS_INFO_ADDR,
                process_info_frame.addr,
                mmu::user_flags::RO | mmu::flags::UXN | mmu::flags::PXN,
            )
            .map_err(|_| "Failed to map process info")?;
            
        self.address_space.track_user_frame(process_info_frame);
        self.process_info_phys = process_info_frame.addr;

        unsafe {
            let info_ptr = mmu::phys_to_virt(self.process_info_phys) as *mut ProcessInfo;
            let info = ProcessInfo::new(self.pid, self.parent_pid, self.box_id);
            core::ptr::write(info_ptr, info);
        }

        // Reset I/O state (but keep FDs and Channel!)
        self.reset_io();
        
        Ok(())
    }

    /// Replace current process image using on-demand loading from a file path.
    pub fn replace_image_from_path(&mut self, path: &str, file_size: usize, args: &[String], env: &[String]) -> Result<(), &'static str> {
        let interp_prefix: Option<&str> = None;
        let (entry_point, mut address_space, sp, brk, stack_bottom, stack_top, mmap_floor, deferred_segments) =
            crate::elf_loader::load_elf_with_stack_from_path(path, file_size, args, env, config().user_stack_size, interp_prefix)
            .map_err(|_| "Failed to load ELF")?;

        mmu::UserAddressSpace::deactivate();

        self.address_space = address_space;
        self.entry_point = entry_point;
        self.brk = brk;
        self.initial_brk = brk;
        self.memory = ProcessMemory::new(brk, stack_bottom, stack_top, mmap_floor);
        self.mmap_regions.clear();
        self.lazy_regions.clear();
        clear_lazy_regions(self.pid);
        self.dynamic_page_tables.clear();
        self.args = args.to_vec();
        self.clear_child_tid = 0;

        for seg in &deferred_segments {
            let source = match &seg.file_source {
                Some(fs) => LazySource::File {
                    path: fs.path.clone(),
                    inode: fs.inode,
                    file_offset: fs.file_offset,
                    filesz: fs.filesz,
                    segment_va: fs.segment_va,
                },
                None => LazySource::Zero,
            };
            push_lazy_region_with_source(self.pid, seg.start_va, seg.size, seg.page_flags, source);
        }

        let heap_lazy_size = compute_heap_lazy_size(brk, &self.memory);
        push_lazy_region(self.pid, brk, heap_lazy_size, crate::mmu::user_flags::RW_NO_EXEC);

        if config().syscall_debug_info_enabled {
            log::debug!("[Process] PID {} replaced (on-demand): entry=0x{:x}, brk=0x{:x}, stack=0x{:x}-0x{:x}, sp=0x{:x}",
                self.pid, entry_point, brk, stack_bottom, stack_top, sp);
        }

        self.context = UserContext::new(entry_point, sp);

        let process_info_frame = (runtime().alloc_page_zeroed)().ok_or("OOM process info")?;
        (runtime().track_frame)(process_info_frame, FrameSource::UserData);

        self.address_space
            .map_page(
                PROCESS_INFO_ADDR,
                process_info_frame.addr,
                mmu::user_flags::RO | mmu::flags::UXN | mmu::flags::PXN,
            )
            .map_err(|_| "Failed to map process info")?;

        self.address_space.track_user_frame(process_info_frame);
        self.process_info_phys = process_info_frame.addr;

        unsafe {
            let info_ptr = mmu::phys_to_virt(self.process_info_phys) as *mut ProcessInfo;
            let info = ProcessInfo::new(self.pid, self.parent_pid, self.box_id);
            core::ptr::write(info_ptr, info);
        }

        self.reset_io();

        Ok(())
    }

    /// Set command line arguments for this process
    ///
    /// Arguments will be passed to the process via the ProcessInfo page.
    pub fn set_args(&mut self, args: &[&str]) {
        self.args = args.iter().map(|s| String::from(*s)).collect();
    }
    
    /// Set current working directory for this process
    pub fn set_cwd(&mut self, cwd: &str) {
        self.cwd = String::from(cwd);
    }

    /// Start executing this process (enters user mode)
    ///
    /// This function does not return normally - it jumps to user space.
    /// When the process makes a syscall or exception, control returns to kernel.
    pub fn run(&mut self) -> ! {
        self.state = ProcessState::Running;

        // Activate the user address space
        self.address_space.activate();

        // Jump to user mode
        unsafe {
            enter_user_mode(&self.context);
        }
    }

    /// Prepare process for execution (internal helper)
    ///
    /// Sets up process state and writes process info to the info page.
    /// Does NOT register in process table or enter userspace.
    fn prepare_for_execution(&mut self) {
        self.state = ProcessState::Running;

        // Reset per-process I/O state
        self.reset_io();

        // Write process info to the physical page (before activating address space)
        unsafe {
            let info_ptr = crate::mmu::phys_to_virt(self.process_info_phys) as *mut ProcessInfo;
            let info = ProcessInfo::new(self.pid, self.parent_pid, self.box_id);
            core::ptr::write(info_ptr, info);
        }
    }

    // ========== Per-Process I/O Methods (thread-safe with size limits) ==========

    /// Set stdin data for this process (with size limit)
    pub fn set_stdin(&mut self, data: &[u8]) {
        let mut stdin = self.stdin.lock();
        stdin.set_with_limit(data, config().proc_stdin_max_size);
    }

    /// Read from this process's stdin
    /// Returns number of bytes read
    pub fn read_stdin(&mut self, buf: &mut [u8]) -> usize {
        let mut stdin = self.stdin.lock();
        stdin.read(buf)
    }

    /// Write to this process's stdout (with size limit)
    ///
    /// Applies "last write wins" policy: if adding data would exceed
    /// PROC_STDOUT_MAX_SIZE, clears buffer before writing.
    pub fn write_stdout(&mut self, data: &[u8]) {
        let mut stdout = self.stdout.lock();
        stdout.write_with_limit(data, config().proc_stdout_max_size);
    }

    /// Take captured stdout (transfers ownership)
    pub fn take_stdout(&mut self) -> Vec<u8> {
        let mut stdout = self.stdout.lock();
        core::mem::take(&mut stdout.data)
    }

    /// Get current program break
    pub fn get_brk(&self) -> usize {
        self.brk
    }

    /// Set program break, returns new value.
    /// Maps any new pages between old and new brk.
    /// Returns the exact requested value (matching Linux brk ABI).
    pub fn set_brk(&mut self, new_brk: usize) -> usize {
        if new_brk < self.initial_brk {
            return self.brk;
        }
        let aligned = (new_brk + 0xFFF) & !0xFFF;
        let old_top = (self.brk + 0xFFF) & !0xFFF;
        if aligned > old_top {
            let mut page = old_top;
            while page < aligned {
                if !self.address_space.is_range_mapped(page, 0x1000) {
                    let _ = self.address_space.alloc_and_map(page, crate::mmu::user_flags::RW_NO_EXEC);
                }
                page += 0x1000;
            }
        }
        self.brk = new_brk;
        self.brk
    }

    /// Reset I/O state for execution
    pub fn reset_io(&mut self) {
        self.stdin.lock().pos = 0;
        self.stdout.lock().clear();
        self.exited = false;
        self.exit_code = 0;
    }

    // ========== File Descriptor Table Methods ==========

    /// Allocate a new file descriptor and insert the entry atomically
    ///
    /// This is the correct pattern to avoid race conditions:
    /// the FD number is allocated and inserted while holding the lock.
    pub fn alloc_fd(&self, entry: FileDescriptor) -> u32 {
        with_irqs_disabled(|| {
            let mut table = self.fd_table.lock();
            let fd = self.next_fd.fetch_add(1, Ordering::SeqCst);
            table.insert(fd, entry);
            fd
        })
    }

    /// Get a file descriptor entry (cloned)
    ///
    /// Returns a clone of the entry to avoid holding the lock.
    pub fn get_fd(&self, fd: u32) -> Option<FileDescriptor> {
        with_irqs_disabled(|| {
            self.fd_table.lock().get(&fd).cloned()
        })
    }

    /// Remove and return a file descriptor entry
    pub fn remove_fd(&self, fd: u32) -> Option<FileDescriptor> {
        with_irqs_disabled(|| {
            self.fd_table.lock().remove(&fd)
        })
    }

    /// Set a file descriptor entry at a specific FD number, replacing any existing entry
    pub fn set_fd(&self, fd: u32, entry: FileDescriptor) {
        with_irqs_disabled(|| {
            self.fd_table.lock().insert(fd, entry);
        });
    }

    /// Update a file descriptor entry (for file position updates, etc.)
    pub fn update_fd<F>(&self, fd: u32, f: F) -> bool
    where
        F: FnOnce(&mut FileDescriptor),
    {
        with_irqs_disabled(|| {
            let mut table = self.fd_table.lock();
            if let Some(entry) = table.get_mut(&fd) {
                f(entry);
                true
            } else {
                false
            }
        })
    }

    pub fn set_cloexec(&self, fd: u32) {
        with_irqs_disabled(|| {
            self.cloexec_fds.lock().insert(fd);
        });
    }

    pub fn clear_cloexec(&self, fd: u32) {
        with_irqs_disabled(|| {
            self.cloexec_fds.lock().remove(&fd);
        });
    }

    pub fn is_cloexec(&self, fd: u32) -> bool {
        with_irqs_disabled(|| {
            self.cloexec_fds.lock().contains(&fd)
        })
    }

    pub fn set_nonblock(&self, fd: u32) {
        with_irqs_disabled(|| {
            self.nonblock_fds.lock().insert(fd);
        });
    }

    pub fn clear_nonblock(&self, fd: u32) {
        with_irqs_disabled(|| {
            self.nonblock_fds.lock().remove(&fd);
        });
    }

    pub fn is_nonblock(&self, fd: u32) -> bool {
        with_irqs_disabled(|| {
            self.nonblock_fds.lock().contains(&fd)
        })
    }

    /// Close all FDs marked close-on-exec, returning them for cleanup.
    pub fn close_cloexec_fds(&self) -> Vec<(u32, FileDescriptor)> {
        with_irqs_disabled(|| {
            let cloexec: Vec<u32> = self.cloexec_fds.lock().iter().copied().collect();
            let mut closed = Vec::new();
            let mut table = self.fd_table.lock();
            for fd in &cloexec {
                if let Some(entry) = table.remove(fd) {
                    closed.push((*fd, entry));
                }
            }
            self.cloexec_fds.lock().clear();
            closed
        })
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        // Free any remaining dynamically allocated page table frames
        // This handles the case where the process is dropped without execute() being called
        for frame in self.dynamic_page_tables.drain(..) {
            (runtime().free_page)(frame);
        }
    }
}

/// Enter user mode with the given context
///
/// This sets up the CPU state and performs an ERET to EL0.
/// Does not return.
#[inline(never)]
#[allow(dead_code)]
pub unsafe fn enter_user_mode(ctx: &UserContext) -> ! {
    // SAFETY: This inline asm sets up CPU state and ERETs to user mode.
    // x30 is pinned as the context pointer and loaded last to avoid corruption.
    unsafe {
        core::arch::asm!(
            // Set system registers from named operands (consumed before GP loads)
            "msr sp_el0, {sp_user}",
            "msr elr_el1, {pc}",
            "msr spsr_el1, {spsr}",
            "msr tpidr_el0, {tls}",
            // Load x0-x29 from context struct (x30 = ctx pointer, stable throughout)
            "ldp x0, x1, [x30]",
            "ldp x2, x3, [x30, #16]",
            "ldp x4, x5, [x30, #32]",
            "ldp x6, x7, [x30, #48]",
            "ldp x8, x9, [x30, #64]",
            "ldp x10, x11, [x30, #80]",
            "ldp x12, x13, [x30, #96]",
            "ldp x14, x15, [x30, #112]",
            "ldp x16, x17, [x30, #128]",
            "ldp x18, x19, [x30, #144]",
            "ldp x20, x21, [x30, #160]",
            "ldp x22, x23, [x30, #176]",
            "ldp x24, x25, [x30, #192]",
            "ldp x26, x27, [x30, #208]",
            "ldp x28, x29, [x30, #224]",
            // Load x30 last (overwrites ctx pointer, no longer needed)
            "ldr x30, [x30, #240]",
            "eret",
            in("x30") ctx as *const UserContext,
            sp_user = in(reg) ctx.sp,
            pc = in(reg) ctx.pc,
            spsr = in(reg) ctx.spsr,
            tls = in(reg) ctx.tpidr,
            options(noreturn)
        )
    }
}

/// Execute a boxed process - enters user mode and never returns
///
/// This function takes ownership of the Box<Process>, registers it in the
/// PROCESS_TABLE (which takes ownership), then enters userspace via ERET.
///
/// MEMORY MANAGEMENT:
/// Previously, Process lived on the thread closure's stack, but execute() never
/// returns (it ERETs to userspace). When the process exits, return_to_kernel()
/// is called from the exception handler context, so the closure never completes
/// and Process::drop() was never called, leaking all physical pages.
///
/// Now, the Process is heap-allocated via Box and owned by PROCESS_TABLE.
/// When return_to_kernel() calls unregister_process(), the Box is returned
/// and dropped, calling Process::drop() -> UserAddressSpace::drop() which
/// frees all physical pages (code, data, stack, heap, page tables).
#[allow(dead_code)]
fn execute_boxed(mut process: Box<Process>) -> ! {
    // Prepare the process (set state, write process info page)
    process.prepare_for_execution();
    
    // Get PID and context pointer before registering (which moves the Box)
    let pid = process.pid;
    
    // Get raw pointer to access process after registration
    // SAFETY: The Box is moved to PROCESS_TABLE which keeps it alive.
    // The pointer remains valid until unregister_process() is called,
    // which only happens in return_to_kernel() after we've left userspace.
    let proc_ptr = &mut *process as *mut Process;
    
    // Register the process in the table - this transfers ownership of the Box
    // to PROCESS_TABLE. The process memory will be freed when unregister_process
    // returns the Box and it goes out of scope.
    register_process(pid, process);
    
    // Get reference back through the raw pointer
    // SAFETY: process is now owned by PROCESS_TABLE and won't move or be freed
    // until unregister_process is called (which happens after we exit userspace)
    let proc_ref = unsafe { &mut *proc_ptr };
    
    // Activate the user address space (sets TTBR0)
    proc_ref.address_space.activate();

    // Now safe to enable IRQs - TTBR0 is set to user tables
    (runtime().enable_irqs)();

    // Enter user mode via ERET - this never returns
    // When user calls exit(), the exception handler calls return_to_kernel()
    // which unregisters the process (dropping the Box and freeing memory)
    unsafe {
        enter_user_mode(&proc_ref.context);
    }
}

/// Check if process has exited and return to kernel if so
/// Called from exception handler after each syscall
#[unsafe(no_mangle)]
pub extern "C" fn check_process_exit() -> bool {
    // Use per-process exit flag instead of global
    match current_process() {
        Some(proc) => proc.exited,
        None => false,
    }
}

/// Return to kernel after process exit
/// 
/// Called from exception handler when process exits.
/// 
/// UNIFIED CONTEXT ARCHITECTURE:
/// Instead of restoring from KernelContext and returning to run_user_until_exit,
/// we now clean up directly and terminate the thread. This eliminates the dual
/// context system (THREAD_CONTEXTS vs KernelContext) that was a source of bugs.
/// 
/// The thread is marked as terminated and the scheduler will reclaim it.
/// Kill all threads sharing the same address space (L0 page table).
/// Used by exit_group and when the address-space owner exits to prevent
/// sibling threads from running with freed page tables.
pub fn kill_thread_group(my_pid: Pid, l0_phys: usize) {
    let siblings: Vec<(Pid, Option<usize>)> = with_irqs_disabled(|| {
        let table = PROCESS_TABLE.lock();
        table.iter()
            .filter(|(pid, proc)| **pid != my_pid && proc.address_space.l0_phys() == l0_phys)
            .map(|(pid, proc)| (*pid, proc.thread_id))
            .collect()
    });

    for (sib_pid, sib_tid) in &siblings {
        if let Some(proc) = lookup_process(*sib_pid) {
            cleanup_process_fds(proc);
        }
        clear_lazy_regions(*sib_pid);

        if let Some(tid) = sib_tid {
            with_irqs_disabled(|| {
                THREAD_PID_MAP.lock().remove(tid);
            });
            if let Some(channel) = remove_channel(*tid) {
                channel.set_exited(137);
            }
        }

        let _dropped = unregister_process(*sib_pid);

        if let Some(tid) = sib_tid {
            crate::threading::mark_thread_terminated(*tid);
        }
    }

    if !siblings.is_empty() {
        log::debug!("[Process] Killed {} sibling thread(s) for PID {}",
            siblings.len(), my_pid);
    }
}

/// Exit code is communicated via ProcessChannel for async callers.
#[unsafe(no_mangle)]
pub extern "C" fn return_to_kernel(exit_code: i32) -> ! {
    let lr: u64;
    unsafe { core::arch::asm!("mov {}, x30", out(reg) lr); }
    let tid = crate::threading::current_thread_id();
    log::debug!("[RTK] code={} tid={} LR={:#x}", exit_code, tid, lr);
    
    // Check if this thread was already killed externally (by kill_process).
    // If so, cleanup has already been done - just skip to the yield loop.
    // This handles the race where kill_process() terminates the thread while
    // it's still running, and it later reaches this exit path.
    let already_terminated = crate::threading::is_thread_terminated(tid);
    
    // Get process info before cleanup (skip if already killed)
    let pid = if !already_terminated {
        if let Some(proc) = current_process() {
            let pid = proc.pid;
            
            // Clean up all open FDs for this process (sockets, child channels)
            // This must happen before unregistering the process so we can access fd_table
            cleanup_process_fds(proc);
            
            Some(pid)
        } else {
            None
        }
    } else {
        None
    };
    
    // Set exit code on ProcessChannel if registered for this thread
    // This notifies async callers (SSH shell, etc.) that the process exited
    // Safe to call even if already removed by kill_process - just returns None
    if let Some(channel) = remove_channel(tid) {
        channel.set_exited(exit_code);
    }
    
    // Clean up THREAD_PID_MAP entry for thread clones
    with_irqs_disabled(|| {
        THREAD_PID_MAP.lock().remove(&tid);
    });

    // CLONE_CHILD_CLEARTID: write 0 to the TID address and wake futex.
    // Must happen while user address space is still active.
    // Verify the page is actually mapped before writing — the address may
    // point to a lazily-mapped page that was never faulted in, and writing
    // from EL1 won't trigger demand paging (only EL0 faults do).
    if !already_terminated {
        if let Some(proc) = lookup_process(pid.unwrap_or(0)) {
            let tid_addr = proc.clear_child_tid;
            if tid_addr != 0 && crate::mmu::is_current_user_page_mapped(tid_addr as usize) {
                unsafe { core::ptr::write(tid_addr as *mut u32, 0); }
                (runtime().futex_wake)(tid_addr as usize, i32::MAX);
            }

            // Robust futex list cleanup: walk the list and mark owned futexes
            // with FUTEX_OWNER_DIED so waiters don't deadlock.
            let robust_head = proc.robust_list_head;
            if robust_head != 0 {
                const FUTEX_OWNER_DIED: u32 = 0x40000000;
                const ROBUST_LIST_LIMIT: usize = 2048;
                let my_tid = proc.pid;
                // robust_list_head layout: { next: *mut robust_list, futex_offset: long, list_op_pending: *mut robust_list }
                if crate::mmu::is_current_user_page_mapped(robust_head as usize) {
                    let futex_offset = unsafe {
                        core::ptr::read((robust_head as usize + 8) as *const i64)
                    };
                    let pending_ptr = unsafe {
                        core::ptr::read((robust_head as usize + 16) as *const u64)
                    };

                    // Walk the linked list
                    let mut entry = unsafe { core::ptr::read(robust_head as *const u64) };
                    let mut count = 0usize;
                    while entry != robust_head && entry != 0 && count < ROBUST_LIST_LIMIT {
                        if crate::mmu::is_current_user_page_mapped(entry as usize) {
                            let futex_addr = (entry as i64 + futex_offset) as usize;
                            if crate::mmu::is_current_user_page_mapped(futex_addr) {
                                let word = unsafe { core::ptr::read(futex_addr as *const u32) };
                                if (word & 0x3FFFFFFF) == my_tid {
                                    unsafe { core::ptr::write(futex_addr as *mut u32, word | FUTEX_OWNER_DIED); }
                                    (runtime().futex_wake)(futex_addr, 1);
                                }
                            }
                            entry = unsafe { core::ptr::read(entry as *const u64) };
                        } else {
                            break;
                        }
                        count += 1;
                    }

                    // Handle pending operation
                    if pending_ptr != 0 && crate::mmu::is_current_user_page_mapped(pending_ptr as usize) {
                        let futex_addr = (pending_ptr as i64 + futex_offset) as usize;
                        if crate::mmu::is_current_user_page_mapped(futex_addr) {
                            let word = unsafe { core::ptr::read(futex_addr as *const u32) };
                            if (word & 0x3FFFFFFF) == my_tid {
                                unsafe { core::ptr::write(futex_addr as *mut u32, word | FUTEX_OWNER_DIED); }
                                (runtime().futex_wake)(futex_addr, 1);
                            }
                        }
                    }
                }
            }
        }
    }

    // Deactivate user address space - restore boot TTBR0
    // CRITICAL: This must happen BEFORE we drop the Process (via unregister_process)
    // because Drop frees the page tables. If we drop first, TTBR0 would point to
    // freed memory causing a crash on any TLB miss.
    crate::mmu::UserAddressSpace::deactivate();
    
    // Now unregister and DROP the process
    // This calls Process::drop() -> UserAddressSpace::drop() which frees:
    // - All user pages (code, data, stack, heap, mmap)
    // - All page table frames (L0, L1, L2, L3)
    // - The ASID
    // This fixes the memory leak where processes would never free their pages.
    if let Some(pid) = pid {
        // Check if this was a primary process for an active box.
        // If so, the entire box should be shut down.
        let box_to_kill = find_primary_box(pid);

        if let Some(bid) = box_to_kill {
            log::debug!("[Process] Primary PID {} exited, shutting down box {:08x}", pid, bid);
            // kill_box handles unregistering the box and killing remaining PIDs
            if let Err(e) = kill_box(bid) {
                log::debug!("[Process] Error: Failed to kill box {:08x}: {}", bid, e);
            }
        }

        // If this process owns the address space (not shared), kill all
        // sibling CLONE_VM threads BEFORE dropping. Dropping the owner frees
        // all page tables; siblings still using them would cause EL1 faults.
        if let Some(proc) = lookup_process(pid) {
            if !proc.address_space.is_shared() {
                let l0_phys = proc.address_space.l0_phys();
                kill_thread_group(pid, l0_phys);
            }
        }

        let (start_us, proc_name) = lookup_process(pid)
            .map(|p| (p.start_time_us, p.name.clone()))
            .unwrap_or((0, alloc::string::String::from("?")));
        let elapsed_us = (runtime().uptime_us)().saturating_sub(start_us);
        let secs = elapsed_us / 1_000_000;
        let frac = (elapsed_us % 1_000_000) / 10_000; // centiseconds

        if process_syscall_stats_enabled() {
            if let Some(proc) = lookup_process(pid) {
                proc.syscall_stats.dump(pid, &proc_name, elapsed_us);
            }
        }

        clear_lazy_regions(pid);
        let _dropped_process = unregister_process(pid);
        log::debug!("[Process] PID {} thread {} exited ({}) [{}.{:02}s]", pid, tid, exit_code, secs, frac);
    } else {
        log::debug!("[Process] Thread {} exited ({})", tid, exit_code);
    }
    
    // Mark thread as terminated so scheduler stops scheduling it
    // Idempotent - safe to call even if already marked by kill_process
    crate::threading::mark_current_terminated();
    
    // Yield forever - thread is terminated, scheduler will reclaim it
    // Thread 0's cleanup routine will free the thread slot
    loop {
        crate::threading::yield_now();
    }
}

/// Clean up all file descriptors owned by a process
fn cleanup_process_fds(proc: &Process) {
    // 1. Collect all special FDs that need manual cleanup
    let fds: alloc::vec::Vec<FileDescriptor> = {
        let table = proc.fd_table.lock();
        table.values().cloned().collect()
    };
    
    // 2. Perform manual cleanup for special FDs
    for fd in fds {
        match fd {
            FileDescriptor::Socket(idx) => {
                (runtime().remove_socket)(idx);
            }
            FileDescriptor::ChildStdout(child_pid) => {
                remove_child_channel(child_pid);
            }
            FileDescriptor::PipeWrite(pipe_id) => {
                (runtime().pipe_close_write)(pipe_id);
            }
            FileDescriptor::PipeRead(pipe_id) => {
                (runtime().pipe_close_read)(pipe_id);
            }
            FileDescriptor::EventFd(efd_id) => {
                (runtime().eventfd_close)(efd_id);
            }
            _ => {}
        }
    }
    
    // 3. Clear the FD table. 
    // This will drop KernelFile and other descriptors, 
    // which in turn will call their respective cleanup logic (like VFS close).
    let mut table = proc.fd_table.lock();
    table.clear();
}

/// Kill a process by PID
///
/// Terminates the process and cleans up all associated resources:
/// - Closes all open sockets and file descriptors
/// - Removes process from process table
/// - Removes process channel
/// - Marks the thread as terminated
///
/// # Arguments
/// * `pid` - Process ID to kill
///
/// # Returns
/// * `Ok(())` if the process was successfully killed
/// * `Err(message)` if the process was not found or could not be killed
pub fn kill_process(pid: Pid) -> Result<(), &'static str> {
    // Look up the process
    let proc = lookup_process(pid).ok_or("Process not found")?;
    
    // Get thread_id before cleanup (needed for channel removal and thread termination)
    let thread_id = proc.thread_id.ok_or("Process has no thread_id (not yet started?)")?;
    
    // Set the interrupt flag FIRST - this allows blocked syscalls (like accept())
    // to detect the interrupt and properly abort their sockets before we clean up.
    // The interrupt check in syscalls will cause them to return EINTR and clean up
    // their own resources (e.g., abort TcpSocket in block_on_accept).
    if let Some(channel) = get_channel(thread_id) {
        channel.set_interrupted();
    }
    
    // Yield a few times to give the blocked thread a chance to detect the interrupt
    // and abort its sockets. This is important for listening sockets in accept().
    for _ in 0..5 {
        crate::threading::yield_now();
    }
    
    // Clean up all open FDs for this process
    // Note: This cleans up sockets in the fd_table, but sockets created inside
    // syscalls (like the TcpSocket in accept()) are handled by the interrupt mechanism.
    cleanup_process_fds(proc);
    
    // Mark process as killed (using signal 9 = SIGKILL)
    proc.exited = true;
    proc.exit_code = 137; // 128 + SIGKILL(9)
    proc.state = ProcessState::Zombie(137);
    
    // Done using proc - the reference becomes invalid after unregister_process
    // drops the Box. We don't access proc after this point.
    // (Using let _ = proc would be redundant since it's just a reference)
    
    // Deactivate user TTBR0 for the killed thread
    // Note: The killed thread will do this itself in return_to_kernel when it
    // eventually runs, but if it's blocked in a syscall it may not run soon.
    // For safety, we rely on the thread to deactivate its own TTBR0.
    
    // Clear lazy region metadata before dropping the process.
    // Without this, the LAZY_REGION_TABLE BTreeMap entry leaks.
    clear_lazy_regions(pid);

    // Unregister from process table and DROP the Box<Process>
    // This calls Process::drop() -> UserAddressSpace::drop() which frees:
    // - All user pages (code, data, stack, heap, mmap)
    // - All page table frames (L0, L1, L2, L3)
    // - The ASID
    let _dropped_process = unregister_process(pid);
    // _dropped_process goes out of scope here, triggering the drop
    
    // Remove and notify the process channel
    if let Some(channel) = remove_channel(thread_id) {
        channel.set_exited(137);
    }
    
    // Mark the thread as terminated so scheduler stops scheduling it
    crate::threading::mark_thread_terminated(thread_id);
    
    log::debug!("[kill] Killed PID {} (thread {})", pid, thread_id);
    
    Ok(())
}


pub fn waitpid(pid: Pid) -> Option<(Pid, i32)> {
    if let Some(ch) = get_child_channel(pid) {
        if ch.has_exited() {
            return Some((pid, ch.exit_code()));
        }
    }
    None
}

/// Fork the current process (deep copy)
/// Returns the new PID to the parent
pub fn fork_process(child_pid: u32, stack_ptr: u64) -> Result<u32, &'static str> {
    if (runtime().is_memory_low)() {
        return Err("Kernel memory low, cannot fork");
    }
    let parent = current_process().ok_or("No current process")?;
    let parent_pid = parent.pid;
    
    // 1. Create new address space
    let mut new_address_space = mmu::UserAddressSpace::new().ok_or("Failed to create address space")?;
    
    // 2. Allocate process info page
    let process_info_frame = (runtime().alloc_page_zeroed)().ok_or("OOM process info")?;
    (runtime().track_frame)(process_info_frame, FrameSource::UserData);
    
    new_address_space
        .map_page(
            PROCESS_INFO_ADDR,
            process_info_frame.addr,
            mmu::user_flags::RO | mmu::flags::UXN | mmu::flags::PXN,
        )
        .map_err(|_| "Failed to map process info")?;
    new_address_space.track_user_frame(process_info_frame);

    // 3. Create Process struct (fallible allocation to avoid kernel panic on OOM)
    let mut new_proc = Box::try_new(Process {
        pid: child_pid,
        pgid: parent.pgid,
        name: parent.name.clone(),
        parent_pid: parent_pid,
        state: ProcessState::Ready,
        context: UserContext::default(), // Will be updated below
        address_space: new_address_space,
        entry_point: parent.entry_point,
        brk: parent.brk,
        initial_brk: parent.initial_brk,
        memory: parent.memory.clone(),
        process_info_phys: process_info_frame.addr,
        args: parent.args.clone(),
        cwd: parent.cwd.clone(),
        stdin: Spinlock::new(StdioBuffer::new()),
        stdout: Spinlock::new(StdioBuffer::new()),
        exited: false,
        exit_code: 0,
        dynamic_page_tables: Vec::new(),
        mmap_regions: Vec::new(),
        lazy_regions: Vec::new(),
        fd_table: {
            let cloned = parent.fd_table.lock().clone();
            for entry in cloned.values() {
                match entry {
                    FileDescriptor::PipeWrite(id) => (runtime().pipe_clone_ref)(*id, true),
                    FileDescriptor::PipeRead(id) => (runtime().pipe_clone_ref)(*id, false),
                    _ => {}
                }
            }
            Spinlock::new(cloned)
        },
        cloexec_fds: Spinlock::new(parent.cloexec_fds.lock().clone()),
        nonblock_fds: Spinlock::new(parent.nonblock_fds.lock().clone()),
        next_fd: AtomicU32::new(parent.next_fd.load(Ordering::Relaxed)),
        thread_id: None,
        spawner_pid: parent.spawner_pid,
        terminal_state: parent.terminal_state.clone(),
        box_id: parent.box_id,
        namespace: parent.namespace.clone(),
        channel: parent.channel.clone(),
        delegate_pid: None,
        clear_child_tid: 0,
        robust_list_head: 0,
        robust_list_len: 0,
        signal_actions: parent.signal_actions,
        start_time_us: (runtime().uptime_us)(),
        last_syscall: core::sync::atomic::AtomicU64::new(0),
        syscall_stats: ProcessSyscallStats::new(),
    }).map_err(|_| "Failed to allocate Process struct (ENOMEM)")?;
    
    // 4. Perform memory copy
    let stack_top = parent.memory.stack_top;
    let stack_size = config().user_stack_size; 
    let stack_start = stack_top - stack_size;
    
    // Snapshot parent's L0 page table pointer so we can translate VAs to
    // physical addresses without relying on TTBR0 staying valid across
    // potential context switches during the (long) copy.
    let parent_l0 = {
        let ttbr0 = mmu::get_current_ttbr0();
        let l0_addr = ttbr0 & 0x0000_FFFF_FFFF_F000;
        mmu::phys_to_virt(l0_addr) as *const u64
    };

    fn copy_range_phys(parent_l0: *const u64, src_va: usize, len: usize, dest_as: &mut mmu::UserAddressSpace) -> Result<(), &'static str> {
        let pages = (len + mmu::PAGE_SIZE - 1) / mmu::PAGE_SIZE;
        let mut copied = 0usize;
        for i in 0..pages {
            let va = src_va + i * mmu::PAGE_SIZE;
            if let Some(src_phys) = mmu::translate_user_va(parent_l0, va) {
                let frame = dest_as.alloc_and_map(va, mmu::user_flags::RW)?;
                unsafe {
                    let src_ptr = mmu::phys_to_virt(src_phys & !0xFFF) as *const u8;
                    let dest_ptr = mmu::phys_to_virt(frame.addr);
                    core::ptr::copy_nonoverlapping(src_ptr, dest_ptr, mmu::PAGE_SIZE);
                }
                copied += 1;
            }
        }
        if config().syscall_debug_info_enabled && copied < pages {
            log::debug!("[fork] copy_range WARNING: 0x{:x}..0x{:x}: {}/{} pages copied ({} unmapped)",
                src_va, src_va + len, copied, pages, pages - copied);
        }
        Ok(())
    }

    copy_range_phys(parent_l0, stack_start, stack_size, &mut new_proc.address_space)?;

    // Copy code+heap range.  Derive code_start from code_end (which is
    // always in the main binary's range) rather than entry_point (which
    // points into the interpreter for dynamically-linked binaries).
    let code_start = if parent.memory.code_end >= 0x1000_0000 {
        0x1000_0000 // PIE binary base
    } else {
        0x400000
    };
    if parent.brk > code_start {
        copy_range_phys(parent_l0, code_start, parent.brk - code_start, &mut new_proc.address_space)?;
    }

    // Copy dynamic linker / interpreter region (0x3000_0000).  These pages
    // are mapped by the ELF loader but not tracked in mmap_regions.
    let interp_base = 0x3000_0000usize;
    let interp_scan_size = 2 * 1024 * 1024; // 2 MB — covers even large musl builds
    if mmu::translate_user_va(parent_l0, interp_base).is_some() {
        copy_range_phys(parent_l0, interp_base, interp_scan_size, &mut new_proc.address_space)?;
    }

    // Copy mmap regions so forked children can run built-in applets (e.g.
    // busybox sh pipes) without crashing on unmapped pages.  We cap total
    // copied pages to avoid OOM when a parent has huge file mappings.
    const MAX_FORK_MMAP_PAGES: usize = 2048; // 8 MB cap
    let mut total_copied_pages: usize = 0;
    let mut child_mmap_regions: Vec<(usize, Vec<PhysFrame>)> = Vec::new();

    for (va_start, parent_frames) in &parent.mmap_regions {
        if total_copied_pages + parent_frames.len() > MAX_FORK_MMAP_PAGES {
            if config().syscall_debug_info_enabled {
                log::debug!("[fork] skipping mmap region 0x{:x} ({} pages) — would exceed cap",
                    va_start, parent_frames.len());
            }
            continue;
        }
        let mut child_frames: Vec<PhysFrame> = Vec::new();
        let mut ok = true;
        for (i, pf) in parent_frames.iter().enumerate() {
            let page_va = va_start + i * mmu::PAGE_SIZE;
            match (runtime().alloc_page_zeroed)() {
                Some(frame) => {
                    (runtime().track_frame)(frame, FrameSource::UserData);
                    unsafe {
                        let src = mmu::phys_to_virt(pf.addr) as *const u8;
                        let dst = mmu::phys_to_virt(frame.addr);
                        core::ptr::copy_nonoverlapping(src, dst, mmu::PAGE_SIZE);
                    }
                    if new_proc.address_space.map_page(page_va, frame.addr, mmu::user_flags::RW).is_err() {
                        ok = false;
                        break;
                    }
                    new_proc.address_space.track_user_frame(frame);
                    child_frames.push(frame);
                }
                None => { ok = false; break; }
            }
        }
        if ok {
            total_copied_pages += child_frames.len();
            child_mmap_regions.push((*va_start, child_frames));
        } else {
            if config().syscall_debug_info_enabled {
                log::debug!("[fork] OOM copying mmap region 0x{:x}, skipping rest", va_start);
            }
            break;
        }
    }

    new_proc.mmap_regions = child_mmap_regions;
    new_proc.lazy_regions = Vec::new(); // managed via LAZY_REGION_TABLE
    new_proc.memory.next_mmap = parent.memory.next_mmap;
    
    // 5. Write ProcessInfo to child's process info page
    unsafe {
        let info_ptr = mmu::phys_to_virt(new_proc.process_info_phys) as *mut ProcessInfo;
        let info = ProcessInfo::new(child_pid, parent_pid, new_proc.box_id);
        core::ptr::write(info_ptr, info);
    }

    // 6. Capture parent's user context and create child context
    let parent_tid = crate::threading::current_thread_id();
    let parent_ctx = crate::threading::get_saved_user_context(parent_tid).ok_or("No saved context")?;
    
    let mut child_ctx = parent_ctx;
    child_ctx.x0 = 0;    // fork returns 0 to child
    child_ctx.spsr = 0;  // Clean EL0t with interrupts enabled
    if stack_ptr != 0 {
        child_ctx.sp = stack_ptr;
    }

    // Store context in the Process struct (entry_point_trampoline uses proc.context)
    new_proc.context = child_ctx;

    // 7. Allocate thread but keep it INITIALIZING
    let tid = crate::threading::spawn_user_thread_initializing(
        entry_point_trampoline as extern "C" fn() -> !, 
        core::ptr::null_mut(), 
        false
    )?;
    
    new_proc.thread_id = Some(tid);
    crate::threading::update_thread_context(tid, &child_ctx);

    // 8. Create a ProcessChannel for exit notification only.
    // The child keeps parent.channel (set in struct init above) for I/O so its
    // stdout writes are visible on the same SSH stream as the parent.
    // The exit-tracking channel is separate to avoid contaminating the I/O channel.
    let exit_channel = Arc::new(ProcessChannel::new());
    register_channel(tid, exit_channel.clone());
    register_child_channel(child_pid, exit_channel, parent_pid);

    // Register process BEFORE marking thread READY
    register_process(child_pid, new_proc);
    clone_lazy_regions(parent_pid, child_pid);
    
    // Now safe to start the thread
    crate::threading::mark_thread_ready(tid);
    
    Ok(child_pid)
}

/// Clone a thread within the same process (CLONE_THREAD | CLONE_VM).
/// The child shares the parent's address space and file descriptors.
pub fn clone_thread(stack: u64, tls: u64, parent_tid_ptr: u64, child_tid_ptr: u64) -> Result<u32, &'static str> {
    if (runtime().is_memory_low)() {
        return Err("Kernel memory low, cannot clone thread");
    }
    let parent = current_process().ok_or("No current process")?;
    let parent_pid = parent.pid;
    let child_pid = allocate_pid();

    let parent_l0_phys = parent.address_space.ttbr0() & 0x0000_FFFF_FFFF_F000;
    let shared_as = mmu::UserAddressSpace::new_shared(parent_l0_phys as usize)
        .ok_or("Failed to create shared address space")?;

    let mut new_proc = Box::try_new(Process {
        pid: child_pid,
        pgid: parent.pgid,
        name: parent.name.clone(),
        parent_pid: parent_pid,
        state: ProcessState::Ready,
        context: UserContext::default(),
        address_space: shared_as,
        entry_point: parent.entry_point,
        brk: parent.brk,
        initial_brk: parent.initial_brk,
        memory: parent.memory.clone(),
        process_info_phys: parent.process_info_phys,
        args: parent.args.clone(),
        cwd: parent.cwd.clone(),
        stdin: Spinlock::new(StdioBuffer::new()),
        stdout: Spinlock::new(StdioBuffer::new()),
        exited: false,
        exit_code: 0,
        dynamic_page_tables: Vec::new(),
        mmap_regions: Vec::new(),
        lazy_regions: Vec::new(), // managed via LAZY_REGION_TABLE
        fd_table: {
            let cloned = parent.fd_table.lock().clone();
            for entry in cloned.values() {
                match entry {
                    FileDescriptor::PipeWrite(id) => (runtime().pipe_clone_ref)(*id, true),
                    FileDescriptor::PipeRead(id) => (runtime().pipe_clone_ref)(*id, false),
                    _ => {}
                }
            }
            Spinlock::new(cloned)
        },
        cloexec_fds: Spinlock::new(parent.cloexec_fds.lock().clone()),
        nonblock_fds: Spinlock::new(parent.nonblock_fds.lock().clone()),
        next_fd: AtomicU32::new(parent.next_fd.load(Ordering::Relaxed)),
        thread_id: None,
        spawner_pid: parent.spawner_pid,
        terminal_state: parent.terminal_state.clone(),
        box_id: parent.box_id,
        namespace: parent.namespace.clone(),
        channel: parent.channel.clone(),
        delegate_pid: None,
        clear_child_tid: child_tid_ptr,
        robust_list_head: 0,
        robust_list_len: 0,
        signal_actions: parent.signal_actions,
        start_time_us: (runtime().uptime_us)(),
        last_syscall: core::sync::atomic::AtomicU64::new(0),
        syscall_stats: ProcessSyscallStats::new(),
    }).map_err(|_| "Failed to allocate Process struct (ENOMEM)")?;

    let parent_tid = crate::threading::current_thread_id();
    let parent_ctx = crate::threading::get_saved_user_context(parent_tid).ok_or("No saved context")?;

    let mut child_ctx = parent_ctx;
    child_ctx.x0 = 0;
    child_ctx.sp = stack;
    child_ctx.tpidr = tls;
    child_ctx.spsr = 0;

    new_proc.context = child_ctx;

    let tid = crate::threading::spawn_user_thread_initializing(
        entry_point_trampoline as extern "C" fn() -> !,
        core::ptr::null_mut(),
        false
    )?;

    new_proc.thread_id = Some(tid);
    crate::threading::update_thread_context(tid, &child_ctx);

    let exit_channel = Arc::new(ProcessChannel::new());
    register_channel(tid, exit_channel.clone());
    register_child_channel(child_pid, exit_channel, parent_pid);

    // Register in THREAD_PID_MAP so current_process() works for this thread
    with_irqs_disabled(|| {
        THREAD_PID_MAP.lock().insert(tid, child_pid);
    });

    register_process(child_pid, new_proc);
    clone_lazy_regions(parent_pid, child_pid);

    // Write child TID/PID to parent_tid_ptr (CLONE_PARENT_SETTID)
    if parent_tid_ptr != 0 {
        unsafe { core::ptr::write(parent_tid_ptr as *mut u32, child_pid); }
    }
    // Write child TID/PID to child_tid_ptr (CLONE_CHILD_CLEARTID)
    if child_tid_ptr != 0 {
        unsafe { core::ptr::write(child_tid_ptr as *mut u32, child_pid); }
    }

    crate::threading::mark_thread_ready(tid);

    if config().syscall_debug_info_enabled {
        log::debug!("[syscall] clone_thread: PID {} -> thread PID {} (tid {})", parent_pid, child_pid, tid);
    }

    Ok(child_pid)
}

/// Allocate a new unique PID (uses the same global counter as Process::from_elf)
pub fn allocate_pid() -> Pid {
    NEXT_PID.fetch_add(1, Ordering::SeqCst)
}

/// Trampoline for new process threads
/// Called by threading::spawn_user_thread
pub extern "C" fn entry_point_trampoline() -> ! {
    let tid = crate::threading::current_thread_id();
    let mut proc_ptr: *mut Process = core::ptr::null_mut();
    
    with_irqs_disabled(|| {
        let mut processes = PROCESS_TABLE.lock();
        for proc in processes.values_mut() {
            if proc.thread_id == Some(tid) {
                proc_ptr = &mut **proc as *mut Process;
                break;
            }
        }
    });
    
    if proc_ptr.is_null() {
        log::debug!("[process] FATAL: No process found for thread {}", tid);
        crate::threading::mark_current_terminated();
        loop { crate::threading::yield_now(); }
    }
    
    unsafe {
        (*proc_ptr).run();
    }
}

/// Execute an ELF binary from the filesystem with per-process I/O (blocking)
///
/// This spawns the process on a user thread and polls for completion.
/// Use exec_async() for non-blocking execution.
///
/// # Arguments
/// * `path` - Path to the ELF binary
/// * `args` - Optional command line arguments (first arg is conventionally the program name)
/// * `stdin` - Optional stdin data for the process
///
/// # Returns
/// Tuple of (exit_code, stdout_data), or error message
pub fn exec_with_io(path: &str, args: Option<&[&str]>, stdin: Option<&[u8]>) -> Result<(i32, Vec<u8>), String> {
    exec_with_io_cwd(path, args, None, stdin, None)
}

/// exec_with_io with explicit cwd
pub fn exec_with_io_cwd(path: &str, args: Option<&[&str]>, env: Option<&[String]>, stdin: Option<&[u8]>, cwd: Option<&str>) -> Result<(i32, Vec<u8>), String> {
    // Spawn process with channel and cwd
    let (thread_id, channel, _pid) = spawn_process_with_channel_cwd(path, args, env, stdin, cwd)?;
    
    // For non-interactive execution, if no stdin was provided, mark it as closed
    // so the process doesn't block forever if it tries to read from it.
    if stdin.is_none() {
        channel.close_stdin();
    }

    // Poll until process exits (blocking)
    loop {
        if channel.has_exited() || crate::threading::is_thread_terminated(thread_id) {
            break;
        }
        // Yield to let process run
        crate::threading::yield_now();
    }
    
    // Collect output
    let mut stdout_data = Vec::new();
    while let Some(data) = channel.try_read() {
        stdout_data.extend_from_slice(&data);
    }
    
    // Cleanup terminated thread
    crate::threading::cleanup_terminated();
    
    Ok((channel.exit_code(), stdout_data))
}

/// Execute an ELF binary from the filesystem (legacy API for backwards compatibility)
///
/// # Arguments
/// * `path` - Path to the ELF binary
///
/// # Returns
/// Exit code of the process, or error message
#[allow(dead_code)]
pub fn exec(path: &str) -> Result<i32, String> {
    let (exit_code, _stdout) = exec_with_io(path, None, None)?;
    Ok(exit_code)
}

/// Spawn a process on a user thread for concurrent execution
///
/// This function creates a new process from the ELF file and spawns it on a
/// dedicated user thread (slots 8-31). The process runs concurrently with
/// other threads and processes.
///
/// # Arguments
/// * `path` - Path to the ELF binary
/// * `args` - Optional command line arguments
/// * `stdin` - Optional stdin data for the process
///
/// # Returns
/// Thread ID of the spawned thread, or error message
pub fn spawn_process(path: &str, args: Option<&[&str]>, stdin: Option<&[u8]>) -> Result<usize, String> {
    let (thread_id, _channel, _pid) = spawn_process_with_channel(path, args, stdin)?;
    Ok(thread_id)
}

/// Spawn a process on a user thread with a channel for I/O
///
/// Like spawn_process, but returns a ProcessChannel that can be used to
/// read the process's output and check its exit status.
///
/// # Arguments
/// * `path` - Path to the ELF binary
/// * `args` - Optional command line arguments
/// * `stdin` - Optional stdin data for the process
/// * `cwd` - Optional current working directory (defaults to "/")
///
/// # Returns
/// Tuple of (thread_id, channel, pid) or error message
pub fn spawn_process_with_channel(
    path: &str,
    args: Option<&[&str]>,
    stdin: Option<&[u8]>,
) -> Result<(usize, Arc<ProcessChannel>, Pid), String> {
    spawn_process_with_channel_cwd(path, args, None, stdin, None)
}

/// Spawn a process on a user thread with a channel for I/O and specified cwd
///
/// # Arguments
/// * `path` - Path to the ELF binary
/// * `args` - Optional command line arguments
/// * `stdin` - Optional stdin data for the process
/// * `cwd` - Optional current working directory (defaults to "/")
///
/// # Returns
/// Tuple of (thread_id, channel, pid) or error message
pub fn spawn_process_with_channel_cwd(
    path: &str,
    args: Option<&[&str]>,
    env: Option<&[String]>,
    stdin: Option<&[u8]>,
    cwd: Option<&str>,
) -> Result<(usize, Arc<ProcessChannel>, Pid), String> {
    spawn_process_with_channel_ext(path, args, env, stdin, cwd, 0)
}

/// Extended version of spawn_process_with_channel
pub fn spawn_process_with_channel_ext(
    path: &str,
    args: Option<&[&str]>,
    env: Option<&[String]>,
    stdin: Option<&[u8]>,
    cwd: Option<&str>,
    box_id: u64,
) -> Result<(usize, Arc<ProcessChannel>, Pid), String> {
    if crate::threading::user_threads_available() == 0 {
        return Err("No available user threads for process execution".into());
    }

    // Reject new processes under memory pressure to prevent OOM cascade
    if (runtime().is_memory_low)() {
        return Err("Kernel memory low, cannot spawn new process".into());
    }

    // If the box has a namespace with mounts (SubdirFs at /), activate a
    // per-thread namespace override so that runtime().read_file and
    // resolve_symlinks go through the container's mount table.
    let container_ns = if box_id != 0 {
        (runtime().get_box_namespace)(box_id)
    } else {
        None
    };
    let use_ns_override = container_ns.as_ref().is_some_and(|ns| !ns.mount.lock().is_empty());

    if use_ns_override {
        (runtime().set_spawn_namespace)(container_ns.as_ref().unwrap().clone());
    }

    let resolved = (runtime().resolve_symlinks)(path);
    let elf_path = &resolved;

    let mut full_args = Vec::new();
    full_args.push(path.to_string());
    if let Some(arg_slice) = args {
        for arg in arg_slice {
            full_args.push(arg.to_string());
        }
    }

    let mut full_env = match env {
        Some(e) if !e.is_empty() => e.to_vec(),
        _ => DEFAULT_ENV.iter().map(|s| String::from(*s)).collect(),
    };

    if box_id != 0 && !full_env.iter().any(|e| e.starts_with("HOSTNAME=")) {
        if let Some(name) = get_box_name(box_id) {
            let hostname: String = core::iter::once("box-")
                .flat_map(|s| s.chars())
                .chain(name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '-' }))
                .collect();
            full_env.push(format!("HOSTNAME={hostname}"));
        }
    }

    let mut process = match (runtime().read_file)(elf_path) {
        Ok(elf_data) => {
            let result = Process::from_elf(elf_path, &full_args, &full_env, &elf_data, None);
            if use_ns_override { (runtime().clear_spawn_namespace)(); }
            result.map_err(|e| format!("Failed to load ELF: {}", e))?
        }
        Err(_) => {
            let file_size = (runtime().file_size)(elf_path)
                .map_err(|e| {
                    if use_ns_override { (runtime().clear_spawn_namespace)(); }
                    format!("Failed to stat {}: {}", elf_path, e)
                })? as usize;
            let result = Process::from_elf_path(elf_path, elf_path, file_size, &full_args, &full_env, None);
            if use_ns_override { (runtime().clear_spawn_namespace)(); }
            result.map_err(|e| format!("Failed to load ELF: {}", e))?
        }
    };

    // Always create a fresh channel per spawned process.
    // Reusing the parent's channel would cause the child's set_exited() call
    // to contaminate the parent's channel, leaking exit codes.
    let channel = Arc::new(ProcessChannel::new());
    
    // Seed the channel with initial stdin data if provided.
    // Empty stdin (Some(b"")) keeps stdin open so sys_write enables ONLCR
    // translation — use this for subprocesses that need terminal-style output.
    if let Some(data) = stdin {
        if !data.is_empty() {
            channel.write_stdin(data);
            channel.close_stdin();
        }
    }

    // Set the channel in the process struct (UNIFIED I/O)
    process.channel = Some(channel.clone());

    // Inherit terminal state from caller if available
    if let Some(shared_state) = current_terminal_state() {
        if config().syscall_debug_info_enabled {
            log::debug!("[Process] Inheriting shared terminal state at {:p} for PID {}", Arc::as_ptr(&shared_state), process.pid);
        }
        process.terminal_state = shared_state;
        
        // Auto-delegate foreground to the new process.
        // For interactive spawns, the child should start in the foreground.
        let pid_to_delegate = process.pid;
        process.terminal_state.lock().foreground_pgid = pid_to_delegate;
    } else {
        if config().syscall_debug_info_enabled {
            log::debug!("[Process] NO shared terminal state found for caller thread {}, using default for PID {}", crate::threading::current_thread_id(), process.pid);
        }
    }

    // Save arguments in process struct for ProcessInfo page
    process.args = if let Some(arg_slice) = args {
        arg_slice.iter().map(|s| String::from(*s)).collect()
    } else {
        Vec::new()
    };

    // Set up stdin if provided
    if let Some(data) = stdin {
        process.set_stdin(data);
    }
    
    // Set up cwd if provided
    if let Some(dir) = cwd {
        process.set_cwd(dir);
    }

    // Set up isolation context (Inherit from caller by default)
    let (caller_box_id, caller_namespace) = match read_current_pid() {
        Some(pid) => {
            if let Some(proc) = lookup_process(pid) {
                (proc.box_id, proc.namespace.clone())
            } else {
                (0, akuma_isolation::global_namespace())
            }
        }
        None => (0, akuma_isolation::global_namespace()),
    };

    if box_id != 0 {
        process.box_id = box_id;
        if let Some(ns) = (runtime().get_box_namespace)(box_id) {
            process.namespace = ns;
        } else {
            process.namespace = caller_namespace;
        }
    } else {
        process.box_id = caller_box_id;
        process.namespace = caller_namespace;
    }

    if config().syscall_debug_info_enabled {
        log::debug!("[Process] Spawning {} (box_id={}, ns_id={})", path, process.box_id, process.namespace.id);
    }

    // Set spawner PID (the process that called spawn, if any)
    // This is used by procfs to control who can write to stdin
    process.spawner_pid = read_current_pid();
    
    // Get the PID before boxing
    let pid = process.pid;

    // Box the process for heap allocation (fallible to avoid kernel panic on OOM)
    let boxed_process = Box::try_new(process)
        .map_err(|_| format!("Failed to allocate Process struct for {path}"))?;

    // CRITICAL: Register the process in the table immediately.
    // This ensures that lookup_process(pid) works as soon as this function returns,
    // allowing reattach() to succeed without races.
    register_process(pid, boxed_process);

    // Register the channel for the thread ID placeholder (0 for now, will be updated)
    // Actually, current_channel() now uses the field in Process struct, so this is mostly for legacy.
    register_channel(0, channel.clone());

    // Spawn on a user thread
    let thread_id = crate::threading::spawn_user_thread_fn_for_process(move || {
        let tid = crate::threading::current_thread_id();
        
        // Update thread_id in the registered process
        if let Some(p) = lookup_process(pid) {
            p.thread_id = Some(tid);
            
            // Move the channel registration to the correct TID
            remove_channel(0);
            register_channel(tid, p.channel.as_ref().unwrap().clone());
            
            // Execute the process (already in the table)
            run_registered_process(pid);
        } else {
            log::debug!("[Process] FATAL: PID {} disappeared during spawn", pid);
            loop { crate::threading::yield_now(); }
        }
    })
    .map_err(|e| format!("Failed to spawn thread: {}", e))?;

    // Set the thread ID in the process table entry for the parent to see immediately
    if let Some(p) = lookup_process(pid) {
        p.thread_id = Some(thread_id);
    }

    Ok((thread_id, channel, pid))
}

/// Execute a process that is already registered in the PROCESS_TABLE
fn run_registered_process(pid: Pid) -> ! {
    let proc = lookup_process(pid).expect("Process not found in run_registered_process");
    
    // Prepare the process (set state, write process info page)
    proc.prepare_for_execution();
    
    // Activate the user address space (sets TTBR0)
    proc.address_space.activate();

    // Now safe to enable IRQs - TTBR0 is set to user tables
    (runtime().enable_irqs)();

    // Enter user mode via ERET - this never returns
    unsafe {
        enter_user_mode(&proc.context);
    }
}

/// Execute a binary asynchronously and return its output when complete
///
/// Spawns the process on a user thread and polls for completion,
/// yielding to other async tasks while waiting. Returns the buffered
/// output when the process exits.
///
/// # Arguments
/// * `path` - Path to the ELF binary
/// * `args` - Optional command line arguments
/// * `stdin` - Optional stdin data for the process
///
/// # Returns
/// Tuple of (exit_code, stdout_data) or error message
pub async fn exec_async(path: &str, args: Option<&[&str]>, stdin: Option<&[u8]>) -> Result<(i32, Vec<u8>), String> {
    exec_async_cwd(path, args, None, stdin, None).await
}

/// exec_async with explicit cwd and env
pub async fn exec_async_cwd(path: &str, args: Option<&[&str]>, env: Option<&[String]>, stdin: Option<&[u8]>, cwd: Option<&str>) -> Result<(i32, Vec<u8>), String> {

    // Spawn process with channel and cwd
    let (thread_id, channel, _pid) = spawn_process_with_channel_cwd(path, args, env, stdin, cwd)?;

    // For non-interactive execution, if no stdin was provided, mark it as closed
    if stdin.is_none() {
        channel.close_stdin();
    }

    // Wait for process to complete
    // Each iteration yields once (returns Pending) so block_on can yield to scheduler
    loop {
        // Check if process has exited or was interrupted
        if channel.has_exited() || crate::threading::is_thread_terminated(thread_id) {
            break;
        }

        if channel.is_interrupted() {
            break;
        }

        // Yield once - this returns Pending, block_on yields, then we get polled again
        YieldOnce::new().await;
    }

    // Collect all output
    let output = channel.read_all();
    let exit_code = if channel.is_interrupted() && !channel.has_exited() {
        130 // Interrupted exit code
    } else {
        channel.exit_code()
    };

    // Final cleanup
    crate::threading::cleanup_terminated();

    Ok((exit_code, output))
}

/// Get the process channel for a running process by thread ID
///
/// Used by the SSH shell to get a handle for interrupting a process.
pub fn get_process_channel(thread_id: usize) -> Option<Arc<ProcessChannel>> {
    get_channel(thread_id)
}

/// Execute a binary with streaming output to an async writer
///
/// Spawns the process on a user thread and streams output to the
/// provided writer as it becomes available. This allows real-time
/// output display while keeping SSH responsive.
///
/// # Arguments
/// * `path` - Path to the ELF binary
/// * `args` - Optional command line arguments
/// * `stdin` - Optional stdin data for the process
/// * `output` - Async writer to stream output to
///
/// # Returns
/// Exit code or error message
pub async fn exec_streaming<W>(path: &str, args: Option<&[&str]>, stdin: Option<&[u8]>, output: &mut W) -> Result<i32, String>
where
    W: embedded_io_async::Write,
{
    exec_streaming_cwd(path, args, None, stdin, None, output).await
}

/// exec_streaming with explicit cwd and env
pub async fn exec_streaming_cwd<W>(path: &str, args: Option<&[&str]>, env: Option<&[String]>, stdin: Option<&[u8]>, cwd: Option<&str>, output: &mut W) -> Result<i32, String>
where
    W: embedded_io_async::Write,
{
    // Spawn process with channel and cwd
    let (thread_id, channel, _pid) = spawn_process_with_channel_cwd(path, args, env, stdin, cwd)?;

    // For non-interactive streaming, if no stdin was provided, mark it as closed
    if stdin.is_none() {
        channel.close_stdin();
    }

    // Stream output until process exits
    loop {
        // Read available data
        if let Some(data) = channel.try_read() {
            if let Err(_e) = output.write_all(&data).await {
                // Writer failed, likely connection closed
                break;
            }
        }

        // Check if process has exited
        if channel.has_exited() || crate::threading::is_thread_terminated(thread_id) {
            break;
        }

        if channel.is_interrupted() {
            break;
        }

        // Yield to scheduler
        YieldOnce::new().await;
    }

    // Drain remaining output
    if let Some(data) = channel.try_read() {
        let _ = output.write_all(&data).await;
    }

    let exit_code = if channel.is_interrupted() && !channel.has_exited() {
        130 // Interrupted
    } else {
        channel.exit_code()
    };

    // Final cleanup
    crate::threading::cleanup_terminated();

    Ok(exit_code)
}
