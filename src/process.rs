//! Process Management
//!
//! Manages user processes including creation, execution, and termination.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};

use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};

/// A future that yields once then completes
/// This allows proper async yielding in poll_fn contexts
struct YieldOnce(bool);

impl YieldOnce {
    fn new() -> Self {
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

use crate::config;
use crate::console;
use crate::elf_loader::{self, ElfError};
use crate::mmu::UserAddressSpace;
use crate::pmm::PhysFrame;

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
pub const ARGV_DATA_SIZE: usize = 1024 - 16;

/// Process info structure shared between kernel and userspace
///
/// The kernel writes this, userspace reads it (read-only mapping).
/// Kernel reads it during syscalls to prevent PID spoofing.
///
/// WARNING: Must not exceed 1024 bytes!
/// Layout:
///   - pid: 4 bytes
///   - ppid: 4 bytes
///   - argc: 4 bytes
///   - argv_len: 4 bytes (total bytes used in argv_data)
///   - argv_data: 1008 bytes (null-separated argument strings)
#[repr(C)]
pub struct ProcessInfo {
    /// Process ID
    pub pid: u32,
    /// Parent process ID
    pub ppid: u32,
    /// Number of command line arguments
    pub argc: u32,
    /// Total bytes used in argv_data
    pub argv_len: u32,
    /// Null-separated argument strings
    pub argv_data: [u8; ARGV_DATA_SIZE],
}

impl ProcessInfo {
    /// Create a new ProcessInfo with no arguments
    pub const fn new(pid: u32, ppid: u32) -> Self {
        Self {
            pid,
            ppid,
            argc: 0,
            argv_len: 0,
            argv_data: [0u8; ARGV_DATA_SIZE],
        }
    }

    /// Create a new ProcessInfo with command line arguments
    ///
    /// Arguments are stored as null-separated strings in argv_data.
    /// Returns None if arguments don't fit in the available space.
    pub fn with_args(pid: u32, ppid: u32, args: &[&str]) -> Option<Self> {
        let mut info = Self::new(pid, ppid);
        
        let mut offset = 0;
        for arg in args {
            let arg_bytes = arg.as_bytes();
            let needed = arg_bytes.len() + 1; // +1 for null terminator
            
            if offset + needed > ARGV_DATA_SIZE {
                return None; // Arguments too large
            }
            
            info.argv_data[offset..offset + arg_bytes.len()].copy_from_slice(arg_bytes);
            info.argv_data[offset + arg_bytes.len()] = 0; // null terminator
            offset += needed;
        }
        
        info.argc = args.len() as u32;
        info.argv_len = offset as u32;
        
        Some(info)
    }
}

// Compile-time check that ProcessInfo fits in 1KB
const _: () = assert!(core::mem::size_of::<ProcessInfo>() == 1024);

/// Process ID type
pub type Pid = u32;

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

/// Register a process in the table (takes ownership)
fn register_process(pid: Pid, proc: Box<Process>) {
    crate::irq::with_irqs_disabled(|| {
        PROCESS_TABLE.lock().insert(pid, proc);
    })
}

/// Unregister a process from the table
///
/// Returns the owned Process so it can be dropped, freeing all memory
/// including the UserAddressSpace and all its physical pages.
fn unregister_process(pid: Pid) -> Option<Box<Process>> {
    crate::irq::with_irqs_disabled(|| {
        PROCESS_TABLE.lock().remove(&pid)
    })
}

// ============================================================================
// Process Channel - Inter-thread communication for process I/O
// ============================================================================

use alloc::collections::VecDeque;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicI32};

/// Channel for streaming process output between threads
///
/// Used to pass output from a process running on a user thread
/// to the async shell that spawned it.
pub struct ProcessChannel {
    /// Output buffer (spinlock-protected for thread safety)
    buffer: Spinlock<VecDeque<u8>>,
    /// Exit code (set when process exits)
    exit_code: AtomicI32,
    /// Whether the process has exited
    exited: AtomicBool,
    /// Interrupt signal (set by Ctrl+C, checked by process)
    interrupted: AtomicBool,
}

impl ProcessChannel {
    /// Create a new empty process channel
    pub fn new() -> Self {
        Self {
            buffer: Spinlock::new(VecDeque::new()),
            exit_code: AtomicI32::new(0),
            exited: AtomicBool::new(false),
            interrupted: AtomicBool::new(false),
        }
    }

    /// Write data to the channel buffer
    pub fn write(&self, data: &[u8]) {
        // CRITICAL: Disable IRQs while holding the lock!
        // If timer fires while locked, another thread accessing channel = deadlock.
        // Also, VecDeque operations can trigger heap allocations which need IRQ protection.
        crate::irq::with_irqs_disabled(|| {
            let mut buf = self.buffer.lock();
            buf.extend(data);
        })
    }

    /// Read available data from the channel (non-blocking)
    /// Returns None if no data is available
    pub fn try_read(&self) -> Option<Vec<u8>> {
        crate::irq::with_irqs_disabled(|| {
            let mut buf = self.buffer.lock();
            if buf.is_empty() {
                None
            } else {
                Some(buf.drain(..).collect())
            }
        })
    }

    /// Read all remaining data from the channel
    pub fn read_all(&self) -> Vec<u8> {
        crate::irq::with_irqs_disabled(|| {
            let mut buf = self.buffer.lock();
            buf.drain(..).collect()
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
}

impl Default for ProcessChannel {
    fn default() -> Self {
        Self::new()
    }
}

/// Global registry mapping thread IDs to their process channels
static PROCESS_CHANNELS: Spinlock<alloc::collections::BTreeMap<usize, Arc<ProcessChannel>>> =
    Spinlock::new(alloc::collections::BTreeMap::new());

/// Register a process channel for a thread
pub fn register_channel(thread_id: usize, channel: Arc<ProcessChannel>) {
    crate::irq::with_irqs_disabled(|| {
        PROCESS_CHANNELS.lock().insert(thread_id, channel);
    })
}

/// Get the process channel for a thread (if any)
pub fn get_channel(thread_id: usize) -> Option<Arc<ProcessChannel>> {
    crate::irq::with_irqs_disabled(|| {
        PROCESS_CHANNELS.lock().get(&thread_id).cloned()
    })
}

/// Remove and return the process channel for a thread
pub fn remove_channel(thread_id: usize) -> Option<Arc<ProcessChannel>> {
    crate::irq::with_irqs_disabled(|| {
        PROCESS_CHANNELS.lock().remove(&thread_id)
    })
}

// ============================================================================
// Child Process Registry (for userspace process management)
// ============================================================================

/// Registry mapping child PIDs to their ProcessChannel
/// Used by parent processes to read child stdout via ChildStdout FD
static CHILD_CHANNELS: Spinlock<alloc::collections::BTreeMap<Pid, Arc<ProcessChannel>>> =
    Spinlock::new(alloc::collections::BTreeMap::new());

/// Register a child process channel (called when spawning via syscall)
pub fn register_child_channel(child_pid: Pid, channel: Arc<ProcessChannel>) {
    crate::irq::with_irqs_disabled(|| {
        CHILD_CHANNELS.lock().insert(child_pid, channel);
    })
}

/// Get a child process channel by PID
pub fn get_child_channel(child_pid: Pid) -> Option<Arc<ProcessChannel>> {
    crate::irq::with_irqs_disabled(|| {
        CHILD_CHANNELS.lock().get(&child_pid).cloned()
    })
}

/// Remove a child process channel (called when child exits or parent closes FD)
pub fn remove_child_channel(child_pid: Pid) -> Option<Arc<ProcessChannel>> {
    crate::irq::with_irqs_disabled(|| {
        CHILD_CHANNELS.lock().remove(&child_pid)
    })
}

/// Get channel for the current thread (used by syscall handlers)
pub fn current_channel() -> Option<Arc<ProcessChannel>> {
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
    crate::irq::with_irqs_disabled(|| {
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
/// Reads PID from the process info page and looks up in process table.
/// Returns None if no process is currently executing.
pub fn current_process() -> Option<&'static mut Process> {
    let pid = read_current_pid()?;
    lookup_process(pid)
}

/// Allocate mmap region for current process
/// Returns the address or 0 on failure
pub fn alloc_mmap(size: usize) -> usize {
    let proc = match current_process() {
        Some(p) => p,
        None => {
            console::print("[mmap] ERROR: No current process\n");
            return 0;
        }
    };

    // Use per-process memory tracking
    match proc.memory.alloc_mmap(size) {
        Some(addr) => addr,
        None => {
            crate::safe_print!(64, "[mmap] REJECT: size 0x{:x} exceeds limit\n", size);
            0
        }
    }
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
    pub name: String,
    pub state: &'static str,
}

/// List all running processes
///
/// Returns a vector of process info for display.
pub fn list_processes() -> Vec<ProcessInfo2> {
    // Take a quick snapshot while holding lock with IRQs disabled
    // to prevent deadlock if timer fires while holding PROCESS_TABLE lock.
    // We collect data into a local Vec while locked, then return it.
    crate::irq::with_irqs_disabled(|| {
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
                name: proc.name.clone(),
                state,
            });
        }

        result
    })
}

/// Find a process PID by thread ID
///
/// Returns the PID of the process running on the given thread, if any.
pub fn find_pid_by_thread(thread_id: usize) -> Option<Pid> {
    crate::irq::with_irqs_disabled(|| {
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
            spsr: 0, // EL0, interrupts enabled
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
}

impl ProcessMemory {
    pub fn new(code_end: usize, stack_bottom: usize, stack_top: usize) -> Self {
        // Mmap region: from 0x10000000 up to (stack_bottom - 1MB buffer)
        // Stack is at top of first 1GB (0x3FFF0000-0x40000000 for 64KB stack)
        let mmap_start = 0x1000_0000;
        let mmap_limit = stack_bottom.saturating_sub(0x10_0000); // 1MB buffer before stack

        Self {
            code_end,
            brk: code_end,
            stack_bottom,
            stack_top,
            next_mmap: mmap_start,
            mmap_limit,
        }
    }

    /// Check if an address range overlaps with stack
    pub fn overlaps_stack(&self, addr: usize, size: usize) -> bool {
        let end = addr.saturating_add(size);
        addr < self.stack_top && end > self.stack_bottom
    }

    /// Allocate mmap region, returns None if would overlap stack
    pub fn alloc_mmap(&mut self, size: usize) -> Option<usize> {
        let addr = self.next_mmap;
        let end = addr.checked_add(size)?;

        if end > self.mmap_limit {
            return None; // Would get too close to stack
        }

        self.next_mmap = end;
        Some(addr)
    }
}

/// A user process
pub struct Process {
    /// Process ID
    pub pid: Pid,
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

    // ========== Per-process I/O ==========
    /// Process stdin buffer (set before execution)
    pub stdin_buf: Vec<u8>,
    /// Position in stdin buffer for reads
    pub stdin_pos: usize,
    /// Process stdout buffer (captured during execution)
    pub stdout_buf: Vec<u8>,
    /// Process has exited
    pub exited: bool,
    /// Exit code (valid when exited=true)
    pub exit_code: i32,

    // ========== Dynamic page table tracking ==========
    /// Page table frames allocated during mmap (for cleanup on exit)
    /// These are allocated by map_user_page() and need to be freed separately
    /// from address_space.page_table_frames since they're created dynamically.
    pub dynamic_page_tables: Vec<PhysFrame>,

    // ========== File Descriptor Table ==========
    /// Per-process file descriptor table
    /// Maps FD numbers to FileDescriptor entries (sockets, files, etc.)
    pub fd_table: Spinlock<alloc::collections::BTreeMap<u32, FileDescriptor>>,
    /// Next available file descriptor number
    pub next_fd: AtomicU32,

    // ========== Thread tracking ==========
    /// Thread ID running this process (set after spawn, used for kill)
    pub thread_id: Option<usize>,
}


impl Process {
    /// Create a new process from ELF data
    pub fn from_elf(name: &str, elf_data: &[u8]) -> Result<Self, ElfError> {
        // Load ELF with stack and pre-allocated heap
        // Stack size is configurable via config::USER_STACK_SIZE
        let (entry_point, mut address_space, stack_pointer, brk, stack_bottom, stack_top) =
            elf_loader::load_elf_with_stack(elf_data, config::USER_STACK_SIZE)?;

        let pid = NEXT_PID.fetch_add(1, Ordering::Relaxed);

        // Allocate and map the process info page (read-only for userspace)
        // The kernel will write to this page before entering userspace
        let process_info_frame = crate::pmm::alloc_page_zeroed().ok_or(ElfError::OutOfMemory)?;
        // Track as user data for this process
        crate::pmm::track_frame(process_info_frame, crate::pmm::FrameSource::UserData, pid);

        // Map as read-only at the fixed address
        // user_flags::RO = AP_RO_ALL, meaning read-only for both EL1 and EL0
        // But we use phys_to_virt to write, bypassing page tables
        address_space
            .map_page(
                PROCESS_INFO_ADDR,
                process_info_frame.addr,
                crate::mmu::user_flags::RO | crate::mmu::flags::UXN | crate::mmu::flags::PXN,
            )
            .map_err(|_| ElfError::MappingFailed("process info page"))?;

        // Track the frame so it's freed when the address space is dropped
        address_space.track_user_frame(process_info_frame);

        // Initialize per-process memory tracking
        let memory = ProcessMemory::new(brk, stack_bottom, stack_top);

        crate::safe_print!(160, "[Process] PID {} memory: code_end=0x{:x}, stack=0x{:x}-0x{:x}, mmap=0x{:x}-0x{:x}\n",
            pid, brk, stack_bottom, stack_top, memory.next_mmap, memory.mmap_limit);

        // Initialize FD table with stdin/stdout/stderr pre-allocated
        let mut fd_map = alloc::collections::BTreeMap::new();
        fd_map.insert(0, FileDescriptor::Stdin);
        fd_map.insert(1, FileDescriptor::Stdout);
        fd_map.insert(2, FileDescriptor::Stderr);

        Ok(Self {
            pid,
            name: String::from(name),
            state: ProcessState::Ready,
            address_space,
            context: UserContext::new(entry_point, stack_pointer),
            parent_pid: 0,
            brk,
            initial_brk: brk,
            memory,
            process_info_phys: process_info_frame.addr,
            // Command line arguments - initialized empty
            args: Vec::new(),
            // Per-process I/O - initialized empty
            stdin_buf: Vec::new(),
            stdin_pos: 0,
            stdout_buf: Vec::new(),
            exited: false,
            exit_code: 0,
            // Dynamic page tables - for mmap-allocated page tables
            dynamic_page_tables: Vec::new(),
            // File descriptor table - stdin/stdout/stderr pre-allocated
            fd_table: Spinlock::new(fd_map),
            next_fd: AtomicU32::new(3), // Start after stdin/stdout/stderr
            // Thread ID - set when spawned
            thread_id: None,
        })
    }

    /// Set command line arguments for this process
    ///
    /// Arguments will be passed to the process via the ProcessInfo page.
    pub fn set_args(&mut self, args: &[&str]) {
        self.args = args.iter().map(|s| String::from(*s)).collect();
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
        // We write directly to physical memory via phys_to_virt since the page
        // is mapped read-only in the user's address space
        unsafe {
            let info_ptr = crate::mmu::phys_to_virt(self.process_info_phys) as *mut ProcessInfo;
            
            // Build full argv with program name as argv[0] (Unix convention)
            // self.args contains only the extra arguments, not argv[0]
            let mut full_args: Vec<&str> = Vec::with_capacity(self.args.len() + 1);
            full_args.push(self.name.as_str());  // argv[0] = program name/path
            for arg in &self.args {
                full_args.push(arg.as_str());
            }
            
            let info = ProcessInfo::with_args(self.pid, self.parent_pid, &full_args)
                .unwrap_or_else(|| {
                    console::print("[Process] Warning: args too large, truncating\n");
                    ProcessInfo::new(self.pid, self.parent_pid)
                });
            
            core::ptr::write(info_ptr, info);
        }
    }

    // ========== Per-Process I/O Methods ==========

    /// Set stdin data for this process
    pub fn set_stdin(&mut self, data: &[u8]) {
        self.stdin_buf.clear();
        self.stdin_buf.extend_from_slice(data);
        self.stdin_pos = 0;
    }

    /// Read from this process's stdin
    /// Returns number of bytes read
    pub fn read_stdin(&mut self, buf: &mut [u8]) -> usize {
        let remaining = &self.stdin_buf[self.stdin_pos..];
        let to_read = buf.len().min(remaining.len());
        buf[..to_read].copy_from_slice(&remaining[..to_read]);
        self.stdin_pos += to_read;
        to_read
    }

    /// Write to this process's stdout
    pub fn write_stdout(&mut self, data: &[u8]) {
        self.stdout_buf.extend_from_slice(data);
    }

    /// Take captured stdout (transfers ownership)
    pub fn take_stdout(&mut self) -> Vec<u8> {
        core::mem::take(&mut self.stdout_buf)
    }

    /// Get current program break
    pub fn get_brk(&self) -> usize {
        self.brk
    }

    /// Set program break, returns new value
    /// Will not go below initial_brk
    pub fn set_brk(&mut self, new_brk: usize) -> usize {
        if new_brk < self.initial_brk {
            return self.brk;
        }
        self.brk = (new_brk + 0xFFF) & !0xFFF; // Page-align
        self.brk
    }

    /// Reset I/O state for execution
    pub fn reset_io(&mut self) {
        self.stdin_pos = 0;
        self.stdout_buf.clear();
        self.exited = false;
        self.exit_code = 0;
    }

    // ========== File Descriptor Table Methods ==========

    /// Allocate a new file descriptor and insert the entry atomically
    ///
    /// This is the correct pattern to avoid race conditions:
    /// the FD number is allocated and inserted while holding the lock.
    pub fn alloc_fd(&self, entry: FileDescriptor) -> u32 {
        crate::irq::with_irqs_disabled(|| {
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
        crate::irq::with_irqs_disabled(|| {
            self.fd_table.lock().get(&fd).cloned()
        })
    }

    /// Remove and return a file descriptor entry
    pub fn remove_fd(&self, fd: u32) -> Option<FileDescriptor> {
        crate::irq::with_irqs_disabled(|| {
            self.fd_table.lock().remove(&fd)
        })
    }

    /// Update a file descriptor entry (for file position updates, etc.)
    pub fn update_fd<F>(&self, fd: u32, f: F) -> bool
    where
        F: FnOnce(&mut FileDescriptor),
    {
        crate::irq::with_irqs_disabled(|| {
            let mut table = self.fd_table.lock();
            if let Some(entry) = table.get_mut(&fd) {
                f(entry);
                true
            } else {
                false
            }
        })
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        // Free any remaining dynamically allocated page table frames
        // This handles the case where the process is dropped without execute() being called
        for frame in self.dynamic_page_tables.drain(..) {
            crate::pmm::free_page(frame);
        }
    }
}

/// Enter user mode with the given context
///
/// This sets up the CPU state and performs an ERET to EL0.
/// Does not return.
#[inline(never)]
#[allow(dead_code)]
unsafe fn enter_user_mode(ctx: &UserContext) -> ! {
    // SAFETY: This inline asm sets up CPU state and ERETs to user mode
    unsafe {
        core::arch::asm!(
            // Set SP_EL0 (user stack pointer)
            "msr sp_el0, {sp}",
            // Set ELR_EL1 (return address = entry point)
            "msr elr_el1, {pc}",
            // Set SPSR_EL1 (saved program status for EL0)
            // SPSR = 0 means EL0, all interrupts enabled
            "msr spsr_el1, {spsr}",
            // Clear registers for clean start
            "mov x0, #0",
            "mov x1, #0",
            "mov x2, #0",
            "mov x3, #0",
            "mov x4, #0",
            "mov x5, #0",
            "mov x6, #0",
            "mov x7, #0",
            "mov x8, #0",
            "mov x9, #0",
            "mov x10, #0",
            "mov x11, #0",
            "mov x12, #0",
            "mov x13, #0",
            "mov x14, #0",
            "mov x15, #0",
            "mov x16, #0",
            "mov x17, #0",
            "mov x18, #0",
            "mov x19, #0",
            "mov x20, #0",
            "mov x21, #0",
            "mov x22, #0",
            "mov x23, #0",
            "mov x24, #0",
            "mov x25, #0",
            "mov x26, #0",
            "mov x27, #0",
            "mov x28, #0",
            "mov x29, #0",
            "mov x30, #0",
            // Jump to EL0
            "eret",
            sp = in(reg) ctx.sp,
            pc = in(reg) ctx.pc,
            spsr = in(reg) ctx.spsr,
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
    crate::irq::enable_irqs();

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
/// Exit code is communicated via ProcessChannel for async callers.
#[unsafe(no_mangle)]
pub extern "C" fn return_to_kernel(exit_code: i32) -> ! {
    let tid = crate::threading::current_thread_id();
    
    // Check if this thread was already killed externally (by kill_process).
    // If so, cleanup has already been done - just skip to the yield loop.
    // This handles the race where kill_process() terminates the thread while
    // it's still running, and it later reaches this exit path.
    let already_terminated = crate::threading::is_thread_terminated(tid);
    
    // Get process info before cleanup (skip if already killed)
    let pid = if !already_terminated {
        if let Some(proc) = current_process() {
            let pid = proc.pid;
            
            // Clean up all open sockets for this process
            // This must happen before unregistering the process so we can access fd_table
            cleanup_process_sockets(proc);
            
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
        let _dropped_process = unregister_process(pid);
        // _dropped_process goes out of scope here and is dropped, freeing all memory
        crate::safe_print!(64, "[Process] PID {} thread {} exited ({})\n", pid, tid, exit_code);
    } else {
        crate::safe_print!(64, "[Process] Thread {} exited ({})\n", tid, exit_code);
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

/// Clean up all sockets owned by a process
///
/// Called during process exit to close all open sockets and free resources.
/// This prevents socket/buffer leaks when processes exit without properly closing their sockets.
fn cleanup_process_sockets(proc: &Process) {
    // Get all socket FDs from the process's fd_table
    let socket_fds: alloc::vec::Vec<(u32, usize)> = {
        let table = proc.fd_table.lock();
        table.iter()
            .filter_map(|(&fd, desc)| {
                if let FileDescriptor::Socket(idx) = desc {
                    Some((fd, *idx))
                } else {
                    None
                }
            })
            .collect()
    };
    
    // Close each socket
    for (_fd, socket_idx) in socket_fds {
        // socket_close handles abort() and deferred buffer cleanup
        let _ = crate::socket::socket_close(socket_idx);
    }
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
    
    // Clean up all open sockets for this process
    // Note: This cleans up sockets in the fd_table, but sockets created inside
    // syscalls (like the TcpSocket in accept()) are handled by the interrupt mechanism.
    cleanup_process_sockets(proc);
    
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
    
    crate::safe_print!(64, "[kill] Killed PID {} (thread {})\n", pid, thread_id);
    
    Ok(())
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
    // Spawn process with channel
    let (thread_id, channel) = spawn_process_with_channel(path, args, stdin)?;
    
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
    let (thread_id, _channel) = spawn_process_with_channel(path, args, stdin)?;
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
///
/// # Returns
/// Tuple of (thread_id, channel) or error message
pub fn spawn_process_with_channel(
    path: &str,
    args: Option<&[&str]>,
    stdin: Option<&[u8]>,
) -> Result<(usize, Arc<ProcessChannel>), String> {
    // Check if user threads are available
    let avail = crate::threading::user_threads_available();
    crate::safe_print!(64, "[spawn_process] path={} user_threads_available={}\n", path, avail);
    if avail == 0 {
        return Err("No available user threads for process execution".into());
    }

    // Read the ELF file
    let elf_data =
        crate::fs::read_file(path).map_err(|e| alloc::format!("Failed to read {}: {}", path, e))?;

    // Create the process
    let mut process = Process::from_elf(path, &elf_data)
        .map_err(|e| alloc::format!("Failed to load ELF: {}", e))?;

    // Set up arguments if provided
    if let Some(arg_slice) = args {
        process.set_args(arg_slice);
    }

    // Set up stdin if provided
    if let Some(data) = stdin {
        process.set_stdin(data);
    }
    
    // Get the PID before boxing
    let pid = process.pid;

    // Box the process for heap allocation - this is CRITICAL for memory management.
    // Previously, the Process lived on the closure's stack, but execute() never returns
    // (it ERETs to userspace), so Process::drop() was never called, causing memory leaks.
    // Now we Box it and register in PROCESS_TABLE which owns it. When the process exits,
    // unregister_process() returns the Box which is then dropped, freeing all memory.
    let mut boxed_process = Box::new(process);

    // Create a channel for this process
    let channel = Arc::new(ProcessChannel::new());
    let channel_for_thread = channel.clone();

    // Spawn on a user thread
    // Use spawn_user_thread_fn_for_process which starts with IRQs disabled
    // to prevent the race where timer fires before activate() sets user TTBR0.
    let thread_id = crate::threading::spawn_user_thread_fn_for_process(move || {
        // NOTE: IRQs are already disabled from thread creation.
        // spawn_user_thread_fn_for_process starts the thread with DAIF.I set,
        // preventing timer from preempting before activate() sets user TTBR0.
        
        // Register channel for this thread so syscalls can find it
        // return_to_kernel() will call remove_channel() and set_exited() when process exits
        let tid = crate::threading::current_thread_id();
        register_channel(tid, channel_for_thread);

        // Set thread_id on process for kill support
        boxed_process.thread_id = Some(tid);

        // Execute the process using execute_boxed which registers the Box
        // in PROCESS_TABLE (transferring ownership) then enters userspace.
        // This never returns - when user exits, return_to_kernel() handles cleanup.
        execute_boxed(boxed_process)
    })
    .map_err(|e| alloc::format!("Failed to spawn thread: {}", e))?;

    crate::safe_print!(64, "[spawn_process] spawned thread {} for {}\n", thread_id, path);
    Ok((thread_id, channel))
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

    // Spawn process with channel
    let (thread_id, channel) = spawn_process_with_channel(path, args, stdin)?;

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
    // Spawn process with channel
    let (thread_id, channel) = spawn_process_with_channel(path, args, stdin)?;

    // Poll for output and completion
    loop {
        // Drain any available output and write to stream
        if let Some(data) = channel.try_read() {
            // Write all data at once, then flush
            let mut buf = alloc::vec::Vec::new();
            for &byte in &data {
                if byte == b'\n' {
                    buf.extend_from_slice(b"\r\n");
                } else {
                    buf.push(byte);
                }
            }
            
            let _ = output.write_all(&buf).await;
            
            // Flush output to push to network
            let _ = output.flush().await;
            
            // Yield aggressively to allow network transmission
            for _ in 0..100 {
                crate::threading::yield_now();
            }
        }

        // Check if process has exited
        if channel.has_exited() || crate::threading::is_thread_terminated(thread_id) {
            // Drain any remaining output
            while let Some(data) = channel.try_read() {
                let mut buf = alloc::vec::Vec::new();
                for &byte in &data {
                    if byte == b'\n' {
                        buf.extend_from_slice(b"\r\n");
                    } else {
                        buf.push(byte);
                    }
                }
                let _ = output.write_all(&buf).await;
            }
            let _ = output.flush().await;
            for _ in 0..100 {
                crate::threading::yield_now();
            }
            break;
        }

        // Yield to scheduler
        YieldOnce::new().await;
    }

    let exit_code = channel.exit_code();

    // Final cleanup
    crate::threading::cleanup_terminated();

    Ok(exit_code)
}
