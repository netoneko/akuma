//! System Call Handlers
//!
//! Implements the syscall interface for user programs.
//! Uses Linux-compatible ABI: syscall number in x8, arguments in x0-x5.

use alloc::format;
use alloc::vec::Vec;
use spinning_top::Spinlock;

use crate::console;

/// Syscall numbers (Linux-compatible subset)
pub mod nr {
    pub const EXIT: u64 = 0;
    pub const READ: u64 = 1;
    pub const WRITE: u64 = 2;
    pub const BRK: u64 = 3;
    pub const MMAP: u64 = 222;   // Linux arm64 mmap
    pub const MUNMAP: u64 = 215; // Linux arm64 munmap
}

/// File descriptor numbers
pub mod fd {
    pub const STDIN: u64 = 0;
    pub const STDOUT: u64 = 1;
    pub const STDERR: u64 = 2;
}

/// Handle a system call
///
/// # Arguments
/// * `syscall_num` - Syscall number from x8
/// * `args` - Arguments from x0-x5
///
/// # Returns
/// Return value to be placed in x0
pub fn handle_syscall(syscall_num: u64, args: &[u64; 6]) -> u64 {
    match syscall_num {
        nr::EXIT => sys_exit(args[0] as i32),
        nr::READ => sys_read(args[0], args[1], args[2] as usize),
        nr::WRITE => sys_write(args[0], args[1], args[2] as usize),
        nr::BRK => sys_brk(args[0] as usize),
        nr::MMAP => sys_mmap(args[0] as usize, args[1] as usize, args[2] as u32, args[3] as u32),
        nr::MUNMAP => sys_munmap(args[0] as usize, args[1] as usize),
        _ => {
            console::print(&format!(
                "[Syscall] Unknown syscall: {}\n",
                syscall_num
            ));
            (-1i64) as u64 // ENOSYS
        }
    }
}

/// sys_brk - Change the program break (heap end)
fn sys_brk(new_brk: usize) -> u64 {
    if new_brk == 0 {
        crate::process::get_brk() as u64
    } else {
        crate::process::set_brk(new_brk) as u64
    }
}

/// sys_mmap - Map memory pages
/// 
/// Uses per-process mmap allocation from process module.
/// Checks for stack overlap.
fn sys_mmap(addr: usize, len: usize, _prot: u32, _flags: u32) -> u64 {
    use crate::mmu::user_flags;
    use crate::pmm;
    
    const PAGE_SIZE: usize = 4096;
    const MAP_FAILED: u64 = (-1i64) as u64;
    
    if len == 0 {
        return MAP_FAILED;
    }
    
    // Round up to page size
    let pages = (len + PAGE_SIZE - 1) / PAGE_SIZE;
    let size = pages * PAGE_SIZE;
    
    // Get the next mmap address from per-process tracking
    let _ = addr; // Unused for now
    let mmap_addr = crate::process::alloc_mmap(size);
    
    if mmap_addr == 0 {
        return MAP_FAILED;
    }
    
    // Map pages using the current process's address space
    for i in 0..pages {
        let va = mmap_addr + i * PAGE_SIZE;
        if let Some(frame) = pmm::alloc_page_zeroed() {
            unsafe {
                crate::mmu::map_user_page(va, frame.addr, user_flags::RW_NO_EXEC);
            }
        } else {
            return MAP_FAILED;
        }
    }
    
    mmap_addr as u64
}

/// sys_munmap - Unmap memory pages
/// 
/// Simplified: just marks the pages as unmapped but doesn't reclaim memory.
/// A full implementation would free the physical frames.
fn sys_munmap(addr: usize, len: usize) -> u64 {
    const PAGE_SIZE: usize = 4096;
    
    if addr == 0 || len == 0 || addr % PAGE_SIZE != 0 {
        return (-1i64) as u64; // EINVAL
    }
    
    // For now, munmap is a no-op (memory leak, but simple)
    // A full implementation would:
    // 1. Find the mapping
    // 2. Unmap the pages from the page table
    // 3. Free the physical frames
    let _ = len;
    
    0 // Success
}

/// sys_exit - Terminate the current process
///
/// # Arguments
/// * `code` - Exit code
fn sys_exit(code: i32) -> u64 {
    // Store exit code for the process manager to retrieve
    LAST_EXIT_CODE.store(code, core::sync::atomic::Ordering::Release);
    PROCESS_EXITED.store(true, core::sync::atomic::Ordering::Release);
    
    // Return won't matter - process is terminated
    code as u64
}

use core::sync::atomic::{AtomicBool, AtomicI32, Ordering};

/// Flag indicating current user process has exited
pub static PROCESS_EXITED: AtomicBool = AtomicBool::new(false);

/// Exit code of the last exited process
pub static LAST_EXIT_CODE: AtomicI32 = AtomicI32::new(0);

/// Process stdout buffer - captures write() syscall output
static PROCESS_STDOUT: Spinlock<Vec<u8>> = Spinlock::new(Vec::new());

/// Process stdin buffer - provides read() syscall input
static PROCESS_STDIN: Spinlock<Vec<u8>> = Spinlock::new(Vec::new());

/// Position in stdin buffer
static STDIN_POS: Spinlock<usize> = Spinlock::new(0);

/// Reset process exit state (called before starting a new process)
/// Does NOT clear stdin/stdout - those are managed by the exec command
pub fn reset_exit_state() {
    PROCESS_EXITED.store(false, Ordering::Release);
    LAST_EXIT_CODE.store(0, Ordering::Release);
    // Clear stdout for new process output
    PROCESS_STDOUT.lock().clear();
    // Reset stdin read position (but keep the data that was set via set_stdin)
    *STDIN_POS.lock() = 0;
}

/// Set stdin for the next process
pub fn set_stdin(data: &[u8]) {
    let mut stdin = PROCESS_STDIN.lock();
    stdin.clear();
    stdin.extend_from_slice(data);
    *STDIN_POS.lock() = 0;
}

/// Get the captured stdout from the process
pub fn take_stdout() -> Vec<u8> {
    let mut stdout = PROCESS_STDOUT.lock();
    core::mem::take(&mut *stdout)
}

/// Check if process has exited and get exit code
pub fn check_exit() -> Option<i32> {
    if PROCESS_EXITED.load(Ordering::Acquire) {
        Some(LAST_EXIT_CODE.load(Ordering::Acquire))
    } else {
        None
    }
}

/// sys_read - Read from a file descriptor
///
/// # Arguments
/// * `fd` - File descriptor (0 = stdin)
/// * `buf` - User buffer pointer
/// * `count` - Number of bytes to read
///
/// # Returns
/// Number of bytes read, or negative error code
fn sys_read(fd_num: u64, buf_ptr: u64, count: usize) -> u64 {
    if fd_num != fd::STDIN {
        return (-1i64) as u64; // EBADF - bad file descriptor
    }

    if buf_ptr == 0 || count == 0 {
        return 0;
    }

    // Read from stdin buffer
    let stdin = PROCESS_STDIN.lock();
    let mut pos = STDIN_POS.lock();
    
    if *pos >= stdin.len() {
        return 0; // EOF
    }
    
    let available = stdin.len() - *pos;
    let to_read = core::cmp::min(count, available);
    
    // Copy to user buffer
    unsafe {
        let dst = buf_ptr as *mut u8;
        let src = stdin.as_ptr().add(*pos);
        core::ptr::copy_nonoverlapping(src, dst, to_read);
    }
    
    *pos += to_read;
    to_read as u64
}

/// sys_write - Write to a file descriptor
///
/// # Arguments
/// * `fd` - File descriptor (1 = stdout, 2 = stderr)
/// * `buf` - User buffer pointer
/// * `count` - Number of bytes to write
///
/// # Returns
/// Number of bytes written, or negative error code
fn sys_write(fd_num: u64, buf_ptr: u64, count: usize) -> u64 {
    if fd_num != fd::STDOUT && fd_num != fd::STDERR {
        return (-1i64) as u64; // EBADF
    }

    if buf_ptr == 0 || count == 0 {
        return 0;
    }

    // SAFETY: We trust the user buffer is valid (TODO: add proper validation)
    // In a real implementation, we'd validate that buf_ptr..buf_ptr+count
    // is within the user's address space
    let buf = unsafe { core::slice::from_raw_parts(buf_ptr as *const u8, count) };

    // Capture output to buffer for shell
    PROCESS_STDOUT.lock().extend_from_slice(buf);

    // Also print to kernel console for debugging
    if let Ok(s) = core::str::from_utf8(buf) {
        console::print(s);
    }

    count as u64
}
