//! Process Management
//!
//! Manages user processes including creation, execution, and termination.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};

use spinning_top::Spinlock;

use crate::console;
use crate::elf_loader::{self, ElfError};
use crate::mmu::UserAddressSpace;

/// Process ID type
pub type Pid = u32;

/// Next available PID
static NEXT_PID: AtomicU32 = AtomicU32::new(1);

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
}

impl Process {
    /// Create a new process from ELF data
    pub fn from_elf(name: &str, elf_data: &[u8]) -> Result<Self, ElfError> {
        // Load ELF with stack
        let (entry_point, address_space, stack_pointer) =
            elf_loader::load_elf_with_stack(elf_data, 64 * 1024)?; // 64KB stack

        let pid = NEXT_PID.fetch_add(1, Ordering::Relaxed);

        Ok(Self {
            pid,
            name: String::from(name),
            state: ProcessState::Ready,
            address_space,
            context: UserContext::new(entry_point, stack_pointer),
            parent_pid: 0,
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

        // Activate the user address space
        self.address_space.activate();

        console::print(&alloc::format!(
            "[Process] Starting '{}' (PID {}) at entry={:#x}, sp={:#x}\n",
            self.name, self.pid, self.context.pc, self.context.sp
        ));

        // Enter user mode and run until exit
        let exit_code = unsafe { run_user_process(&self.context) };

        // Deactivate user address space
        UserAddressSpace::deactivate();

        self.state = ProcessState::Zombie(exit_code);

        console::print(&alloc::format!(
            "[Process] '{}' (PID {}) exited with code {}\n",
            self.name, self.pid, exit_code
        ));

        exit_code
    }
}

/// Enter user mode with the given context
///
/// This sets up the CPU state and performs an ERET to EL0.
/// Does not return.
#[inline(never)]
unsafe fn enter_user_mode(ctx: &UserContext) -> ! {
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
    );
}

/// Run a user process until it exits
///
/// Returns the exit code
unsafe fn run_user_process(ctx: &UserContext) -> i32 {
    // For now, use a simple approach: enter user mode and return when the
    // process calls exit. In a full implementation, this would involve
    // the scheduler.

    // Set up the user context
    core::arch::asm!(
        // Set SP_EL0 (user stack pointer)
        "msr sp_el0, {sp}",
        // Set ELR_EL1 (return address = entry point)
        "msr elr_el1, {pc}",
        // Set SPSR_EL1 (saved program status for EL0)
        "msr spsr_el1, {spsr}",
        // Clear most registers
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
        // x0 will be 0 (no args for now)
        "mov x0, #0",
        // Jump to EL0
        "eret",
        sp = in(reg) ctx.sp,
        pc = in(reg) ctx.pc,
        spsr = in(reg) ctx.spsr,
        options(noreturn)
    );
}

/// Execute an ELF binary from the filesystem
///
/// # Arguments
/// * `path` - Path to the ELF binary
///
/// # Returns
/// Exit code of the process, or error message
pub fn exec(path: &str) -> Result<i32, String> {
    // Read the ELF file
    let elf_data = crate::fs::read_file(path).map_err(|e| alloc::format!("Failed to read {}: {}", path, e))?;

    // Create and execute the process
    let mut process =
        Process::from_elf(path, &elf_data).map_err(|e| alloc::format!("Failed to load ELF: {}", e))?;

    Ok(process.execute())
}

