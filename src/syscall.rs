//! System Call Handlers
//!
//! Implements the syscall interface for user programs.
//! Uses Linux-compatible ABI: syscall number in x8, arguments in x0-x5.

use crate::console;

/// Syscall numbers (Linux-compatible subset)
pub mod nr {
    pub const EXIT: u64 = 0;
    pub const READ: u64 = 1;
    pub const WRITE: u64 = 2;
    pub const BRK: u64 = 3;
    pub const OPENAT: u64 = 56;
    pub const CLOSE: u64 = 57;
    pub const LSEEK: u64 = 62;
    pub const FSTAT: u64 = 80;
    pub const NANOSLEEP: u64 = 101; // Linux arm64 nanosleep
    pub const SOCKET: u64 = 198;
    pub const BIND: u64 = 200;
    pub const LISTEN: u64 = 201;
    pub const ACCEPT: u64 = 202;
    pub const CONNECT: u64 = 203;
    pub const SENDTO: u64 = 206;
    pub const RECVFROM: u64 = 207;
    pub const SHUTDOWN: u64 = 210;
    pub const MUNMAP: u64 = 215; // Linux arm64 munmap
    pub const UPTIME: u64 = 216;
    pub const MMAP: u64 = 222; // Linux arm64 mmap
    pub const GETDENTS64: u64 = 61; // Linux arm64 getdents64
    pub const MKDIRAT: u64 = 34;     // Linux arm64 mkdirat
    // Custom syscalls (300+)
    pub const RESOLVE_HOST: u64 = 300;
    pub const SPAWN: u64 = 301;      // Spawn a child process, returns (pid, stdout_fd)
    pub const KILL: u64 = 302;       // Kill a process by PID
    pub const WAITPID: u64 = 303;    // Wait for child, returns exit status
    pub const GETRANDOM: u64 = 304;  // Fill buffer with random bytes from VirtIO RNG
    pub const TIME: u64 = 305;        // Get current Unix timestamp (seconds since epoch)
    pub const CHDIR: u64 = 306;       // Change current working directory
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
        nr::OPENAT => sys_openat(args[0] as i32, args[1], args[2] as usize, args[3] as u32, args[4] as u32),
        nr::CLOSE => sys_close(args[0] as u32),
        nr::LSEEK => sys_lseek(args[0] as u32, args[1] as i64, args[2] as i32),
        nr::FSTAT => sys_fstat(args[0] as u32, args[1]),
        nr::NANOSLEEP => sys_nanosleep(args[0], args[1]),
        nr::SOCKET => sys_socket(args[0] as i32, args[1] as i32, args[2] as i32),
        nr::BIND => sys_bind(args[0] as u32, args[1], args[2] as usize),
        nr::LISTEN => sys_listen(args[0] as u32, args[1] as i32),
        nr::ACCEPT => sys_accept(args[0] as u32, args[1], args[2]),
        nr::CONNECT => sys_connect(args[0] as u32, args[1], args[2] as usize),
        nr::SENDTO => sys_sendto(args[0] as u32, args[1], args[2] as usize, args[3] as i32),
        nr::RECVFROM => sys_recvfrom(args[0] as u32, args[1], args[2] as usize, args[3] as i32),
        nr::SHUTDOWN => sys_shutdown(args[0] as u32, args[1] as i32),
        nr::MMAP => sys_mmap(
            args[0] as usize,
            args[1] as usize,
            args[2] as u32,
            args[3] as u32,
        ),
        nr::MUNMAP => sys_munmap(args[0] as usize, args[1] as usize),
        nr::UPTIME => sys_uptime(),
        nr::RESOLVE_HOST => sys_resolve_host(args[0], args[1] as usize, args[2]),
        nr::GETDENTS64 => sys_getdents64(args[0] as u32, args[1], args[2] as usize),
        nr::MKDIRAT => sys_mkdirat(args[0] as i32, args[1], args[2] as usize, args[3] as u32),
        nr::SPAWN => sys_spawn(args[0], args[1] as usize, args[2], args[3] as usize, args[4], args[5] as usize),
        nr::KILL => sys_kill(args[0] as u32),
        nr::WAITPID => sys_waitpid(args[0] as u32, args[1]),
        nr::GETRANDOM => sys_getrandom(args[0], args[1] as usize),
        nr::TIME => sys_time(),
        nr::CHDIR => sys_chdir(args[0], args[1] as usize),
        _ => {
            crate::safe_print!(64, "[Syscall] Unknown syscall: {}\n", syscall_num);
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
/// Uses the wait queue scheduler to yield during sleep, allowing other threads
/// to run. Each thread has its own exception stack, so context switching during
/// syscalls is safe.
///
/// # Arguments
/// * `seconds` - Number of seconds to sleep
/// * `nanoseconds` - Additional nanoseconds to sleep
///
/// # Returns
/// 0 on success, EINTR if interrupted
fn sys_nanosleep(seconds: u64, nanoseconds: u64) -> u64 {
    let total_us = seconds * 1_000_000 + nanoseconds / 1_000;
    let deadline = crate::timer::uptime_us() + total_us;

    // Wait queue based sleep:
    // 1. Mark thread as WAITING with wake_time = deadline
    // 2. Yield to scheduler (switches to another thread)
    // 3. Scheduler periodically checks wake_time and marks us READY when elapsed
    // 4. When scheduled again, we resume here and check if done
    //
    // Each thread has its own exception stack (trap frame area), so the
    // context switch during syscall handling is safe.

    loop {
        // Check if sleep is complete
        if crate::timer::uptime_us() >= deadline {
            return 0; // Done sleeping
        }

        // Check for interrupt signal (Ctrl+C)
        if crate::process::is_current_interrupted() {
            // Interrupted - mark process as exited
            if let Some(proc) = crate::process::current_process() {
                proc.exited = true;
                proc.exit_code = 130;
                proc.state = crate::process::ProcessState::Zombie(130);
            }
            return EINTR;
        }

        // Block until deadline - yields to scheduler, wakes when time elapses
        crate::threading::schedule_blocking(deadline);
        
        // We've been woken - loop to check if deadline passed or if interrupted
    }
}

fn sys_uptime() -> u64 {
    crate::timer::uptime_us()
}

/// sys_time - Get current Unix timestamp
///
/// Returns the current time as seconds since Unix epoch (1970-01-01 00:00:00 UTC).
/// Uses the PL031 RTC if available, falls back to 0 if not initialized.
fn sys_time() -> u64 {
    crate::timer::utc_seconds().unwrap_or(0)
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

    // Collect frames for this mmap region (for munmap to find later)
    let mut region_frames = alloc::vec::Vec::with_capacity(pages);

    // Map pages and track frames for cleanup on process exit
    for i in 0..pages {
        let va = mmap_addr + i * PAGE_SIZE;
        if let Some(frame) = pmm::alloc_page_zeroed() {
            // Debug tracking: record this as a user data allocation
            pmm::track_frame(frame, pmm::FrameSource::UserData, proc.pid);

            // Track frame so it will be freed when process exits
            // (redundant with mmap_regions but kept for safety)
            proc.address_space.track_user_frame(frame);

            // Also track in region_frames for munmap
            region_frames.push(frame);

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

    // Record this mmap region so munmap can find the frames
    crate::process::record_mmap_region(mmap_addr, region_frames);

    mmap_addr as u64
}

/// sys_munmap - Unmap memory pages
///
/// Finds the mmap'd region, unmaps pages from page table, and frees physical frames.
fn sys_munmap(addr: usize, len: usize) -> u64 {
    use crate::pmm;

    const PAGE_SIZE: usize = 4096;

    if addr == 0 || len == 0 || addr % PAGE_SIZE != 0 {
        return (-1i64) as u64; // EINVAL
    }

    // Find and remove the mmap region
    let frames = match crate::process::remove_mmap_region(addr) {
        Some(f) => f,
        None => {
            // No region at this address - could be already unmapped or invalid
            return (-1i64) as u64; // EINVAL
        }
    };

    // Get process to access address space
    let proc = match crate::process::current_process() {
        Some(p) => p,
        None => return (-1i64) as u64,
    };

    // Unmap each page and free the frame
    for (i, frame) in frames.into_iter().enumerate() {
        let va = addr + i * PAGE_SIZE;

        // Unmap from page table (clears PTE, invalidates TLB)
        if let Err(_) = proc.address_space.unmap_page(va) {
            // Page wasn't mapped - continue anyway
        }

        // Remove from user_frames tracking (so it won't be double-freed on exit)
        proc.address_space.remove_user_frame(frame);

        // Free the physical frame
        pmm::free_page(frame);
    }

    let _ = len; // len is implicit from the recorded region size

    0 // Success
}

/// sys_exit - Terminate the current process
///
/// # Arguments
/// * `code` - Exit code
fn sys_exit(code: i32) -> u64 {
    // Validate exit code at syscall entry - detect if userspace passed garbage
    let code_u32 = code as u32;
    if code_u32 >= 0x40000000 && code_u32 < 0x50000000 {
        crate::console::print("[sys_exit] WARNING: exit code looks like kernel address!\n");
        crate::safe_print!(64, "  code={} (0x{:x})\n", code, code_u32);
    }
    
    // Update per-process state only
    if let Some(proc) = crate::process::current_process() {
        proc.exited = true;
        proc.exit_code = code;
        proc.state = crate::process::ProcessState::Zombie(code);
        
        // Debug: verify the write happened correctly
        if proc.exit_code != code {
            crate::safe_print!(64, "[sys_exit] EXIT CODE MISMATCH! wrote {} but read {}\n",
                code, proc.exit_code);
        }
    }
    // Note: If no current process, this is a kernel thread calling exit which is harmless

    // Return won't matter - process is terminated
    code as u64
}

/// sys_read - Read from a file descriptor
///
/// # Arguments
/// * `fd` - File descriptor
/// * `buf` - User buffer pointer
/// * `count` - Number of bytes to read
///
/// # Returns
/// Number of bytes read, or negative error code
fn sys_read(fd_num: u64, buf_ptr: u64, count: usize) -> u64 {
    if buf_ptr == 0 || count == 0 {
        return 0;
    }

    let proc = match crate::process::current_process() {
        Some(p) => p,
        None => return (-libc_errno::EBADF as i64) as u64,
    };

    // Get file descriptor entry
    let fd_entry = match proc.get_fd(fd_num as u32) {
        Some(e) => e,
        None => return (-libc_errno::EBADF as i64) as u64,
    };

    match fd_entry {
        FileDescriptor::Stdin => {
            // First try reading from the ProcessChannel's stdin buffer (interactive input)
            // This allows SSH to forward input to running processes
            let mut temp_buf = alloc::vec![0u8; count];
            let bytes_from_channel = if let Some(channel) = crate::process::current_channel() {
                channel.read_stdin(&mut temp_buf)
            } else {
                0
            };
            
            if bytes_from_channel > 0 {
                unsafe {
                    let dst = buf_ptr as *mut u8;
                    core::ptr::copy_nonoverlapping(temp_buf.as_ptr(), dst, bytes_from_channel);
                }
                return bytes_from_channel as u64;
            }
            
            // Fall back to process stdin buffer (pre-populated stdin from pipes, etc.)
            let bytes_read = proc.read_stdin(&mut temp_buf);
            if bytes_read > 0 {
                unsafe {
                    let dst = buf_ptr as *mut u8;
                    core::ptr::copy_nonoverlapping(temp_buf.as_ptr(), dst, bytes_read);
                }
            }
            bytes_read as u64
        }
        FileDescriptor::Stdout | FileDescriptor::Stderr => {
            // Can't read from stdout/stderr
            (-libc_errno::EBADF as i64) as u64
        }
        FileDescriptor::File(ref file) => {
            // Read from file
            let path = file.path.clone();
            let position = file.position;

            // Read entire file (TODO: optimize with partial reads)
            let file_data = match crate::fs::read_file(&path) {
                Ok(data) => data,
                Err(_) => return (-libc_errno::EIO as i64) as u64,
            };

            // Calculate how much to read
            if position >= file_data.len() {
                return 0; // EOF
            }
            let available = file_data.len() - position;
            let to_read = count.min(available);

            // Copy to user buffer
            unsafe {
                let dst = buf_ptr as *mut u8;
                core::ptr::copy_nonoverlapping(
                    file_data[position..].as_ptr(),
                    dst,
                    to_read,
                );
            }

            // Update file position
            proc.update_fd(fd_num as u32, |entry| {
                if let FileDescriptor::File(f) = entry {
                    f.position += to_read;
                }
            });

            to_read as u64
        }
        FileDescriptor::Socket(_socket_idx) => {
            // TODO: Read from socket via embassy-net
            // For now, return EAGAIN
            (-libc_errno::EAGAIN as i64) as u64
        }
        FileDescriptor::ChildStdout(child_pid) => {
            // Read from child process stdout via ProcessChannel
            use crate::process;
            
            if let Some(channel) = process::get_child_channel(child_pid) {
                if let Some(data) = channel.try_read() {
                    let to_copy = data.len().min(count);
                    if to_copy > 0 {
                        unsafe {
                            let dst = buf_ptr as *mut u8;
                            core::ptr::copy_nonoverlapping(data.as_ptr(), dst, to_copy);
                        }
                    }
                    to_copy as u64
                } else if channel.has_exited() {
                    0 // EOF - child exited
                } else {
                    // No data available yet, would block
                    (-libc_errno::EAGAIN as i64) as u64
                }
            } else {
                0 // Channel gone, child exited
            }
        }
    }
}

/// sys_write - Write to a file descriptor
///
/// # Arguments
/// * `fd` - File descriptor
/// * `buf` - User buffer pointer
/// * `count` - Number of bytes to write
///
/// # Returns
/// Number of bytes written, or negative error code
fn sys_write(fd_num: u64, buf_ptr: u64, count: usize) -> u64 {
    if buf_ptr == 0 || count == 0 {
        return 0;
    }

    let proc = match crate::process::current_process() {
        Some(p) => p,
        None => return (-libc_errno::EBADF as i64) as u64,
    };

    // Get file descriptor entry
    let fd_entry = match proc.get_fd(fd_num as u32) {
        Some(e) => e,
        None => return (-libc_errno::EBADF as i64) as u64,
    };

    // SAFETY: We trust the user buffer is valid (TODO: add proper validation)
    let buf = unsafe { core::slice::from_raw_parts(buf_ptr as *const u8, count) };

    match fd_entry {
        FileDescriptor::Stdout | FileDescriptor::Stderr => {
            // Write to process channel for streaming output
            if let Some(channel) = crate::process::current_channel() {
                channel.write(buf);
            }

            // Write to per-process stdout buffer
            proc.write_stdout(buf);

            // Print to kernel console
            if let Ok(s) = core::str::from_utf8(buf) {
                console::print(s);
            }

            count as u64
        }
        FileDescriptor::File(ref file) => {
            // Write to file
            let path = file.path.clone();
            let flags = file.flags;
            let position = file.position;

            // Check if file was opened for writing
            use crate::process::open_flags;
            if flags & open_flags::O_WRONLY == 0 && flags & open_flags::O_RDWR == 0 {
                return (-libc_errno::EBADF as i64) as u64;
            }

            // For append mode, always append
            if flags & open_flags::O_APPEND != 0 {
                match crate::fs::append_file(&path, buf) {
                    Ok(()) => return count as u64,
                    Err(_) => return (-libc_errno::EIO as i64) as u64,
                }
            }

            // For regular write, we need to write at the current position
            // Read existing file, modify at position, write back
            let mut file_data = match crate::fs::read_file(&path) {
                Ok(data) => data,
                Err(_) => alloc::vec::Vec::new(), // File doesn't exist or is empty
            };

            // Extend file if writing past end
            let end_pos = position + count;
            if end_pos > file_data.len() {
                file_data.resize(end_pos, 0);
            }

            // Copy data at position
            file_data[position..end_pos].copy_from_slice(buf);

            // Write back
            match crate::fs::write_file(&path, &file_data) {
                Ok(()) => {
                    // Update file position
                    proc.update_fd(fd_num as u32, |entry| {
                        if let FileDescriptor::File(f) = entry {
                            f.position += count;
                        }
                    });
                    count as u64
                }
                Err(_) => (-libc_errno::EIO as i64) as u64,
            }
        }
        FileDescriptor::Socket(_socket_idx) => {
            // TODO: Write to socket via embassy-net
            // For now, return success with bytes "written"
            count as u64
        }
        _ => (-libc_errno::EBADF as i64) as u64,
    }
}

// ============================================================================
// Socket Syscalls
// ============================================================================

use crate::process::FileDescriptor;
use crate::socket::{self, SocketAddrV4, SockAddrIn, libc_errno};

/// sys_socket - Create a socket
///
/// # Arguments
/// * `domain` - Address family (AF_INET = 2)
/// * `sock_type` - Socket type (SOCK_STREAM = 1)
/// * `protocol` - Protocol (0 or IPPROTO_TCP = 6)
fn sys_socket(domain: i32, sock_type: i32, _protocol: i32) -> u64 {
    // Only support AF_INET + SOCK_STREAM for now
    if domain != socket::socket_const::AF_INET {
        return (-libc_errno::EINVAL as i64) as u64;
    }
    if sock_type != socket::socket_const::SOCK_STREAM {
        return (-libc_errno::EINVAL as i64) as u64;
    }

    // Allocate kernel socket
    let socket_idx = match socket::alloc_socket(sock_type) {
        Some(idx) => idx,
        None => return (-libc_errno::EAGAIN as i64) as u64,
    };

    // Allocate FD in process
    let proc = match crate::process::current_process() {
        Some(p) => p,
        None => {
            socket::remove_socket(socket_idx);
            return (-libc_errno::EBADF as i64) as u64;
        }
    };

    let fd = proc.alloc_fd(FileDescriptor::Socket(socket_idx));
    fd as u64
}

/// sys_bind - Bind socket to address
fn sys_bind(fd: u32, addr_ptr: u64, addr_len: usize) -> u64 {
    if addr_len < SockAddrIn::SIZE {
        return (-libc_errno::EINVAL as i64) as u64;
    }

    // Get socket index from FD
    let socket_idx = match get_socket_from_fd(fd) {
        Some(idx) => idx,
        None => return (-libc_errno::EBADF as i64) as u64,
    };

    // Read sockaddr from user memory
    let sockaddr = unsafe {
        core::ptr::read(addr_ptr as *const SockAddrIn)
    };
    let addr = sockaddr.to_addr();

    // Bind the socket
    match socket::socket_bind(socket_idx, addr) {
        Ok(()) => 0,
        Err(e) => (-e as i64) as u64,
    }
}

/// sys_listen - Mark socket as listening
fn sys_listen(fd: u32, backlog: i32) -> u64 {
    let socket_idx = match get_socket_from_fd(fd) {
        Some(idx) => idx,
        None => return (-libc_errno::EBADF as i64) as u64,
    };

    match socket::socket_listen(socket_idx, backlog as usize) {
        Ok(()) => 0,
        Err(e) => (-e as i64) as u64,
    }
}

/// sys_accept - Accept a connection (blocking)
///
/// This blocks until a connection is available, yielding to the scheduler.
/// Creates a new socket for the accepted connection.
fn sys_accept(fd: u32, addr_ptr: u64, addr_len_ptr: u64) -> u64 {
    use embassy_net::tcp::TcpSocket;
    
    let socket_idx = match get_socket_from_fd(fd) {
        Some(idx) => idx,
        None => {
            crate::console::print("[accept] EBADF: fd not found\n");
            return (-libc_errno::EBADF as i64) as u64;
        }
    };

    // Verify socket is listening and get local address
    let state = match socket::get_socket_state(socket_idx) {
        Some(s) => s,
        None => {
            crate::console::print("[accept] EBADF: socket state not found\n");
            return (-libc_errno::EBADF as i64) as u64;
        }
    };

    let local_addr = match state {
        socket::SocketState::Listening { local_addr, .. } => local_addr,
        _ => {
            crate::console::print("[accept] EINVAL: socket not listening\n");
            return (-libc_errno::EINVAL as i64) as u64;
        }
    };

    // Increment ref count on listening socket to prevent close during accept
    if socket::socket_inc_ref(socket_idx).is_none() {
        crate::console::print("[accept] EBADF: inc_ref failed\n");
        return (-libc_errno::EBADF as i64) as u64;
    }

    // Get the appropriate network stack (loopback for 127.x.x.x)
    let is_loopback = local_addr.ip[0] == 127;
    let stack = if is_loopback {
        match crate::async_net::get_loopback_stack() {
            Some(s) => s,
            None => {
                crate::console::print("[accept] ENETDOWN: no loopback stack\n");
                socket::socket_dec_ref(socket_idx);
                return (-libc_errno::ENETDOWN as i64) as u64;
            }
        }
    } else {
        match crate::async_net::get_global_stack() {
            Some(s) => s,
            None => {
                crate::console::print("[accept] ENETDOWN: no global stack\n");
                socket::socket_dec_ref(socket_idx);
                return (-libc_errno::ENETDOWN as i64) as u64;
            }
        }
    };

    // Allocate buffer slot for the NEW connection socket
    let new_slot = match socket::alloc_buffer_slot() {
        Some(s) => s,
        None => {
            crate::console::print("[accept] ENOMEM: no buffer slots\n");
            socket::socket_dec_ref(socket_idx);
            return (-libc_errno::ENOMEM as i64) as u64;
        }
    };

    // Get buffers for the new socket
    let (rx_buf, tx_buf) = unsafe { socket::get_buffers(new_slot) };

    crate::safe_print!(64, "[accept] Waiting on port {} (slot {})\n", local_addr.port, new_slot);

    // Use block_on_accept which stores socket directly in table to avoid
    // returning TcpSocket by value (which causes stack corruption)
    let new_socket_idx = match block_on_accept(stack, rx_buf, tx_buf, local_addr.port, new_slot, local_addr) {
        Ok(v) => v,
        Err(e) => {
            crate::safe_print!(48, "[accept] failed: errno={}\n", e);
            socket::free_buffer_slot(new_slot);
            socket::socket_dec_ref(socket_idx);
            return (-e as i64) as u64;
        }
    };

    // Decrement ref count on listening socket (we're done using it)
    socket::socket_dec_ref(socket_idx);

    // MINIMAL VERSION - no remote_addr lookup, no extra logging
    let _ = (addr_ptr, addr_len_ptr); // silence warnings

    // Allocate FD for new socket
    let proc = match crate::process::current_process() {
        Some(p) => p,
        None => {
            socket::socket_close(new_socket_idx).ok();
            return (-libc_errno::EBADF as i64) as u64;
        }
    };

    let new_fd = proc.alloc_fd(FileDescriptor::Socket(new_socket_idx));
    crate::safe_print!(48, "[accept] fd={} idx={}\n", new_fd, new_socket_idx);
    new_fd as u64
}

/// sys_connect - Connect to remote address (blocking)
fn sys_connect(fd: u32, addr_ptr: u64, addr_len: usize) -> u64 {
    use embassy_net::tcp::TcpSocket;
    
    if addr_len < SockAddrIn::SIZE {
        return (-libc_errno::EINVAL as i64) as u64;
    }

    let socket_idx = match get_socket_from_fd(fd) {
        Some(idx) => idx,
        None => return (-libc_errno::EBADF as i64) as u64,
    };

    // Verify socket is in correct state (unbound or bound)
    let state = match socket::get_socket_state(socket_idx) {
        Some(s) => s,
        None => return (-libc_errno::EBADF as i64) as u64,
    };

    match state {
        socket::SocketState::Unbound | socket::SocketState::Bound { .. } => {}
        socket::SocketState::Connected { .. } => {
            return (-libc_errno::EISCONN as i64) as u64;
        }
        _ => return (-libc_errno::EINVAL as i64) as u64,
    }

    // Read sockaddr from user memory
    let sockaddr = unsafe {
        core::ptr::read(addr_ptr as *const SockAddrIn)
    };
    let remote_addr = sockaddr.to_addr();

    // Increment ref count to prevent close during connect
    if socket::socket_inc_ref(socket_idx).is_none() {
        return (-libc_errno::EBADF as i64) as u64;
    }

    // Get the appropriate network stack (loopback for 127.x.x.x)
    let is_loopback = remote_addr.ip[0] == 127;
    let stack = if is_loopback {
        match crate::async_net::get_loopback_stack() {
            Some(s) => s,
            None => {
                socket::socket_dec_ref(socket_idx);
                crate::safe_print!(64, "[sys_connect] No loopback stack available\n");
                return (-libc_errno::ENETDOWN as i64) as u64;
            }
        }
    } else {
        match crate::async_net::get_global_stack() {
            Some(s) => s,
            None => {
                socket::socket_dec_ref(socket_idx);
                return (-libc_errno::ENETDOWN as i64) as u64;
            }
        }
    };

    // Get buffer slot for this socket
    let buffer_slot = match socket::get_socket_buffer_slot(socket_idx) {
        Some(s) => s,
        None => {
            socket::socket_dec_ref(socket_idx);
            return (-libc_errno::EBADF as i64) as u64;
        }
    };

    // Get buffers
    let (rx_buf, tx_buf) = unsafe { socket::get_buffers(buffer_slot) };

    // Block on connect
    let (tcp_socket, local_ep) = match block_on_connect(stack, rx_buf, tx_buf, remote_addr.to_endpoint()) {
        Ok(v) => v,
        Err(e) => {
            socket::socket_dec_ref(socket_idx);
            return (-e as i64) as u64;
        }
    };

    // Get local address
    let local_addr = match local_ep {
        Some(ep) => socket::SocketAddrV4::from_endpoint(ep).unwrap_or(socket::SocketAddrV4::new([0,0,0,0], 0)),
        None => socket::SocketAddrV4::new([0, 0, 0, 0], 0),
    };

    // Store the socket handle and update state
    socket::store_socket_handle(socket_idx, tcp_socket);
    let _ = socket::socket_set_connected(socket_idx, local_addr, remote_addr);

    // Decrement ref count
    socket::socket_dec_ref(socket_idx);

    0
}

/// sys_sendto - Send data on socket (blocking)
///
/// Writes data to a connected socket. Blocks until data is sent or an error occurs.
fn sys_sendto(fd: u32, buf_ptr: u64, len: usize, _flags: i32) -> u64 {
    let socket_idx = match get_socket_from_fd(fd) {
        Some(idx) => idx,
        None => return (-libc_errno::EBADF as i64) as u64,
    };

    if buf_ptr == 0 || len == 0 {
        return 0;
    }

    // Increment ref count to prevent close during write
    if socket::socket_inc_ref(socket_idx).is_none() {
        return (-libc_errno::EBADF as i64) as u64;
    }

    // Use a kernel buffer for embassy-net, then copy from user space
    // Embassy-net can't safely access user buffers directly
    const KERNEL_BUF_SIZE: usize = 4096;
    let mut kernel_buf = [0u8; KERNEL_BUF_SIZE];

    let mut total_written = 0usize;
    let mut iterations = 0usize;
    const MAX_ITERATIONS: usize = 100_000;

    loop {
        // Check for interrupt
        if crate::process::is_current_interrupted() {
            socket::socket_dec_ref(socket_idx);
            return if total_written > 0 {
                total_written as u64
            } else {
                (-libc_errno::EINTR as i64) as u64
            };
        }

        // Calculate how much to write this iteration
        let remaining = len - total_written;
        if remaining == 0 {
            socket::socket_dec_ref(socket_idx);
            return total_written as u64;
        }
        let chunk_size = core::cmp::min(remaining, KERNEL_BUF_SIZE);

        // Copy from user buffer to kernel buffer
        unsafe {
            core::ptr::copy_nonoverlapping(
                (buf_ptr as *const u8).add(total_written),
                kernel_buf.as_mut_ptr(),
                chunk_size,
            );
        }

        // Try to write with socket handle using KERNEL buffer
        let result = socket::with_socket_handle(socket_idx, |socket| {
            use core::future::Future;
            use core::pin::Pin;
            use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

            static VTABLE: RawWakerVTable = RawWakerVTable::new(
                |_| RawWaker::new(core::ptr::null(), &VTABLE),
                |_| {}, |_| {}, |_| {},
            );

            let waker = unsafe { Waker::from_raw(RawWaker::new(core::ptr::null(), &VTABLE)) };
            let mut cx = Context::from_waker(&waker);

            let mut write_future = socket.write(&kernel_buf[..chunk_size]);
            let pinned = unsafe { Pin::new_unchecked(&mut write_future) };

            match pinned.poll(&mut cx) {
                Poll::Ready(Ok(n)) => Ok(n),
                Poll::Ready(Err(_)) => Err(libc_errno::EIO),
                Poll::Pending => Ok(0), // Would block, return 0 to indicate retry
            }
        });

        match result {
            Ok(Ok(n)) if n > 0 => {
                total_written += n;
                if total_written >= len {
                    // All data written - flush to ensure transmission
                    let _ = socket::with_socket_handle(socket_idx, |socket| {
                        use core::future::Future;
                        use core::pin::Pin;
                        use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

                        static VTABLE: RawWakerVTable = RawWakerVTable::new(
                            |_| RawWaker::new(core::ptr::null(), &VTABLE),
                            |_| {}, |_| {}, |_| {},
                        );

                        let waker = unsafe { Waker::from_raw(RawWaker::new(core::ptr::null(), &VTABLE)) };
                        let mut cx = Context::from_waker(&waker);

                        // Poll flush a few times to help push data out
                        for _ in 0..100 {
                            let mut flush_future = socket.flush();
                            let pinned = unsafe { Pin::new_unchecked(&mut flush_future) };
                            match pinned.poll(&mut cx) {
                                Poll::Ready(_) => break,
                                Poll::Pending => {
                                    crate::threading::yield_now();
                                }
                            }
                        }
                    });
                    socket::socket_dec_ref(socket_idx);
                    return total_written as u64;
                }
                // More data to write, continue loop
                iterations = 0; // Reset timeout since we made progress
            }
            Ok(Ok(_)) => {
                // Would block - yield and retry
                iterations += 1;
                if iterations >= MAX_ITERATIONS {
                    socket::socket_dec_ref(socket_idx);
                    return if total_written > 0 {
                        total_written as u64
                    } else {
                        (-libc_errno::ETIMEDOUT as i64) as u64
                    };
                }
                crate::threading::yield_now();
                for _ in 0..100 { core::hint::spin_loop(); }
            }
            Ok(Err(e)) | Err(e) => {
                socket::socket_dec_ref(socket_idx);
                return if total_written > 0 {
                    total_written as u64
                } else {
                    (-e as i64) as u64
                };
            }
        }
    }
}

/// sys_recvfrom - Receive data from socket (blocking)
///
/// Reads data from a connected socket. Blocks until data is available or an error occurs.
fn sys_recvfrom(fd: u32, buf_ptr: u64, len: usize, _flags: i32) -> u64 {
    let socket_idx = match get_socket_from_fd(fd) {
        Some(idx) => idx,
        None => return (-libc_errno::EBADF as i64) as u64,
    };

    if buf_ptr == 0 || len == 0 {
        return 0;
    }

    // Increment ref count to prevent close during read
    if socket::socket_inc_ref(socket_idx).is_none() {
        return (-libc_errno::EBADF as i64) as u64;
    }

    // Use a kernel buffer for embassy-net, then copy to user space
    // Embassy-net can't safely access user buffers directly
    const KERNEL_BUF_SIZE: usize = 4096;
    let mut kernel_buf = [0u8; KERNEL_BUF_SIZE];
    let read_len = core::cmp::min(len, KERNEL_BUF_SIZE);

    let mut iterations = 0usize;
    // Short timeout (50 iterations * ~10ms yield = ~500ms) then return EAGAIN
    // This allows userspace to do work (like print progress dots) while waiting
    const EAGAIN_ITERATIONS: usize = 50;

    loop {
        // Check for interrupt
        if crate::process::is_current_interrupted() {
            socket::socket_dec_ref(socket_idx);
            return (-libc_errno::EINTR as i64) as u64;
        }

        // Try to read with socket handle into KERNEL buffer
        let result = socket::with_socket_handle(socket_idx, |socket| {
            use core::future::Future;
            use core::pin::Pin;
            use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

            static VTABLE: RawWakerVTable = RawWakerVTable::new(
                |_| RawWaker::new(core::ptr::null(), &VTABLE),
                |_| {}, |_| {}, |_| {},
            );

            let waker = unsafe { Waker::from_raw(RawWaker::new(core::ptr::null(), &VTABLE)) };
            let mut cx = Context::from_waker(&waker);

            let mut read_future = socket.read(&mut kernel_buf[..read_len]);
            let pinned = unsafe { Pin::new_unchecked(&mut read_future) };

            match pinned.poll(&mut cx) {
                Poll::Ready(Ok(n)) => Ok(n as isize),
                Poll::Ready(Err(_)) => Ok(0), // EOF or error, return 0
                Poll::Pending => Ok(-1), // Would block
            }
        });

        match result {
            Ok(Ok(n)) if n >= 0 => {
                // Copy from kernel buffer to user buffer
                let bytes_read = n as usize;
                if bytes_read > 0 {
                    unsafe {
                        core::ptr::copy_nonoverlapping(
                            kernel_buf.as_ptr(),
                            buf_ptr as *mut u8,
                            bytes_read,
                        );
                    }
                }
                socket::socket_dec_ref(socket_idx);
                return n as u64;
            }
            Ok(Ok(_)) => {
                // Would block (-1) - yield and retry
                iterations += 1;
                if iterations >= EAGAIN_ITERATIONS {
                    // Return EAGAIN so userspace can do other work (print dots, etc.)
                    socket::socket_dec_ref(socket_idx);
                    return (-libc_errno::EAGAIN as i64) as u64;
                }
                crate::threading::yield_now();
                for _ in 0..100 { core::hint::spin_loop(); }
            }
            Ok(Err(e)) | Err(e) => {
                socket::socket_dec_ref(socket_idx);
                return (-e as i64) as u64;
            }
        }
    }
}

/// sys_shutdown - Shutdown socket
fn sys_shutdown(fd: u32, how: i32) -> u64 {
    let socket_idx = match get_socket_from_fd(fd) {
        Some(idx) => idx,
        None => return (-libc_errno::EBADF as i64) as u64,
    };

    // Validate shutdown mode
    if how < 0 || how > 2 {
        return (-libc_errno::EINVAL as i64) as u64;
    }

    // TODO: Actually shutdown via embassy-net
    let _ = socket_idx;
    0
}

/// sys_close - Close a file descriptor
fn sys_close(fd: u32) -> u64 {
    let proc = match crate::process::current_process() {
        Some(p) => p,
        None => return (-libc_errno::EBADF as i64) as u64,
    };

    // Remove FD from process table
    let entry = match proc.remove_fd(fd) {
        Some(e) => e,
        None => return (-libc_errno::EBADF as i64) as u64,
    };

    // Handle based on FD type
    match entry {
        FileDescriptor::Socket(socket_idx) => {
            // Close the kernel socket
            match socket::socket_close(socket_idx) {
                Ok(()) => 0,
                Err(e) => (-e as i64) as u64,
            }
        }
        FileDescriptor::File(_) => {
            // TODO: Close file handle
            0
        }
        FileDescriptor::Stdin | FileDescriptor::Stdout | FileDescriptor::Stderr => {
            // Don't actually close stdio
            0
        }
        FileDescriptor::ChildStdout(child_pid) => {
            // Close child stdout FD - remove channel reference
            crate::process::remove_child_channel(child_pid);
            0
        }
    }
}

/// Helper: Get socket index from file descriptor
fn get_socket_from_fd(fd: u32) -> Option<usize> {
    let proc = crate::process::current_process()?;
    match proc.get_fd(fd)? {
        FileDescriptor::Socket(idx) => Some(idx),
        _ => None,
    }
}

// ============================================================================
// DNS Syscall
// ============================================================================

use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

/// sys_resolve_host - Resolve hostname to IPv4 address
///
/// # Arguments
/// * `hostname_ptr` - Pointer to hostname string
/// * `hostname_len` - Length of hostname string
/// * `result_ptr` - Pointer to write 4-byte IPv4 result
///
/// # Returns
/// 0 on success, negative error code on failure
fn sys_resolve_host(hostname_ptr: u64, hostname_len: usize, result_ptr: u64) -> u64 {
    // Validate pointers
    if hostname_ptr == 0 || result_ptr == 0 || hostname_len == 0 {
        return (-libc_errno::EINVAL as i64) as u64;
    }

    // Read hostname from user memory
    let hostname_bytes = unsafe {
        core::slice::from_raw_parts(hostname_ptr as *const u8, hostname_len)
    };
    let hostname = match core::str::from_utf8(hostname_bytes) {
        Ok(s) => s,
        Err(_) => return (-libc_errno::EINVAL as i64) as u64,
    };

    // Handle localhost specially (no network needed)
    if hostname == "localhost" {
        unsafe {
            let result = result_ptr as *mut [u8; 4];
            *result = [127, 0, 0, 1];
        }
        return 0;
    }

    // Try to parse as IPv4 literal
    if let Ok(ipv4) = hostname.parse::<embassy_net::Ipv4Address>() {
        unsafe {
            let result = result_ptr as *mut [u8; 4];
            *result = ipv4.octets();
        }
        return 0;
    }

    // Get the network stack
    let stack = match crate::async_net::get_global_stack() {
        Some(s) => s,
        None => return (-libc_errno::EHOSTUNREACH as i64) as u64,
    };

    // Create the DNS query future
    // We need to own the hostname since the future is async
    let hostname_owned = alloc::string::String::from(hostname);
    let dns_future = async move {
        crate::dns::resolve_host(&hostname_owned, &stack).await
    };

    // Block on the async DNS resolution
    let result = block_on_dns(dns_future);

    match result {
        Ok((embassy_net::IpAddress::Ipv4(ipv4), _duration)) => {
            unsafe {
                let result = result_ptr as *mut [u8; 4];
                *result = ipv4.octets();
            }
            0
        }
        _ => (-libc_errno::EHOSTUNREACH as i64) as u64,
    }
}

// ============================================================================
// File I/O Syscalls
// ============================================================================

use crate::process::KernelFile;

/// Linux stat structure (simplified)
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Stat {
    pub st_dev: u64,
    pub st_ino: u64,
    pub st_mode: u32,
    pub st_nlink: u32,
    pub st_uid: u32,
    pub st_gid: u32,
    pub st_rdev: u64,
    pub __pad1: u64,
    pub st_size: i64,
    pub st_blksize: i32,
    pub __pad2: i32,
    pub st_blocks: i64,
    pub st_atime: i64,
    pub st_atime_nsec: i64,
    pub st_mtime: i64,
    pub st_mtime_nsec: i64,
    pub st_ctime: i64,
    pub st_ctime_nsec: i64,
    pub __unused: [i32; 2],
}

/// sys_openat - Open a file
///
/// # Arguments
/// * `dirfd` - Directory file descriptor (ignored, always use absolute paths)
/// * `pathname_ptr` - Pointer to pathname string
/// * `pathname_len` - Length of pathname
/// * `flags` - Open flags (O_RDONLY, O_WRONLY, O_RDWR, O_CREAT, etc.)
/// * `mode` - File mode (for O_CREAT)
fn sys_openat(_dirfd: i32, pathname_ptr: u64, pathname_len: usize, flags: u32, _mode: u32) -> u64 {
    if pathname_ptr == 0 || pathname_len == 0 {
        return (-libc_errno::EINVAL as i64) as u64;
    }

    // Read pathname from user memory
    let pathname_bytes = unsafe {
        core::slice::from_raw_parts(pathname_ptr as *const u8, pathname_len)
    };
    let pathname = match core::str::from_utf8(pathname_bytes) {
        Ok(s) => s,
        Err(_) => return (-libc_errno::EINVAL as i64) as u64,
    };

    // Check if file exists (unless O_CREAT)
    use crate::process::open_flags;
    if !crate::fs::exists(pathname) {
        if flags & open_flags::O_CREAT == 0 {
            return (-libc_errno::ENOENT as i64) as u64;
        }
        // Create empty file
        if crate::fs::write_file(pathname, &[]).is_err() {
            return (-libc_errno::EIO as i64) as u64;
        }
    }

    // Truncate if O_TRUNC
    if flags & open_flags::O_TRUNC != 0 {
        if crate::fs::write_file(pathname, &[]).is_err() {
            return (-libc_errno::EIO as i64) as u64;
        }
    }

    // Create kernel file handle
    let file = KernelFile::new(alloc::string::String::from(pathname), flags);

    // Allocate FD
    let proc = match crate::process::current_process() {
        Some(p) => p,
        None => return (-libc_errno::EBADF as i64) as u64,
    };

    let fd = proc.alloc_fd(FileDescriptor::File(file));
    fd as u64
}

/// sys_mkdirat - Create a directory
///
/// # Arguments
/// * `dirfd` - Directory file descriptor (ignored, always uses absolute path)
/// * `pathname_ptr` - Pointer to pathname string
/// * `pathname_len` - Length of pathname
/// * `mode` - Directory mode (ignored)
fn sys_mkdirat(_dirfd: i32, pathname_ptr: u64, pathname_len: usize, _mode: u32) -> u64 {
    if pathname_ptr == 0 || pathname_len == 0 {
        return (-libc_errno::EINVAL as i64) as u64;
    }

    // Read pathname from user memory
    let pathname_bytes = unsafe {
        core::slice::from_raw_parts(pathname_ptr as *const u8, pathname_len)
    };
    let pathname = match core::str::from_utf8(pathname_bytes) {
        Ok(s) => s,
        Err(_) => return (-libc_errno::EINVAL as i64) as u64,
    };

    // Create directory
    match crate::fs::create_dir(pathname) {
        Ok(()) => 0,
        Err(_) => (-libc_errno::EIO as i64) as u64,
    }
}

/// sys_lseek - Reposition file offset
///
/// # Arguments
/// * `fd` - File descriptor
/// * `offset` - Offset value
/// * `whence` - SEEK_SET (0), SEEK_CUR (1), SEEK_END (2)
fn sys_lseek(fd: u32, offset: i64, whence: i32) -> u64 {
    let proc = match crate::process::current_process() {
        Some(p) => p,
        None => return (-libc_errno::EBADF as i64) as u64,
    };

    let fd_entry = match proc.get_fd(fd) {
        Some(e) => e,
        None => return (-libc_errno::EBADF as i64) as u64,
    };

    let file = match fd_entry {
        FileDescriptor::File(f) => f,
        _ => return (-libc_errno::EINVAL as i64) as u64,
    };

    // Get file size for SEEK_END
    let file_size = match crate::fs::file_size(&file.path) {
        Ok(s) => s as i64,
        Err(_) => return (-libc_errno::EIO as i64) as u64,
    };

    // Calculate new position
    let current_pos = file.position as i64;
    let new_pos = match whence {
        0 => offset,                    // SEEK_SET
        1 => current_pos + offset,      // SEEK_CUR
        2 => file_size + offset,        // SEEK_END
        _ => return (-libc_errno::EINVAL as i64) as u64,
    };

    if new_pos < 0 {
        return (-libc_errno::EINVAL as i64) as u64;
    }

    // Update file position
    proc.update_fd(fd, |entry| {
        if let FileDescriptor::File(f) = entry {
            f.position = new_pos as usize;
        }
    });

    new_pos as u64
}

/// sys_fstat - Get file status
///
/// # Arguments
/// * `fd` - File descriptor
/// * `statbuf_ptr` - Pointer to stat structure to fill
fn sys_fstat(fd: u32, statbuf_ptr: u64) -> u64 {
    if statbuf_ptr == 0 {
        return (-libc_errno::EINVAL as i64) as u64;
    }

    let proc = match crate::process::current_process() {
        Some(p) => p,
        None => return (-libc_errno::EBADF as i64) as u64,
    };

    let fd_entry = match proc.get_fd(fd) {
        Some(e) => e,
        None => return (-libc_errno::EBADF as i64) as u64,
    };

    let file = match fd_entry {
        FileDescriptor::File(f) => f,
        _ => return (-libc_errno::EINVAL as i64) as u64,
    };

    // Get file size
    let file_size = match crate::fs::file_size(&file.path) {
        Ok(s) => s as i64,
        Err(_) => return (-libc_errno::EIO as i64) as u64,
    };

    // Fill stat structure
    let stat = Stat {
        st_size: file_size,
        st_mode: 0o100644, // Regular file, rw-r--r--
        st_blksize: 4096,
        st_blocks: (file_size + 511) / 512,
        ..Default::default()
    };

    // Write to user memory
    unsafe {
        core::ptr::write(statbuf_ptr as *mut Stat, stat);
    }

    0
}

/// Block on an async DNS future
///
/// Similar to SSH's block_on but specifically for DNS operations.
/// Polls the future in a loop, yielding to scheduler between polls.
fn block_on_dns<F: Future>(mut future: F) -> F::Output {
    // Pin the future on the stack
    let mut future = unsafe { Pin::new_unchecked(&mut future) };

    // Create a no-op waker
    static VTABLE: RawWakerVTable = RawWakerVTable::new(
        |_| RawWaker::new(core::ptr::null(), &VTABLE),
        |_| {},
        |_| {},
        |_| {},
    );

    let mut iterations = 0;
    const MAX_ITERATIONS: usize = 10000; // Timeout after ~10 seconds

    loop {
        let raw_waker = RawWaker::new(core::ptr::null(), &VTABLE);
        let waker = unsafe { Waker::from_raw(raw_waker) };
        let mut cx = Context::from_waker(&waker);

        // Disable preemption during poll (embassy-net uses RefCell)
        crate::threading::disable_preemption();
        let poll_result = future.as_mut().poll(&mut cx);
        crate::threading::enable_preemption();

        match poll_result {
            Poll::Ready(output) => return output,
            Poll::Pending => {
                iterations += 1;
                if iterations >= MAX_ITERATIONS {
                    // Timeout - return with whatever we have
                    // This prevents infinite loops
                    panic!("DNS resolution timeout");
                }

                // Check for interrupt
                if crate::process::is_current_interrupted() {
                    panic!("DNS resolution interrupted");
                }

                // Yield to scheduler
                crate::threading::yield_now();

                // Small spin delay
                for _ in 0..100 {
                    core::hint::spin_loop();
                }
            }
        }
    }
}

// ============================================================================
// Socket Blocking Helper
// ============================================================================

/// Block on an async socket future with proper error handling
///
/// This is the main blocking helper for socket operations. It:
/// 1. Polls the future with preemption disabled (protects embassy RefCell)
/// 2. Yields to scheduler when pending (allows thread 0 to poll network runner)
/// 3. Checks for interrupts (Ctrl+C)
/// 4. Times out after MAX_ITERATIONS to prevent hangs
///
/// Returns Ok(T) on success, Err(errno) on failure.
fn block_on_socket<F, T>(mut future: F) -> Result<T, i32>
where
    F: Future<Output = Result<T, embassy_net::tcp::Error>>,
{
    // Pin the future on the stack
    let mut future = unsafe { Pin::new_unchecked(&mut future) };

    // Create a no-op waker (we poll manually)
    static VTABLE: RawWakerVTable = RawWakerVTable::new(
        |_| RawWaker::new(core::ptr::null(), &VTABLE),
        |_| {},
        |_| {},
        |_| {},
    );

    let mut iterations = 0;
    const MAX_ITERATIONS: usize = 100_000; // ~100 seconds with 1ms yield

    loop {
        // Check for interrupt BEFORE polling (fast path for Ctrl+C)
        if crate::process::is_current_interrupted() {
            return Err(libc_errno::EINTR);
        }

        let raw_waker = RawWaker::new(core::ptr::null(), &VTABLE);
        let waker = unsafe { Waker::from_raw(raw_waker) };
        let mut cx = Context::from_waker(&waker);

        // CRITICAL: Disable preemption during poll
        // Embassy-net uses RefCell internally which panics on re-entrant borrow.
        // If we get preempted while holding a RefCell borrow, another thread
        // might try to borrow it too.
        crate::threading::disable_preemption();
        let poll_result = future.as_mut().poll(&mut cx);
        crate::threading::enable_preemption();

        match poll_result {
            Poll::Ready(Ok(val)) => return Ok(val),
            Poll::Ready(Err(e)) => {
                // Map embassy-net errors to errno
                return Err(map_tcp_error(e));
            }
            Poll::Pending => {
                iterations += 1;
                if iterations >= MAX_ITERATIONS {
                    return Err(libc_errno::ETIMEDOUT);
                }

                // Yield to scheduler - this allows thread 0 to poll the network runner
                // which processes actual network I/O
                crate::threading::yield_now();

                // Small spin delay to reduce scheduler overhead
                for _ in 0..100 {
                    core::hint::spin_loop();
                }
            }
        }
    }
}

/// Map embassy-net TCP errors to errno values
fn map_tcp_error(e: embassy_net::tcp::Error) -> i32 {
    match e {
        embassy_net::tcp::Error::ConnectionReset => libc_errno::ECONNREFUSED,
    }
}

/// Simpler block_on for socket operations that don't return Result
fn block_on_socket_infallible<F, T>(mut future: F) -> Result<T, i32>
where
    F: Future<Output = T>,
{
    let mut future = unsafe { Pin::new_unchecked(&mut future) };

    static VTABLE: RawWakerVTable = RawWakerVTable::new(
        |_| RawWaker::new(core::ptr::null(), &VTABLE),
        |_| {},
        |_| {},
        |_| {},
    );

    let mut iterations = 0;
    const MAX_ITERATIONS: usize = 100_000;

    loop {
        if crate::process::is_current_interrupted() {
            return Err(libc_errno::EINTR);
        }

        let raw_waker = RawWaker::new(core::ptr::null(), &VTABLE);
        let waker = unsafe { Waker::from_raw(raw_waker) };
        let mut cx = Context::from_waker(&waker);

        crate::threading::disable_preemption();
        let poll_result = future.as_mut().poll(&mut cx);
        crate::threading::enable_preemption();

        match poll_result {
            Poll::Ready(val) => return Ok(val),
            Poll::Pending => {
                iterations += 1;
                if iterations >= MAX_ITERATIONS {
                    return Err(libc_errno::ETIMEDOUT);
                }
                crate::threading::yield_now();
                for _ in 0..100 {
                    core::hint::spin_loop();
                }
            }
        }
    }
}

/// Block on TCP accept - creates socket and waits for connection
///
/// This is a specialized blocking helper for accept that handles the
/// TcpSocket lifetime properly. The socket is created inside and returned
/// on success.
///
/// SAFETY: We use UnsafeCell to work around the borrow checker because:
/// 1. The accept future borrows the socket mutably
/// 2. We need to access the socket after the future completes
/// 3. We carefully manage the lifetime - socket is only accessed when future is done
/// Accepts a connection and stores the socket directly in the socket table.
/// Returns socket_idx on success. Remote address can be retrieved from socket state.
/// This avoids returning TcpSocket or IpEndpoint by value which can cause stack issues.
fn block_on_accept(
    stack: embassy_net::Stack<'static>,
    rx_buf: &'static mut [u8],
    tx_buf: &'static mut [u8],
    port: u16,
    buffer_slot: usize,
    local_addr: socket::SocketAddrV4,
) -> Result<usize, i32> {
    use core::cell::UnsafeCell;
    use embassy_net::tcp::TcpSocket;

    static VTABLE: RawWakerVTable = RawWakerVTable::new(
        |_| RawWaker::new(core::ptr::null(), &VTABLE),
        |_| {},
        |_| {},
        |_| {},
    );

    // Create socket DIRECTLY IN A BOX to avoid any moves after creation
    // TcpSocket registers itself with smoltcp's internal state during accept,
    // and moving it afterwards corrupts that state
    crate::threading::disable_preemption();
    let socket_boxed = alloc::boxed::Box::new(TcpSocket::new(stack, rx_buf, tx_buf));
    crate::threading::enable_preemption();

    // Use UnsafeCell to allow mutable access to the boxed socket
    let socket_cell: UnsafeCell<alloc::boxed::Box<TcpSocket<'static>>> = UnsafeCell::new(socket_boxed);

    let mut iterations = 0usize;
    // Longer timeout for accept - we want to wait indefinitely for connections
    // Each iteration is ~1ms, so 10_000_000 is ~2.7 hours
    // The embassy socket timeout (60s) will handle actual network timeouts
    const MAX_ITERATIONS: usize = 10_000_000;

    // Create the accept future using unsafe to get mutable reference to the socket in the box
    // SAFETY: We have exclusive access to socket_cell
    let socket_ref: &mut TcpSocket<'static> = unsafe { &mut **socket_cell.get() };
    
    // No timeout for listener - we want to wait indefinitely for connections
    crate::threading::disable_preemption();
    socket_ref.set_timeout(None);
    crate::threading::enable_preemption();
    
    let mut accept_fut = socket_ref.accept(port);
    let mut accept_fut = unsafe { Pin::new_unchecked(&mut accept_fut) };

    loop {
        // Check for interrupt
        if crate::process::is_current_interrupted() {
            // Drop the future first to release borrow, then abort socket
            drop(accept_fut);
            let mut socket_boxed = unsafe { socket_cell.into_inner() };
            socket_boxed.abort();
            drop(socket_boxed);
            return Err(libc_errno::EINTR);
        }

        let waker = unsafe { Waker::from_raw(RawWaker::new(core::ptr::null(), &VTABLE)) };
        let mut cx = Context::from_waker(&waker);

        // Poll with preemption disabled
        crate::threading::disable_preemption();
        let result = accept_fut.as_mut().poll(&mut cx);
        crate::threading::enable_preemption();

        match result {
            Poll::Ready(Ok(())) => {
                // CRITICAL: Drop the future first to release the mutable borrow!
                drop(accept_fut);
                
                // Get the Box<TcpSocket> - the socket itself doesn't move, only the Box pointer
                let socket_boxed = unsafe { socket_cell.into_inner() };
                
                // Get remote endpoint from the boxed socket (socket stays in same heap location)
                crate::threading::disable_preemption();
                let remote_ep = socket_boxed.remote_endpoint();
                crate::threading::enable_preemption();
                
                let remote_addr = match remote_ep {
                    Some(ep) => socket::SocketAddrV4::from_endpoint(ep)
                        .unwrap_or(socket::SocketAddrV4::new([0,0,0,0], 0)),
                    None => socket::SocketAddrV4::new([0, 0, 0, 0], 0),
                };
                
                // Store pre-boxed socket directly in table (socket never moves from heap)
                let socket_idx = socket::alloc_socket_with_handle_boxed(
                    socket::socket_const::SOCK_STREAM,
                    buffer_slot,
                    socket_boxed,
                    socket::SocketState::Connected { local_addr, remote_addr },
                );
                
                return Ok(socket_idx);
            }
            Poll::Ready(Err(e)) => {
                crate::safe_print!(64, "[block_on_accept] embassy error: {:?}\n", e);
                drop(accept_fut);
                let mut socket_boxed = unsafe { socket_cell.into_inner() };
                socket_boxed.abort();
                drop(socket_boxed);
                return Err(libc_errno::ECONNREFUSED);
            }
            Poll::Pending => {
                iterations += 1;
                if iterations >= MAX_ITERATIONS {
                    // Drop future first to release borrow
                    drop(accept_fut);
                    let mut socket_boxed = unsafe { socket_cell.into_inner() };
                    socket_boxed.abort();
                    drop(socket_boxed);
                    return Err(libc_errno::ETIMEDOUT);
                }
                crate::threading::yield_now();
                for _ in 0..100 {
                    core::hint::spin_loop();
                }
            }
        }
    }
}

/// Block on TCP connect - creates socket and connects to remote endpoint
fn block_on_connect(
    stack: embassy_net::Stack<'static>,
    rx_buf: &'static mut [u8],
    tx_buf: &'static mut [u8],
    endpoint: embassy_net::IpEndpoint,
) -> Result<(embassy_net::tcp::TcpSocket<'static>, Option<embassy_net::IpEndpoint>), i32> {
    use core::cell::UnsafeCell;
    use embassy_net::tcp::TcpSocket;

    static VTABLE: RawWakerVTable = RawWakerVTable::new(
        |_| RawWaker::new(core::ptr::null(), &VTABLE),
        |_| {},
        |_| {},
        |_| {},
    );

    // Create socket with preemption disabled
    crate::threading::disable_preemption();
    let socket = TcpSocket::new(stack, rx_buf, tx_buf);
    crate::threading::enable_preemption();

    let socket_cell = UnsafeCell::new(socket);

    let mut iterations = 0usize;
    const MAX_ITERATIONS: usize = 100_000;

    // Get mutable reference for connect
    let socket_ref = unsafe { &mut *socket_cell.get() };
    
    crate::threading::disable_preemption();
    socket_ref.set_timeout(Some(embassy_time::Duration::from_secs(30)));
    crate::threading::enable_preemption();
    
    let mut connect_fut = socket_ref.connect(endpoint);
    let mut connect_fut = unsafe { Pin::new_unchecked(&mut connect_fut) };

    loop {
        if crate::process::is_current_interrupted() {
            drop(connect_fut);
            let socket = unsafe { socket_cell.into_inner() };
            drop(socket);
            return Err(libc_errno::EINTR);
        }

        let waker = unsafe { Waker::from_raw(RawWaker::new(core::ptr::null(), &VTABLE)) };
        let mut cx = Context::from_waker(&waker);

        crate::threading::disable_preemption();
        let result = connect_fut.as_mut().poll(&mut cx);
        crate::threading::enable_preemption();

        match result {
            Poll::Ready(Ok(())) => {
                // CRITICAL: Drop the future first to release the mutable borrow!
                drop(connect_fut);
                let mut socket = unsafe { socket_cell.into_inner() };
                crate::threading::disable_preemption();
                // Clear the timeout after successful connect - reads may take much longer
                // (e.g., waiting for LLM inference which can take 60+ seconds)
                socket.set_timeout(None);
                let local = socket.local_endpoint();
                crate::threading::enable_preemption();
                return Ok((socket, local));
            }
            Poll::Ready(Err(e)) => {
                crate::safe_print!(64, "[block_on_connect] embassy error: {:?}\n", e);
                // Drop future first to release borrow
                drop(connect_fut);
                let socket = unsafe { socket_cell.into_inner() };
                drop(socket);
                return Err(libc_errno::ECONNREFUSED);
            }
            Poll::Pending => {
                iterations += 1;
                if iterations >= MAX_ITERATIONS {
                    drop(connect_fut);
                    let socket = unsafe { socket_cell.into_inner() };
                    drop(socket);
                    return Err(libc_errno::ETIMEDOUT);
                }
                crate::threading::yield_now();
                for _ in 0..100 {
                    core::hint::spin_loop();
                }
            }
        }
    }
}

// ============================================================================
// Process Management Syscalls
// ============================================================================

/// sys_spawn - Spawn a child process
///
/// # Arguments
/// * `path_ptr` - Pointer to path string
/// * `path_len` - Length of path string  
/// * `args_ptr` - Pointer to null-separated args string (can be 0)
/// * `args_len` - Length of args string
/// * `stdin_ptr` - Pointer to stdin data (can be 0)
/// * `stdin_len` - Length of stdin data
///
/// # Returns
/// On success: child PID in low 32 bits, stdout FD in high 32 bits
/// On failure: negative errno
fn sys_spawn(path_ptr: u64, path_len: usize, args_ptr: u64, args_len: usize, stdin_ptr: u64, stdin_len: usize) -> u64 {
    use alloc::string::String;
    use alloc::vec::Vec;
    use crate::process::{self, FileDescriptor};

    // Read path from user memory
    let path = unsafe {
        let slice = core::slice::from_raw_parts(path_ptr as *const u8, path_len);
        match core::str::from_utf8(slice) {
            Ok(s) => String::from(s),
            Err(_) => return (-libc_errno::EINVAL as i64) as u64,
        }
    };

    // Parse args if provided
    let args: Vec<String> = if args_ptr != 0 && args_len > 0 {
        unsafe {
            let slice = core::slice::from_raw_parts(args_ptr as *const u8, args_len);
            // Args are null-separated
            slice.split(|&b| b == 0)
                .filter(|s| !s.is_empty())
                .filter_map(|s| core::str::from_utf8(s).ok())
                .map(String::from)
                .collect()
        }
    } else {
        Vec::new()
    };
    
    // Read stdin data if provided
    let stdin_data: Option<Vec<u8>> = if stdin_ptr != 0 && stdin_len > 0 {
        unsafe {
            let slice = core::slice::from_raw_parts(stdin_ptr as *const u8, stdin_len);
            Some(slice.to_vec())
        }
    } else {
        None
    };

    // Convert args to slice of &str for spawn
    let args_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let args_opt = if args_refs.is_empty() { None } else { Some(args_refs.as_slice()) };
    
    // Convert stdin to slice reference
    let stdin_opt = stdin_data.as_deref();
    
    // Get parent's cwd to inherit (if parent exists)
    let parent_cwd: Option<String> = process::current_process().map(|p| p.cwd.clone());
    let cwd_opt = parent_cwd.as_deref();

    // Spawn the process with inherited cwd
    let (_thread_id, channel, child_pid) = match process::spawn_process_with_channel_cwd(&path, args_opt, stdin_opt, cwd_opt) {
        Ok(result) => result,
        Err(e) => {
            crate::safe_print!(64, "[sys_spawn] Failed: {}\n", e);
            return (-libc_errno::ENOENT as i64) as u64;
        }
    };

    // Register the channel so parent can read child stdout
    process::register_child_channel(child_pid, channel);

    // Allocate a FD for child stdout in parent's FD table
    let proc = match process::current_process() {
        Some(p) => p,
        None => return (-libc_errno::ESRCH as i64) as u64,
    };

    let stdout_fd = proc.alloc_fd(FileDescriptor::ChildStdout(child_pid));

    // Return PID in low 32 bits, FD in high 32 bits
    let result = (child_pid as u64) | ((stdout_fd as u64) << 32);
    result
}

/// sys_kill - Kill a process by PID
///
/// # Arguments
/// * `pid` - Process ID to kill
///
/// # Returns
/// 0 on success, negative errno on failure
fn sys_kill(pid: u32) -> u64 {
    match crate::process::kill_process(pid) {
        Ok(()) => 0,
        Err(_) => (-libc_errno::ESRCH as i64) as u64,
    }
}

/// sys_waitpid - Wait for a child process
///
/// # Arguments
/// * `pid` - Child PID to wait for (0 = any child)
/// * `status_ptr` - Pointer to store exit status (can be 0)
///
/// # Returns
/// - If child has exited: child PID
/// - If child still running: 0 (non-blocking)
/// - On error: negative errno
fn sys_waitpid(pid: u32, status_ptr: u64) -> u64 {
    use crate::process::{self, Pid};

    // Get the child channel
    let channel = match process::get_child_channel(pid as Pid) {
        Some(ch) => ch,
        None => return (-libc_errno::ECHILD as i64) as u64,
    };

    // Check if child has exited
    if channel.has_exited() {
        let exit_code = channel.exit_code();

        // Store exit status if pointer provided
        if status_ptr != 0 {
            // Linux waitpid status format: exit_code << 8
            let status = (exit_code as u32) << 8;
            unsafe {
                *(status_ptr as *mut u32) = status;
            }
        }

        // Clean up the channel
        process::remove_child_channel(pid as Pid);

        pid as u64
    } else {
        // Child still running, return 0 (WNOHANG behavior)
        0
    }
}

/// sys_getdents64 - Get directory entries
///
/// # Arguments
/// * `fd` - Directory file descriptor
/// * `buf_ptr` - Buffer to store directory entries
/// * `buf_size` - Size of buffer
///
/// # Returns
/// Number of bytes read, 0 at end of directory, negative errno on error
fn sys_getdents64(fd: u32, buf_ptr: u64, buf_size: usize) -> u64 {
    use crate::process::FileDescriptor;

    // Get current process
    let proc = match crate::process::current_process() {
        Some(p) => p,
        None => return (-libc_errno::EBADF as i64) as u64,
    };

    // Get file descriptor entry
    let fd_entry = match proc.get_fd(fd) {
        Some(e) => e,
        None => return (-libc_errno::EBADF as i64) as u64,
    };

    // Must be a directory file
    let (path, position) = match fd_entry {
        FileDescriptor::File(ref file) => (file.path.clone(), file.position),
        _ => return (-libc_errno::ENOTDIR as i64) as u64,
    };

    // List directory
    let entries = match crate::fs::list_dir(&path) {
        Ok(e) => e,
        Err(_) => return (-libc_errno::ENOTDIR as i64) as u64,
    };

    // Skip entries we've already returned (based on position)
    let skip_count = position;
    
    if skip_count >= entries.len() {
        return 0; // No more entries
    }

    // Linux dirent64 structure (simplified)
    #[repr(C)]
    struct Dirent64 {
        d_ino: u64,      // Inode number (fake it)
        d_off: i64,      // Offset to next entry
        d_reclen: u16,   // Length of this record
        d_type: u8,      // File type
        // d_name follows (null-terminated)
    }

    const DT_REG: u8 = 8;  // Regular file
    const DT_DIR: u8 = 4;  // Directory
    
    // Linux dirent64 header size (without struct padding): d_ino(8) + d_off(8) + d_reclen(2) + d_type(1) = 19
    // Note: size_of::<Dirent64>() is 24 due to alignment padding, but Linux expects 19
    const DIRENT64_HEADER_SIZE: usize = 19;

    let mut written = 0usize;
    let mut entries_returned = 0usize;
    let buf = buf_ptr as *mut u8;

    for (i, entry) in entries.iter().skip(skip_count).enumerate() {
        let name_bytes = entry.name.as_bytes();
        let record_len = DIRENT64_HEADER_SIZE + name_bytes.len() + 1;
        // Align to 8 bytes
        let aligned_len = (record_len + 7) & !7;

        if written + aligned_len > buf_size {
            break; // Buffer full
        }

        let d_type = if entry.is_dir { DT_DIR } else { DT_REG };

        unsafe {
            let dirent_ptr = buf.add(written) as *mut Dirent64;
            (*dirent_ptr).d_ino = (skip_count + i + 1) as u64;
            (*dirent_ptr).d_off = (skip_count + i + 2) as i64;
            (*dirent_ptr).d_reclen = aligned_len as u16;
            (*dirent_ptr).d_type = d_type;

            // Copy name after the header (at offset 19, not size_of which includes padding)
            let name_ptr = buf.add(written + DIRENT64_HEADER_SIZE);
            core::ptr::copy_nonoverlapping(name_bytes.as_ptr(), name_ptr, name_bytes.len());
            *name_ptr.add(name_bytes.len()) = 0; // Null terminator
        }

        written += aligned_len;
        entries_returned += 1;
    }

    // Update position in FD table
    if entries_returned > 0 {
        let new_position = skip_count + entries_returned;
        proc.update_fd(fd, |fd_entry| {
            if let FileDescriptor::File(file) = fd_entry {
                file.position = new_position;
            }
        });
    }

    written as u64
}

/// sys_getrandom - Fill a buffer with random bytes from VirtIO RNG
///
/// # Arguments
/// * `buf_ptr` - Pointer to userspace buffer
/// * `len` - Number of bytes to fill
///
/// # Returns
/// Number of bytes written on success, negative errno on failure
fn sys_getrandom(buf_ptr: u64, len: usize) -> u64 {
    use crate::rng;

    // Validate pointer and length
    if buf_ptr == 0 || len == 0 {
        return 0;
    }

    // Limit to reasonable size to prevent abuse
    const MAX_GETRANDOM_SIZE: usize = 256;
    let actual_len = len.min(MAX_GETRANDOM_SIZE);

    // Allocate a temporary buffer in kernel space
    let mut temp_buf = alloc::vec![0u8; actual_len];

    // Fill with random bytes from VirtIO RNG
    if let Err(_) = rng::fill_bytes(&mut temp_buf) {
        return (-libc_errno::EIO as i64) as u64;
    }

    // Copy to userspace
    let buf = buf_ptr as *mut u8;
    unsafe {
        core::ptr::copy_nonoverlapping(temp_buf.as_ptr(), buf, actual_len);
    }

    actual_len as u64
}

/// sys_chdir - Change current working directory
///
/// # Arguments
/// * `path_ptr` - Pointer to path string
/// * `path_len` - Length of path string
///
/// # Returns
/// 0 on success, negative errno on failure
fn sys_chdir(path_ptr: u64, path_len: usize) -> u64 {
    use alloc::string::String;
    use crate::process::{self, ProcessInfo, CWD_DATA_SIZE};

    // Read path from user memory
    let path = unsafe {
        let slice = core::slice::from_raw_parts(path_ptr as *const u8, path_len);
        match core::str::from_utf8(slice) {
            Ok(s) => String::from(s),
            Err(_) => return (-libc_errno::EINVAL as i64) as u64,
        }
    };

    // Verify directory exists
    if crate::fs::list_dir(&path).is_err() {
        return (-libc_errno::ENOENT as i64) as u64;
    }

    // Update process's cwd
    let proc = match process::current_process() {
        Some(p) => p,
        None => return (-libc_errno::ESRCH as i64) as u64,
    };

    // Update the process's cwd field
    proc.set_cwd(&path);

    // Update ProcessInfo page so getcwd() returns the new value
    unsafe {
        let info_ptr = crate::mmu::phys_to_virt(proc.process_info_phys) as *mut ProcessInfo;
        let info = &mut *info_ptr;
        
        let path_bytes = path.as_bytes();
        if path_bytes.len() < CWD_DATA_SIZE {
            info.cwd_data[..path_bytes.len()].copy_from_slice(path_bytes);
            info.cwd_data[path_bytes.len()] = 0;
            info.cwd_len = path_bytes.len() as u32;
        }
    }

    0
}
