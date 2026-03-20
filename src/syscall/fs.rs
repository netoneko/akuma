use super::*;
use akuma_net::socket::{self, libc_errno};
use akuma_exec::mmu::user_access::{copy_from_user_safe, copy_to_user_safe};

const EROFS: u64 = (-30i64) as u64;

fn fs_error_to_errno(e: crate::vfs::FsError) -> u64 {
    use crate::vfs::FsError;
    match e {
        FsError::NotFound => ENOENT,
        FsError::PermissionDenied => EPERM,
        FsError::AlreadyExists => EEXIST,
        FsError::NotADirectory => ENOTDIR,
        FsError::NotAFile => EISDIR,
        FsError::DirectoryNotEmpty => ENOTEMPTY,
        FsError::NoSpace => ENOSPC,
        FsError::ReadOnly => EROFS,
        FsError::InvalidPath => EINVAL,
        _ => EPERM,
    }
}

pub(super) fn resolve_path_at(dirfd: i32, raw_path: &str) -> String {
    if raw_path.starts_with('/') {
        return crate::vfs::canonicalize_path(raw_path);
    }
    let base = if dirfd == -100 {
        if let Some(proc) = akuma_exec::process::current_process() {
            proc.cwd.clone()
        } else {
            String::from("/")
        }
    } else if dirfd >= 0 {
        if let Some(proc) = akuma_exec::process::current_process() {
            if let Some(akuma_exec::process::FileDescriptor::File(f)) = proc.get_fd(dirfd as u32) {
                f.path.clone()
            } else {
                String::from("/")
            }
        } else {
            String::from("/")
        }
    } else {
        String::from("/")
    };
    if raw_path == "." || raw_path.is_empty() {
        base
    } else {
        crate::vfs::resolve_path(&base, raw_path)
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(super) struct IoVec {
    pub(super) iov_base: u64,
    pub(super) iov_len: usize,
}

#[repr(C)] #[derive(Default)] pub struct Stat { pub st_dev: u64, pub st_ino: u64, pub st_mode: u32, pub st_nlink: u32, pub st_uid: u32, pub st_gid: u32, pub st_rdev: u64, pub __pad1: u64, pub st_size: i64, pub st_blksize: i32, pub __pad2: i32, pub st_blocks: i64, pub st_atime: i64, pub st_atime_nsec: i64, pub st_mtime: i64, pub st_mtime_nsec: i64, pub st_ctime: i64, pub st_ctime_nsec: i64, pub __unused: [i32; 2] }

const fn makedev(major: u64, minor: u64) -> u64 {
    (major << 8) | minor
}

pub(super) fn sys_read(fd_num: u64, buf_ptr: u64, count: usize) -> u64 {
    if !validate_user_ptr(buf_ptr, count) { return EFAULT; }
    let proc = match akuma_exec::process::current_process() { Some(p) => p, None => return !0u64 };
    let fd = match proc.get_fd(fd_num as u32) { Some(e) => e, None => return !0u64 };
    
    if crate::config::SYSCALL_DEBUG_INFO_ENABLED && fd_num == 0 {
        crate::safe_print!(128, "[syscall] read(stdin, count={})\n", count);
    }

    match fd {
        akuma_exec::process::FileDescriptor::Stdin => {
            let ch = match akuma_exec::process::current_channel() {
                Some(c) => c,
                None => {
                    let mut temp = alloc::vec![0u8; count];
                    let n = proc.read_stdin(&mut temp);
                    if n > 0 { 
                        if unsafe { copy_to_user_safe(buf_ptr as *mut u8, temp.as_ptr(), n).is_err() } {
                            return EFAULT;
                        }
                    }
                    if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
                        crate::safe_print!(128, "[syscall] read(stdin) fallback returned {}\n", n);
                    }
                    return n as u64;
                }
            };

            let mut kernel_buf = alloc::vec![0u8; count];
            
            loop {
                let is_pipe = ch.is_stdin_closed();

                if !is_pipe {
                    let term_state_lock = akuma_exec::process::current_terminal_state();
                    if let Some(ref ts_lock) = term_state_lock {
                        let mut ts = ts_lock.lock();
                        if ts.is_canonical() && !ts.canon_ready.is_empty() {
                            let ready = ts.drain_canon_ready(count);
                            let to_read = ready.len();
                            kernel_buf[..to_read].copy_from_slice(&ready);
                            drop(ts);
                            if unsafe { copy_to_user_safe(buf_ptr as *mut u8, kernel_buf.as_ptr(), to_read).is_err() } {
                                return EFAULT;
                            }
                            if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
                                crate::safe_print!(128, "[syscall] read(stdin) returned {} bytes from canon_ready\n", to_read);
                            }
                            return to_read as u64;
                        }
                    }
                }

                let n = ch.read_stdin(&mut kernel_buf);
                if n > 0 {
                    if !is_pipe {
                        let term_state_lock = akuma_exec::process::current_terminal_state();
                        if let Some(ref ts_lock) = term_state_lock {
                            let mut ts = ts_lock.lock();

                            ts.map_cr_to_nl(&mut kernel_buf[..n]);

                            if ts.is_canonical() {
                                let result = ts.process_canon_input(&kernel_buf[..n]);
                                if !result.echo.is_empty() {
                                    ch.write(&result.echo);
                                }
                                if result.eof {
                                    drop(ts);
                                    return 0;
                                }

                                if !ts.canon_ready.is_empty() {
                                    let ready = ts.drain_canon_ready(count);
                                    let to_read = ready.len();
                                    kernel_buf[..to_read].copy_from_slice(&ready);
                                    drop(ts);
                                    if unsafe { copy_to_user_safe(buf_ptr as *mut u8, kernel_buf.as_ptr(), to_read).is_err() } {
                                        return EFAULT;
                                    }
                                    if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
                                        crate::safe_print!(128, "[syscall] read(stdin) returned {} bytes (canonical)\n", to_read);
                                    }
                                    return to_read as u64;
                                }
                                continue;
                            } else {
                                if let Some(echo_buf) = ts.echo_noncanon(&kernel_buf[..n]) {
                                    if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
                                        crate::safe_print!(128, "[syscall] read: echoing {} bytes\n", echo_buf.len());
                                    }
                                    ch.write(&echo_buf);
                                }
                            }
                        }
                    }

                    if unsafe { copy_to_user_safe(buf_ptr as *mut u8, kernel_buf.as_ptr(), n).is_err() } {
                        return EFAULT;
                    }
                    if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
                        let mut snippet = [0u8; 32];
                        let sn_len = n.min(32);
                        snippet[..sn_len].copy_from_slice(&kernel_buf[..sn_len]);
                        for byte in &mut snippet[..sn_len] {
                            if *byte < 32 || *byte > 126 { *byte = b'.'; }
                        }
                        let snippet_str = core::str::from_utf8(&snippet[..sn_len]).unwrap_or("...");
                        crate::safe_print!(128, "[syscall] read(stdin) returned {} bytes \"{}\"\n", n, snippet_str);
                    }
                    return n as u64;
                }

                if ch.is_stdin_closed() {
                    if !is_pipe {
                        let term_state_lock = akuma_exec::process::current_terminal_state();
                        if let Some(ref ts_lock) = term_state_lock {
                            let mut ts = ts_lock.lock();
                            if ts.is_canonical() && !ts.canon_buffer.is_empty() {
                                ts.flush_canon_buffer();
                                let ready = ts.drain_canon_ready(count);
                                let to_read = ready.len();
                                kernel_buf[..to_read].copy_from_slice(&ready);
                                drop(ts);
                                if unsafe { copy_to_user_safe(buf_ptr as *mut u8, kernel_buf.as_ptr(), to_read).is_err() } {
                                    return EFAULT;
                                }
                                return to_read as u64;
                            }
                        }
                    }
                    if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
                        crate::safe_print!(128, "[syscall] read(stdin) returned 0 (EOF)\n");
                    }
                    return 0;
                }

                if akuma_exec::process::is_current_interrupted() {
                    return EINTR;
                }

                let term_state_lock = match akuma_exec::process::current_terminal_state() {
                    Some(state) => state,
                    None => {
                        if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
                            crate::safe_print!(128, "[syscall] read(stdin) no terminal state, EOF\n");
                        }
                        return 0;
                    }
                };

                {
                    akuma_exec::threading::disable_preemption();
                    let term_state = term_state_lock.lock();
                    let thread_id = akuma_exec::threading::current_thread_id();
                    term_state.set_input_waker(akuma_exec::threading::get_waker_for_thread(thread_id));
                    akuma_exec::threading::enable_preemption();
                }

                akuma_exec::threading::schedule_blocking(u64::MAX);

                {
                    akuma_exec::threading::disable_preemption();
                    let term_state = term_state_lock.lock();
                    term_state.input_waker.lock().take();
                    akuma_exec::threading::enable_preemption();
                }
            }
        }
        akuma_exec::process::FileDescriptor::File(ref f) => {
            let limit = 64 * 1024;
            let to_read = count.min(limit);
            let mut temp = alloc::vec![0u8; to_read];

            match crate::fs::read_at(&f.path, f.position, &mut temp) {
                Ok(n) => {
                    if n > 0 {
                        if unsafe { copy_to_user_safe(buf_ptr as *mut u8, temp.as_ptr(), n).is_err() } {
                            return EFAULT;
                        }
                        proc.update_fd(fd_num as u32, |entry| if let akuma_exec::process::FileDescriptor::File(file) = entry { file.position += n; });
                    }
                    if crate::config::SYSCALL_DEBUG_IO_ENABLED {
                        crate::safe_print!(256, "[syscall] read(fd={}, file={}, pos={}, req={}) = {}\n", fd_num, &f.path, f.position, to_read, n);
                    }
                    n as u64
                }
                Err(_) => !0u64
            }
        }
        akuma_exec::process::FileDescriptor::Socket(idx) => {
            let limit = 64 * 1024;
            let to_read = count.min(limit);
            let mut temp = alloc::vec![0u8; to_read];
            let nonblock = super::net::fd_is_nonblock(fd_num as u32);
            let result = if socket::is_udp_socket(idx) {
                socket::socket_recv_udp(idx, &mut temp, nonblock).map(|(n, _)| n)
            } else {
                socket::socket_recv(idx, &mut temp, nonblock)
            };
            if crate::config::SYSCALL_DEBUG_NET_ENABLED {
                match &result {
                    Ok(n) => crate::tprint!(128, "[sock] read fd={} req={} got={}\n", fd_num, count, n),
                    Err(e) if *e == akuma_net::socket::libc_errno::EAGAIN => {
                        crate::tprint!(64, "[sock] read fd={} EAGAIN (drained)\n", fd_num);
                    }
                    Err(e) => crate::tprint!(128, "[sock] read fd={} err={}\n", fd_num, *e as i64),
                }
            }
            match result {
                Ok(n) => {
                    if n > 0 {
                        if unsafe { copy_to_user_safe(buf_ptr as *mut u8, temp.as_ptr(), n).is_err() } {
                            return EFAULT;
                        }
                    }
                    // Reset EPOLLET edge after every successful TCP read. Go (and other callers
                    // using read() rather than recvfrom/recvmsg) do not always drain to EAGAIN
                    // before going back to epoll. Without this reset the EPOLLET "last_ready"
                    // stays set to EPOLLIN so the next poll sees new_bits=0 and fires no event,
                    // leaving the socket unread even though more data is buffered.
                    if !socket::is_udp_socket(idx) {
                        super::poll::epoll_on_fd_drained(fd_num as u32);
                    }
                    n as u64
                }
                Err(e) => {
                    if e == akuma_net::socket::libc_errno::EAGAIN {
                        // Socket was drained — reset EPOLLET edge so next data arrival fires EPOLLIN.
                        super::poll::epoll_on_fd_drained(fd_num as u32);
                    }
                    (-(e as i64)) as u64
                }
            }
        }
        akuma_exec::process::FileDescriptor::ChildStdout(child_pid) => {
            if let Some(ch) = akuma_exec::process::get_child_channel(child_pid) {
                let mut temp = alloc::vec![0u8; count];
                let n = ch.read(&mut temp);
                if n > 0 { 
                    if unsafe { copy_to_user_safe(buf_ptr as *mut u8, temp.as_ptr(), n).is_err() } {
                        return EFAULT;
                    }
                }
                n as u64
            } else {
                !0u64
            }
        }
        akuma_exec::process::FileDescriptor::PipeRead(pipe_id) => {
            let mut temp = alloc::vec![0u8; count];
            loop {
                let (n, eof) = super::pipe::pipe_read(pipe_id, &mut temp);
                if n > 0 {
                    if unsafe { copy_to_user_safe(buf_ptr as *mut u8, temp.as_ptr(), n).is_err() } {
                        return EFAULT;
                    }
                    return n as u64;
                }
                if eof {
                    return 0;
                }
                if akuma_exec::process::is_current_interrupted() {
                    return EINTR;
                }
                let tid = akuma_exec::threading::current_thread_id();
                if !super::pipe::pipe_check_set_reader(pipe_id, tid) {
                    akuma_exec::threading::schedule_blocking(u64::MAX);
                }
            }
        }
        akuma_exec::process::FileDescriptor::EventFd(efd_id) => {
            if count < 8 { return EINVAL; }
            let nonblock = super::eventfd::eventfd_is_nonblock(efd_id) || super::net::fd_is_nonblock(fd_num as u32);
            loop {
                match super::eventfd::eventfd_read(efd_id) {
                    Ok(val) => {
                        let mut temp = [0u8; 8];
                        unsafe { core::ptr::write(temp.as_mut_ptr() as *mut u64, val); }
                        if unsafe { copy_to_user_safe(buf_ptr as *mut u8, temp.as_ptr(), 8).is_err() } {
                            return EFAULT;
                        }
                        return 8;
                    }
                    Err(_) => {
                        if nonblock { return EAGAIN; }
                        if akuma_exec::process::is_current_interrupted() { return EINTR; }
                        let tid = akuma_exec::threading::current_thread_id();
                        super::eventfd::eventfd_set_reader_thread(efd_id, tid);
                        akuma_exec::threading::schedule_blocking(u64::MAX);
                    }
                }
            }
        }
        akuma_exec::process::FileDescriptor::DevNull => 0,
        akuma_exec::process::FileDescriptor::DevUrandom => {
            let mut temp = alloc::vec![0u8; count];
            if crate::rng::fill_bytes(&mut temp).is_ok() {
                if unsafe { copy_to_user_safe(buf_ptr as *mut u8, temp.as_ptr(), count).is_err() } {
                    return EFAULT;
                }
                count as u64
            } else {
                !0u64
            }
        }
        akuma_exec::process::FileDescriptor::TimerFd(timer_id) => {
            let result = super::timerfd::timerfd_read(timer_id);
            if result == EAGAIN { return EAGAIN; }
            if count >= 8 && validate_user_ptr(buf_ptr, 8) {
                let mut temp = [0u8; 8];
                unsafe { core::ptr::write(temp.as_mut_ptr() as *mut u64, result); }
                if unsafe { copy_to_user_safe(buf_ptr as *mut u8, temp.as_ptr(), 8).is_err() } {
                    return EFAULT;
                }
                8
            } else { EINVAL }
        }
        akuma_exec::process::FileDescriptor::EpollFd(_) => EINVAL,
        _ => !0u64
    }
}

pub(super) fn sys_pread64(fd_num: u32, buf_ptr: u64, count: usize, offset: i64) -> u64 {
    if offset < 0 { return EINVAL; }
    if !validate_user_ptr(buf_ptr, count) { return EFAULT; }
    let proc = match akuma_exec::process::current_process() { Some(p) => p, None => return EBADF };
    let fd = match proc.get_fd(fd_num) { Some(e) => e, None => return EBADF };

    match fd {
        akuma_exec::process::FileDescriptor::File(ref f) => {
            let limit = 64 * 1024;
            let to_read = count.min(limit);
            let mut temp = alloc::vec![0u8; to_read];
            match crate::fs::read_at(&f.path, offset as usize, &mut temp) {
                Ok(n) => {
                    if n > 0 {
                        if unsafe { copy_to_user_safe(buf_ptr as *mut u8, temp.as_ptr(), n).is_err() } {
                            return EFAULT;
                        }
                    }
                    if crate::config::SYSCALL_DEBUG_IO_ENABLED {
                        crate::safe_print!(256, "[syscall] pread64(fd={}, file={}, off={}, req={}) = {}\n", fd_num, &f.path, offset, to_read, n);
                    }
                    n as u64
                }
                Err(_) => !0u64
            }
        }
        akuma_exec::process::FileDescriptor::DevNull => 0,
        akuma_exec::process::FileDescriptor::DevUrandom => {
            let mut temp = alloc::vec![0u8; count];
            if crate::rng::fill_bytes(&mut temp).is_ok() {
                if unsafe { copy_to_user_safe(buf_ptr as *mut u8, temp.as_ptr(), count).is_err() } {
                    return EFAULT;
                }
                count as u64
            } else {
                !0u64
            }
        }
        akuma_exec::process::FileDescriptor::TimerFd(_) => EAGAIN,
        akuma_exec::process::FileDescriptor::EpollFd(_) => EINVAL,
        _ => EBADF
    }
}

pub(super) fn sys_pwrite64(fd_num: u32, buf_ptr: u64, count: usize, offset: i64) -> u64 {
    if offset < 0 { return EINVAL; }
    if !validate_user_ptr(buf_ptr, count) { return EFAULT; }
    let proc = match akuma_exec::process::current_process() { Some(p) => p, None => return EBADF };
    let fd = match proc.get_fd(fd_num) { Some(e) => e, None => return EBADF };

    match fd {
        akuma_exec::process::FileDescriptor::File(ref f) => {
            // Allocate kernel buffer and copy safely
            let mut buf = alloc::vec![0u8; count];
            if unsafe { copy_from_user_safe(buf.as_mut_ptr(), buf_ptr as *const u8, count).is_err() } {
                return EFAULT;
            }
            match crate::fs::write_at(&f.path, offset as usize, &buf) {
                Ok(n) => n as u64,
                Err(_) => !0u64
            }
        }
        akuma_exec::process::FileDescriptor::DevNull | akuma_exec::process::FileDescriptor::DevUrandom => count as u64,
        _ => EBADF
    }
}

pub(super) fn sys_write(fd_num: u64, buf_ptr: u64, count: usize) -> u64 {
    if !validate_user_ptr(buf_ptr, count) { return EFAULT; }
    let proc = match akuma_exec::process::current_process() { Some(p) => p, None => return !0u64 };
    let fd = match proc.get_fd(fd_num as u32) { Some(e) => e, None => return !0u64 };

    // For File descriptors, capture the initial position now (before the loop).
    // The `fd` variable is a clone — proc.update_fd() updates the real fd table but
    // `fd` itself never reflects those updates. Track write_pos independently so
    // multi-chunk writes advance the offset correctly instead of overwriting offset 0.
    let mut write_pos = if let akuma_exec::process::FileDescriptor::File(ref f) = fd {
        f.position
    } else {
        0
    };

    let chunk_size = count.min(64 * 1024);
    let mut kernel_buf = alloc::vec![0u8; chunk_size];
    let mut total_written = 0;
    
    while total_written < count {
        let remaining = count - total_written;
        let this_chunk = remaining.min(chunk_size);
        
        let user_ptr = (buf_ptr as usize + total_written) as *const u8;
        if unsafe { copy_from_user_safe(kernel_buf.as_mut_ptr(), user_ptr, this_chunk).is_err() } {
            if total_written > 0 { return total_written as u64; }
            return EFAULT;
        }
        
        let buf_slice = &kernel_buf[..this_chunk];
        
        let written = match fd {
            akuma_exec::process::FileDescriptor::Stdout | akuma_exec::process::FileDescriptor::Stderr => {
                if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
                    if total_written == 0 {
                      crate::safe_print!(96, "[OUT] pid={} fd={} len={}\n", proc.pid, fd_num, count);
                    } else {
                    let display_len = this_chunk.min(64);
                    let mut snippet = [0u8; 64];
                    let n = display_len.min(snippet.len());
                    snippet[..n].copy_from_slice(&buf_slice[..n]);
                    for byte in &mut snippet[..n] {
                        if *byte < 32 || *byte > 126 { *byte = b'.'; }
                    }
                    let snippet_str = core::str::from_utf8(&snippet[..n]).unwrap_or("...");
                    crate::tprint!(192, "[OUT] pid={} fd={} len={} \"{}\"\n", proc.pid, fd_num, count, snippet_str);
                    }
                }

                if let Some(ch) = akuma_exec::process::current_channel() {
                    if ch.is_stdin_closed() {
                        ch.write(buf_slice);
                    } else {
                        let term_state_opt = akuma_exec::process::current_terminal_state();
                        if let Some(ts_lock) = term_state_opt {
                            let translated = ts_lock.lock().translate_output(buf_slice);
                            ch.write(&translated);
                        } else {
                            ch.write(buf_slice);
                        }
                    }
                }
                
                if crate::config::STDOUT_TO_KERNEL_LOG_COPY_ENABLED {
                    proc.write_stdout(buf_slice);
                }
                
                this_chunk as u64
            }
            akuma_exec::process::FileDescriptor::File(ref f) => {
                match crate::fs::write_at(&f.path, write_pos, buf_slice) {
                    Ok(n) => {
                        write_pos += n;
                        proc.update_fd(fd_num as u32, |entry| if let akuma_exec::process::FileDescriptor::File(file) = entry { file.position += n; });
                        n as u64
                    }
                    Err(_) => {
                        if total_written > 0 { return total_written as u64; }
                        return !0u64;
                    }
                }
            }
            akuma_exec::process::FileDescriptor::Socket(idx) => {
                let nonblock = super::net::fd_is_nonblock(fd_num as u32);
                let result = if socket::is_udp_socket(idx) {
                    match socket::udp_default_peer(idx) {
                        Some(peer) => socket::socket_send_udp(idx, buf_slice, peer),
                        None => Err(libc_errno::EDESTADDRREQ),
                    }
                } else {
                    socket::socket_send(idx, buf_slice, nonblock)
                };
                
                if crate::config::SYSCALL_DEBUG_NET_ENABLED && total_written == 0 {
                    match &result {
                        Ok(n) => crate::tprint!(96, "[TCP] write fd={} len={} sent={}\n", fd_num, count, n),
                        Err(e) => crate::tprint!(96, "[TCP] write fd={} len={} err={}\n", fd_num, count, *e as i64),
                    }
                }
                
                match result {
                    Ok(n) => n as u64,
                    Err(e) => {
                        if total_written > 0 { return total_written as u64; }
                        return (-(e as i64)) as u64;
                    }
                }
            }
            akuma_exec::process::FileDescriptor::PipeWrite(pipe_id) => {
                match super::pipe::pipe_write(pipe_id, buf_slice) {
                    Ok(n) => n as u64,
                    Err(e) => {
                        crate::safe_print!(128, "[syscall] write: PipeWrite fd={} pipe_id={} EPIPE ({} bytes)\n", fd_num, pipe_id, buf_slice.len());
                        if total_written > 0 { return total_written as u64; }
                        return (-(e as i64)) as u64;
                    }
                }
            }
            akuma_exec::process::FileDescriptor::EventFd(efd_id) => {
                if this_chunk < 8 { return EINVAL; } // Should enforce 8 byte writes
                let val = unsafe { core::ptr::read(buf_slice.as_ptr() as *const u64) };
                if val == u64::MAX { return EINVAL; }
                if crate::config::SYSCALL_DEBUG_NET_ENABLED {
                    crate::tprint!(96, "[eventfd] write via fd={} id={} val={}\n", fd_num, efd_id, val);
                }
                match super::eventfd::eventfd_write(efd_id, val) {
                    Ok(()) => 8,
                    Err(e) => (-(e as i64)) as u64,
                }
            }
            akuma_exec::process::FileDescriptor::DevNull | akuma_exec::process::FileDescriptor::DevUrandom => this_chunk as u64,
            _ => !0u64
        };
        
        // If write failed or returned error code (large positive u64)
        if (written as i64) < 0 {
            if total_written > 0 { return total_written as u64; }
            return written;
        }
        
        let written_usize = written as usize;
        total_written += written_usize;
        
        // If partial write, stop (short write)
        if written_usize < this_chunk {
            break;
        }
        
        // Special case: some FDs don't support chunking or offsets (like EventFd)
        // If we wrote something, checking FDs type to break might be complex.
        // Assuming file-like behavior.
    }
    
    total_written as u64
}

pub(super) fn sys_readv(fd_num: u64, iov_ptr: u64, iov_cnt: usize) -> u64 {
    let iov_size = iov_cnt * core::mem::size_of::<IoVec>();
    if !validate_user_ptr(iov_ptr, iov_size) { return EFAULT; }
    
    let mut kernel_iovs = alloc::vec![IoVec { iov_base: 0, iov_len: 0 }; iov_cnt];
    if unsafe { copy_from_user_safe(kernel_iovs.as_mut_ptr() as *mut u8, iov_ptr as *const u8, iov_size).is_err() } {
        return EFAULT;
    }
    
    let mut total_read: u64 = 0;
    for i in 0..iov_cnt {
        let iov = &kernel_iovs[i];
        if iov.iov_len == 0 { continue; }
        let n = sys_read(fd_num, iov.iov_base, iov.iov_len);
        if (n as i64) < 0 {
            if total_read == 0 { return n; }
            break;
        }
        total_read += n;
        if (n as usize) < iov.iov_len { break; }
    }
    if crate::config::SYSCALL_DEBUG_IO_ENABLED {
        crate::safe_print!(128, "[syscall] readv(fd={}, cnt={}) = {}\n", fd_num, iov_cnt, total_read);
    }
    total_read
}

pub(super) fn sys_writev(fd_num: u64, iov_ptr: u64, iov_cnt: usize) -> u64 {
    let iov_size = iov_cnt * core::mem::size_of::<IoVec>();
    if !validate_user_ptr(iov_ptr, iov_size) { return EFAULT; }
    
    let mut kernel_iovs = alloc::vec![IoVec { iov_base: 0, iov_len: 0 }; iov_cnt];
    if unsafe { copy_from_user_safe(kernel_iovs.as_mut_ptr() as *mut u8, iov_ptr as *const u8, iov_size).is_err() } {
        return EFAULT;
    }
    
    let mut total_written: u64 = 0;
    for i in 0..iov_cnt {
        let iov = &kernel_iovs[i];
        let written = sys_write(fd_num, iov.iov_base, iov.iov_len);
        if (written as i64) < 0 {
            if total_written == 0 { return written; }
            break;
        }
        total_written += written;
    }
    total_written
}

pub(super) fn sys_fstatfs(fd: u32, buf_ptr: u64) -> u64 {
    if !validate_user_ptr(buf_ptr, 120) { return EFAULT; }
    if let Some(proc) = akuma_exec::process::current_process() {
        if proc.get_fd(fd).is_none() { return EBADF; }
    } else { return ENOSYS; }
    #[repr(C)]
    struct Statfs {
        f_type: i64,
        f_bsize: i64,
        f_blocks: i64,
        f_bfree: i64,
        f_bavail: i64,
        f_files: i64,
        f_ffree: i64,
        f_fsid: [i32; 2],
        f_namelen: i64,
        f_frsize: i64,
        f_flags: i64,
        f_spare: [i64; 4],
    }
    let st = Statfs {
        f_type: 0xEF53,
        f_bsize: 4096,
        f_blocks: 65536,
        f_bfree: 32768,
        f_bavail: 32768,
        f_files: 16384,
        f_ffree: 8192,
        f_fsid: [0, 0],
        f_namelen: 255,
        f_frsize: 4096,
        f_flags: 0,
        f_spare: [0; 4],
    };
    if unsafe { copy_to_user_safe(buf_ptr as *mut u8, &st as *const Statfs as *const u8, core::mem::size_of::<Statfs>()).is_err() } {
        return EFAULT;
    }
    0
}

pub(super) fn sys_dup(oldfd: u32) -> u64 {
    let proc = match akuma_exec::process::current_process() { Some(p) => p, None => return ENOSYS };
    let entry = match proc.get_fd(oldfd) {
        Some(e) => e,
        None => return EBADF,
    };
    match &entry {
        akuma_exec::process::FileDescriptor::PipeWrite(id) => super::pipe::pipe_clone_ref(*id, true),
        akuma_exec::process::FileDescriptor::PipeRead(id) => super::pipe::pipe_clone_ref(*id, false),
        _ => {}
    }
    let newfd = proc.alloc_fd(entry);
    proc.clear_cloexec(newfd);
    if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
        crate::safe_print!(128, "[syscall] dup(oldfd={}) = {}\n", oldfd, newfd);
    }
    newfd as u64
}

pub(super) fn sys_dup3(oldfd: u32, newfd: u32, flags: u32) -> u64 {
    if oldfd == newfd { return EINVAL; }
    let proc = match akuma_exec::process::current_process() { Some(p) => p, None => return ENOSYS };
    let entry = match proc.get_fd(oldfd) {
        Some(e) => e,
        None => return EBADF,
    };

    if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
        let pid = proc.pid;
        crate::safe_print!(128, "[syscall] dup3(oldfd={}, newfd={}, flags=0x{:x}) PID {}\n", oldfd, newfd, flags, pid);
    }

    // Increment refcount for the new entry BEFORE atomically swapping it in.
    // This must happen before swap_fd so the pipe isn't prematurely destroyed
    // if another thread closes oldfd between these two steps.
    match &entry {
        akuma_exec::process::FileDescriptor::PipeWrite(id) => super::pipe::pipe_clone_ref(*id, true),
        akuma_exec::process::FileDescriptor::PipeRead(id) => super::pipe::pipe_clone_ref(*id, false),
        _ => {}
    }

    // Atomically replace newfd and retrieve the old entry in one operation.
    // This prevents a TOCTOU race on shared fd tables (CLONE_FILES goroutines).
    let old_entry = proc.swap_fd(newfd, entry);

    if flags & akuma_exec::process::open_flags::O_CLOEXEC != 0 {
        proc.set_cloexec(newfd);
    } else {
        proc.clear_cloexec(newfd);
    }

    // Close the old entry AFTER inserting the new one.
    if let Some(old) = old_entry {
        match old {
            akuma_exec::process::FileDescriptor::PipeWrite(id) => super::pipe::pipe_close_write(id),
            akuma_exec::process::FileDescriptor::PipeRead(id) => super::pipe::pipe_close_read(id),
            akuma_exec::process::FileDescriptor::Socket(idx) => { akuma_net::socket::remove_socket(idx); }
            akuma_exec::process::FileDescriptor::EventFd(efd_id) => {
                super::eventfd::eventfd_close(efd_id);
            }
            _ => {}
        }
    }

    newfd as u64
}

pub(super) fn sys_openat(dirfd: i32, path_ptr: u64, flags: u32, mode: u32) -> u64 {
    let raw_path = match copy_from_user_str(path_ptr, 1024) {
        Ok(p) => p,
        Err(e) => return e,
    };

    if crate::config::SYSCALL_DEBUG_IO_ENABLED {
        crate::tprint!(128, "[syscall] openat(dirfd={}, path={:?}, flags=0x{:x}, mode=0x{:x})\n", dirfd, raw_path, flags, mode);
    }

    let path = if raw_path.starts_with('/') {
        crate::vfs::canonicalize_path(&raw_path)
    } else {
        let base = if dirfd == -100 {
            if let Some(proc) = akuma_exec::process::current_process() {
                proc.cwd.clone()
            } else {
                String::from("/")
            }
        } else if dirfd >= 0 {
            if let Some(proc) = akuma_exec::process::current_process() {
                if let Some(akuma_exec::process::FileDescriptor::File(f)) = proc.get_fd(dirfd as u32) {
                    f.path.clone()
                } else {
                    if crate::config::SYSCALL_DEBUG_IO_ENABLED {
                        crate::safe_print!(128, "[syscall] openat: bad dirfd={}\n", dirfd);
                    }
                    return EBADF;
                }
            } else {
                return EBADF;
            }
        } else {
            String::from("/")
        };
        if raw_path == "." || raw_path.is_empty() {
            base
        } else {
            crate::vfs::resolve_path(&base, &raw_path)
        }
    };

    let path = crate::vfs::resolve_symlinks(&path);

    if path == "/dev/null" {
        if let Some(proc) = akuma_exec::process::current_process() {
            let fd = proc.alloc_fd(akuma_exec::process::FileDescriptor::DevNull);
            if flags & akuma_exec::process::open_flags::O_CLOEXEC != 0 {
                proc.set_cloexec(fd);
            }
            if crate::config::SYSCALL_DEBUG_IO_ENABLED {
                crate::safe_print!(256, "[syscall] openat(/dev/null) = fd {} flags=0x{:x}\n", fd, flags);
            }
            return fd as u64;
        }
        return !0u64;
    }

    if path == "/dev/urandom" || path == "/dev/random" {
        if let Some(proc) = akuma_exec::process::current_process() {
            let fd = proc.alloc_fd(akuma_exec::process::FileDescriptor::DevUrandom);
            if flags & akuma_exec::process::open_flags::O_CLOEXEC != 0 {
                proc.set_cloexec(fd);
            }
            if crate::config::SYSCALL_DEBUG_IO_ENABLED {
                crate::safe_print!(256, "[syscall] openat({}) = fd {} flags=0x{:x}\n", &path, fd, flags);
            }
            return fd as u64;
        }
        return !0u64;
    }

    let path = if path == "/proc/self/exe" {
        if let Some(proc) = akuma_exec::process::current_process() {
            proc.name.clone()
        } else {
            return ENOENT;
        }
    } else {
        path
    };

    if !crate::fs::exists(&path) {
        let is_creat = flags & akuma_exec::process::open_flags::O_CREAT != 0;
        if !is_creat {
            if crate::config::SYSCALL_DEBUG_IO_ENABLED {
                crate::safe_print!(256, "[syscall] openat({}) ENOENT flags=0x{:x}\n", &path, flags);
            }
            return ENOENT;
        }

        let (parent_raw, _) = crate::vfs::split_path(&path);
        if !parent_raw.is_empty() {
            let parent_path = if parent_raw.starts_with('/') {
                String::from(parent_raw)
            } else {
                format!("/{}", parent_raw)
            };
            if parent_path != "/" && !crate::fs::exists(&parent_path) {
                if crate::config::SYSCALL_DEBUG_IO_ENABLED {
                    crate::safe_print!(256, "[syscall] openat({}) parent {} not found flags=0x{:x}\n", &path, &parent_path, flags);
                }
                return ENOENT;
            }
        }
    }

    if let Some(proc) = akuma_exec::process::current_process() {
        let file_existed = crate::fs::exists(&path);
        if !file_existed && (flags & akuma_exec::process::open_flags::O_CREAT != 0) {
            let _ = crate::fs::write_file(&path, &[]);
            if mode & 0o7777 != 0 {
                let _ = crate::vfs::chmod(&path, mode & 0o7777);
            }
        } else if file_existed && (flags & akuma_exec::process::open_flags::O_TRUNC != 0) {
            let _ = crate::fs::write_file(&path, &[]);
        }
        let fd = proc.alloc_fd(akuma_exec::process::FileDescriptor::File(akuma_exec::process::KernelFile::new(path.clone(), flags)));
        if flags & akuma_exec::process::open_flags::O_CLOEXEC != 0 {
            proc.set_cloexec(fd);
        }
        if crate::config::SYSCALL_DEBUG_IO_ENABLED {
            crate::safe_print!(256, "[syscall] openat({}) = fd {} flags=0x{:x}\n", &path, fd, flags);
        }
        fd as u64
    } else { !0u64 }
}

pub(crate) fn sys_close(fd: u32) -> u64 {
    if let Some(proc) = akuma_exec::process::current_process() {
        if let Some(entry) = proc.remove_fd(fd) {
            proc.clear_cloexec(fd);
            match entry {
                akuma_exec::process::FileDescriptor::Socket(idx) => { akuma_net::socket::remove_socket(idx); }
                akuma_exec::process::FileDescriptor::ChildStdout(child_pid) => {
                    akuma_exec::process::remove_child_channel(child_pid);
                }
                akuma_exec::process::FileDescriptor::PipeWrite(pipe_id) => {
                    super::pipe::pipe_close_write(pipe_id);
                }
                akuma_exec::process::FileDescriptor::PipeRead(pipe_id) => {
                    super::pipe::pipe_close_read(pipe_id);
                }
                akuma_exec::process::FileDescriptor::EventFd(efd_id) => {
                    super::eventfd::eventfd_close(efd_id);
                }
                akuma_exec::process::FileDescriptor::EpollFd(epoll_id) => {
                    super::poll::epoll_destroy(epoll_id);
                }
                akuma_exec::process::FileDescriptor::PidFd(pidfd_id) => {
                    super::pidfd::pidfd_close(pidfd_id);
                }
                _ => {}
            }
            proc.clear_nonblock(fd);
            0
        } else { !0u64 }
    } else { !0u64 }
}

pub(crate) fn sys_close_range(first: u32, last: u32, flags: u32) -> u64 {
    const CLOSE_RANGE_CLOEXEC: u32 = 4;
    let proc = match akuma_exec::process::current_process() {
        Some(p) => p,
        None => return EBADF,
    };

    let fds: Vec<u32> = crate::irq::with_irqs_disabled(|| {
        proc.fds.table.lock().range(first..=last).map(|(&fd, _)| fd).collect()
    });

    for fd in fds {
        if flags & CLOSE_RANGE_CLOEXEC != 0 {
            proc.set_cloexec(fd);
        } else {
            if let Some(entry) = proc.remove_fd(fd) {
                proc.clear_cloexec(fd);
                match entry {
                    akuma_exec::process::FileDescriptor::Socket(idx) => { akuma_net::socket::remove_socket(idx); }
                    akuma_exec::process::FileDescriptor::ChildStdout(child_pid) => {
                        akuma_exec::process::remove_child_channel(child_pid);
                    }
                    akuma_exec::process::FileDescriptor::PipeWrite(pipe_id) => {
                        super::pipe::pipe_close_write(pipe_id);
                    }
                    akuma_exec::process::FileDescriptor::PipeRead(pipe_id) => {
                        super::pipe::pipe_close_read(pipe_id);
                    }
                    akuma_exec::process::FileDescriptor::EventFd(efd_id) => {
                        super::eventfd::eventfd_close(efd_id);
                    }
                    _ => {}
                }
                proc.clear_nonblock(fd);
            }
        }
    }
    0
}

pub(super) fn sys_lseek(fd: u32, offset: i64, whence: i32) -> u64 {
    if let Some(proc) = akuma_exec::process::current_process() {
        if let Some(akuma_exec::process::FileDescriptor::DevNull) = proc.get_fd(fd) {
            return 0;
        }
        let mut new_pos = 0i64;
        let mut success = false;
        proc.update_fd(fd, |entry| {
            if let akuma_exec::process::FileDescriptor::File(f) = entry {
                let size = crate::fs::file_size(&f.path).unwrap_or(0) as i64;
                new_pos = match whence { 0 => offset, 1 => f.position as i64 + offset, 2 => size + offset, _ => -1 };
                if new_pos >= 0 { f.position = new_pos as usize; success = true; }
            }
        });
        if success { new_pos as u64 } else { !0u64 }
    } else { !0u64 }
}

pub(super) fn sys_fstat(fd: u32, stat_ptr: u64) -> u64 {
    let stat_size = core::mem::size_of::<Stat>();
    if !validate_user_ptr(stat_ptr, stat_size) { return EFAULT; }
    let proc = match akuma_exec::process::current_process() { Some(p) => p, None => return !0u64 };
    
    let mut stat = Stat::default();
    let res = match proc.get_fd(fd) {
        Some(akuma_exec::process::FileDescriptor::File(f)) => {
            if let Ok(meta) = crate::vfs::metadata(&f.path) {
                stat = Stat { st_dev: 1, st_ino: meta.inode, st_size: meta.size as i64, st_mode: meta.mode, st_nlink: if meta.is_dir { 2 } else { 1 }, st_blksize: 4096, st_blocks: ((meta.size as i64) + 511) / 512, st_atime: meta.accessed.unwrap_or(0) as i64, st_mtime: meta.modified.unwrap_or(0) as i64, st_ctime: meta.created.unwrap_or(0) as i64, ..Default::default() };
                if crate::config::SYSCALL_DEBUG_IO_ENABLED {
                    crate::safe_print!(256, "[syscall] fstat(fd={}, file={}) size={} mode=0o{:o}\n", fd, &f.path, meta.size, meta.mode);
                }
                0
            } else { !0u64 }
        }
        Some(akuma_exec::process::FileDescriptor::DevNull) => {
            stat = Stat { st_dev: 0, st_ino: 1, st_size: 0, st_mode: 0o20666, st_nlink: 1, st_rdev: makedev(1, 3), st_blksize: 4096, ..Default::default() };
            0
        }
        Some(akuma_exec::process::FileDescriptor::DevUrandom) => {
            stat = Stat { st_dev: 0, st_ino: 9, st_size: 0, st_mode: 0o20666, st_nlink: 1, st_rdev: makedev(1, 9), st_blksize: 4096, ..Default::default() };
            0
        }
        Some(akuma_exec::process::FileDescriptor::TimerFd(_)) | Some(akuma_exec::process::FileDescriptor::EpollFd(_)) => {
            stat = Stat { st_dev: 0, st_ino: 0, st_size: 0, st_mode: 0o100600, st_nlink: 1, st_blksize: 4096, ..Default::default() };
            0
        }
        Some(akuma_exec::process::FileDescriptor::Stdin) | Some(akuma_exec::process::FileDescriptor::Stdout) | Some(akuma_exec::process::FileDescriptor::Stderr) => {
            stat = Stat { st_dev: 0, st_ino: 0, st_size: 0, st_mode: 0o20620, st_nlink: 1, st_rdev: makedev(136, 0), st_blksize: 1024, ..Default::default() };
            0
        }
        Some(akuma_exec::process::FileDescriptor::PipeRead(_)) | Some(akuma_exec::process::FileDescriptor::PipeWrite(_)) => {
            stat = Stat { st_dev: 0, st_ino: 0, st_size: 0, st_mode: 0o10600, st_nlink: 1, st_blksize: 4096, ..Default::default() };
            0
        }
        _ => !0u64,
    };
    
    if res == 0 {
        if unsafe { copy_to_user_safe(stat_ptr as *mut u8, &stat as *const Stat as *const u8, stat_size).is_err() } {
            return EFAULT;
        }
    }
    res
}

pub(super) fn sys_newfstatat(dirfd: i32, path_ptr: u64, stat_ptr: u64, _flags: u32) -> u64 {
    let path = match copy_from_user_str(path_ptr, 512) {
        Ok(p) => p,
        Err(e) => return e,
    };
    if !validate_user_ptr(stat_ptr, core::mem::size_of::<Stat>()) { return EFAULT; }

    let resolved_path = if path.starts_with('/') {
         String::from(&path)
    } else {
        let base_path = if dirfd == -100 {
             if let Some(proc) = akuma_exec::process::current_process() {
                 proc.cwd.clone()
             } else {
                 return !0u64;
             }
        } else if dirfd >= 0 {
             if let Some(proc) = akuma_exec::process::current_process() {
                 if let Some(akuma_exec::process::FileDescriptor::File(f)) = proc.get_fd(dirfd as u32) {
                     f.path.clone()
                 } else {
                     return !0u64;
                 }
             } else {
                 return !0u64;
             }
        } else {
            return !0u64;
        };
        crate::vfs::resolve_path(&base_path, &path)
    };
    
    let mut stat = Stat::default();
    let res = (|| {
        if resolved_path == "/dev/null" {
            stat = Stat { st_dev: 0, st_ino: 1, st_size: 0, st_mode: 0o20666, st_nlink: 1, st_rdev: makedev(1, 3), st_blksize: 4096, ..Default::default() };
            return 0;
        }

        const AT_SYMLINK_NOFOLLOW: u32 = 0x100;
        let follow = _flags & AT_SYMLINK_NOFOLLOW == 0;

        if !follow && crate::vfs::is_symlink(&resolved_path) {
            let target = crate::vfs::read_symlink(&resolved_path).unwrap_or_default();
            stat = Stat {
                st_dev: 1,
                st_ino: 1,
                st_size: target.len() as i64,
                st_mode: 0o120777,
                st_nlink: 1,
                st_blksize: 4096,
                ..Default::default()
            };
            return 0;
        }

        let final_path = if follow { crate::vfs::resolve_symlinks(&resolved_path) } else { resolved_path };

        if let Ok(meta) = crate::vfs::metadata(&final_path) {
            if crate::config::SYSCALL_DEBUG_IO_ENABLED {
                crate::safe_print!(128, "[syscall] newfstatat({}) mode=0o{:o} size={}\n", final_path, meta.mode, meta.size);
            }
            stat = Stat { 
                st_dev: 1,
                st_ino: meta.inode,
                st_size: meta.size as i64, 
                st_mode: meta.mode, 
                st_nlink: if meta.is_dir { 2 } else { 1 },
                st_blksize: 4096,
                st_blocks: ((meta.size as i64) + 511) / 512,
                st_atime: meta.accessed.unwrap_or(0) as i64,
                st_mtime: meta.modified.unwrap_or(0) as i64,
                st_ctime: meta.created.unwrap_or(0) as i64,
                ..Default::default() 
            };
            return 0;
        }

        if crate::vfs::is_symlink(&final_path) {
            let target = crate::vfs::read_symlink(&final_path).unwrap_or_default();
            stat = Stat {
                st_dev: 1,
                st_ino: 1,
                st_size: target.len() as i64,
                st_mode: 0o120777,
                st_nlink: 1,
                st_blksize: 4096,
                ..Default::default()
            };
            return 0;
        }
        
        if crate::config::SYSCALL_DEBUG_IO_ENABLED {
            crate::safe_print!(128, "[syscall] newfstatat: ENOENT {}\n", final_path);
        }
        ENOENT
    })();

    if res == 0 {
        let stat_size = core::mem::size_of::<Stat>();
        if unsafe { copy_to_user_safe(stat_ptr as *mut u8, &stat as *const Stat as *const u8, stat_size).is_err() } {
            return EFAULT;
        }
    }
    res
}

pub(super) fn sys_fchmod(fd: u32, mode: u32) -> u64 {
    let proc = match akuma_exec::process::current_process() { Some(p) => p, None => return EBADF };
    match proc.get_fd(fd) {
        Some(akuma_exec::process::FileDescriptor::File(f)) => {
            match crate::vfs::chmod(&f.path, mode) {
                Ok(()) => 0,
                Err(e) => fs_error_to_errno(e),
            }
        }
        Some(akuma_exec::process::FileDescriptor::DevNull) => 0,
        _ => 0,
    }
}

pub(super) fn sys_fchmodat(dirfd: i32, path_ptr: u64, mode: u32) -> u64 {
    let raw_path = match copy_from_user_str(path_ptr, 512) {
        Ok(p) => p,
        Err(e) => return e,
    };

    let path = if raw_path.starts_with('/') {
        crate::vfs::canonicalize_path(&raw_path)
    } else {
        let base = if dirfd == -100 {
            if let Some(proc) = akuma_exec::process::current_process() {
                proc.cwd.clone()
            } else {
                return EBADF;
            }
        } else if dirfd >= 0 {
            if let Some(proc) = akuma_exec::process::current_process() {
                if let Some(akuma_exec::process::FileDescriptor::File(f)) = proc.get_fd(dirfd as u32) {
                    f.path.clone()
                } else {
                    return EBADF;
                }
            } else {
                return EBADF;
            }
        } else {
            return EBADF;
        };
        crate::vfs::resolve_path(&base, &raw_path)
    };

    let path = crate::vfs::resolve_symlinks(&path);

    if path == "/dev/null" {
        return 0;
    }

    match crate::vfs::chmod(&path, mode) {
        Ok(()) => 0,
        Err(e) => fs_error_to_errno(e),
    }
}

pub(super) fn sys_fallocate(fd: u32, mode: i32, offset: i64, len: i64) -> u64 {
    if offset < 0 || len <= 0 {
        return super::EINVAL;
    }
    let proc = match akuma_exec::process::current_process() { Some(p) => p, None => return EBADF };
    match proc.get_fd(fd) {
        Some(akuma_exec::process::FileDescriptor::File(f)) => {
            match crate::vfs::fallocate(&f.path, mode, offset as u64, len as u64) {
                Ok(()) => 0,
                Err(e) => fs_error_to_errno(e),
            }
        }
        Some(akuma_exec::process::FileDescriptor::DevNull) => 0,
        _ => EBADF,
    }
}

pub(super) fn sys_ftruncate(fd: u32, length: i64) -> u64 {
    let proc = match akuma_exec::process::current_process() { Some(p) => p, None => return EBADF };
    match proc.get_fd(fd) {
        Some(akuma_exec::process::FileDescriptor::File(f)) => {
            match crate::vfs::truncate(&f.path, length as u64) {
                Ok(()) => 0,
                Err(e) => fs_error_to_errno(e),
            }
        }
        Some(akuma_exec::process::FileDescriptor::DevNull) => 0,
        _ => EBADF,
    }
}

pub(super) fn sys_truncate(path_ptr: u64, length: i64) -> u64 {
    let path = match copy_from_user_str(path_ptr, 512) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let resolved = if path.starts_with('/') {
        path
    } else if let Some(proc) = akuma_exec::process::current_process() {
        crate::vfs::resolve_path(&proc.cwd, &path)
    } else {
        return EBADF;
    };
    match crate::vfs::truncate(&resolved, length as u64) {
        Ok(()) => 0,
        Err(e) => fs_error_to_errno(e),
    }
}

pub(super) fn sys_statx(dirfd: i32, path_ptr: u64, flags: u32, _mask: u32, buf_ptr: u64) -> u64 {
    const STATX_SIZE: usize = 256;
    if !validate_user_ptr(buf_ptr, STATX_SIZE) { return EFAULT; }

    let path = match copy_from_user_str(path_ptr, 512) {
        Ok(p) => p,
        Err(e) => return e,
    };

    let resolved_path = if path.is_empty() {
        const AT_EMPTY_PATH: u32 = 0x1000;
        if flags & AT_EMPTY_PATH != 0 && dirfd >= 0 {
            if let Some(proc) = akuma_exec::process::current_process() {
                if let Some(akuma_exec::process::FileDescriptor::File(f)) = proc.get_fd(dirfd as u32) {
                    f.path.clone()
                } else {
                    return EBADF;
                }
            } else {
                return EBADF;
            }
        } else {
            return ENOENT;
        }
    } else if path.starts_with('/') {
        String::from(&path)
    } else {
        let base_path = if dirfd == -100 {
            if let Some(proc) = akuma_exec::process::current_process() {
                proc.cwd.clone()
            } else {
                return EBADF;
            }
        } else if dirfd >= 0 {
            if let Some(proc) = akuma_exec::process::current_process() {
                if let Some(akuma_exec::process::FileDescriptor::File(f)) = proc.get_fd(dirfd as u32) {
                    f.path.clone()
                } else {
                    return EBADF;
                }
            } else {
                return EBADF;
            }
        } else {
            return EINVAL;
        };
        crate::vfs::resolve_path(&base_path, &path)
    };

    const AT_SYMLINK_NOFOLLOW: u32 = 0x100;
    let follow = flags & AT_SYMLINK_NOFOLLOW == 0;

    let (mode, ino, size, nlink, atime, mtime, ctime, rdev_major, rdev_minor) =
        if resolved_path == "/dev/null" {
            (0o20666u16, 1u64, 0u64, 1u32, 0i64, 0i64, 0i64, 1u32, 3u32)
        } else if !follow && crate::vfs::is_symlink(&resolved_path) {
            let target = crate::vfs::read_symlink(&resolved_path).unwrap_or_default();
            (0o120777u16, 1, target.len() as u64, 1, 0, 0, 0, 0, 0)
        } else {
            let final_path = if follow { crate::vfs::resolve_symlinks(&resolved_path) } else { resolved_path };
            if let Ok(meta) = crate::vfs::metadata(&final_path) {
                (meta.mode as u16, meta.inode, meta.size,
                 if meta.is_dir { 2 } else { 1 },
                 meta.accessed.unwrap_or(0) as i64,
                 meta.modified.unwrap_or(0) as i64,
                 meta.created.unwrap_or(0) as i64,
                 0, 0)
            } else if crate::vfs::is_symlink(&final_path) {
                let target = crate::vfs::read_symlink(&final_path).unwrap_or_default();
                (0o120777u16, 1, target.len() as u64, 1, 0, 0, 0, 0, 0)
            } else {
                return ENOENT;
            }
        };

    let blksize: u32 = 4096;
    let blocks: u64 = (size + 511) / 512;

    // STATX_BASIC_STATS covers type/mode/nlink/uid/gid/ino/size/blocks/times
    const STATX_BASIC_STATS: u32 = 0x07ff;

    let mut buf = [0u8; STATX_SIZE];
    unsafe {
        let p = buf.as_mut_ptr();
        // stx_mask (u32 @ 0)
        core::ptr::write(p.add(0) as *mut u32, STATX_BASIC_STATS);
        // stx_blksize (u32 @ 4)
        core::ptr::write(p.add(4) as *mut u32, blksize);
        // stx_attributes (u64 @ 8) — none
        // stx_nlink (u32 @ 16)
        core::ptr::write(p.add(16) as *mut u32, nlink);
        // stx_uid (u32 @ 20)
        // stx_gid (u32 @ 24)
        // stx_mode (u16 @ 28)
        core::ptr::write(p.add(28) as *mut u16, mode);
        // stx_ino (u64 @ 32)
        core::ptr::write(p.add(32) as *mut u64, ino);
        // stx_size (u64 @ 40)
        core::ptr::write(p.add(40) as *mut u64, size);
        // stx_blocks (u64 @ 48)
        core::ptr::write(p.add(48) as *mut u64, blocks);
        // stx_attributes_mask (u64 @ 56) — none
        // stx_atime (statx_timestamp @ 64): tv_sec(i64) + tv_nsec(u32) + __reserved(i32) = 16 bytes
        core::ptr::write(p.add(64) as *mut i64, atime);
        // stx_btime (@ 80)
        // stx_ctime (@ 96)
        core::ptr::write(p.add(96) as *mut i64, ctime);
        // stx_mtime (@ 112)
        core::ptr::write(p.add(112) as *mut i64, mtime);
        // stx_rdev_major (u32 @ 128)
        core::ptr::write(p.add(128) as *mut u32, rdev_major);
        // stx_rdev_minor (u32 @ 132)
        core::ptr::write(p.add(132) as *mut u32, rdev_minor);
        // stx_dev_major (u32 @ 136)
        core::ptr::write(p.add(136) as *mut u32, 0);
        // stx_dev_minor (u32 @ 140)
        core::ptr::write(p.add(140) as *mut u32, 1);
        // stx_mnt_id (u64 @ 144)
        core::ptr::write(p.add(144) as *mut u64, 1);
    }

    if unsafe { copy_to_user_safe(buf_ptr as *mut u8, buf.as_ptr(), STATX_SIZE).is_err() } {
        return EFAULT;
    }
    0
}

pub(super) fn sys_faccessat2(dirfd: i32, path_ptr: u64, _mode: u32, _flags: u32) -> u64 {
    let path = match copy_from_user_str(path_ptr, 512) {
        Ok(p) => p,
        Err(e) => return e,
    };
    
    let resolved_path = if path.starts_with('/') {
         path
    } else {
        let base_path = if dirfd == -100 {
             if let Some(proc) = akuma_exec::process::current_process() {
                 proc.cwd.clone()
             } else {
                 return !0u64;
             }
        } else if dirfd >= 0 {
             if let Some(proc) = akuma_exec::process::current_process() {
                 if let Some(akuma_exec::process::FileDescriptor::File(f)) = proc.get_fd(dirfd as u32) {
                     f.path.clone()
                 } else {
                     return !0u64;
                 }
             } else {
                 return !0u64;
             }
        } else {
            return !0u64;
        };
        crate::vfs::resolve_path(&base_path, &path)
    };
    
    let final_path = crate::vfs::resolve_symlinks(&resolved_path);
    if crate::fs::exists(&final_path) || crate::vfs::is_symlink(&resolved_path) {
        0
    } else {
        if crate::config::SYSCALL_DEBUG_IO_ENABLED {
            crate::safe_print!(128, "[syscall] faccessat: ENOENT {}\n", final_path);
        }
        ENOENT
    }
}

pub(super) fn sys_getcwd(buf_ptr: u64, size: usize) -> u64 {
    if !validate_user_ptr(buf_ptr, size) { return EFAULT; }
    if let Some(proc) = akuma_exec::process::current_process() {
        let cwd_bytes = proc.cwd.as_bytes();
        if cwd_bytes.len() + 1 > size {
            return (-libc_errno::ERANGE as i64) as u64;
        }
        let mut temp = alloc::vec![0u8; cwd_bytes.len() + 1];
        temp[..cwd_bytes.len()].copy_from_slice(cwd_bytes);
        temp[cwd_bytes.len()] = 0;
        
        if unsafe { copy_to_user_safe(buf_ptr as *mut u8, temp.as_ptr(), temp.len()).is_err() } {
            return EFAULT;
        }
        return temp.len() as u64;
    }
    ENOENT
}

pub(super) fn sys_fcntl(fd: u32, cmd: u32, arg: u64) -> u64 {
    const F_DUPFD: u32 = 0;
    const F_GETFD: u32 = 1;
    const F_SETFD: u32 = 2;
    const F_GETFL: u32 = 3;
    const F_SETFL: u32 = 4;
    // Advisory record locking — no-op stubs (we have no lock state)
    const F_SETLK: u32 = 6;
    const F_SETLKW: u32 = 7;
    const F_GETLK: u32 = 5;
    const F_DUPFD_CLOEXEC: u32 = 1030;
    const FD_CLOEXEC: u64 = 1;
    const O_NONBLOCK: u64 = 0x800;

    let proc = match akuma_exec::process::current_process() { Some(p) => p, None => return EBADF };

    if proc.get_fd(fd).is_none() {
        return EBADF;
    }

    match cmd {
        F_DUPFD | F_DUPFD_CLOEXEC => {
            let entry = match proc.get_fd(fd) { Some(e) => e, None => return EBADF };
            // Bump pipe refcount before inserting the duplicate entry.
            match &entry {
                akuma_exec::process::FileDescriptor::PipeWrite(id) => super::pipe::pipe_clone_ref(*id, true),
                akuma_exec::process::FileDescriptor::PipeRead(id) => super::pipe::pipe_clone_ref(*id, false),
                _ => {}
            }
            let new_fd = proc.alloc_fd(entry);
            if cmd == F_DUPFD_CLOEXEC {
                proc.set_cloexec(new_fd);
            }
            new_fd as u64
        }
        F_GETFD => {
            if proc.is_cloexec(fd) { FD_CLOEXEC } else { 0 }
        }
        F_SETFD => {
            if arg & FD_CLOEXEC != 0 {
                proc.set_cloexec(fd);
            } else {
                proc.clear_cloexec(fd);
            }
            0
        }
        F_GETFL => {
            if proc.is_nonblock(fd) { O_NONBLOCK } else { 0 }
        }
        F_SETFL => {
            if arg & O_NONBLOCK != 0 {
                proc.set_nonblock(fd);
            } else {
                proc.clear_nonblock(fd);
            }
            0
        }
        // Advisory locks: no-op (we have no file locking state)
        F_GETLK | F_SETLK | F_SETLKW => 0,
        _ => {
            crate::safe_print!(192, "[fcntl] UNSUPPORTED: pid={} fd={} cmd={} arg={:#x}\n",
                proc.pid, fd, cmd, arg);
            EINVAL
        },
    }
}

pub(super) fn sys_mkdirat(dirfd: i32, path_ptr: u64, _mode: u32) -> u64 {
    let raw_path = match copy_from_user_str(path_ptr, 512) {
        Ok(p) => p,
        Err(e) => return e,
    };

    let path = if raw_path.starts_with('/') {
        crate::vfs::canonicalize_path(&raw_path)
    } else {
        let base = if dirfd == -100 {
            if let Some(proc) = akuma_exec::process::current_process() {
                proc.cwd.clone()
            } else {
                return EBADF;
            }
        } else if dirfd >= 0 {
            if let Some(proc) = akuma_exec::process::current_process() {
                if let Some(akuma_exec::process::FileDescriptor::File(f)) = proc.get_fd(dirfd as u32) {
                    f.path.clone()
                } else {
                    return EBADF;
                }
            } else {
                return EBADF;
            }
        } else {
            return EBADF;
        };
        crate::vfs::resolve_path(&base, &raw_path)
    };

    if crate::config::SYSCALL_DEBUG_IO_ENABLED {
        crate::safe_print!(256, "[syscall] mkdirat({}) dirfd={}\n", &path, dirfd);
    }

    match crate::fs::create_dir(&path) {
        Ok(()) => 0,
        Err(e) => fs_error_to_errno(e),
    }
}

pub(super) fn sys_unlinkat(dirfd: i32, path_ptr: u64, flags: u32) -> u64 {
    let path = match copy_from_user_str(path_ptr, 512) {
        Ok(p) => p,
        Err(e) => return e,
    };

    let resolved = if path.starts_with('/') {
        crate::vfs::canonicalize_path(&path)
    } else {
        let base = if dirfd == -100 {
            if let Some(proc) = akuma_exec::process::current_process() {
                proc.cwd.clone()
            } else {
                return EBADF;
            }
        } else if dirfd >= 0 {
            if let Some(proc) = akuma_exec::process::current_process() {
                if let Some(akuma_exec::process::FileDescriptor::File(f)) = proc.get_fd(dirfd as u32) {
                    f.path.clone()
                } else {
                    return EBADF;
                }
            } else {
                return EBADF;
            }
        } else {
            return EBADF;
        };
        crate::vfs::resolve_path(&base, &path)
    };

    if crate::config::SYSCALL_DEBUG_IO_ENABLED {
        crate::safe_print!(256, "[syscall] unlinkat({}) flags=0x{:x}\n", &resolved, flags);
    }

    const AT_REMOVEDIR: u32 = 0x200;
    if flags & AT_REMOVEDIR != 0 {
        match crate::fs::remove_dir(&resolved) {
            Ok(()) => 0,
            Err(e) => fs_error_to_errno(e),
        }
    } else {
        crate::vfs::remove_symlink(&resolved);
        match crate::fs::remove_file(&resolved) {
            Ok(()) => 0,
            Err(e) => fs_error_to_errno(e),
        }
    }
}

pub(super) fn sys_renameat(olddirfd: i32, oldpath_ptr: u64, newdirfd: i32, newpath_ptr: u64) -> u64 {
    let raw_old = match copy_from_user_str(oldpath_ptr, 512) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let raw_new = match copy_from_user_str(newpath_ptr, 512) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let oldpath = resolve_path_at(olddirfd, &raw_old);
    let newpath = resolve_path_at(newdirfd, &raw_new);
    crate::safe_print!(256, "[syscall] renameat: {} -> {}\n", oldpath, newpath);
    if crate::fs::rename(&oldpath, &newpath).is_ok() { 0 } else { !0u64 }
}

const RENAME_NOREPLACE: u32 = 1;
const RENAME_EXCHANGE: u32 = 2;

pub(super) fn sys_renameat2(olddirfd: i32, oldpath_ptr: u64, newdirfd: i32, newpath_ptr: u64, flags: u32) -> u64 {
    if flags & !(RENAME_NOREPLACE | RENAME_EXCHANGE) != 0 {
        return super::EINVAL;
    }
    if flags & RENAME_NOREPLACE != 0 && flags & RENAME_EXCHANGE != 0 {
        return super::EINVAL;
    }

    let raw_old = match copy_from_user_str(oldpath_ptr, 512) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let raw_new = match copy_from_user_str(newpath_ptr, 512) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let oldpath = resolve_path_at(olddirfd, &raw_old);
    let newpath = resolve_path_at(newdirfd, &raw_new);

    if flags & RENAME_NOREPLACE != 0 && crate::vfs::exists(&newpath) {
        return super::EEXIST;
    }

    if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
        crate::safe_print!(256, "[syscall] renameat2: {} -> {} flags=0x{:x}\n", oldpath, newpath, flags);
    }
    if crate::fs::rename(&oldpath, &newpath).is_ok() { 0 } else { !0u64 }
}

pub(super) fn sys_symlinkat(target_ptr: u64, newdirfd: i32, linkpath_ptr: u64) -> u64 {
    let target = match copy_from_user_str(target_ptr, 1024) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let raw_link = match copy_from_user_str(linkpath_ptr, 1024) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let link_path = resolve_path_at(newdirfd, &raw_link);
    if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
        crate::safe_print!(256, "[syscall] symlinkat: {} -> {}\n", link_path, target);
    }
    match crate::vfs::create_symlink(&link_path, &target) {
        Ok(_) => 0,
        Err(e) => fs_error_to_errno(e),
    }
}

pub(super) fn sys_linkat(_olddirfd: i32, oldpath_ptr: u64, _newdirfd: i32, newpath_ptr: u64, _flags: u32) -> u64 {
    let oldpath = match copy_from_user_str(oldpath_ptr, 1024) { Ok(p) => p, Err(e) => return e };
    let newpath = match copy_from_user_str(newpath_ptr, 1024) { Ok(p) => p, Err(e) => return e };
    let src = resolve_path_at(_olddirfd, &oldpath);
    let dst = resolve_path_at(_newdirfd, &newpath);
    if let Ok(data) = crate::fs::read_file(&src) {
        if crate::fs::write_file(&dst, &data).is_ok() { return 0; }
    }
    !0u64
}

pub(super) fn sys_readlinkat(dirfd: i32, path_ptr: u64, buf_ptr: u64, bufsize: usize) -> u64 {
    let raw_path = match copy_from_user_str(path_ptr, 1024) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let path = resolve_path_at(dirfd, &raw_path);

    if path == "/proc/self/exe" {
        if !validate_user_ptr(buf_ptr, bufsize) { return EFAULT; }
        let exe = if let Some(proc) = akuma_exec::process::current_process() {
            proc.name.clone()
        } else {
            String::from("/bin/unknown")
        };
        let bytes = exe.as_bytes();
        let copy_len = bytes.len().min(bufsize);
        if unsafe { copy_to_user_safe(buf_ptr as *mut u8, bytes.as_ptr(), copy_len).is_err() } {
            return EFAULT;
        }
        return copy_len as u64;
    }

    if let Some(target) = crate::vfs::read_symlink(&path) {
        if !validate_user_ptr(buf_ptr, bufsize) { return EFAULT; }
        let bytes = target.as_bytes();
        let copy_len = bytes.len().min(bufsize);
        if unsafe { copy_to_user_safe(buf_ptr as *mut u8, bytes.as_ptr(), copy_len).is_err() } {
            return EFAULT;
        }
        return copy_len as u64;
    }

    if crate::vfs::exists(&path) {
        EINVAL
    } else {
        ENOENT
    }
}

pub(super) fn sys_getdents64(fd: u32, ptr: u64, size: usize) -> u64 {
    if !validate_user_ptr(ptr, size) { return EFAULT; }
    if let Some(proc) = akuma_exec::process::current_process() {
        if let Some(akuma_exec::process::FileDescriptor::File(f)) = proc.get_fd(fd) {
            if let Ok(entries) = crate::fs::list_dir(&f.path) {
                if f.position >= entries.len() { return 0; }
                let mut kernel_buf = alloc::vec![0u8; size];
                let mut written = 0;
                for entry in entries.iter().skip(f.position) {
                    let reclen = (19 + entry.name.len() + 1 + 7) & !7;
                    if written + reclen > size { break; }
                    let p = unsafe { kernel_buf.as_mut_ptr().add(written) };
                    unsafe {
                        core::ptr::write_unaligned(p as *mut u64, 1);
                        core::ptr::write_unaligned(p.add(8) as *mut u64, 1);
                        core::ptr::write_unaligned(p.add(16) as *mut u16, reclen as u16);
                        let d_type: u8 = if entry.is_dir { 4 } else if entry.is_symlink { 10 } else { 8 };
                        p.add(18).write(d_type);
                        core::ptr::copy_nonoverlapping(entry.name.as_ptr(), p.add(19), entry.name.len());
                        p.add(19 + entry.name.len()).write(0);
                    }
                    written += reclen;
                    proc.update_fd(fd, |e| if let akuma_exec::process::FileDescriptor::File(file) = e { file.position += 1; });
                }
                if written > 0 {
                    if unsafe { copy_to_user_safe(ptr as *mut u8, kernel_buf.as_ptr(), written).is_err() } {
                        return EFAULT;
                    }
                }
                return written as u64;
            }
        }
    }
    !0u64
}

pub(super) fn sys_fchdir(fd: u32) -> u64 {
    let proc = match akuma_exec::process::current_process() { Some(p) => p, None => return !0u64 };
    let entry = match proc.get_fd(fd) {
        Some(e) => e,
        None => return EBADF,
    };
    let path = match entry {
        akuma_exec::process::FileDescriptor::File(f) => f.path.clone(),
        _ => return ENOTDIR,
    };
    if let Ok(meta) = crate::vfs::metadata(&path) {
        if meta.is_dir {
            proc.set_cwd(&path);
            if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
                crate::safe_print!(128, "[syscall] fchdir(fd={}) -> \"{}\"\n", fd, path);
            }
            return 0;
        }
    }
    ENOTDIR
}

pub(super) fn sys_chdir(ptr: u64) -> u64 {
    let path = match copy_from_user_str(ptr, 512) {
        Ok(p) => p,
        Err(e) => return e,
    };
    
    if let Some(proc) = akuma_exec::process::current_process() {
        let new_cwd = crate::vfs::resolve_path(&proc.cwd, &path);
        
        if crate::fs::exists(&new_cwd) {
            if let Ok(meta) = crate::vfs::metadata(&new_cwd) {
                if meta.is_dir {
                    proc.set_cwd(&new_cwd);
                    return 0;
                }
            }
        }
        return ENOENT;
    }
    !0u64
}
