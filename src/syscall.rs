//! System Call Handlers
//!
//! Implements the syscall interface for user programs.
//! Uses Linux-compatible ABI: syscall number in x8, arguments in x0-x5.

use alloc::format;

use crate::console;

/// Syscall numbers (Linux-compatible subset)
pub mod nr {
    pub const EXIT: u64 = 0;
    pub const READ: u64 = 1;
    pub const WRITE: u64 = 2;
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
        _ => {
            console::print(&format!(
                "[Syscall] Unknown syscall: {}\n",
                syscall_num
            ));
            (-1i64) as u64 // ENOSYS
        }
    }
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

/// Reset process exit state (called before starting a new process)
pub fn reset_exit_state() {
    PROCESS_EXITED.store(false, Ordering::Release);
    LAST_EXIT_CODE.store(0, Ordering::Release);
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

    // TODO: Read from process's stdin when process I/O is implemented
    // For now, return 0 (EOF)
    0
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

    // Convert to string and print
    if let Ok(s) = core::str::from_utf8(buf) {
        console::print(s);
    } else {
        // Print as raw bytes if not valid UTF-8
        for &byte in buf {
            console::print_char(byte as char);
        }
    }

    count as u64
}

