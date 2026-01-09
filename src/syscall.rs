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
    pub const BRK: u64 = 3;
    pub const NANOSLEEP: u64 = 101; // Linux arm64 nanosleep
    pub const MMAP: u64 = 222; // Linux arm64 mmap
    pub const MUNMAP: u64 = 215; // Linux arm64 munmap
    pub const UPTIME: u64 = 216;
}

/// Error code for interrupted syscall
const EINTR: u64 = (-4i64) as u64;

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
    // Check for interrupt signal (Ctrl+C) before processing syscall
    // This allows the process to be interrupted between syscalls
    if crate::process::is_current_interrupted() {
        // Trigger exit with interrupt code
        if let Some(proc) = crate::process::current_process() {
            proc.exited = true;
            proc.exit_code = 130; // 128 + SIGINT(2) - standard interrupted exit code
            proc.state = crate::process::ProcessState::Zombie(130);
        }
        return EINTR;
    }

    match syscall_num {
        nr::EXIT => sys_exit(args[0] as i32),
        nr::READ => sys_read(args[0], args[1], args[2] as usize),
        nr::WRITE => sys_write(args[0], args[1], args[2] as usize),
        nr::BRK => sys_brk(args[0] as usize),
        nr::NANOSLEEP => sys_nanosleep(args[0], args[1]),
        nr::MMAP => sys_mmap(
            args[0] as usize,
            args[1] as usize,
            args[2] as u32,
            args[3] as u32,
        ),
        nr::MUNMAP => sys_munmap(args[0] as usize, args[1] as usize),
        nr::UPTIME => sys_uptime(),
        _ => {
            console::print(&format!("[Syscall] Unknown syscall: {}\n", syscall_num));
            (-1i64) as u64 // ENOSYS
        }
    }
}

/// sys_brk - Change the program break (heap end)
fn sys_brk(new_brk: usize) -> u64 {
    let proc = match crate::process::current_process() {
        Some(p) => p,
        None => return 0,
    };

    if new_brk == 0 {
        proc.get_brk() as u64
    } else {
        proc.set_brk(new_brk) as u64
    }
}

/// sys_nanosleep - Sleep for a specified duration
///
/// Sleeps in short intervals to allow interrupt checking (Ctrl+C).
/// The thread cannot be preempted during the syscall, but will be
/// preempted once it returns to EL0 (userspace).
///
/// # Arguments
/// * `seconds` - Number of seconds to sleep
/// * `nanoseconds` - Additional nanoseconds to sleep
///
/// # Returns
/// 0 on success, EINTR if interrupted
fn sys_nanosleep(seconds: u64, nanoseconds: u64) -> u64 {
    let total_us = seconds * 1_000_000 + nanoseconds / 1_000;
    let start = crate::timer::uptime_us();
    let deadline = start + total_us;

    // NOTE: We cannot enable IRQs or yield during syscall handling because:
    // 1. Syscalls run in EL1 exception context with specific ELR/SPSR state
    // 2. Timer interrupts would trigger context switches that corrupt this state
    // 3. The thread will be preempted naturally once it returns to EL0
    //
    // For long sleeps, we check for interrupts periodically.

    // Sleep in small increments to allow interrupt checking
    const CHECK_INTERVAL_US: u64 = 10_000; // Check every 10ms

    while crate::timer::uptime_us() < deadline {
        // Check for interrupt signal
        if crate::process::is_current_interrupted() {
            // Interrupted by Ctrl+C
            if let Some(proc) = crate::process::current_process() {
                proc.exited = true;
                proc.exit_code = 130;
                proc.state = crate::process::ProcessState::Zombie(130);
            }
            return EINTR;
        }

        // Short delay
        let remaining = deadline.saturating_sub(crate::timer::uptime_us());
        let sleep_time = remaining.min(CHECK_INTERVAL_US);
        if sleep_time > 0 {
            crate::timer::delay_us(sleep_time);
        }
    }

    0 // Success
}

fn sys_uptime() -> u64 {
    crate::timer::uptime_us()
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

    // Get current process to track frames for cleanup
    let proc = match crate::process::current_process() {
        Some(p) => p,
        None => return MAP_FAILED,
    };

    // Map pages and track frames for cleanup on process exit
    for i in 0..pages {
        let va = mmap_addr + i * PAGE_SIZE;
        if let Some(frame) = pmm::alloc_page_zeroed() {
            // Debug tracking: record this as a user data allocation
            pmm::track_frame(frame, pmm::FrameSource::UserData, proc.pid);

            // Track frame so it will be freed when process exits
            proc.address_space.track_user_frame(frame);

            // Map the page and collect any newly allocated page table frames
            let table_frames =
                unsafe { crate::mmu::map_user_page(va, frame.addr, user_flags::RW_NO_EXEC) };

            // Track dynamically allocated page table frames for cleanup
            for table_frame in table_frames {
                // Debug tracking: record page table allocations
                pmm::track_frame(table_frame, pmm::FrameSource::UserPageTable, proc.pid);
                proc.dynamic_page_tables.push(table_frame);
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
    // Update per-process state only
    if let Some(proc) = crate::process::current_process() {
        proc.exited = true;
        proc.exit_code = code;
        proc.state = crate::process::ProcessState::Zombie(code);
    }

    // Return won't matter - process is terminated
    code as u64
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

    // Get the current process for per-process stdin
    let proc = match crate::process::current_process() {
        Some(p) => p,
        None => return (-1i64) as u64,
    };

    // Create a temporary buffer to read into
    let mut temp_buf = alloc::vec![0u8; count];
    let bytes_read = proc.read_stdin(&mut temp_buf);

    if bytes_read == 0 {
        return 0; // EOF
    }

    // Copy to user buffer
    unsafe {
        let dst = buf_ptr as *mut u8;
        core::ptr::copy_nonoverlapping(temp_buf.as_ptr(), dst, bytes_read);
    }

    bytes_read as u64
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

    // Write to process channel for streaming output (if one exists)
    if let Some(channel) = crate::process::current_channel() {
        channel.write(buf);
    }

    // Write to per-process stdout buffer (for legacy exec_with_io)
    if let Some(proc) = crate::process::current_process() {
        proc.write_stdout(buf);
    }

    // Also print to kernel console for debugging
    if let Ok(s) = core::str::from_utf8(buf) {
        console::print(s);
    }

    count as u64
}
