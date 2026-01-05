//! Process Management
//!
//! Manages user processes including creation, execution, and termination.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};

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

/// Process info structure shared between kernel and userspace
/// 
/// The kernel writes this, userspace reads it (read-only mapping).
/// Kernel reads it during syscalls to prevent PID spoofing.
/// 
/// WARNING: Must not exceed 1024 bytes! Currently uses ~8 bytes.
/// Add a compile-time assertion if adding fields.
#[repr(C)]
pub struct ProcessInfo {
    /// Process ID
    pub pid: u32,
    /// Parent process ID
    pub ppid: u32,
    // Future fields: uid, gid, etc.
    // Reserved space to reach 1KB
    _reserved: [u8; 1024 - 8],
}

impl ProcessInfo {
    pub const fn new(pid: u32, ppid: u32) -> Self {
        Self {
            pid,
            ppid,
            _reserved: [0u8; 1024 - 8],
        }
    }
}

// Compile-time check that ProcessInfo fits in 1KB
const _: () = assert!(core::mem::size_of::<ProcessInfo>() == 1024);

/// Process ID type
pub type Pid = u32;

/// Next available PID
static NEXT_PID: AtomicU32 = AtomicU32::new(1);

/// Wrapper for process pointer to allow Send
/// 
/// SAFETY: Process pointers are only accessed from kernel context
/// with proper synchronization through the Spinlock.
#[derive(Clone, Copy)]
struct ProcessPtr(*mut Process);

// SAFETY: We ensure single-threaded access through the Spinlock
unsafe impl Send for ProcessPtr {}

/// Process table: maps PID to process pointer
/// 
/// Processes are stored here when created and removed when they exit.
/// Syscall handlers use read_current_pid() + lookup_process() to find
/// the calling process.
static PROCESS_TABLE: Spinlock<alloc::collections::BTreeMap<Pid, ProcessPtr>> = 
    Spinlock::new(alloc::collections::BTreeMap::new());

/// Register a process in the table
fn register_process(pid: Pid, proc: *mut Process) {
    PROCESS_TABLE.lock().insert(pid, ProcessPtr(proc));
}

/// Unregister a process from the table
fn unregister_process(pid: Pid) {
    PROCESS_TABLE.lock().remove(&pid);
}

/// Read the current process PID from the process info page
/// 
/// During a syscall, TTBR0 is still set to the user's page tables,
/// so reading from PROCESS_INFO_ADDR gives us the calling process's PID.
/// This prevents PID spoofing since the page is read-only for userspace.
pub fn read_current_pid() -> Option<Pid> {
    // Read from the fixed address in the current address space
    // SAFETY: If a process is running, this address is mapped and contains valid ProcessInfo
    let pid = unsafe { (*(PROCESS_INFO_ADDR as *const ProcessInfo)).pid };
    if pid == 0 {
        None
    } else {
        Some(pid)
    }
}

/// Look up a process by PID
/// 
/// Returns a mutable reference to the process if found.
/// SAFETY: The caller must ensure no other code is mutating the process.
pub fn lookup_process(pid: Pid) -> Option<&'static mut Process> {
    let table = PROCESS_TABLE.lock();
    table.get(&pid).map(|&ProcessPtr(ptr)| unsafe { &mut *ptr })
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
            console::print(&alloc::format!(
                "[mmap] REJECT: size 0x{:x} exceeds limit\n", size
            ));
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
    let table = PROCESS_TABLE.lock();
    let mut result = Vec::new();
    
    for (&pid, &ProcessPtr(ptr)) in table.iter() {
        let proc = unsafe { &*ptr };
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
    pub x29: u64, // Frame pointer
    pub x30: u64, // Link register
    pub sp: u64,  // Stack pointer (SP_EL0)
    pub pc: u64,  // Program counter (ELR_EL1)
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
    
    // ========== Kernel context for return ==========
    
    /// Saved kernel context (callee-saved registers for returning after exit)
    pub kernel_ctx: KernelContext,
    
    // ========== Dynamic page table tracking ==========
    
    /// Page table frames allocated during mmap (for cleanup on exit)
    /// These are allocated by map_user_page() and need to be freed separately
    /// from address_space.page_table_frames since they're created dynamically.
    pub dynamic_page_tables: Vec<PhysFrame>,
}

/// Kernel context - callee-saved registers that must be preserved across user mode execution
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct KernelContext {
    pub sp: u64,
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
    pub x29: u64,  // frame pointer
    pub x30: u64,  // return address
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
        let process_info_frame = crate::pmm::alloc_page_zeroed()
            .ok_or(ElfError::OutOfMemory)?;
        
        // Map as read-only at the fixed address
        // user_flags::RO = AP_RO_ALL, meaning read-only for both EL1 and EL0
        // But we use phys_to_virt to write, bypassing page tables
        address_space.map_page(
            PROCESS_INFO_ADDR, 
            process_info_frame.addr, 
            crate::mmu::user_flags::RO | crate::mmu::flags::UXN | crate::mmu::flags::PXN
        ).map_err(|_| ElfError::MappingFailed("process info page"))?;
        
        // Track the frame so it's freed when the address space is dropped
        address_space.track_user_frame(process_info_frame);
        
        // Initialize per-process memory tracking
        let memory = ProcessMemory::new(brk, stack_bottom, stack_top);
        
        console::print(&alloc::format!(
            "[Process] PID {} memory: code_end=0x{:x}, stack=0x{:x}-0x{:x}, mmap=0x{:x}-0x{:x}\n",
            pid, brk, stack_bottom, stack_top, memory.next_mmap, memory.mmap_limit
        ));

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
            // Per-process I/O - initialized empty
            stdin_buf: Vec::new(),
            stdin_pos: 0,
            stdout_buf: Vec::new(),
            exited: false,
            exit_code: 0,
            // Kernel context - set before entering user mode
            kernel_ctx: KernelContext::default(),
            // Dynamic page tables - for mmap-allocated page tables
            dynamic_page_tables: Vec::new(),
        })
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

    /// Execute the process synchronously, returning when it exits
    ///
    /// Returns the exit code
    pub fn execute(&mut self) -> i32 {
        self.state = ProcessState::Running;

        // Reset per-process I/O state
        self.reset_io();
        
        // Write process info to the physical page (before activating address space)
        // We write directly to physical memory via phys_to_virt since the page
        // is mapped read-only in the user's address space
        unsafe {
            let info_ptr = crate::mmu::phys_to_virt(self.process_info_phys) as *mut ProcessInfo;
            core::ptr::write(info_ptr, ProcessInfo::new(self.pid, self.parent_pid));
        }
        
        // Register this process in the table for PID-based lookup
        register_process(self.pid, self as *mut Process);

        // Activate the user address space (sets TTBR0)
        // After this, reading from PROCESS_INFO_ADDR will return our ProcessInfo
        self.address_space.activate();

        // Enter user mode - this will ERET to user code
        // When user calls exit(), it sets proc.exited = true
        // and the exception handler calls return_to_kernel() to return here
        let ctx_ptr = &mut self.kernel_ctx as *mut KernelContext;
        let exit_code = unsafe { 
            run_user_until_exit(self.context.sp, self.context.pc, ctx_ptr) 
        };

        // Unregister this process from the table
        unregister_process(self.pid);
        
        // Deactivate user address space
        UserAddressSpace::deactivate();
        
        // Free dynamically allocated page table frames (from mmap calls)
        for frame in self.dynamic_page_tables.drain(..) {
            crate::pmm::free_page(frame);
        }

        self.state = ProcessState::Zombie(exit_code);

        console::print(&alloc::format!(
            "[Process] '{}' (PID {}) exited with code {}\n",
            self.name, self.pid, exit_code
        ));

        exit_code
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
/// Called from exception handler when process exits
#[unsafe(no_mangle)]
pub extern "C" fn return_to_kernel(exit_code: i32) -> ! {
    // Get kernel context from current process
    let ctx_ptr = match current_process() {
        Some(proc) => &proc.kernel_ctx as *const KernelContext,
        None => {
            crate::console::print("[return_to_kernel] ERROR: no current process!\n");
            loop { core::hint::spin_loop(); }
        }
    };
    
    unsafe {
        // Restore all callee-saved registers from context, then return
        core::arch::asm!(
            // Use x9 as scratch register for context pointer
            "mov x9, {ctx}",
            // Restore callee-saved registers
            "ldp x19, x20, [x9, #8]",
            "ldp x21, x22, [x9, #24]",
            "ldp x23, x24, [x9, #40]",
            "ldp x25, x26, [x9, #56]",
            "ldp x27, x28, [x9, #72]",
            "ldp x29, x30, [x9, #88]",
            // Restore sp last
            "ldr x9, [x9, #0]",
            "mov sp, x9",
            // Set return value and return
            "mov x0, {exit_code}",
            "ret",
            ctx = in(reg) ctx_ptr,
            exit_code = in(reg) exit_code as i64,
            options(noreturn)
        );
    }
}

/// Run a user process until it exits
///
/// This saves kernel context, enters user mode (EL0) via ERET.
/// When exit() is called, return_to_kernel() jumps back here.
///
/// Arguments passed via x0-x2:
/// - x0: user_sp
/// - x1: user_pc  
/// - x2: kernel context pointer
///
/// Returns exit code in x0
#[unsafe(naked)]
unsafe extern "C" fn run_user_until_exit(user_sp: u64, user_pc: u64, ctx_ptr: *mut KernelContext) -> i32 {
    core::arch::naked_asm!(
        // Save callee-saved registers to context struct (x2 = ctx_ptr)
        "mov x9, sp",
        "str x9, [x2, #0]",      // sp at offset 0
        "stp x19, x20, [x2, #8]",
        "stp x21, x22, [x2, #24]",
        "stp x23, x24, [x2, #40]",
        "stp x25, x26, [x2, #56]",
        "stp x27, x28, [x2, #72]",
        "stp x29, x30, [x2, #88]",
        
        // Set up user context (x0 = user_sp, x1 = user_pc)
        "msr sp_el0, x0",
        "msr elr_el1, x1",
        "mov x9, #0",            // SPSR for EL0
        "msr spsr_el1, x9",
        "isb",
        
        // Clear user registers
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
        
        // Enter user mode
        // return_to_kernel() will restore context and ret back here
        "eret",
        // After eret, we never reach here in normal flow
        // return_to_kernel() will restore registers and ret,
        // which returns to the caller of run_user_until_exit
    )
}

/// Execute an ELF binary from the filesystem with per-process I/O
///
/// # Arguments
/// * `path` - Path to the ELF binary
/// * `stdin` - Optional stdin data for the process
///
/// # Returns
/// Tuple of (exit_code, stdout_data), or error message
pub fn exec_with_io(path: &str, stdin: Option<&[u8]>) -> Result<(i32, Vec<u8>), String> {
    // Read the ELF file
    let elf_data = crate::fs::read_file(path).map_err(|e| alloc::format!("Failed to read {}: {}", path, e))?;

    // Create the process
    let mut process =
        Process::from_elf(path, &elf_data).map_err(|e| alloc::format!("Failed to load ELF: {}", e))?;
    
    // Set up stdin if provided
    if let Some(data) = stdin {
        process.set_stdin(data);
    }

    // Execute and capture output
    let exit_code = process.execute();
    let stdout_data = process.take_stdout();
    
    Ok((exit_code, stdout_data))
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
    let (exit_code, _stdout) = exec_with_io(path, None)?;
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
/// * `stdin` - Optional stdin data for the process
///
/// # Returns
/// Thread ID of the spawned thread, or error message
pub fn spawn_process(path: &str, stdin: Option<&[u8]>) -> Result<usize, String> {
    // Check if user threads are available
    if crate::threading::user_threads_available() == 0 {
        return Err("No available user threads for process execution".into());
    }
    
    // Read the ELF file
    let elf_data = crate::fs::read_file(path)
        .map_err(|e| alloc::format!("Failed to read {}: {}", path, e))?;

    // Create the process
    let mut process = Process::from_elf(path, &elf_data)
        .map_err(|e| alloc::format!("Failed to load ELF: {}", e))?;
    
    // Set up stdin if provided
    if let Some(data) = stdin {
        process.set_stdin(data);
    }

    // Spawn on a user thread
    let thread_id = crate::threading::spawn_user_thread_fn(move || {
        // Execute the process
        process.execute();
        
        // Mark thread as terminated when process exits
        crate::threading::mark_current_terminated();
        
        // This loop should never be reached, but just in case
        loop {
            crate::threading::yield_now();
        }
    }).map_err(|e| alloc::format!("Failed to spawn thread: {}", e))?;
    
    Ok(thread_id)
}

