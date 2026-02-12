//! System Call Handlers
//!
//! Implements the syscall interface for user programs.
//! Uses Linux-compatible ABI: syscall number in x8, arguments in x0-x5.

use crate::console;
use crate::config;

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
    // Terminal Syscalls (307-313)
    pub const SET_TERMINAL_ATTRIBUTES: u64 = 307;
    pub const GET_TERMINAL_ATTRIBUTES: u64 = 308;
    pub const SET_CURSOR_POSITION: u64 = 309;
    pub const HIDE_CURSOR: u64 = 310;
    pub const SHOW_CURSOR: u64 = 311;
    pub const CLEAR_SCREEN: u64 = 312;
    pub const POLL_INPUT_EVENT: u64 = 313;
    pub const GET_CPU_STATS: u64 = 314;
}

/// Thread CPU statistics for top command
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct ThreadCpuStat {
    pub tid: u32,
    pub pid: u32,
    pub total_time_us: u64,
    pub state: u8,
    pub _reserved: [u8; 3],
    pub name: [u8; 16],
}

/// Error code for interrupted syscall
const EINTR: u64 = (-4i64) as u64;

/// Handle a system call
pub fn handle_syscall(syscall_num: u64, args: &[u64; 6]) -> u64 {
    if crate::process::is_current_interrupted() {
        if let Some(proc) = crate::process::current_process() {
            proc.exited = true;
            proc.exit_code = 130;
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
        nr::MMAP => sys_mmap(args[0] as usize, args[1] as usize, args[2] as u32, args[3] as u32),
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
        nr::SET_TERMINAL_ATTRIBUTES => 0,
        nr::GET_TERMINAL_ATTRIBUTES => 0,
        nr::SET_CURSOR_POSITION => 0,
        nr::HIDE_CURSOR => 0,
        nr::SHOW_CURSOR => 0,
        nr::CLEAR_SCREEN => 0,
        nr::POLL_INPUT_EVENT => sys_poll_input_event(args[0], args[1] as usize, args[2]),
        nr::GET_CPU_STATS => sys_get_cpu_stats(args[0], args[1] as usize),
        _ => !0 // ENOSYS
    }
}

fn sys_exit(code: i32) -> u64 {
    if let Some(proc) = crate::process::current_process() {
        proc.exited = true;
        proc.exit_code = code;
        proc.state = crate::process::ProcessState::Zombie(code);
    }
    code as u64
}

fn sys_read(fd_num: u64, buf_ptr: u64, count: usize) -> u64 {
    let proc = match crate::process::current_process() { Some(p) => p, None => return !0u64 };
    let fd = match proc.get_fd(fd_num as u32) { Some(e) => e, None => return !0u64 };
    match fd {
        crate::process::FileDescriptor::Stdin => {
            let mut temp = alloc::vec![0u8; count];
            let n = if let Some(ch) = crate::process::current_channel() { ch.read_stdin(&mut temp) } else { proc.read_stdin(&mut temp) };
            if n > 0 { unsafe { core::ptr::copy_nonoverlapping(temp.as_ptr(), buf_ptr as *mut u8, n); } }
            n as u64
        }
        crate::process::FileDescriptor::File(ref f) => {
            let data = match crate::fs::read_file(&f.path) { Ok(d) => d, Err(_) => return !0u64 };
            if f.position >= data.len() { return 0; }
            let n = count.min(data.len() - f.position);
            unsafe { core::ptr::copy_nonoverlapping(data[f.position..].as_ptr(), buf_ptr as *mut u8, n); }
            proc.update_fd(fd_num as u32, |entry| if let crate::process::FileDescriptor::File(file) = entry { file.position += n; });
            n as u64
        }
        crate::process::FileDescriptor::Socket(_) => {
            let buf = unsafe { core::slice::from_raw_parts_mut(buf_ptr as *mut u8, count) };
            match crate::socket::socket_recv(fd_num as usize, buf) {
                Ok(n) => n as u64,
                Err(e) => (-(e as i64)) as u64,
            }
        }
        _ => !0u64
    }
}

fn sys_write(fd_num: u64, buf_ptr: u64, count: usize) -> u64 {
    let proc = match crate::process::current_process() { Some(p) => p, None => return !0u64 };
    let fd = match proc.get_fd(fd_num as u32) { Some(e) => e, None => return !0u64 };
    let buf = unsafe { core::slice::from_raw_parts(buf_ptr as *const u8, count) };
    match fd {
        crate::process::FileDescriptor::Stdout | crate::process::FileDescriptor::Stderr => {
            if let Some(ch) = crate::process::current_channel() { ch.write(buf); }
            proc.write_stdout(buf);
            count as u64
        }
        crate::process::FileDescriptor::File(ref f) => {
            let mut data = crate::fs::read_file(&f.path).unwrap_or_default();
            if f.position + count > data.len() { data.resize(f.position + count, 0); }
            data[f.position..f.position + count].copy_from_slice(buf);
            if crate::fs::write_file(&f.path, &data).is_ok() {
                proc.update_fd(fd_num as u32, |entry| if let crate::process::FileDescriptor::File(file) = entry { file.position += count; });
                count as u64
            } else { !0u64 }
        }
        crate::process::FileDescriptor::Socket(_) => {
            match crate::socket::socket_send(fd_num as usize, buf) {
                Ok(n) => n as u64,
                Err(e) => (-(e as i64)) as u64,
            }
        }
        _ => !0u64
    }
}

fn sys_brk(new_brk: usize) -> u64 {
    if let Some(proc) = crate::process::current_process() {
        if new_brk == 0 { proc.get_brk() as u64 } else { proc.set_brk(new_brk) as u64 }
    } else { 0 }
}

fn sys_openat(_dirfd: i32, path_ptr: u64, path_len: usize, flags: u32, _mode: u32) -> u64 {
    let path = unsafe { core::str::from_utf8(core::slice::from_raw_parts(path_ptr as *const u8, path_len)).unwrap_or("") };
    if let Some(proc) = crate::process::current_process() {
        // Handle O_TRUNC: truncate existing file to zero length
        if flags & crate::process::open_flags::O_TRUNC != 0 {
            // Only truncate if file exists; ignore errors (file might not exist yet with O_CREAT)
            let _ = crate::fs::write_file(path, &[]);
        }
        let fd = proc.alloc_fd(crate::process::FileDescriptor::File(crate::process::KernelFile::new(path.into(), flags)));
        fd as u64
    } else { !0u64 }
}

fn sys_close(fd: u32) -> u64 {
    if let Some(proc) = crate::process::current_process() {
        if let Some(entry) = proc.remove_fd(fd) {
            if let crate::process::FileDescriptor::Socket(idx) = entry { crate::socket::remove_socket(idx); }
            0
        } else { !0u64 }
    } else { !0u64 }
}

fn sys_lseek(fd: u32, offset: i64, whence: i32) -> u64 {
    if let Some(proc) = crate::process::current_process() {
        let mut new_pos = 0i64;
        let mut success = false;
        proc.update_fd(fd, |entry| {
            if let crate::process::FileDescriptor::File(f) = entry {
                let size = crate::fs::file_size(&f.path).unwrap_or(0) as i64;
                new_pos = match whence { 0 => offset, 1 => f.position as i64 + offset, 2 => size + offset, _ => -1 };
                if new_pos >= 0 { f.position = new_pos as usize; success = true; }
            }
        });
        if success { new_pos as u64 } else { !0u64 }
    } else { !0u64 }
}

#[repr(C)] #[derive(Default)] pub struct Stat { pub st_dev: u64, pub st_ino: u64, pub st_mode: u32, pub st_nlink: u32, pub st_uid: u32, pub st_gid: u32, pub st_rdev: u64, pub __pad1: u64, pub st_size: i64, pub st_blksize: i32, pub __pad2: i32, pub st_blocks: i64, pub st_atime: i64, pub st_atime_nsec: i64, pub st_mtime: i64, pub st_mtime_nsec: i64, pub st_ctime: i64, pub st_ctime_nsec: i64, pub __unused: [i32; 2] }

fn sys_fstat(fd: u32, stat_ptr: u64) -> u64 {
    let proc = match crate::process::current_process() { Some(p) => p, None => return !0u64 };
    if let Some(crate::process::FileDescriptor::File(f)) = proc.get_fd(fd) {
        if let Ok(meta) = crate::vfs::metadata(&f.path) {
            let stat = Stat { st_size: meta.size as i64, st_mode: if meta.is_dir { 0o40755 } else { 0o100644 }, ..Default::default() };
            unsafe { core::ptr::write(stat_ptr as *mut Stat, stat); }
            return 0;
        }
    }
    !0u64
}

fn sys_mkdirat(_dirfd: i32, path_ptr: u64, path_len: usize, _mode: u32) -> u64 {
    let path = unsafe { core::str::from_utf8(core::slice::from_raw_parts(path_ptr as *const u8, path_len)).unwrap_or("") };
    if crate::fs::create_dir(path).is_ok() { 0 } else { !0u64 }
}

fn sys_nanosleep(seconds: u64, nanoseconds: u64) -> u64 {
    let total_us = seconds * 1_000_000 + nanoseconds / 1_000;
    let deadline = crate::timer::uptime_us() + total_us;
    loop {
        if crate::timer::uptime_us() >= deadline { return 0; }
        if crate::process::is_current_interrupted() { return EINTR; }
        crate::threading::schedule_blocking(deadline);
    }
}

use crate::socket::{self, SocketAddrV4, SockAddrIn, libc_errno};

fn sys_socket(domain: i32, sock_type: i32, _proto: i32) -> u64 {
    if domain != 2 || sock_type != 1 { return !0u64; }
    if let Some(idx) = socket::alloc_socket(sock_type) {
        if let Some(proc) = crate::process::current_process() { return proc.alloc_fd(crate::process::FileDescriptor::Socket(idx)) as u64; }
    }
    !0u64
}

fn sys_bind(fd: u32, addr_ptr: u64, len: usize) -> u64 {
    if len < 16 { return !0u64; }
    let addr = unsafe { core::ptr::read(addr_ptr as *const SockAddrIn) }.to_addr();
    if let Some(idx) = get_socket_from_fd(fd) { if socket::socket_bind(idx, addr).is_ok() { return 0; } }
    !0u64
}

fn sys_listen(fd: u32, backlog: i32) -> u64 {
    if let Some(idx) = get_socket_from_fd(fd) { if socket::socket_listen(idx, backlog as usize).is_ok() { return 0; } }
    !0u64
}

fn sys_accept(fd: u32, addr_ptr: u64, _len_ptr: u64) -> u64 {
    if let Some(idx) = get_socket_from_fd(fd) {
        if let Ok((new_idx, addr)) = socket::socket_accept(idx) {
            if let Some(proc) = crate::process::current_process() {
                if addr_ptr != 0 { unsafe { core::ptr::write(addr_ptr as *mut SockAddrIn, SockAddrIn::from_addr(&addr)); } }
                return proc.alloc_fd(crate::process::FileDescriptor::Socket(new_idx)) as u64;
            }
        }
    }
    !0u64
}

fn sys_connect(fd: u32, addr_ptr: u64, len: usize) -> u64 {
    if len < 16 { return !0u64; }
    let addr = unsafe { core::ptr::read(addr_ptr as *const SockAddrIn) }.to_addr();
    if let Some(idx) = get_socket_from_fd(fd) { if socket::socket_connect(idx, addr).is_ok() { return 0; } }
    !0u64
}

fn sys_sendto(fd: u32, buf_ptr: u64, len: usize, _flags: i32) -> u64 {
    let buf = unsafe { core::slice::from_raw_parts(buf_ptr as *const u8, len) };
    let idx = match get_socket_from_fd(fd) {
        Some(i) => i,
        None => return (-libc_errno::EBADF as i64) as u64,
    };
    match socket::socket_send(idx, buf) {
        Ok(n) => n as u64,
        Err(e) => (-e as i64) as u64,
    }
}

fn sys_recvfrom(fd: u32, buf_ptr: u64, len: usize, _flags: i32) -> u64 {
    let buf = unsafe { core::slice::from_raw_parts_mut(buf_ptr as *mut u8, len) };
    let idx = match get_socket_from_fd(fd) {
        Some(i) => i,
        None => return (-libc_errno::EBADF as i64) as u64,
    };
    match socket::socket_recv(idx, buf) {
        Ok(n) => n as u64,
        Err(e) => (-e as i64) as u64,
    }
}

fn sys_shutdown(_fd: u32, _how: i32) -> u64 { 0 }

fn get_socket_from_fd(fd: u32) -> Option<usize> {
    let proc = crate::process::current_process()?;
    if let Some(crate::process::FileDescriptor::Socket(idx)) = proc.get_fd(fd) { Some(idx) } else { None }
}

fn sys_mmap(addr: usize, len: usize, _prot: u32, _flags: u32) -> u64 {
    if len == 0 { return !0u64; }
    let pages = (len + 4095) / 4096;
    let mmap_addr = crate::process::alloc_mmap(pages * 4096);
    if mmap_addr == 0 { return !0u64; }
    if let Some(proc) = crate::process::current_process() {
        let mut frames = alloc::vec::Vec::new();
        for i in 0..pages {
            if let Some(frame) = crate::pmm::alloc_page_zeroed() {
                frames.push(frame);
                unsafe { crate::mmu::map_user_page(mmap_addr + i * 4096, frame.addr, crate::mmu::user_flags::RW_NO_EXEC); }
                proc.address_space.track_user_frame(frame);
            } else { return !0u64; }
        }
        crate::process::record_mmap_region(mmap_addr, frames);
        mmap_addr as u64
    } else { !0u64 }
}

fn sys_munmap(addr: usize, _len: usize) -> u64 {
    if let Some(frames) = crate::process::remove_mmap_region(addr) {
        if let Some(proc) = crate::process::current_process() {
            for (i, frame) in frames.into_iter().enumerate() {
                let _ = proc.address_space.unmap_page(addr + i * 4096);
                proc.address_space.remove_user_frame(frame);
                crate::pmm::free_page(frame);
            }
            return 0;
        }
    }
    !0u64
}

fn sys_uptime() -> u64 { crate::timer::uptime_us() }

fn sys_resolve_host(path_ptr: u64, path_len: usize, res_ptr: u64) -> u64 {
    let host = unsafe { core::str::from_utf8(core::slice::from_raw_parts(path_ptr as *const u8, path_len)).unwrap_or("") };
    match crate::dns::resolve_host_blocking(host) {
        Ok(ipv4) => {
            unsafe { *(res_ptr as *mut [u8; 4]) = ipv4.octets(); }
            0
        }
        Err(_) => !0u64,
    }
}

fn sys_getdents64(fd: u32, ptr: u64, size: usize) -> u64 {
    if let Some(proc) = crate::process::current_process() {
        if let Some(crate::process::FileDescriptor::File(f)) = proc.get_fd(fd) {
            if let Ok(entries) = crate::fs::list_dir(&f.path) {
                if f.position >= entries.len() { return 0; }
                let mut written = 0;
                for entry in entries.iter().skip(f.position) {
                    let reclen = (19 + entry.name.len() + 1 + 7) & !7;
                    if written + reclen > size { break; }
                    unsafe {
                        let p = (ptr as *mut u8).add(written);
                        core::ptr::write_unaligned(p as *mut u64, 1);
                        core::ptr::write_unaligned(p.add(8) as *mut u64, 1);
                        core::ptr::write_unaligned(p.add(16) as *mut u16, reclen as u16);
                        p.add(18).write(if entry.is_dir { 4 } else { 8 });
                        core::ptr::copy_nonoverlapping(entry.name.as_ptr(), p.add(19), entry.name.len());
                        p.add(19 + entry.name.len()).write(0);
                    }
                    written += reclen;
                    proc.update_fd(fd, |e| if let crate::process::FileDescriptor::File(file) = e { file.position += 1; });
                }
                return written as u64;
            }
        }
    }
    !0u64
}

fn sys_spawn(path_ptr: u64, path_len: usize, _args_ptr: u64, _args_len: usize, stdin_ptr: u64, stdin_len: usize) -> u64 {
    let path = unsafe { core::str::from_utf8(core::slice::from_raw_parts(path_ptr as *const u8, path_len)).unwrap_or("") };
    let stdin = if stdin_ptr != 0 { Some(unsafe { core::slice::from_raw_parts(stdin_ptr as *const u8, stdin_len) }) } else { None };
    if let Ok((_tid, ch, pid)) = crate::process::spawn_process_with_channel(path, None, stdin) {
        crate::process::register_child_channel(pid, ch);
        if let Some(proc) = crate::process::current_process() { return (pid as u64) | ((proc.alloc_fd(crate::process::FileDescriptor::ChildStdout(pid)) as u64) << 32); }
    }
    !0u64
}

fn sys_kill(pid: u32) -> u64 { if crate::process::kill_process(pid).is_ok() { 0 } else { !0u64 } }

fn sys_waitpid(pid: u32, status_ptr: u64) -> u64 {
    if let Some(ch) = crate::process::get_child_channel(pid) {
        if ch.has_exited() {
            if status_ptr != 0 { unsafe { *(status_ptr as *mut u32) = (ch.exit_code() as u32) << 8; } }
            crate::process::remove_child_channel(pid);
            return pid as u64;
        }
    }
    0
}

fn sys_getrandom(ptr: u64, len: usize) -> u64 {
    let mut buf = alloc::vec![0u8; len.min(256)];
    if crate::rng::fill_bytes(&mut buf).is_ok() { unsafe { core::ptr::copy_nonoverlapping(buf.as_ptr(), ptr as *mut u8, buf.len()); } return buf.len() as u64; }
    !0u64
}

fn sys_time() -> u64 { crate::timer::utc_time_us().unwrap_or(0) }

fn sys_chdir(ptr: u64, len: usize) -> u64 {
    let path = unsafe { core::str::from_utf8(core::slice::from_raw_parts(ptr as *const u8, len)).unwrap_or("") };
    if let Some(proc) = crate::process::current_process() { proc.set_cwd(path); return 0; }
    !0u64
}

fn sys_poll_input_event(ptr: u64, count: usize, timeout_us: u64) -> u64 {
    let deadline = if timeout_us == !0 { !0 } else { crate::timer::uptime_us() + timeout_us };
    if let Some(proc) = crate::process::current_process() {
        loop {
            let mut buf = [0u8; 128];
            let n = proc.terminal_state.lock().read_input(&mut buf);
            if n > 0 { 
                let to_copy = n.min(count);
                unsafe { core::ptr::copy_nonoverlapping(buf.as_ptr(), ptr as *mut u8, to_copy); } 
                return to_copy as u64; 
            }
            if crate::timer::uptime_us() >= deadline { return 0; }
            crate::threading::schedule_blocking(deadline);
        }
    }
    0
}

fn sys_get_cpu_stats(ptr: u64, max: usize) -> u64 {
    let count = max.min(config::MAX_THREADS);
    for i in 0..count {
        let mut stat = ThreadCpuStat { tid: i as u32, total_time_us: crate::threading::get_thread_cpu_time(i), state: crate::threading::get_thread_state(i), ..Default::default() };
        unsafe { core::ptr::write_volatile((ptr as *mut ThreadCpuStat).add(i), stat); }
    }
    count as u64
}
