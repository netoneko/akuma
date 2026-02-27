//! System Call Handlers
//!
//! Implements the syscall interface for user programs.
//! Uses Linux-compatible ABI: syscall number in x8, arguments in x0-x5.

use crate::console;
use crate::config;
use crate::terminal::mode_flags;
use alloc::string::String;
use alloc::vec::Vec;
use alloc::collections::BTreeMap;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use spinning_top::Spinlock;

// ============================================================================
// Kernel Pipe Infrastructure
// ============================================================================

struct KernelPipe {
    buffer: Vec<u8>,
    write_count: u32,
    read_count: u32,
    reader_thread: Option<usize>,
}

static PIPES: Spinlock<BTreeMap<u32, KernelPipe>> = Spinlock::new(BTreeMap::new());
static NEXT_PIPE_ID: AtomicU32 = AtomicU32::new(1);

fn pipe_create() -> u32 {
    let id = NEXT_PIPE_ID.fetch_add(1, Ordering::SeqCst);
    crate::irq::with_irqs_disabled(|| {
        PIPES.lock().insert(id, KernelPipe {
            buffer: Vec::new(),
            write_count: 1,
            read_count: 1,
            reader_thread: None,
        });
    });
    id
}

/// Increment reference count for a duplicated pipe fd (called by dup3 and fork)
pub fn pipe_clone_ref(id: u32, is_write: bool) {
    crate::irq::with_irqs_disabled(|| {
        let mut pipes = PIPES.lock();
        if let Some(pipe) = pipes.get_mut(&id) {
            if is_write {
                pipe.write_count += 1;
            } else {
                pipe.read_count += 1;
            }
        }
    });
}

fn pipe_write(id: u32, data: &[u8]) -> usize {
    crate::irq::with_irqs_disabled(|| {
        let mut pipes = PIPES.lock();
        if let Some(pipe) = pipes.get_mut(&id) {
            pipe.buffer.extend_from_slice(data);
            if let Some(tid) = pipe.reader_thread.take() {
                crate::threading::get_waker_for_thread(tid).wake();
            }
            data.len()
        } else {
            0
        }
    })
}

fn pipe_read(id: u32, buf: &mut [u8]) -> (usize, bool) {
    crate::irq::with_irqs_disabled(|| {
        let mut pipes = PIPES.lock();
        if let Some(pipe) = pipes.get_mut(&id) {
            let n = buf.len().min(pipe.buffer.len());
            if n > 0 {
                buf[..n].copy_from_slice(&pipe.buffer[..n]);
                pipe.buffer.drain(..n);
                (n, false)
            } else if pipe.write_count == 0 {
                (0, true) // EOF — all writers closed
            } else {
                (0, false) // No data yet, writers still open
            }
        } else {
            (0, true) // Pipe gone, treat as EOF
        }
    })
}

pub fn pipe_close_write(id: u32) {
    crate::irq::with_irqs_disabled(|| {
        let mut pipes = PIPES.lock();
        if let Some(pipe) = pipes.get_mut(&id) {
            pipe.write_count = pipe.write_count.saturating_sub(1);
            if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
                crate::safe_print!(128, "[pipe] close_write pipe={} write_count={} read_count={}\n", id, pipe.write_count, pipe.read_count);
            }
            if pipe.write_count == 0 {
                if let Some(tid) = pipe.reader_thread.take() {
                    crate::threading::get_waker_for_thread(tid).wake();
                }
            }
            if pipe.write_count == 0 && pipe.read_count == 0 {
                pipes.remove(&id);
            }
        }
    });
}

pub fn pipe_close_read(id: u32) {
    crate::irq::with_irqs_disabled(|| {
        let mut pipes = PIPES.lock();
        if let Some(pipe) = pipes.get_mut(&id) {
            pipe.read_count = pipe.read_count.saturating_sub(1);
            if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
                crate::safe_print!(128, "[pipe] close_read pipe={} write_count={} read_count={}\n", id, pipe.write_count, pipe.read_count);
            }
            if pipe.write_count == 0 && pipe.read_count == 0 {
                pipes.remove(&id);
            }
        }
    });
}

fn pipe_set_reader_thread(id: u32, tid: usize) {
    crate::irq::with_irqs_disabled(|| {
        let mut pipes = PIPES.lock();
        if let Some(pipe) = pipes.get_mut(&id) {
            pipe.reader_thread = Some(tid);
        }
    });
}

/// Flag to bypass pointer validation during kernel-originated syscall tests
pub static BYPASS_VALIDATION: AtomicBool = AtomicBool::new(false);

/// Syscall numbers (Linux-compatible subset)
pub mod nr {
    pub const EXIT: u64 = 93;
    pub const READ: u64 = 63;
    pub const WRITE: u64 = 64;
    pub const READV: u64 = 65;
    pub const WRITEV: u64 = 66;
    pub const IOCTL: u64 = 29;
    pub const BRK: u64 = 214;
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
    pub const GETSOCKNAME: u64 = 204;
    pub const GETPEERNAME: u64 = 205;
    pub const SETSOCKOPT: u64 = 208;
    pub const GETSOCKOPT: u64 = 209;
    pub const SHUTDOWN: u64 = 210;
    pub const SENDMSG: u64 = 211;
    pub const RECVMSG: u64 = 212;
    pub const CLONE: u64 = 220;
    pub const EXECVE: u64 = 221;
    pub const MUNMAP: u64 = 215; // Linux arm64 munmap
    pub const MREMAP: u64 = 216;    // Linux arm64 mremap
    pub const MMAP: u64 = 222; // Linux arm64 mmap
    pub const GETDENTS64: u64 = 61; // Linux arm64 getdents64
    pub const PSELECT6: u64 = 72;    // Linux arm64 pselect6
    pub const PPOLL: u64 = 73;       // Linux arm64 ppoll
    pub const MKDIRAT: u64 = 34;     // Linux arm64 mkdirat
    pub const UNLINKAT: u64 = 35;    // Linux arm64 unlinkat
    pub const SYMLINKAT: u64 = 36;   // Linux arm64 symlinkat
    pub const LINKAT: u64 = 37;      // Linux arm64 linkat
    pub const RENAMEAT: u64 = 38;    // Linux arm64 renameat
    pub const READLINKAT: u64 = 78;  // Linux arm64 readlinkat
    pub const SET_TID_ADDRESS: u64 = 96;
    pub const EXIT_GROUP: u64 = 94;
    pub const RT_SIGPROCMASK: u64 = 135;
    pub const RT_SIGACTION: u64 = 134; // Linux arm64 rt_sigaction
    pub const RT_SIGSUSPEND: u64 = 133;
    pub const GETRANDOM: u64 = 278;  // Linux arm64 getrandom
    pub const GETCWD: u64 = 17;      // Linux arm64 getcwd
    pub const FCNTL: u64 = 25;       // Linux arm64 fcntl
    pub const DUP: u64 = 23;         // Linux arm64 dup
    pub const FSTATFS: u64 = 44;     // Linux arm64 fstatfs
    pub const DUP3: u64 = 24;        // Linux arm64 dup3
    pub const PIPE2: u64 = 59;       // Linux arm64 pipe2
    pub const NEWFSTATAT: u64 = 79;  // Linux arm64 newfstatat
    pub const FACCESSAT: u64 = 48;   // Linux arm64 faccessat
    pub const CLOCK_GETTIME: u64 = 113; // Linux arm64 clock_gettime
    pub const FACCESSAT2: u64 = 439;    // Linux arm64 faccessat2
    pub const WAIT4: u64 = 260;         // Linux arm64 wait4
    // Custom syscalls (300+)
    pub const RESOLVE_HOST: u64 = 300;
    pub const SPAWN: u64 = 301;      // Spawn a child process, returns (pid, stdout_fd)
    pub const KILL: u64 = 302;       // Kill a process by PID
    pub const WAITPID: u64 = 303;    // Wait for child, returns exit status
    pub const TIME: u64 = 305;        // Get current Unix timestamp (seconds since epoch)
    pub const CHDIR: u64 = 49;        // Linux arm64 chdir
    // Terminal Syscalls (307-313)
    pub const SET_TERMINAL_ATTRIBUTES: u64 = 307;
    pub const GET_TERMINAL_ATTRIBUTES: u64 = 308;
    pub const SET_CURSOR_POSITION: u64 = 309;
    pub const HIDE_CURSOR: u64 = 310;
    pub const SHOW_CURSOR: u64 = 311;
    pub const CLEAR_SCREEN: u64 = 312;
    pub const POLL_INPUT_EVENT: u64 = 313;
    pub const GET_CPU_STATS: u64 = 314;
    pub const SPAWN_EXT: u64 = 315;
    pub const REGISTER_BOX: u64 = 316;
    pub const KILL_BOX: u64 = 317;
    pub const REATTACH: u64 = 318;
    pub const UPTIME: u64 = 319;
    pub const SET_TPIDR_EL0: u64 = 320;
    // Framebuffer Syscalls (321-323)
    pub const FB_INIT: u64 = 321;
    pub const FB_DRAW: u64 = 322;
    pub const FB_INFO: u64 = 323;
    pub const GETPID: u64 = 172;
    pub const GETPPID: u64 = 173;
    pub const GETUID: u64 = 174;
    pub const GETEUID: u64 = 175;
    pub const GETGID: u64 = 176;
    pub const GETEGID: u64 = 177;
    pub const GETTID: u64 = 178;
    // Linux standard numbers
    pub const KILL_LINUX: u64 = 129;
    pub const SETPGID: u64 = 154;
    pub const GETPGID: u64 = 155;
    pub const SETSID: u64 = 157;
    pub const UNAME: u64 = 160;
    pub const FLOCK: u64 = 32;
    pub const UMASK: u64 = 166;
    pub const UTIMENSAT: u64 = 88;
    pub const FDATASYNC: u64 = 83;
    pub const FSYNC: u64 = 82;
    pub const FCHDIR: u64 = 50;
    pub const FCHMOD: u64 = 52;
    pub const FCHMODAT: u64 = 53;
    pub const FCHOWNAT: u64 = 54;
    pub const MADVISE: u64 = 233;
}

/// Thread CPU statistics for top command
#[repr(C, align(8))]
#[derive(Debug, Clone, Copy, Default)]
pub struct ThreadCpuStat {
    pub tid: u32,
    pub pid: u32,
    pub box_id: u64,
    pub total_time_us: u64,
    pub state: u8,
    pub _reserved: [u8; 7],
    pub name: [u8; 16],
}

/// Error code for interrupted syscall
const EINTR: u64 = (-4i64) as u64;
/// Error code for no such file or directory
const ENOENT: u64 = (-2i64) as u64;
/// Error code for bad address
const EFAULT: u64 = (-14i64) as u64;
/// Error code for invalid argument
const EINVAL: u64 = (-22i64) as u64;
/// Error code for permission denied
const EACCES: u64 = (-13i64) as u64;
/// Error code for bad file descriptor
const EBADF: u64 = (-9i64) as u64;
/// Error code for function not implemented
const ENOSYS: u64 = (-38i64) as u64;
const EPERM: u64 = (-1i64) as u64;
const ENOTDIR: u64 = (-20i64) as u64;
const EISDIR: u64 = (-21i64) as u64;
const ENOTEMPTY: u64 = (-39i64) as u64;
const EEXIST: u64 = (-17i64) as u64;
const ENOSPC: u64 = (-28i64) as u64;

/// Encode an exit code into Linux-compatible wait status.
/// Negative codes are treated as signal kills (e.g. -11 → SIGSEGV).
fn encode_wait_status(code: i32) -> u32 {
    if code < 0 {
        // Signal death: low 7 bits = signal number (no core dump bit)
        let sig = (-code) as u32 & 0x7F;
        sig
    } else {
        // Normal exit: (code & 0xFF) << 8
        ((code as u32) & 0xFF) << 8
    }
}
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

/// Validate a user pointer for reading/writing
/// 
/// Pointers must be below the userspace limit (0x40000000)
/// and above the process info page (0x1000).
fn validate_user_ptr(ptr: u64, len: usize) -> bool {
    if BYPASS_VALIDATION.load(Ordering::Acquire) { return true; }
    if ptr < 0x1000 { return false; }
    let end = match ptr.checked_add(len as u64) {
        Some(e) => e,
        None => return false,
    };
    if end > 0x4000_0000 { return false; }
    
    // CRITICAL: Check if the range is actually mapped in the current page tables
    if !crate::mmu::is_current_user_range_mapped(ptr as usize, len) {
        return false;
    }
    
    true
}

/// Copy a null-terminated string from userspace
fn copy_from_user_str(ptr: u64, max_len: usize) -> Result<String, u64> {
    if !BYPASS_VALIDATION.load(Ordering::Acquire) {
        if ptr < 0x1000 || ptr >= 0x4000_0000 { return Err(EFAULT); }
        if !crate::mmu::is_current_user_range_mapped(ptr as usize, 1) { return Err(EFAULT); }
    }
    let mut len = 0;
    while len < max_len {
        let addr = ptr + len as u64;
        if !BYPASS_VALIDATION.load(Ordering::Acquire) {
            if addr >= 0x4000_0000 { return Err(EFAULT); }
            // Check mapping every page boundary
            if addr % 4096 == 0 {
                if !crate::mmu::is_current_user_range_mapped(addr as usize, 1) { return Err(EFAULT); }
            }
        }
        let c = unsafe { *(addr as *const u8) };
        if len < 16 {
            // crate::safe_print!(64, "[syscall] copy_from_user_str: addr={:#x} c={}\n", addr, c as char);
        }
        if c == 0 { break; }
        len += 1;
    }
    if len == max_len {
        let first_bytes = unsafe { core::slice::from_raw_parts(ptr as *const u8, 16) };
        crate::safe_print!(128, "[syscall] copy_from_user_str: not null terminated within {} bytes at {:#x}. First 16 bytes: {:?}\n", max_len, ptr, first_bytes);
        return Err(EINVAL);
    }
    
    let slice = unsafe { core::slice::from_raw_parts(ptr as *const u8, len) };
    match core::str::from_utf8(slice) {
        Ok(s) => Ok(String::from(s)),
        Err(_) => {
            crate::safe_print!(64, "[syscall] copy_from_user_str: invalid UTF-8\n");
            Err(EINVAL)
        },
    }
}

fn sys_pselect6(nfds: usize, readfds_ptr: u64, writefds_ptr: u64, _exceptfds_ptr: u64, timeout_ptr: u64, _sigmask_ptr: u64) -> u64 {
    if nfds == 0 { return 0; }
    const MAX_FDS: usize = 1024;
    if nfds > MAX_FDS { return EINVAL; }
    let nwords = (nfds + 63) / 64;
    let fd_set_bytes = nwords * 8;

    let mut orig_read = [0u64; MAX_FDS / 64];
    let mut orig_write = [0u64; MAX_FDS / 64];

    if readfds_ptr != 0 {
        if !validate_user_ptr(readfds_ptr, fd_set_bytes) { return EFAULT; }
        unsafe { core::ptr::copy_nonoverlapping(readfds_ptr as *const u64, orig_read.as_mut_ptr(), nwords); }
    }
    if writefds_ptr != 0 {
        if !validate_user_ptr(writefds_ptr, fd_set_bytes) { return EFAULT; }
        unsafe { core::ptr::copy_nonoverlapping(writefds_ptr as *const u64, orig_write.as_mut_ptr(), nwords); }
    }

    let infinite = timeout_ptr == 0;
    let timeout_us = if !infinite {
        if !validate_user_ptr(timeout_ptr, 16) { return EFAULT; }
        let ts = unsafe { &*(timeout_ptr as *const Timespec) };
        (ts.tv_sec as u64) * 1000_000 + (ts.tv_nsec as u64) / 1000
    } else {
        0
    };

    let start_time = crate::timer::uptime_us();

    loop {
        crate::smoltcp_net::poll();
        let mut ready_count: u64 = 0;
        let mut out_read = [0u64; MAX_FDS / 64];
        let mut out_write = [0u64; MAX_FDS / 64];

        for fd in 0..nfds {
            let word = fd / 64;
            let bit = fd % 64;
            let mask = 1u64 << bit;

            let in_read = orig_read[word] & mask != 0;
            let in_write = orig_write[word] & mask != 0;
            if !in_read && !in_write { continue; }

            let socket_idx = if fd > 2 {
                if let Some(proc) = crate::process::current_process() {
                    if let Some(crate::process::FileDescriptor::Socket(idx)) = proc.get_fd(fd as u32) {
                        Some(idx)
                    } else { None }
                } else { None }
            } else { None };

            if in_read {
                let readable = if fd == 0 {
                    crate::process::current_channel().map_or(false, |ch| ch.has_stdin_data())
                } else if let Some(idx) = socket_idx {
                    if socket::is_udp_socket(idx) {
                        socket_get_udp_handle(idx).map_or(false, |h| crate::smoltcp_net::udp_can_recv(h))
                    } else {
                        socket_can_recv_tcp(idx)
                    }
                } else {
                    fd > 2
                };
                if readable { out_read[word] |= mask; ready_count += 1; }
            }

            if in_write {
                let writable = if let Some(idx) = socket_idx {
                    if socket::is_udp_socket(idx) {
                        socket_get_udp_handle(idx).map_or(false, |h| crate::smoltcp_net::udp_can_send(h))
                    } else {
                        true
                    }
                } else {
                    true
                };
                if writable { out_write[word] |= mask; ready_count += 1; }
            }
        }

        if ready_count > 0 {
            if readfds_ptr != 0 { unsafe { core::ptr::copy_nonoverlapping(out_read.as_ptr(), readfds_ptr as *mut u64, nwords); } }
            if writefds_ptr != 0 { unsafe { core::ptr::copy_nonoverlapping(out_write.as_ptr(), writefds_ptr as *mut u64, nwords); } }
            return ready_count;
        }

        if !infinite && (crate::timer::uptime_us() - start_time) >= timeout_us {
            if readfds_ptr != 0 { unsafe { core::ptr::write_bytes(readfds_ptr as *mut u8, 0, fd_set_bytes); } }
            if writefds_ptr != 0 { unsafe { core::ptr::write_bytes(writefds_ptr as *mut u8, 0, fd_set_bytes); } }
            return 0;
        }

        crate::threading::yield_now();
    }
}

fn sys_ppoll(fds_ptr: u64, nfds: usize, timeout_ptr: u64, _sigmask: u64) -> u64 {
    if nfds == 0 { return 0; }
    if !validate_user_ptr(fds_ptr, nfds * 8) { return EFAULT; }

    let infinite = timeout_ptr == 0;
    let timeout_us = if !infinite {
        if !validate_user_ptr(timeout_ptr, 16) { return EFAULT; }
        let ts = unsafe { &*(timeout_ptr as *const Timespec) };
        (ts.tv_sec as u64) * 1000_000 + (ts.tv_nsec as u64) / 1000
    } else {
        0
    };

    let start_time = crate::timer::uptime_us();

    loop {
        crate::smoltcp_net::poll();
        let mut ready_count = 0;
        unsafe {
            let fds = core::slice::from_raw_parts_mut(fds_ptr as *mut PollFd, nfds);
            for fd in fds.iter_mut() {
                fd.revents = 0;

                if fd.fd < 0 { continue; }

                // Determine if this fd is a socket
                let socket_idx = if fd.fd > 2 {
                    if let Some(proc) = crate::process::current_process() {
                        if let Some(crate::process::FileDescriptor::Socket(idx)) = proc.get_fd(fd.fd as u32) {
                            Some(idx)
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                } else {
                    None
                };

                // 1. Check for POLLIN (Read)
                if fd.events & 1 != 0 {
                    if fd.fd == 0 {
                        if let Some(ch) = crate::process::current_channel() {
                            if ch.has_stdin_data() {
                                fd.revents |= 1;
                            }
                        }
                    } else if let Some(idx) = socket_idx {
                        if socket::is_udp_socket(idx) {
                            if let Some(handle) = socket_get_udp_handle(idx) {
                                if crate::smoltcp_net::udp_can_recv(handle) {
                                    fd.revents |= 1;
                                }
                            }
                        } else {
                            if socket_can_recv_tcp(idx) {
                                fd.revents |= 1;
                            }
                        }
                    } else if fd.fd > 2 {
                        fd.revents |= 1;
                    }
                }

                // 2. Check for POLLOUT (Write)
                if fd.events & 4 != 0 {
                    if let Some(idx) = socket_idx {
                        if socket::is_udp_socket(idx) {
                            if let Some(handle) = socket_get_udp_handle(idx) {
                                if crate::smoltcp_net::udp_can_send(handle) {
                                    fd.revents |= 4;
                                }
                            }
                        } else {
                            fd.revents |= 4;
                        }
                    } else if fd.fd == 1 || fd.fd == 2 || fd.fd > 2 {
                        fd.revents |= 4;
                    }
                }

                if fd.revents != 0 {
                    ready_count += 1;
                }
            }
        }

        if ready_count > 0 {
            return ready_count as u64;
        }

        if !infinite && (crate::timer::uptime_us() - start_time) >= timeout_us {
            return 0;
        }

        crate::threading::yield_now();
    }
}

#[repr(C)]
struct PollFd {
    fd: i32,
    events: i16,
    revents: i16,
}

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

    if crate::config::SYSCALL_DEBUG_IO_ENABLED && syscall_num != nr::WRITE && syscall_num != nr::READ && syscall_num != nr::READV && syscall_num != nr::WRITEV && syscall_num != nr::IOCTL && syscall_num != nr::PSELECT6 && syscall_num != nr::PPOLL {
        crate::safe_print!(128, "[SC] nr={} a0=0x{:x} a1=0x{:x} a2=0x{:x}\n", syscall_num, args[0], args[1], args[2]);
    }

    match syscall_num {
        nr::EXIT => sys_exit(args[0] as i32),
        nr::READ => sys_read(args[0], args[1], args[2] as usize),
        nr::WRITE => sys_write(args[0], args[1], args[2] as usize),
        nr::READV => sys_readv(args[0], args[1], args[2] as usize),
        nr::WRITEV => sys_writev(args[0], args[1], args[2] as usize),
        nr::IOCTL => sys_ioctl(args[0] as u32, args[1] as u32, args[2]),
        nr::DUP => sys_dup(args[0] as u32),
        nr::FSTATFS => sys_fstatfs(args[0] as u32, args[1]),
        nr::DUP3 => sys_dup3(args[0] as u32, args[1] as u32, args[2] as u32),
        nr::PIPE2 => sys_pipe2(args[0], args[1] as u32),
        nr::BRK => sys_brk(args[0] as usize),
        nr::OPENAT => sys_openat(args[0] as i32, args[1], args[2] as u32, args[3] as u32),
        nr::CLOSE => sys_close(args[0] as u32),
        nr::LSEEK => sys_lseek(args[0] as u32, args[1] as i64, args[2] as i32),
        nr::FSTAT => sys_fstat(args[0] as u32, args[1]),
        nr::NANOSLEEP => sys_nanosleep(args[0], args[1]),
        nr::SOCKET => sys_socket(args[0] as i32, args[1] as i32, args[2] as i32),
        nr::BIND => sys_bind(args[0] as u32, args[1], args[2] as usize),
        nr::LISTEN => sys_listen(args[0] as u32, args[1] as i32),
        nr::ACCEPT => sys_accept(args[0] as u32, args[1], args[2]),
        nr::CONNECT => sys_connect(args[0] as u32, args[1], args[2] as usize),
        nr::SENDTO => sys_sendto(args[0] as u32, args[1], args[2] as usize, args[3] as i32, args[4], args[5] as usize),
        nr::RECVFROM => sys_recvfrom(args[0] as u32, args[1], args[2] as usize, args[3] as i32, args[4], args[5]),
        nr::GETSOCKNAME => sys_getsockname(args[0] as u32, args[1], args[2]),
        nr::GETPEERNAME => 0, // stub
        nr::SETSOCKOPT => 0, // stub — no socket options to set
        nr::GETSOCKOPT => 0, // stub
        nr::SHUTDOWN => sys_shutdown(args[0] as u32, args[1] as i32),
        nr::SENDMSG => sys_sendmsg(args[0] as u32, args[1], args[2] as i32),
        nr::RECVMSG => sys_recvmsg(args[0] as u32, args[1], args[2] as i32),
        nr::MREMAP => sys_mremap(args[0] as usize, args[1] as usize, args[2] as usize, args[3] as u32),
        nr::MMAP => sys_mmap(args[0] as usize, args[1] as usize, args[2] as u32, args[3] as u32, args[4] as i32, args[5] as usize),
        nr::MUNMAP => sys_munmap(args[0] as usize, args[1] as usize),
        nr::CLONE => sys_clone(args[0], args[1], args[2], args[3], args[4]),
        nr::EXECVE => sys_execve(args[0], args[1], args[2]),
        nr::UPTIME => sys_uptime(),
        nr::RESOLVE_HOST => sys_resolve_host(args[0], args[1] as usize, args[2]),
        nr::GETDENTS64 => sys_getdents64(args[0] as u32, args[1], args[2] as usize),
        nr::PSELECT6 => sys_pselect6(args[0] as usize, args[1], args[2], args[3], args[4], args[5]),
        nr::PPOLL => sys_ppoll(args[0], args[1] as usize, args[2], args[3]),
        nr::MKDIRAT => sys_mkdirat(args[0] as i32, args[1], args[2] as u32),
        nr::UNLINKAT => sys_unlinkat(args[0] as i32, args[1], args[2] as u32),
        nr::SYMLINKAT => sys_symlinkat(args[0], args[1] as i32, args[2]),
        nr::LINKAT => sys_linkat(args[0] as i32, args[1], args[2] as i32, args[3], args[4] as u32),
        nr::RENAMEAT => sys_renameat(args[0] as i32, args[1], args[2] as i32, args[3]),
        nr::READLINKAT => sys_readlinkat(args[0] as i32, args[1], args[2], args[3] as usize),
        nr::SPAWN => sys_spawn(args[0], args[1], args[2], args[3], args[4] as usize, args[5]),
        nr::KILL => sys_kill(args[0] as u32, args[1] as u32),
        nr::WAITPID => sys_waitpid(args[0] as u32, args[1]),
        nr::GETRANDOM => sys_getrandom(args[0], args[1] as usize),
        nr::TIME => sys_time(),
        nr::CHDIR => sys_chdir(args[0]),
        nr::FCHDIR => sys_fchdir(args[0] as u32),
        nr::SET_TERMINAL_ATTRIBUTES => sys_set_terminal_attributes(args[0], args[1], args[2]),
        nr::GET_TERMINAL_ATTRIBUTES => sys_get_terminal_attributes(args[0], args[1]),
        nr::SET_CURSOR_POSITION => sys_set_cursor_position(args[0], args[1]),
        nr::HIDE_CURSOR => sys_hide_cursor(),
        nr::SHOW_CURSOR => sys_show_cursor(),
        nr::CLEAR_SCREEN => sys_clear_screen(),
        nr::POLL_INPUT_EVENT => sys_poll_input_event(args[0], args[1] as usize, args[2]),
        nr::GET_CPU_STATS => sys_get_cpu_stats(args[0], args[1] as usize),
        nr::SPAWN_EXT => sys_spawn_ext(args[0], args[1] as usize, args[2], args[3], args[4], args[5]),
        nr::REGISTER_BOX => sys_register_box(args[0] as u64, args[1], args[2] as usize, args[3], args[4] as usize, args[5] as u32),
        nr::KILL_BOX => sys_kill_box(args[0] as u64),
        nr::REATTACH => sys_reattach(args[0] as u32),
        nr::SET_TID_ADDRESS => 1, // Return dummy TID
        nr::EXIT_GROUP => sys_exit(args[0] as i32),
        nr::RT_SIGPROCMASK => 0,  // Success (do nothing)
        nr::RT_SIGSUSPEND => 0,   // Success (do nothing)
        nr::RT_SIGACTION => 0,    // Success (do nothing)
        nr::GETCWD => sys_getcwd(args[0], args[1] as usize),
        nr::FCNTL => sys_fcntl(args[0] as u32, args[1] as u32, args[2]),
        nr::NEWFSTATAT => sys_newfstatat(args[0] as i32, args[1], args[2], args[3] as u32),
        nr::FACCESSAT => sys_faccessat2(args[0] as i32, args[1], args[2] as u32, 0),
        nr::CLOCK_GETTIME => sys_clock_gettime(args[0] as u32, args[1]),
        nr::FACCESSAT2 => sys_faccessat2(args[0] as i32, args[1], args[2] as u32, args[3] as u32),
        nr::WAIT4 => sys_wait4(args[0] as i32, args[1], args[2] as i32, args[3]),
        nr::SET_TPIDR_EL0 => sys_set_tpidr_el0(args[0]),
        nr::FB_INIT => sys_fb_init(args[0] as u32, args[1] as u32),
        nr::FB_DRAW => sys_fb_draw(args[0], args[1] as usize),
        nr::FB_INFO => sys_fb_info(args[0]),
        nr::GETPID => sys_getpid(),
        nr::GETPPID => sys_getppid(),
        nr::GETUID => 0,
        nr::GETEUID => sys_geteuid(),
        nr::GETGID => 0,
        nr::GETEGID => 0,
        nr::GETTID => crate::threading::current_thread_id() as u64,
        nr::KILL_LINUX => sys_kill(args[0] as u32, args[1] as u32),
        nr::SETPGID => sys_setpgid(args[0] as u32, args[1] as u32),
        nr::GETPGID => sys_getpgid(args[0] as u32),
        nr::SETSID => sys_setsid(),
        nr::UNAME => sys_uname(args[0]),
        nr::FLOCK => 0, // stub — single-user OS, no contention
        nr::UMASK => 0o022, // stub — return default umask, ignore argument
        nr::UTIMENSAT => 0, // stub — timestamps not critical
        nr::FDATASYNC => 0, // stub — single block device, always coherent
        nr::FSYNC => 0, // stub — single block device, always coherent
        nr::FCHMOD => 0, // stub — no permission enforcement
        nr::FCHMODAT => 0, // stub — no permission enforcement
        nr::FCHOWNAT => 0, // stub — single-user OS, no ownership
        nr::MADVISE => sys_madvise(args[0] as usize, args[1] as usize, args[2] as i32),
        _ => {
            crate::safe_print!(128, "[syscall] Unknown syscall: {} (args: [0x{:x}, 0x{:x}, 0x{:x}, 0x{:x}, 0x{:x}, 0x{:x}])\n",
                syscall_num, args[0], args[1], args[2], args[3], args[4], args[5]);
            ENOSYS
        }
    }
}

fn sys_set_tpidr_el0(address: u64) -> u64 {
    unsafe {
        core::arch::asm!("msr tpidr_el0, {}", "isb", in(reg) address);
    }
    0
}

fn sys_setpgid(pid: u32, pgid: u32) -> u64 {
    let target_pid = if pid == 0 {
        match crate::process::read_current_pid() { Some(p) => p, None => return !0u64 }
    } else {
        pid
    };

    let target_pgid = if pgid == 0 { target_pid } else { pgid };

    if let Some(proc) = crate::process::lookup_process(target_pid) {
        if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
            crate::safe_print!(128, "[syscall] setpgid(pid={}, pgid={}): old={}, new={}\n", target_pid, pgid, proc.pgid, target_pgid);
        }
        proc.pgid = target_pgid;
        0
    } else {
        ENOENT
    }
}

fn sys_getpgid(pid: u32) -> u64 {
    let target_pid = if pid == 0 {
        match crate::process::read_current_pid() { 
            Some(p) => p, 
            None => {
                // System thread fallback: use TID as PGID
                let tid = crate::threading::current_thread_id();
                if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
                    crate::safe_print!(128, "[syscall] getpgid(0) kernel fallback: returning TID {}\n", tid);
                }
                return tid as u64;
            }
        }
    } else {
        pid
    };

    if let Some(proc) = crate::process::lookup_process(target_pid) {
        if crate::config::SYSCALL_DEBUG_INFO_ENABLED && pid == 0 {
            crate::safe_print!(128, "[syscall] getpgid(0) for PID {}: returning PGID {}\n", target_pid, proc.pgid);
        }
        proc.pgid as u64
    } else {
        // If it's a system thread (not in process table), return its TID
        if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
            crate::safe_print!(128, "[syscall] getpgid({}) not found: returning TID fallback {}\n", target_pid, target_pid);
        }
        target_pid as u64
    }
}

fn sys_setsid() -> u64 {
    if let Some(proc) = crate::process::current_process() {
        proc.pgid = proc.pid;
        proc.pid as u64 // New SID is the PID
    } else {
        !0u64
    }
}

fn sys_uname(buf: u64) -> u64 {
    const FIELD_LEN: usize = 65;

    fn write_field(base: *mut u8, offset: usize, value: &[u8]) {
        unsafe {
            let dst = base.add(offset * FIELD_LEN);
            let len = value.len().min(FIELD_LEN - 1);
            core::ptr::copy_nonoverlapping(value.as_ptr(), dst, len);
            // Ensure null-terminated (buffer was zeroed)
        }
    }

    let ptr = buf as *mut u8;
    // Zero the entire struct (6 fields x 65 bytes)
    unsafe { core::ptr::write_bytes(ptr, 0, FIELD_LEN * 6); }

    write_field(ptr, 0, b"Akuma");          // sysname
    write_field(ptr, 1, b"akuma");          // nodename
    write_field(ptr, 2, b"0.1.0");          // release
    write_field(ptr, 3, b"Akuma OS");       // version
    write_field(ptr, 4, b"aarch64");        // machine
    write_field(ptr, 5, b"(none)");         // domainname

    0
}

fn sys_exit(code: i32) -> u64 {
    if let Some(proc) = crate::process::current_process() {
        proc.exited = true;
        proc.exit_code = code;
        proc.state = crate::process::ProcessState::Zombie(code);
    }
    code as u64
}

fn sys_ioctl(fd: u32, cmd: u32, arg: u64) -> u64 {
    // Command constants from Linux
    const TCGETS: u32 = 0x5401;
    const TCSETS: u32 = 0x5402;
    const TCSETSW: u32 = 0x5403;
    const TCSETSF: u32 = 0x5404;
    const TIOCGWINSZ: u32 = 0x5413;
    const TIOCGPGRP: u32 = 0x540f;
    const TIOCSPGRP: u32 = 0x5410;

    if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
        crate::safe_print!(128, "[syscall] ioctl(fd={}, cmd=0x{:x}, arg=0x{:x})\n", fd, cmd, arg);
    }

    let proc = match crate::process::current_process() { Some(p) => p, None => return !0u64 };
    
    // We only support terminal ioctls on stdin/stdout for now
    if fd > 2 {
        return (-(25i64)) as u64; // ENOTTY
    }

    let result = match cmd {
        TCGETS => {
            if !validate_user_ptr(arg, 36) { return EFAULT; }
            let term_state_lock = match crate::process::current_terminal_state() {
                Some(state) => state,
                None => return (-(12i64)) as u64, // ENOMEM
            };
            let ts = term_state_lock.lock();
            unsafe {
                let ptr = arg as *mut u32;
                *ptr.add(0) = ts.iflag;
                *ptr.add(1) = ts.oflag;
                *ptr.add(2) = ts.cflag;
                *ptr.add(3) = ts.lflag;
                
                let cc_ptr = ptr.add(4) as *mut u8;
                core::ptr::copy_nonoverlapping(ts.cc.as_ptr(), cc_ptr, 20);
            }
            0
        }
        TCSETS | TCSETSW | TCSETSF => {
            if !validate_user_ptr(arg, 36) { return EFAULT; }
            let term_state_lock = match crate::process::current_terminal_state() {
                Some(state) => state,
                None => return (-(12i64)) as u64, // ENOMEM
            };
            let mut ts = term_state_lock.lock();
            unsafe {
                let ptr = arg as *const u32;
                ts.iflag = *ptr.add(0);
                ts.oflag = *ptr.add(1);
                ts.cflag = *ptr.add(2);
                ts.lflag = *ptr.add(3);
                
                if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
                    crate::safe_print!(128, "[syscall] TCSETS: iflag=0x{:x} oflag=0x{:x} cflag=0x{:x} lflag=0x{:x}\n",
                        ts.iflag, ts.oflag, ts.cflag, ts.lflag);
                }
                
                let cc_ptr = ptr.add(4) as *const u8;
                core::ptr::copy_nonoverlapping(cc_ptr, ts.cc.as_mut_ptr(), 20);
            }

            // Sync with process channel
            if let Some(ch) = crate::process::current_channel() {
                let is_raw = (ts.lflag & mode_flags::ICANON) == 0;
                ch.set_raw_mode(is_raw);
                if cmd == TCSETSF {
                    ch.flush_stdin();
                }
            }
            0
        }
        TIOCGWINSZ => {
            if !validate_user_ptr(arg, 8) { return EFAULT; }
            let term_state_lock = match crate::process::current_terminal_state() {
                Some(state) => state,
                None => return (-(12i64)) as u64, // ENOMEM
            };
            let ts = term_state_lock.lock();
            unsafe {
                let ptr = arg as *mut u16;
                *ptr.add(0) = ts.term_height; // rows
                *ptr.add(1) = ts.term_width;  // cols
                *ptr.add(2) = 0;  // xpixel
                *ptr.add(3) = 0;  // ypixel
            }
            0
        }
        TIOCGPGRP => {
            if !validate_user_ptr(arg, 4) { return EFAULT; }
            let term_state_lock = match crate::process::current_terminal_state() {
                Some(state) => state,
                None => return (-(12i64)) as u64, // ENOMEM
            };
            let ts = term_state_lock.lock();
            unsafe {
                let pgid = ts.foreground_pgid;
                if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
                    crate::safe_print!(128, "[syscall] TIOCGPGRP: returning foreground_pgid {}\n", pgid);
                }
                *(arg as *mut u32) = pgid;
            }
            0
        }
        TIOCSPGRP => {
            if !validate_user_ptr(arg, 4) { return EFAULT; }
            let term_state_lock = match crate::process::current_terminal_state() {
                Some(state) => state,
                None => return (-(12i64)) as u64, // ENOMEM
            };
            let mut ts = term_state_lock.lock();
            unsafe {
                let pgid = *(arg as *const u32);
                if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
                    crate::safe_print!(128, "[syscall] TIOCSPGRP: setting foreground_pgid to {}\n", pgid);
                }
                ts.foreground_pgid = pgid;
            }
            0
        }
        _ => (-(25i64)) as u64, // ENOTTY
    };

    if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
        crate::safe_print!(128, "[syscall] ioctl result={}\n", result as i64);
    }
    result
}

fn sys_read(fd_num: u64, buf_ptr: u64, count: usize) -> u64 {
    if !validate_user_ptr(buf_ptr, count) { return EFAULT; }
    let proc = match crate::process::current_process() { Some(p) => p, None => return !0u64 };
    let fd = match proc.get_fd(fd_num as u32) { Some(e) => e, None => return !0u64 };
    
    if crate::config::SYSCALL_DEBUG_INFO_ENABLED && fd_num == 0 {
        crate::safe_print!(128, "[syscall] read(stdin, count={})\n", count);
    }

    match fd {
        crate::process::FileDescriptor::Stdin => {
            let ch = match crate::process::current_channel() {
                Some(c) => c,
                None => {
                    // Fallback for processes without a channel (unlikely in modern Akuma)
                    let mut temp = alloc::vec![0u8; count];
                    let n = proc.read_stdin(&mut temp);
                    if n > 0 { unsafe { core::ptr::copy_nonoverlapping(temp.as_ptr(), buf_ptr as *mut u8, n); } }
                    if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
                        crate::safe_print!(128, "[syscall] read(stdin) fallback returned {}\n", n);
                    }
                    return n as u64;
                }
            };

            let mut kernel_buf = alloc::vec![0u8; count];
            
            // Blocking read loop
            loop {
                // Skip TTY processing for piped stdin — pipes are not TTYs.
                // A closed stdin means data was pre-loaded (pipeline) or absent,
                // either way it's not an interactive TTY.
                let is_pipe = ch.is_stdin_closed();

                // In canonical mode, check for already-completed lines first
                if !is_pipe {
                    let term_state_lock = crate::process::current_terminal_state();
                    if let Some(ref ts_lock) = term_state_lock {
                        let mut ts = ts_lock.lock();
                        if (ts.lflag & crate::terminal::mode_flags::ICANON) != 0
                            && !ts.canon_ready.is_empty()
                        {
                            let to_read = count.min(ts.canon_ready.len());
                            for i in 0..to_read {
                                kernel_buf[i] = ts.canon_ready.pop_front().unwrap();
                            }
                            drop(ts);
                            unsafe {
                                core::ptr::copy_nonoverlapping(
                                    kernel_buf.as_ptr(),
                                    buf_ptr as *mut u8,
                                    to_read,
                                );
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
                        let term_state_lock = crate::process::current_terminal_state();
                        if let Some(ref ts_lock) = term_state_lock {
                            let mut ts = ts_lock.lock();

                            // ICRNL: Map CR to NL on input
                            if (ts.iflag & crate::terminal::mode_flags::ICRNL) != 0 {
                                for i in 0..n {
                                    if kernel_buf[i] == b'\r' {
                                        kernel_buf[i] = b'\n';
                                    }
                                }
                            }

                            if (ts.lflag & crate::terminal::mode_flags::ICANON) != 0 {
                                // Canonical mode: line-buffer input, handle erase/kill
                                let echo = (ts.lflag & crate::terminal::mode_flags::ECHO) != 0;
                                let echoe = (ts.lflag & crate::terminal::mode_flags::ECHOE) != 0;
                                let echonl = (ts.lflag & crate::terminal::mode_flags::ECHONL) != 0;
                                let onlcr = (ts.oflag & crate::terminal::mode_flags::ONLCR) != 0;
                                let verase = ts.cc[crate::terminal::cc_index::VERASE];
                                let veof = ts.cc[crate::terminal::cc_index::VEOF];
                                let vkill = ts.cc[crate::terminal::cc_index::VKILL];

                                for i in 0..n {
                                    let byte = kernel_buf[i];

                                    if byte == verase || byte == 0x08 {
                                        if ts.canon_buffer.pop().is_some() && echoe {
                                            ch.write(b"\x08 \x08");
                                        }
                                    } else if byte == vkill && vkill != 0 {
                                        let erased = ts.canon_buffer.len();
                                        ts.canon_buffer.clear();
                                        if echoe {
                                            for _ in 0..erased {
                                                ch.write(b"\x08 \x08");
                                            }
                                        }
                                    } else if byte == veof && veof != 0 {
                                        if !ts.canon_buffer.is_empty() {
                                            let line: Vec<u8> = ts.canon_buffer.drain(..).collect();
                                            ts.canon_ready.extend(line);
                                        } else {
                                            drop(ts);
                                            return 0; // EOF
                                        }
                                    } else if byte == b'\n' {
                                        ts.canon_buffer.push(byte);
                                        if echo || echonl {
                                            if onlcr {
                                                ch.write(b"\r\n");
                                            } else {
                                                ch.write(b"\n");
                                            }
                                        }
                                        let line: Vec<u8> = ts.canon_buffer.drain(..).collect();
                                        ts.canon_ready.extend(line);
                                    } else {
                                        ts.canon_buffer.push(byte);
                                        if echo {
                                            ch.write(&[byte]);
                                        }
                                    }
                                }

                                if !ts.canon_ready.is_empty() {
                                    let to_read = count.min(ts.canon_ready.len());
                                    for i in 0..to_read {
                                        kernel_buf[i] = ts.canon_ready.pop_front().unwrap();
                                    }
                                    drop(ts);
                                    unsafe {
                                        core::ptr::copy_nonoverlapping(
                                            kernel_buf.as_ptr(),
                                            buf_ptr as *mut u8,
                                            to_read,
                                        );
                                    }
                                    if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
                                        crate::safe_print!(128, "[syscall] read(stdin) returned {} bytes (canonical)\n", to_read);
                                    }
                                    return to_read as u64;
                                }
                                // No complete line yet, keep waiting for more input
                                continue;
                            } else {
                                // Non-canonical mode: echo and return immediately
                                if (ts.lflag & crate::terminal::mode_flags::ECHO) != 0 {
                                    if (ts.oflag & crate::terminal::mode_flags::ONLCR) != 0 {
                                        let mut echo_buf = Vec::with_capacity(n * 2);
                                        for i in 0..n {
                                            if kernel_buf[i] == b'\n' {
                                                echo_buf.extend_from_slice(b"\r\n");
                                            } else {
                                                echo_buf.push(kernel_buf[i]);
                                            }
                                        }
                                        if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
                                            crate::safe_print!(128, "[syscall] read: echoing {} bytes (ONLCR mapped)\n", echo_buf.len());
                                        }
                                        ch.write(&echo_buf);
                                    } else {
                                        if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
                                            crate::safe_print!(128, "[syscall] read: echoing {} bytes\n", n);
                                        }
                                        ch.write(&kernel_buf[..n]);
                                    }
                                }
                            }
                        }
                    }

                    unsafe { core::ptr::copy_nonoverlapping(kernel_buf.as_ptr(), buf_ptr as *mut u8, n); }
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

                // Check for EOF if channel is closed
                if ch.is_stdin_closed() {
                    // In canonical mode, deliver any partially buffered line before EOF
                    if !is_pipe {
                        let term_state_lock = crate::process::current_terminal_state();
                        if let Some(ref ts_lock) = term_state_lock {
                            let mut ts = ts_lock.lock();
                            if (ts.lflag & crate::terminal::mode_flags::ICANON) != 0
                                && !ts.canon_buffer.is_empty()
                            {
                                let line: Vec<u8> = ts.canon_buffer.drain(..).collect();
                                ts.canon_ready.extend(line);
                                let to_read = count.min(ts.canon_ready.len());
                                for i in 0..to_read {
                                    kernel_buf[i] = ts.canon_ready.pop_front().unwrap();
                                }
                                drop(ts);
                                unsafe {
                                    core::ptr::copy_nonoverlapping(
                                        kernel_buf.as_ptr(),
                                        buf_ptr as *mut u8,
                                        to_read,
                                    );
                                }
                                return to_read as u64;
                            }
                        }
                    }
                    if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
                        crate::safe_print!(128, "[syscall] read(stdin) returned 0 (EOF)\n");
                    }
                    return 0; // EOF
                }

                // Check for interrupt
                if crate::process::is_current_interrupted() {
                    return EINTR;
                }

                // Register waker and block
                let term_state_lock = match crate::process::current_terminal_state() {
                    Some(state) => state,
                    None => {
                        if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
                            crate::safe_print!(128, "[syscall] read(stdin) no terminal state, EOF\n");
                        }
                        return 0;
                    }
                };

                {
                    crate::threading::disable_preemption();
                    let mut term_state = term_state_lock.lock();
                    let thread_id = crate::threading::current_thread_id();
                    term_state.set_input_waker(crate::threading::get_waker_for_thread(thread_id));
                    crate::threading::enable_preemption();
                }

                // Yield until woken by new input
                crate::threading::schedule_blocking(u64::MAX);

                // Clear waker
                {
                    crate::threading::disable_preemption();
                    let mut term_state = term_state_lock.lock();
                    term_state.input_waker.lock().take();
                    crate::threading::enable_preemption();
                }
            }
        }
        crate::process::FileDescriptor::File(ref f) => {
            let limit = 64 * 1024;
            let to_read = count.min(limit);
            let mut temp = alloc::vec![0u8; to_read];

            match crate::fs::read_at(&f.path, f.position, &mut temp) {
                Ok(n) => {
                    if n > 0 {
                        unsafe { core::ptr::copy_nonoverlapping(temp.as_ptr(), buf_ptr as *mut u8, n); }
                        proc.update_fd(fd_num as u32, |entry| if let crate::process::FileDescriptor::File(file) = entry { file.position += n; });
                    }
                    if crate::config::SYSCALL_DEBUG_IO_ENABLED {
                        crate::safe_print!(256, "[syscall] read(fd={}, file={}, pos={}, req={}) = {}\n", fd_num, &f.path, f.position, to_read, n);
                    }
                    n as u64
                }
                Err(_) => !0u64
            }
        }
        crate::process::FileDescriptor::Socket(idx) => {
            let buf = unsafe { core::slice::from_raw_parts_mut(buf_ptr as *mut u8, count) };
            let result = crate::socket::socket_recv(idx, buf);
            if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
                match &result {
                    Ok(n) => crate::safe_print!(128, "[syscall] read(socket fd={}, req={}) = {}\n", fd_num, count, n),
                    Err(e) => crate::safe_print!(128, "[syscall] read(socket fd={}, req={}) = err {}\n", fd_num, count, *e as i64),
                }
            }
            match result {
                Ok(n) => n as u64,
                Err(e) => (-(e as i64)) as u64,
            }
        }
        crate::process::FileDescriptor::ChildStdout(child_pid) => {
            if let Some(ch) = crate::process::get_child_channel(child_pid) {
                let mut temp = alloc::vec![0u8; count];
                let n = ch.read(&mut temp);
                if n > 0 { unsafe { core::ptr::copy_nonoverlapping(temp.as_ptr(), buf_ptr as *mut u8, n); } }
                n as u64
            } else {
                !0u64
            }
        }
        crate::process::FileDescriptor::PipeRead(pipe_id) => {
            let mut temp = alloc::vec![0u8; count];
            loop {
                let (n, eof) = pipe_read(pipe_id, &mut temp);
                if n > 0 {
                    unsafe { core::ptr::copy_nonoverlapping(temp.as_ptr(), buf_ptr as *mut u8, n); }
                    return n as u64;
                }
                if eof {
                    return 0;
                }
                if crate::process::is_current_interrupted() {
                    return EINTR;
                }
                let tid = crate::threading::current_thread_id();
                pipe_set_reader_thread(pipe_id, tid);
                crate::threading::schedule_blocking(u64::MAX);
            }
        }
        _ => !0u64
    }
}

fn sys_write(fd_num: u64, buf_ptr: u64, count: usize) -> u64 {
    if !validate_user_ptr(buf_ptr, count) { return EFAULT; }
    let proc = match crate::process::current_process() { Some(p) => p, None => return !0u64 };
    let fd = match proc.get_fd(fd_num as u32) { Some(e) => e, None => return !0u64 };
    let buf = unsafe { core::slice::from_raw_parts(buf_ptr as *const u8, count) };

    match fd {
        crate::process::FileDescriptor::Stdout | crate::process::FileDescriptor::Stderr => {
            if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
                let display_len = count.min(32);
                let mut snippet = [0u8; 32];
                let n = display_len.min(snippet.len());
                snippet[..n].copy_from_slice(&buf[..n]);
                // Simple printable check
                for byte in &mut snippet[..n] {
                    if *byte < 32 || *byte > 126 { *byte = b'.'; }
                }
                let snippet_str = core::str::from_utf8(&snippet[..n]).unwrap_or("...");
                crate::safe_print!(128, "[syscall] write(fd={}, count={}) \"{}\"\n", fd_num, count, snippet_str);
            }

            // Write to process channel (for SSH)
            if let Some(ch) = crate::process::current_channel() {
                // Skip ONLCR for piped processes — the pipeline handler
                // (execute_external) does its own \n→\r\n translation for the
                // final stage output. Translating here too would double it.
                let translate = if ch.is_stdin_closed() {
                    false
                } else {
                    let term_state_opt = crate::process::current_terminal_state();
                    if let Some(ts_lock) = term_state_opt {
                        let ts = ts_lock.lock();
                        (ts.oflag & mode_flags::ONLCR) != 0
                    } else {
                        true
                    }
                };

                if translate {
                    let mut translated = Vec::with_capacity(buf.len() + 8);
                    for &byte in buf {
                        if byte == b'\n' {
                            translated.extend_from_slice(b"\r\n");
                        } else {
                            translated.push(byte);
                        }
                    }
                    ch.write(&translated);
                } else {
                    ch.write(buf);
                }
            }
            
            // Also write to procfs/kernel log
            if config::STDOUT_TO_KERNEL_LOG_COPY_ENABLED {
                proc.write_stdout(buf);
            }
            
            count as u64
        }
        crate::process::FileDescriptor::File(ref f) => {
            match crate::fs::write_at(&f.path, f.position, buf) {
                Ok(n) => {
                    proc.update_fd(fd_num as u32, |entry| if let crate::process::FileDescriptor::File(file) = entry { file.position += n; });
                    n as u64
                }
                Err(_) => !0u64
            }
        }
        crate::process::FileDescriptor::Socket(idx) => {
            let result = crate::socket::socket_send(idx, buf);
            if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
                match &result {
                    Ok(n) => crate::safe_print!(128, "[syscall] write(socket fd={}, req={}) = {}\n", fd_num, count, n),
                    Err(e) => crate::safe_print!(128, "[syscall] write(socket fd={}, req={}) = err {}\n", fd_num, count, *e as i64),
                }
            }
            match result {
                Ok(n) => n as u64,
                Err(e) => (-(e as i64)) as u64,
            }
        }
        crate::process::FileDescriptor::PipeWrite(pipe_id) => {
            pipe_write(pipe_id, buf) as u64
        }
        _ => !0u64
    }
}

#[repr(C)]
struct IoVec {
    iov_base: u64,
    iov_len: usize,
}

fn sys_readv(fd_num: u64, iov_ptr: u64, iov_cnt: usize) -> u64 {
    if !validate_user_ptr(iov_ptr, iov_cnt * core::mem::size_of::<IoVec>()) { return EFAULT; }
    let mut total_read: u64 = 0;
    for i in 0..iov_cnt {
        let iov = unsafe { &*((iov_ptr as *const IoVec).add(i)) };
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

fn sys_writev(fd_num: u64, iov_ptr: u64, iov_cnt: usize) -> u64 {
    if !validate_user_ptr(iov_ptr, iov_cnt * core::mem::size_of::<IoVec>()) { return EFAULT; }
    let mut total_written: u64 = 0;
    for i in 0..iov_cnt {
        let iov = unsafe { &*((iov_ptr as *const IoVec).add(i)) };
        let written = sys_write(fd_num, iov.iov_base, iov.iov_len);
        if (written as i64) < 0 {
            if total_written == 0 { return written; }
            break;
        }
        total_written += written;
    }
    total_written
}

fn sys_fstatfs(fd: u32, buf_ptr: u64) -> u64 {
    if !validate_user_ptr(buf_ptr, 120) { return EFAULT; }
    if let Some(proc) = crate::process::current_process() {
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
        f_type: 0xEF53,  // EXT2_SUPER_MAGIC
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
    unsafe { core::ptr::copy_nonoverlapping(&st as *const Statfs as *const u8, buf_ptr as *mut u8, core::mem::size_of::<Statfs>()); }
    0
}

fn sys_dup(oldfd: u32) -> u64 {
    let proc = match crate::process::current_process() { Some(p) => p, None => return ENOSYS };
    let entry = match proc.get_fd(oldfd) {
        Some(e) => e,
        None => return EBADF,
    };
    match &entry {
        crate::process::FileDescriptor::PipeWrite(id) => pipe_clone_ref(*id, true),
        crate::process::FileDescriptor::PipeRead(id) => pipe_clone_ref(*id, false),
        _ => {}
    }
    let newfd = proc.alloc_fd(entry);
    // dup() never sets CLOEXEC on the new fd
    proc.clear_cloexec(newfd);
    if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
        crate::safe_print!(128, "[syscall] dup(oldfd={}) = {}\n", oldfd, newfd);
    }
    newfd as u64
}

fn sys_dup3(oldfd: u32, newfd: u32, flags: u32) -> u64 {
    if oldfd == newfd { return EINVAL; }
    let proc = match crate::process::current_process() { Some(p) => p, None => return ENOSYS };
    let entry = match proc.get_fd(oldfd) {
        Some(e) => e,
        None => return EBADF,
    };

    if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
        let pid = proc.pid;
        crate::safe_print!(128, "[syscall] dup3(oldfd={}, newfd={}, flags=0x{:x}) PID {}\n", oldfd, newfd, flags, pid);
    }

    // If newfd was a pipe fd, close it properly before replacing
    if let Some(old_entry) = proc.get_fd(newfd) {
        match old_entry {
            crate::process::FileDescriptor::PipeWrite(id) => pipe_close_write(id),
            crate::process::FileDescriptor::PipeRead(id) => pipe_close_read(id),
            _ => {}
        }
    }

    // Increment ref count for cloned pipe fds
    match &entry {
        crate::process::FileDescriptor::PipeWrite(id) => pipe_clone_ref(*id, true),
        crate::process::FileDescriptor::PipeRead(id) => pipe_clone_ref(*id, false),
        _ => {}
    }

    proc.set_fd(newfd, entry);

    // dup3 sets CLOEXEC on newfd only if O_CLOEXEC is in flags
    if flags & crate::process::open_flags::O_CLOEXEC != 0 {
        proc.set_cloexec(newfd);
    } else {
        proc.clear_cloexec(newfd);
    }

    newfd as u64
}

fn sys_pipe2(fds_ptr: u64, flags: u32) -> u64 {
    if !validate_user_ptr(fds_ptr, 8) { return EFAULT; }
    let proc = match crate::process::current_process() { Some(p) => p, None => return ENOSYS };

    let pipe_id = pipe_create();
    let fd_r = proc.alloc_fd(crate::process::FileDescriptor::PipeRead(pipe_id));
    let fd_w = proc.alloc_fd(crate::process::FileDescriptor::PipeWrite(pipe_id));

    if flags & crate::process::open_flags::O_CLOEXEC != 0 {
        proc.set_cloexec(fd_r);
        proc.set_cloexec(fd_w);
    }

    unsafe {
        *(fds_ptr as *mut [i32; 2]) = [fd_r as i32, fd_w as i32];
    }
    0
}

fn sys_brk(new_brk: usize) -> u64 {
    if let Some(proc) = crate::process::current_process() {
        if new_brk == 0 { proc.get_brk() as u64 } else { proc.set_brk(new_brk) as u64 }
    } else { 0 }
}

fn resolve_path_at(dirfd: i32, raw_path: &str) -> String {
    if raw_path.starts_with('/') {
        return crate::vfs::canonicalize_path(raw_path);
    }
    let base = if dirfd == -100 { // AT_FDCWD
        if let Some(proc) = crate::process::current_process() {
            proc.cwd.clone()
        } else {
            String::from("/")
        }
    } else if dirfd >= 0 {
        if let Some(proc) = crate::process::current_process() {
            if let Some(crate::process::FileDescriptor::File(f)) = proc.get_fd(dirfd as u32) {
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

fn sys_openat(dirfd: i32, path_ptr: u64, flags: u32, _mode: u32) -> u64 {
    let raw_path = match copy_from_user_str(path_ptr, 1024) {
        Ok(p) => p,
        Err(e) => return e,
    };

    let path = if raw_path.starts_with('/') {
        crate::vfs::canonicalize_path(&raw_path)
    } else {
        let base = if dirfd == -100 { // AT_FDCWD
            if let Some(proc) = crate::process::current_process() {
                proc.cwd.clone()
            } else {
                String::from("/")
            }
        } else if dirfd >= 0 {
            if let Some(proc) = crate::process::current_process() {
                if let Some(crate::process::FileDescriptor::File(f)) = proc.get_fd(dirfd as u32) {
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

    if !crate::fs::exists(&path) {
        let is_creat = flags & crate::process::open_flags::O_CREAT != 0;
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
                alloc::format!("/{}", parent_raw)
            };
            if parent_path != "/" && !crate::fs::exists(&parent_path) {
                if crate::config::SYSCALL_DEBUG_IO_ENABLED {
                    crate::safe_print!(256, "[syscall] openat({}) parent {} not found flags=0x{:x}\n", &path, &parent_path, flags);
                }
                return ENOENT;
            }
        }
    }

    if let Some(proc) = crate::process::current_process() {
        if flags & crate::process::open_flags::O_TRUNC != 0 {
            let _ = crate::fs::write_file(&path, &[]);
        }
        let fd = proc.alloc_fd(crate::process::FileDescriptor::File(crate::process::KernelFile::new(path.clone(), flags)));
        if flags & crate::process::open_flags::O_CLOEXEC != 0 {
            proc.set_cloexec(fd);
        }
        if crate::config::SYSCALL_DEBUG_IO_ENABLED {
            crate::safe_print!(256, "[syscall] openat({}) = fd {} flags=0x{:x}\n", &path, fd, flags);
        }
        fd as u64
    } else { !0u64 }
}

fn sys_close(fd: u32) -> u64 {
    if let Some(proc) = crate::process::current_process() {
        if let Some(entry) = proc.remove_fd(fd) {
            proc.clear_cloexec(fd);
            match entry {
                crate::process::FileDescriptor::Socket(idx) => { crate::socket::remove_socket(idx); }
                crate::process::FileDescriptor::ChildStdout(child_pid) => {
                    crate::process::remove_child_channel(child_pid);
                }
                crate::process::FileDescriptor::PipeWrite(pipe_id) => {
                    pipe_close_write(pipe_id);
                }
                crate::process::FileDescriptor::PipeRead(pipe_id) => {
                    pipe_close_read(pipe_id);
                }
                _ => {}
            }
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
    if !validate_user_ptr(stat_ptr, core::mem::size_of::<Stat>()) { return EFAULT; }
    let proc = match crate::process::current_process() { Some(p) => p, None => return !0u64 };
    if let Some(crate::process::FileDescriptor::File(f)) = proc.get_fd(fd) {
        if let Ok(meta) = crate::vfs::metadata(&f.path) {
            let stat = Stat { st_dev: 1, st_ino: meta.inode, st_size: meta.size as i64, st_mode: if meta.is_dir { 0o40755 } else { 0o100644 }, st_nlink: if meta.is_dir { 2 } else { 1 }, st_blksize: 4096, ..Default::default() };
            unsafe { core::ptr::write(stat_ptr as *mut Stat, stat); }
            if crate::config::SYSCALL_DEBUG_IO_ENABLED {
                crate::safe_print!(256, "[syscall] fstat(fd={}, file={}) size={}\n", fd, &f.path, meta.size);
            }
            return 0;
        }
    }
    !0u64
}

fn sys_newfstatat(dirfd: i32, path_ptr: u64, stat_ptr: u64, _flags: u32) -> u64 {
    let path = match copy_from_user_str(path_ptr, 512) {
        Ok(p) => p,
        Err(e) => return e,
    };
    if !validate_user_ptr(stat_ptr, core::mem::size_of::<Stat>()) { return EFAULT; }
    
    // Resolve path.
    // Logic:
    // 1. If path is absolute, use it directly.
    // 2. If path is relative:
    //    a. If dirfd is AT_FDCWD (-100), relative to process CWD.
    //    b. If dirfd is a valid FD, relative to that FD's path.
    //    c. Otherwise error.

    let resolved_path = if path.starts_with('/') {
         String::from(&path)
    } else {
        let base_path = if dirfd == -100 { // AT_FDCWD
             if let Some(proc) = crate::process::current_process() {
                 proc.cwd.clone()
             } else {
                 return !0u64; // EBADF
             }
        } else if dirfd >= 0 {
             if let Some(proc) = crate::process::current_process() {
                 if let Some(crate::process::FileDescriptor::File(f)) = proc.get_fd(dirfd as u32) {
                     // Check if it is a directory? For now assume yes if used as dirfd.
                     f.path.clone()
                 } else {
                     return !0u64; // EBADF
                 }
             } else {
                 return !0u64;
             }
        } else {
            return !0u64; // EBADF
        };
        crate::vfs::resolve_path(&base_path, &path)
    };
    
    const AT_SYMLINK_NOFOLLOW: u32 = 0x100;
    let follow = _flags & AT_SYMLINK_NOFOLLOW == 0;

    if !follow && crate::vfs::is_symlink(&resolved_path) {
        let target = crate::vfs::read_symlink(&resolved_path).unwrap_or_default();
        let stat = Stat {
            st_dev: 1,
            st_ino: 1,
            st_size: target.len() as i64,
            st_mode: 0o120777, // S_IFLNK | 0777
            st_nlink: 1,
            st_blksize: 4096,
            ..Default::default()
        };
        unsafe { core::ptr::write(stat_ptr as *mut Stat, stat); }
        return 0;
    }

    let final_path = if follow { crate::vfs::resolve_symlinks(&resolved_path) } else { resolved_path };

    if let Ok(meta) = crate::vfs::metadata(&final_path) {
        let stat = Stat { 
            st_dev: 1,
            st_ino: meta.inode,
            st_size: meta.size as i64, 
            st_mode: if meta.is_dir { 0o40755 } else { 0o100644 }, 
            st_nlink: if meta.is_dir { 2 } else { 1 },
            st_blksize: 4096,
            ..Default::default() 
        };
        unsafe { core::ptr::write(stat_ptr as *mut Stat, stat); }
        return 0;
    }

    if crate::vfs::is_symlink(&final_path) {
        let target = crate::vfs::read_symlink(&final_path).unwrap_or_default();
        let stat = Stat {
            st_dev: 1,
            st_ino: 1,
            st_size: target.len() as i64,
            st_mode: 0o120777,
            st_nlink: 1,
            st_blksize: 4096,
            ..Default::default()
        };
        unsafe { core::ptr::write(stat_ptr as *mut Stat, stat); }
        return 0;
    }
    
    ENOENT
}

#[repr(C)]
struct Timespec {
    tv_sec: i64,
    tv_nsec: i64,
}

fn sys_clock_gettime(clock_id: u32, tp_ptr: u64) -> u64 {
    if !validate_user_ptr(tp_ptr, core::mem::size_of::<Timespec>()) { return EFAULT; }
    
    // clock_id: 0 = CLOCK_REALTIME, 1 = CLOCK_MONOTONIC
    let (sec, nsec) = match clock_id {
        0 => {
            let us = crate::timer::utc_time_us().unwrap_or(0);
            ((us / 1_000_000) as i64, ((us % 1_000_000) * 1_000) as i64)
        }
        1 | _ => {
            let us = crate::timer::uptime_us();
            ((us / 1_000_000) as i64, ((us % 1_000_000) * 1_000) as i64)
        }
    };
    
    unsafe {
        *(tp_ptr as *mut Timespec) = Timespec { tv_sec: sec, tv_nsec: nsec };
    }
    0
}

fn sys_faccessat2(dirfd: i32, path_ptr: u64, _mode: u32, _flags: u32) -> u64 {
    let path = match copy_from_user_str(path_ptr, 512) {
        Ok(p) => p,
        Err(e) => return e,
    };
    
    let resolved_path = if path.starts_with('/') {
         path
    } else {
        let base_path = if dirfd == -100 { // AT_FDCWD
             if let Some(proc) = crate::process::current_process() {
                 proc.cwd.clone()
             } else {
                 return !0u64; // EBADF
             }
        } else if dirfd >= 0 {
             if let Some(proc) = crate::process::current_process() {
                 if let Some(crate::process::FileDescriptor::File(f)) = proc.get_fd(dirfd as u32) {
                     f.path.clone()
                 } else {
                     return !0u64; // EBADF
                 }
             } else {
                 return !0u64;
             }
        } else {
            return !0u64; // EBADF
        };
        crate::vfs::resolve_path(&base_path, &path)
    };
    
    let final_path = crate::vfs::resolve_symlinks(&resolved_path);
    if crate::fs::exists(&final_path) || crate::vfs::is_symlink(&resolved_path) {
        0
    } else {
        ENOENT
    }
}

fn sys_clone(flags: u64, stack: u64, _parent_tid: u64, _tls: u64, _child_tid: u64) -> u64 {
    // Basic vfork support: CLONE_VM (0x100) | CLONE_VFORK (0x4000)
    // make uses 0x4111 (SIGCHLD | CLONE_VM | CLONE_VFORK | CLONE_CHILD_SETTID)
    if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
        crate::safe_print!(128, "[syscall] clone(flags=0x{:x}, stack=0x{:x})\n", flags, stack);
    }

    if flags & 0x4000 != 0 || flags & 0x11 == 0x11 {
        // vfork-like clone: Create a copy of the current process
        
        let parent_proc = match crate::process::current_process() {
            Some(p) => p,
            None => return !0u64, // ENOSYS
        };
        
        // Allocate new PID for the child
        let child_pid = crate::process::allocate_pid();
        
        if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
            crate::safe_print!(128, "[syscall] clone: forking PID {} -> {} (vfork-like)\n", parent_proc.pid, child_pid);
        }

        // Delegate to process::fork_process
        match crate::process::fork_process(child_pid, stack) {
            Ok(new_pid) => {
                return new_pid as u64;
            },
            Err(e) => {
                crate::safe_print!(128, "[syscall] clone: fork failed: {}\n", e);
                return !0u64; // EAGAIN
            }
        }
    }
    
    if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
        crate::safe_print!(128, "[syscall] clone: flags not supported, returning ENOSYS\n");
    }
    ENOSYS
}

fn sys_execve(path_ptr: u64, argv_ptr: u64, envp_ptr: u64) -> u64 {
    let path = match copy_from_user_str(path_ptr, 1024) {
        Ok(p) => p,
        Err(e) => {
            crate::safe_print!(64, "[syscall] execve: path copy failed with {}\n", e as i64);
            return e;
        },
    };
    
    // Resolve path
    let resolved_path = if path.starts_with('/') {
        path
    } else {
        if let Some(proc) = crate::process::current_process() {
            crate::vfs::resolve_path(&proc.cwd, &path)
        } else {
            path
        }
    };
    let resolved_path = crate::vfs::resolve_symlinks(&resolved_path);

    // Parse argv from user space
    let mut args = Vec::new();
    if argv_ptr != 0 {
        let mut i = 0;
        loop {
            if !validate_user_ptr(argv_ptr + i * 8, 8) { break; }
            let str_ptr = unsafe { *((argv_ptr + i * 8) as *const u64) };
            if str_ptr == 0 { break; }
            if let Ok(s) = copy_from_user_str(str_ptr, 1024) {
                args.push(s);
            } else {
                if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
                    crate::safe_print!(64, "[syscall] execve: failed to copy argv[{}]\n", i);
                }
                break;
            }
            i += 1;
        }
    }

    // Parse envp from user space
    let mut env = Vec::new();
    if envp_ptr != 0 {
        let mut i = 0;
        loop {
            if !validate_user_ptr(envp_ptr + i * 8, 8) { break; }
            let str_ptr = unsafe { *((envp_ptr + i * 8) as *const u64) };
            if str_ptr == 0 { break; }
            if let Ok(s) = copy_from_user_str(str_ptr, 1024) {
                env.push(s);
            } else {
                break;
            }
            i += 1;
        }
    }

    do_execve(resolved_path, args, env)
}

fn do_execve(resolved_path: String, args: Vec<String>, env: Vec<String>) -> u64 {
    // Read the file
    let file_data = match crate::fs::read_file(&resolved_path) {
        Ok(data) => data,
        Err(_) => {
            crate::safe_print!(128, "[syscall] execve: failed to read {}\n", resolved_path);
            return ENOENT;
        }
    };

    // Check for shebang (#!) scripts
    if file_data.len() >= 2 && file_data[0] == b'#' && file_data[1] == b'!' {
        return exec_shebang(&resolved_path, &file_data, args, env);
    }

    // Otherwise treat as ELF binary
    let mut proc = match crate::process::current_process() {
        Some(p) => p,
        None => return !0u64,
    };

    // Close all FDs marked close-on-exec before replacing the process image
    let closed_fds = proc.close_cloexec_fds();
    for (_fd, entry) in closed_fds {
        match entry {
            crate::process::FileDescriptor::PipeWrite(pipe_id) => pipe_close_write(pipe_id),
            crate::process::FileDescriptor::PipeRead(pipe_id) => pipe_close_read(pipe_id),
            crate::process::FileDescriptor::Socket(idx) => crate::socket::remove_socket(idx),
            crate::process::FileDescriptor::ChildStdout(child_pid) => {
                crate::process::remove_child_channel(child_pid);
            }
            _ => {}
        }
    }

    if let Err(e) = proc.replace_image(&file_data, &args, &env) {
        crate::safe_print!(128, "[syscall] execve: replace_image failed for {}: {}\n", resolved_path, e);
        return ENOENT;
    }

    proc.name = resolved_path.clone();

    if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
        crate::safe_print!(128, "[syscall] execve: replaced image for PID {} with {}\n", proc.pid, resolved_path);
    }

    // Activate the new address space (replace_image deactivated the old one)
    proc.address_space.activate();

    // Now jump to the new entry point. This never returns.
    unsafe {
        crate::process::enter_user_mode(&proc.context);
    }
}

/// Handle shebang (#!) scripts: parse interpreter and re-exec
fn exec_shebang(script_path: &str, file_data: &[u8], original_args: Vec<String>, env: Vec<String>) -> u64 {
    // Find the end of the shebang line
    let line_end = file_data.iter().position(|&b| b == b'\n').unwrap_or(file_data.len().min(256));
    let shebang_line = match core::str::from_utf8(&file_data[2..line_end]) {
        Ok(s) => s.trim(),
        Err(_) => {
            crate::safe_print!(128, "[syscall] execve: invalid shebang in {}\n", script_path);
            return ENOENT;
        }
    };

    if shebang_line.is_empty() {
        crate::safe_print!(128, "[syscall] execve: empty shebang in {}\n", script_path);
        return ENOENT;
    }

    // Split into interpreter and optional single argument
    let (interpreter, shebang_arg) = match shebang_line.split_once(char::is_whitespace) {
        Some((interp, arg)) => (interp.trim(), Some(arg.trim())),
        None => (shebang_line, None),
    };

    let interpreter = crate::vfs::resolve_symlinks(interpreter);

    if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
        if let Some(arg) = shebang_arg {
            crate::safe_print!(128, "[syscall] execve: shebang {} {} {}\n", interpreter, arg, script_path);
        } else {
            crate::safe_print!(128, "[syscall] execve: shebang {} {}\n", interpreter, script_path);
        }
    }

    // Build new argv: [interpreter, shebang_arg?, script_path, original_args[1:]...]
    let mut new_args = Vec::new();
    new_args.push(interpreter.clone());
    if let Some(arg) = shebang_arg {
        if !arg.is_empty() {
            new_args.push(String::from(arg));
        }
    }
    new_args.push(String::from(script_path));
    // Append original args after argv[0] (skip the original program name)
    if original_args.len() > 1 {
        new_args.extend_from_slice(&original_args[1..]);
    }

    do_execve(interpreter, new_args, env)
}

fn sys_wait4(pid: i32, status_ptr: u64, options: i32, _rusage: u64) -> u64 {
    if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
        crate::safe_print!(128, "[syscall] wait4(pid={}, options=0x{:x})\n", pid, options);
    }

    let wnohang = options & 1 != 0;

    let current_pid = match crate::process::read_current_pid() {
        Some(p) => p,
        None => return (-libc_errno::ECHILD as i64) as u64,
    };

    if pid > 0 {
        // Wait for specific child
        let p = pid as u32;
        if let Some(ch) = crate::process::get_child_channel(p) {
            loop {
                if ch.has_exited() {
                    let code = ch.exit_code();
                    if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
                        crate::safe_print!(128, "[syscall] wait4: PID {} exited with code {}\n", p, code);
                    }
                    if status_ptr != 0 && validate_user_ptr(status_ptr, 4) {
                        unsafe { *(status_ptr as *mut u32) = encode_wait_status(code); }
                    }
                    crate::process::remove_child_channel(p);
                    return p as u64;
                }

                if wnohang {
                    return 0;
                }
                crate::threading::yield_now();
            }
        }
    } else if pid == -1 || pid == 0 {
        // Wait for any child (pid=-1) or any child in same pgid (pid=0, treat same)
        if !crate::process::has_children(current_pid) {
            if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
                crate::safe_print!(128, "[syscall] wait4: no children for PID {}\n", current_pid);
            }
            return (-libc_errno::ECHILD as i64) as u64;
        }

        loop {
            if let Some((child_pid, ch)) = crate::process::find_exited_child(current_pid) {
                let code = ch.exit_code();
                if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
                    crate::safe_print!(128, "[syscall] wait4: PID {} exited with code {}\n", child_pid, code);
                }
                if status_ptr != 0 && validate_user_ptr(status_ptr, 4) {
                    unsafe { *(status_ptr as *mut u32) = encode_wait_status(code); }
                }
                crate::process::remove_child_channel(child_pid);
                return child_pid as u64;
            }

            if wnohang {
                return 0;
            }
            crate::threading::yield_now();
        }
    }

    // Fallback (ECHILD)
    if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
        crate::safe_print!(128, "[syscall] wait4: no child found for PID {}\n", pid);
    }
    (-libc_errno::ECHILD as i64) as u64
}

fn sys_getcwd(buf_ptr: u64, size: usize) -> u64 {
    if !validate_user_ptr(buf_ptr, size) { return EFAULT; }
    if let Some(proc) = crate::process::current_process() {
        let cwd_bytes = proc.cwd.as_bytes();
        // Check if buffer is large enough (including null terminator)
        if cwd_bytes.len() + 1 > size {
            return (-libc_errno::ERANGE as i64) as u64;
        }
        unsafe {
            core::ptr::copy_nonoverlapping(cwd_bytes.as_ptr(), buf_ptr as *mut u8, cwd_bytes.len());
            *(buf_ptr as *mut u8).add(cwd_bytes.len()) = 0; // Null terminate
        }
        // Return length including null terminator
        return (cwd_bytes.len() + 1) as u64;
    }
    ENOENT
}

fn sys_fcntl(fd: u32, cmd: u32, arg: u64) -> u64 {
    const F_GETFD: u32 = 1;
    const F_SETFD: u32 = 2;
    const F_GETFL: u32 = 3;
    const F_SETFL: u32 = 4;
    const FD_CLOEXEC: u64 = 1;

    let proc = match crate::process::current_process() { Some(p) => p, None => return 0 };

    match cmd {
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
        F_GETFL => 0,
        F_SETFL => 0,
        _ => 0,
    }
}

fn sys_mkdirat(_dirfd: i32, path_ptr: u64, _mode: u32) -> u64 {
    let path = match copy_from_user_str(path_ptr, 512) {
        Ok(p) => p,
        Err(e) => return e,
    };
    crate::safe_print!(128, "[syscall] mkdirat: {}\n", path);
    if crate::fs::create_dir(&path).is_ok() { 0 } else { !0u64 }
}

fn sys_unlinkat(dirfd: i32, path_ptr: u64, flags: u32) -> u64 {
    let path = match copy_from_user_str(path_ptr, 512) {
        Ok(p) => p,
        Err(e) => return e,
    };

    let resolved = if path.starts_with('/') {
        crate::vfs::canonicalize_path(&path)
    } else {
        let base = if dirfd == -100 {
            if let Some(proc) = crate::process::current_process() {
                proc.cwd.clone()
            } else {
                return EBADF;
            }
        } else if dirfd >= 0 {
            if let Some(proc) = crate::process::current_process() {
                if let Some(crate::process::FileDescriptor::File(f)) = proc.get_fd(dirfd as u32) {
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
        match crate::fs::remove_file(&resolved) {
            Ok(()) => 0,
            Err(e) => fs_error_to_errno(e),
        }
    }
}

fn sys_renameat(olddirfd: i32, oldpath_ptr: u64, newdirfd: i32, newpath_ptr: u64) -> u64 {
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

fn sys_symlinkat(target_ptr: u64, newdirfd: i32, linkpath_ptr: u64) -> u64 {
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
        Err(_) => !0u64,
    }
}

fn sys_linkat(_olddirfd: i32, oldpath_ptr: u64, _newdirfd: i32, newpath_ptr: u64, _flags: u32) -> u64 {
    let oldpath = match copy_from_user_str(oldpath_ptr, 1024) { Ok(p) => p, Err(e) => return e };
    let newpath = match copy_from_user_str(newpath_ptr, 1024) { Ok(p) => p, Err(e) => return e };
    let src = resolve_path_at(_olddirfd, &oldpath);
    let dst = resolve_path_at(_newdirfd, &newpath);
    if let Ok(data) = crate::fs::read_file(&src) {
        if crate::fs::write_file(&dst, &data).is_ok() { return 0; }
    }
    !0u64
}

fn sys_readlinkat(dirfd: i32, path_ptr: u64, buf_ptr: u64, bufsize: usize) -> u64 {
    let raw_path = match copy_from_user_str(path_ptr, 1024) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let path = resolve_path_at(dirfd, &raw_path);
    if let Some(target) = crate::vfs::read_symlink(&path) {
        if !validate_user_ptr(buf_ptr, bufsize) { return EFAULT; }
        let bytes = target.as_bytes();
        let copy_len = bytes.len().min(bufsize);
        unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), buf_ptr as *mut u8, copy_len); }
        return copy_len as u64;
    }
    EINVAL
}

fn sys_nanosleep(seconds: u64, nanoseconds: u64) -> u64 {
    let total_us = seconds.saturating_mul(1_000_000).saturating_add(nanoseconds / 1_000);
    let deadline = crate::timer::uptime_us().saturating_add(total_us);
    loop {
        if crate::timer::uptime_us() >= deadline { return 0; }
        if crate::process::is_current_interrupted() { return EINTR; }
        crate::threading::schedule_blocking(deadline);
    }
}

use crate::socket::{self, SocketAddrV4, SockAddrIn, libc_errno};

fn sys_socket(domain: i32, sock_type: i32, _proto: i32) -> u64 {
    let base_type = sock_type & 0xFF; // mask off SOCK_CLOEXEC (0x80000) and SOCK_NONBLOCK (0x800)
    let cloexec = sock_type & 0x80000 != 0;
    if domain != 2 || (base_type != 1 && base_type != 2) {
        crate::safe_print!(96, "[syscall] socket(domain={}, type=0x{:x}): unsupported\n", domain, sock_type);
        return !0u64;
    }
    if let Some(idx) = socket::alloc_socket(base_type) {
        if let Some(proc) = crate::process::current_process() {
            let fd = proc.alloc_fd(crate::process::FileDescriptor::Socket(idx));
            if cloexec {
                proc.set_cloexec(fd);
            }
            crate::safe_print!(96, "[syscall] socket(type={}) = fd {}\n", if base_type == 2 { "UDP" } else { "TCP" }, fd);
            return fd as u64;
        }
    }
    !0u64
}

fn sys_bind(fd: u32, addr_ptr: u64, len: usize) -> u64 {
    if len < 16 { return !0u64; }
    if !validate_user_ptr(addr_ptr, len) { return EFAULT; }
    let addr = unsafe { core::ptr::read(addr_ptr as *const SockAddrIn) }.to_addr();
    crate::safe_print!(96, "[syscall] bind(fd={}, port={}, ip={}.{}.{}.{})\n", fd, addr.port, addr.ip[0], addr.ip[1], addr.ip[2], addr.ip[3]);
    if let Some(idx) = get_socket_from_fd(fd) {
        match socket::socket_bind(idx, addr) {
            Ok(()) => return 0,
            Err(e) => {
                crate::safe_print!(64, "[syscall] bind failed: {}\n", e);
                return !0u64;
            }
        }
    }
    !0u64
}

fn sys_listen(fd: u32, backlog: i32) -> u64 {
    if let Some(idx) = get_socket_from_fd(fd) { if socket::socket_listen(idx, backlog as usize).is_ok() { return 0; } }
    !0u64
}

fn sys_accept(fd: u32, addr_ptr: u64, len_ptr: u64) -> u64 {
    if addr_ptr != 0 && !validate_user_ptr(addr_ptr, 16) { return EFAULT; }
    if len_ptr != 0 && !validate_user_ptr(len_ptr, 4) { return EFAULT; }
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
    if !validate_user_ptr(addr_ptr, len) { return EFAULT; }
    let addr = unsafe { core::ptr::read(addr_ptr as *const SockAddrIn) }.to_addr();
    crate::safe_print!(96, "[syscall] connect(fd={}, ip={}.{}.{}.{}:{})\n", fd, addr.ip[0], addr.ip[1], addr.ip[2], addr.ip[3], addr.port);
    if let Some(idx) = get_socket_from_fd(fd) {
        match socket::socket_connect(idx, addr) {
            Ok(()) => {
                crate::safe_print!(64, "[syscall] connect(fd={}) = OK\n", fd);
                return 0;
            }
            Err(e) => {
                crate::safe_print!(64, "[syscall] connect(fd={}) = err {}\n", fd, e);
                return (-e as i64) as u64;
            }
        }
    }
    !0u64
}

fn sys_getsockname(fd: u32, addr_ptr: u64, len_ptr: u64) -> u64 {
    if addr_ptr == 0 || len_ptr == 0 { return (-libc_errno::EINVAL as i64) as u64; }
    if !validate_user_ptr(len_ptr, 4) { return EFAULT; }
    let idx = match get_socket_from_fd(fd) {
        Some(i) => i,
        None => return (-libc_errno::EBADF as i64) as u64,
    };
    let port = socket::with_socket(idx, |s| s.bind_port.unwrap_or(0)).unwrap_or(0);
    let local_ip = crate::smoltcp_net::get_local_ip();
    let sa = SockAddrIn {
        sin_family: 2,
        sin_port: port.to_be(),
        sin_addr: u32::from_ne_bytes(local_ip),
        sin_zero: [0u8; 8],
    };
    if validate_user_ptr(addr_ptr, core::mem::size_of::<SockAddrIn>()) {
        unsafe {
            core::ptr::write(addr_ptr as *mut SockAddrIn, sa);
            core::ptr::write(len_ptr as *mut u32, core::mem::size_of::<SockAddrIn>() as u32);
        }
    }
    0
}

fn sys_sendto(fd: u32, buf_ptr: u64, len: usize, _flags: i32, dest_addr: u64, addr_len: usize) -> u64 {
    if !validate_user_ptr(buf_ptr, len) { return EFAULT; }
    let buf = unsafe { core::slice::from_raw_parts(buf_ptr as *const u8, len) };
    let idx = match get_socket_from_fd(fd) {
        Some(i) => i,
        None => return (-libc_errno::EBADF as i64) as u64,
    };

    if socket::is_udp_socket(idx) {
        let dest = if dest_addr != 0 && addr_len >= 16 {
            if !validate_user_ptr(dest_addr, addr_len) { return EFAULT; }
            let a = unsafe { core::ptr::read(dest_addr as *const SockAddrIn) }.to_addr();
            crate::safe_print!(96, "[syscall] sendto(fd={}, len={}, dest={}.{}.{}.{}:{})\n", fd, len, a.ip[0], a.ip[1], a.ip[2], a.ip[3], a.port);
            a
        } else {
            match socket::udp_default_peer(idx) {
                Some(peer) => peer,
                None => return (-libc_errno::EINVAL as i64) as u64,
            }
        };
        match socket::socket_send_udp(idx, buf, dest) {
            Ok(n) => n as u64,
            Err(e) => (-e as i64) as u64,
        }
    } else {
        match socket::socket_send(idx, buf) {
            Ok(n) => n as u64,
            Err(e) => (-e as i64) as u64,
        }
    }
}

fn sys_recvfrom(fd: u32, buf_ptr: u64, len: usize, _flags: i32, src_addr: u64, addr_len_ptr: u64) -> u64 {
    if !validate_user_ptr(buf_ptr, len) { return EFAULT; }
    let buf = unsafe { core::slice::from_raw_parts_mut(buf_ptr as *mut u8, len) };
    let idx = match get_socket_from_fd(fd) {
        Some(i) => i,
        None => return (-libc_errno::EBADF as i64) as u64,
    };

    if socket::is_udp_socket(idx) {
        match socket::socket_recv_udp(idx, buf) {
            Ok((n, from)) => {
                if src_addr != 0 && addr_len_ptr != 0 {
                    if validate_user_ptr(src_addr, core::mem::size_of::<SockAddrIn>())
                        && validate_user_ptr(addr_len_ptr, core::mem::size_of::<u32>())
                    {
                        let sa = SockAddrIn::from_addr(&from);
                        unsafe { core::ptr::write(src_addr as *mut SockAddrIn, sa); }
                        unsafe { core::ptr::write(addr_len_ptr as *mut u32, core::mem::size_of::<SockAddrIn>() as u32); }
                    }
                }
                n as u64
            }
            Err(e) => (-e as i64) as u64,
        }
    } else {
        match socket::socket_recv(idx, buf) {
            Ok(n) => n as u64,
            Err(e) => (-e as i64) as u64,
        }
    }
}

fn sys_shutdown(_fd: u32, _how: i32) -> u64 { 0 }

// msghdr layout on aarch64 (LP64 little-endian):
//   0: *msg_name      (8 bytes)
//   8: msg_namelen     (4 bytes + 4 padding)
//  16: *msg_iov        (8 bytes)
//  24: msg_iovlen      (4 bytes + 4 padding)
//  32: *msg_control    (8 bytes)
//  40: msg_controllen  (8 bytes)
//  48: msg_flags       (4 bytes)
#[repr(C)]
struct MsgHdr {
    msg_name: u64,
    msg_namelen: u32,
    _pad1: u32,
    msg_iov: u64,
    msg_iovlen: u32,
    _pad2: u32,
    msg_control: u64,
    msg_controllen: u64,
    msg_flags: i32,
}

fn sys_sendmsg(fd: u32, msg_ptr: u64, _flags: i32) -> u64 {
    if !validate_user_ptr(msg_ptr, core::mem::size_of::<MsgHdr>()) { return EFAULT; }
    let msg = unsafe { &*(msg_ptr as *const MsgHdr) };

    if msg.msg_iovlen == 0 { return 0; }
    if !validate_user_ptr(msg.msg_iov, msg.msg_iovlen as usize * core::mem::size_of::<IoVec>()) { return EFAULT; }
    let iovs = unsafe { core::slice::from_raw_parts(msg.msg_iov as *const IoVec, msg.msg_iovlen as usize) };

    let iov = &iovs[0];
    if iov.iov_len == 0 { return 0; }
    if !validate_user_ptr(iov.iov_base, iov.iov_len as usize) { return EFAULT; }
    let buf = unsafe { core::slice::from_raw_parts(iov.iov_base as *const u8, iov.iov_len as usize) };

    let idx = match get_socket_from_fd(fd) {
        Some(i) => i,
        None => return (-libc_errno::EBADF as i64) as u64,
    };

    if socket::is_udp_socket(idx) {
        let dest = if msg.msg_name != 0 && msg.msg_namelen >= 16 {
            if !validate_user_ptr(msg.msg_name, msg.msg_namelen as usize) { return EFAULT; }
            unsafe { core::ptr::read(msg.msg_name as *const SockAddrIn) }.to_addr()
        } else {
            match socket::udp_default_peer(idx) {
                Some(peer) => peer,
                None => return (-libc_errno::EINVAL as i64) as u64,
            }
        };
        match socket::socket_send_udp(idx, buf, dest) {
            Ok(n) => n as u64,
            Err(e) => (-e as i64) as u64,
        }
    } else {
        match socket::socket_send(idx, buf) {
            Ok(n) => n as u64,
            Err(e) => (-e as i64) as u64,
        }
    }
}

fn sys_recvmsg(fd: u32, msg_ptr: u64, _flags: i32) -> u64 {
    if !validate_user_ptr(msg_ptr, core::mem::size_of::<MsgHdr>()) { return EFAULT; }
    let msg = unsafe { &mut *(msg_ptr as *mut MsgHdr) };

    if msg.msg_iovlen == 0 { return 0; }
    if !validate_user_ptr(msg.msg_iov, msg.msg_iovlen as usize * core::mem::size_of::<IoVec>()) { return EFAULT; }
    let iovs = unsafe { core::slice::from_raw_parts(msg.msg_iov as *const IoVec, msg.msg_iovlen as usize) };

    let iov = &iovs[0];
    if iov.iov_len == 0 { return 0; }
    if !validate_user_ptr(iov.iov_base, iov.iov_len as usize) { return EFAULT; }
    let buf = unsafe { core::slice::from_raw_parts_mut(iov.iov_base as *mut u8, iov.iov_len as usize) };

    let idx = match get_socket_from_fd(fd) {
        Some(i) => i,
        None => return (-libc_errno::EBADF as i64) as u64,
    };

    if socket::is_udp_socket(idx) {
        match socket::socket_recv_udp(idx, buf) {
            Ok((n, from)) => {
                if msg.msg_name != 0 && msg.msg_namelen >= core::mem::size_of::<SockAddrIn>() as u32 {
                    if validate_user_ptr(msg.msg_name, core::mem::size_of::<SockAddrIn>()) {
                        let sa = SockAddrIn::from_addr(&from);
                        unsafe { core::ptr::write(msg.msg_name as *mut SockAddrIn, sa); }
                        msg.msg_namelen = core::mem::size_of::<SockAddrIn>() as u32;
                    }
                }
                msg.msg_flags = 0;
                n as u64
            }
            Err(e) => (-e as i64) as u64,
        }
    } else {
        match socket::socket_recv(idx, buf) {
            Ok(n) => {
                msg.msg_flags = 0;
                n as u64
            }
            Err(e) => (-e as i64) as u64,
        }
    }
}

fn get_socket_from_fd(fd: u32) -> Option<usize> {
    let proc = crate::process::current_process()?;
    if let Some(crate::process::FileDescriptor::Socket(idx)) = proc.get_fd(fd) { Some(idx) } else { None }
}

fn socket_get_udp_handle(idx: usize) -> Option<crate::smoltcp_net::SocketHandle> {
    socket::with_socket(idx, |sock| {
        if let socket::SocketType::Datagram { handle, .. } = &sock.inner {
            Some(*handle)
        } else {
            None
        }
    }).flatten()
}

fn socket_can_recv_tcp(idx: usize) -> bool {
    socket::with_socket(idx, |sock| {
        if let socket::SocketType::Stream(h) = &sock.inner {
            crate::smoltcp_net::with_network(|net| {
                let s = net.sockets.get::<smoltcp::socket::tcp::Socket>(*h);
                s.can_recv() || !s.is_active()
            }).unwrap_or(false)
        } else {
            false
        }
    }).unwrap_or(false)
}

fn sys_mmap(addr: usize, len: usize, _prot: u32, flags: u32, fd: i32, offset: usize) -> u64 {
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

        const MAP_ANONYMOUS: u32 = 0x20;
        if flags & MAP_ANONYMOUS == 0 && fd >= 0 {
            if let Some(crate::process::FileDescriptor::File(f)) = proc.get_fd(fd as u32) {
                let path = f.path.clone();
                let mut buf = alloc::vec![0u8; len];
                if let Ok(n) = crate::fs::read_at(&path, offset, &mut buf) {
                    let copy_len = n.min(len);
                    unsafe {
                        core::ptr::copy_nonoverlapping(buf.as_ptr(), mmap_addr as *mut u8, copy_len);
                    }
                    if crate::config::SYSCALL_DEBUG_IO_ENABLED {
                        crate::safe_print!(256, "[syscall] mmap(fd={}, file={}, off={}, len={}) = 0x{:x} (read {} bytes)\n", fd, &path, offset, len, mmap_addr, copy_len);
                    }
                }
            }
        }

        mmap_addr as u64
    } else { !0u64 }
}

fn sys_mremap(old_addr: usize, old_size: usize, new_size: usize, flags: u32) -> u64 {
    if new_size == 0 { return EINVAL; }
    const MREMAP_MAYMOVE: u32 = 1;

    let old_pages = (old_size + 4095) / 4096;
    let new_pages = (new_size + 4095) / 4096;

    if new_pages <= old_pages {
        return old_addr as u64;
    }

    if flags & MREMAP_MAYMOVE == 0 {
        return ENOMEM;
    }

    let new_addr = crate::process::alloc_mmap(new_pages * 4096);
    if new_addr == 0 { return ENOMEM; }

    if let Some(proc) = crate::process::current_process() {
        let mut new_frames = alloc::vec::Vec::new();
        for i in 0..new_pages {
            if let Some(frame) = crate::pmm::alloc_page_zeroed() {
                new_frames.push(frame);
                unsafe { crate::mmu::map_user_page(new_addr + i * 4096, frame.addr, crate::mmu::user_flags::RW_NO_EXEC); }
                proc.address_space.track_user_frame(frame);
            } else { return ENOMEM; }
        }

        let copy_len = old_size.min(new_size);
        unsafe { core::ptr::copy_nonoverlapping(old_addr as *const u8, new_addr as *mut u8, copy_len); }

        crate::process::record_mmap_region(new_addr, new_frames);

        if let Some(old_frames) = crate::process::remove_mmap_region(old_addr) {
            for (i, frame) in old_frames.into_iter().enumerate() {
                let _ = proc.address_space.unmap_page(old_addr + i * 4096);
                proc.address_space.remove_user_frame(frame);
                crate::pmm::free_page(frame);
            }
        }

        new_addr as u64
    } else { ENOMEM }
}

const ENOMEM: u64 = (-12i64) as u64;

fn sys_madvise(addr: usize, len: usize, advice: i32) -> u64 {
    const MADV_DONTNEED: i32 = 4;
    if advice == MADV_DONTNEED && len > 0 {
        let aligned_addr = addr & !0xFFF;
        let aligned_end = (addr + len + 0xFFF) & !0xFFF;
        let aligned_len = aligned_end - aligned_addr;
        if aligned_len <= 128 * 1024 * 1024 {
            unsafe {
                core::ptr::write_bytes(aligned_addr as *mut u8, 0, aligned_len);
            }
        }
    }
    0
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

fn sys_register_box(id: u64, name_ptr: u64, name_len: usize, root_ptr: u64, root_len: usize, primary_pid: u32) -> u64 {
    if !validate_user_ptr(name_ptr, name_len) { return EFAULT; }
    if !validate_user_ptr(root_ptr, root_len) { return EFAULT; }
    let name = unsafe { core::str::from_utf8(core::slice::from_raw_parts(name_ptr as *const u8, name_len)).unwrap_or("unknown") };
    let root = unsafe { core::str::from_utf8(core::slice::from_raw_parts(root_ptr as *const u8, root_len)).unwrap_or("/") };
    let creator_pid = crate::process::read_current_pid().unwrap_or(0);

    crate::process::register_box(crate::process::BoxInfo {
        id,
        name: String::from(name),
        root_dir: String::from(root),
        creator_pid,
        primary_pid,
    });
    0
}

fn sys_uptime() -> u64 { crate::timer::uptime_us() }

fn sys_resolve_host(path_ptr: u64, path_len: usize, res_ptr: u64) -> u64 {
    if !validate_user_ptr(path_ptr, path_len) { return EFAULT; }
    if !validate_user_ptr(res_ptr, 4) { return EFAULT; }
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
    if !validate_user_ptr(ptr, size) { return EFAULT; }
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

#[repr(C)]
pub struct SpawnOptions {
    pub cwd_ptr: u64,
    pub cwd_len: usize,
    pub root_dir_ptr: u64,
    pub root_dir_len: usize,
    pub args_ptr: u64,
    pub args_len: usize,
    pub stdin_ptr: u64,
    pub stdin_len: usize,
    pub box_id: u64,
}

/// Helper to parse a NULL-terminated array of string pointers (char** argv)
fn parse_argv_array(ptr: u64) -> Vec<String> {
    if ptr == 0 { return Vec::new(); }
    let mut args = Vec::new();
    let mut i = 0;
    loop {
        // Read pointer from the array
        if !BYPASS_VALIDATION.load(Ordering::Acquire) {
            if !validate_user_ptr(ptr + i * 8, 8) { break; }
        }
        let str_ptr = unsafe { *((ptr + i * 8) as *const u64) };
        if str_ptr == 0 { break; }
        
        // Copy string from the pointer
        match copy_from_user_str(str_ptr, 1024) {
            Ok(s) => args.push(s),
            Err(_) => break,
        }
        i += 1;
    }
    args
}

fn sys_spawn(path_ptr: u64, argv_ptr: u64, envp_ptr: u64, stdin_ptr: u64, stdin_len: usize, _a5: u64) -> u64 {
    let path = match copy_from_user_str(path_ptr, 512) {
        Ok(p) => p,
        Err(e) => return e,
    };
    
    let args_vec = parse_argv_array(argv_ptr);
    let env_vec = parse_argv_array(envp_ptr);
    
    let args_refs: Vec<&str> = if args_vec.len() > 1 {
        args_vec.iter().skip(1).map(|s| s.as_str()).collect()
    } else {
        Vec::new()
    };
    
    let stdin = if stdin_ptr != 0 {
        if !BYPASS_VALIDATION.load(Ordering::Acquire) {
            if !validate_user_ptr(stdin_ptr, stdin_len) { return EFAULT; }
        }
        Some(unsafe { core::slice::from_raw_parts(stdin_ptr as *const u8, stdin_len) })
    } else {
        None
    };

    if let Ok((_tid, ch, pid)) = crate::process::spawn_process_with_channel_cwd(&path, Some(&args_refs), Some(&env_vec), stdin, None) {
        if let Some(proc) = crate::process::current_process() {
            crate::process::register_child_channel(pid, ch, proc.pid);
            return (pid as u64) | ((proc.alloc_fd(crate::process::FileDescriptor::ChildStdout(pid)) as u64) << 32);
        }
    }
    !0u64
}

fn sys_spawn_ext(path_ptr: u64, path_len: usize, options_ptr: u64, _a3: u64, _a4: u64, _a5: u64) -> u64 {
    if !validate_user_ptr(path_ptr, path_len) { return EFAULT; }
    if !validate_user_ptr(options_ptr, core::mem::size_of::<SpawnOptions>()) { return EFAULT; }
    
    let path = unsafe { core::str::from_utf8(core::slice::from_raw_parts(path_ptr as *const u8, path_len)).unwrap_or("") };
    
    let options = if options_ptr != 0 {
        Some(unsafe { &*(options_ptr as *const SpawnOptions) })
    } else {
        None
    };

    if options.is_none() { return !0u64; }
    let o = options.unwrap();

    let cwd = if o.cwd_ptr != 0 {
        Some(unsafe { core::str::from_utf8(core::slice::from_raw_parts(o.cwd_ptr as *const u8, o.cwd_len)).unwrap_or("/") })
    } else {
        None
    };

    let root_dir = if o.root_dir_ptr != 0 {
        Some(unsafe { core::str::from_utf8(core::slice::from_raw_parts(o.root_dir_ptr as *const u8, o.root_dir_len)).unwrap_or("/") })
    } else {
        None
    };

    let args_vec = parse_argv_array(o.args_ptr);
    let args_refs: Vec<&str> = if args_vec.len() > 1 {
        args_vec.iter().skip(1).map(|s| s.as_str()).collect()
    } else {
        args_vec.iter().map(|s| s.as_str()).collect()
    };
    let args_opt = if args_refs.is_empty() { None } else { Some(args_refs.as_slice()) };

    let stdin = if o.stdin_ptr != 0 {
        Some(unsafe { core::slice::from_raw_parts(o.stdin_ptr as *const u8, o.stdin_len) })
    } else {
        None
    };

    // Call internal helper with extended options
    if let Ok((_tid, ch, pid)) = crate::process::spawn_process_with_channel_ext(path, args_opt, None, stdin, cwd, root_dir, o.box_id) {
        if let Some(proc) = crate::process::current_process() {
            crate::process::register_child_channel(pid, ch, proc.pid);
            return (pid as u64) | ((proc.alloc_fd(crate::process::FileDescriptor::ChildStdout(pid)) as u64) << 32);
        }
    }
    !0u64
}

fn sys_kill(pid: u32, _sig: u32) -> u64 {
    // Safety: prevent killing init or Box 0 implicitly if we add box killing logic here
    if pid == 0 { return 0; } // Success for process group 0 (stub)
    if pid <= 1 { return !0u64; }
    if crate::process::kill_process(pid).is_ok() { 0 } else { !0u64 }
}

fn sys_kill_box(box_id: u64) -> u64 {

    if crate::process::kill_box(box_id).is_ok() { 0 } else { !0u64 }

}



fn sys_reattach(pid: u32) -> u64 {

    if crate::process::reattach_process(pid).is_ok() { 0 } else { !0u64 }

}





fn sys_waitpid(pid: u32, status_ptr: u64) -> u64 {
    if status_ptr != 0 && !validate_user_ptr(status_ptr, 4) { return EFAULT; }

    if let Some(ch) = crate::process::get_child_channel(pid) {
        if ch.has_exited() {
            if status_ptr != 0 { unsafe { *(status_ptr as *mut u32) = encode_wait_status(ch.exit_code()); } }
            return pid as u64;
        }
    }
    0
}

fn sys_getrandom(ptr: u64, len: usize) -> u64 {
    if !validate_user_ptr(ptr, len) { return EFAULT; }
    let mut buf = alloc::vec![0u8; len.min(256)];
    if crate::rng::fill_bytes(&mut buf).is_ok() { unsafe { core::ptr::copy_nonoverlapping(buf.as_ptr(), ptr as *mut u8, buf.len()); } return buf.len() as u64; }
    !0u64
}

fn sys_time() -> u64 { crate::timer::utc_time_us().unwrap_or(0) }

fn sys_fchdir(fd: u32) -> u64 {
    let proc = match crate::process::current_process() { Some(p) => p, None => return !0u64 };
    let entry = match proc.get_fd(fd) {
        Some(e) => e,
        None => return EBADF,
    };
    let path = match entry {
        crate::process::FileDescriptor::File(f) => f.path.clone(),
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

fn sys_chdir(ptr: u64) -> u64 {
    let path = match copy_from_user_str(ptr, 512) {
        Ok(p) => p,
        Err(e) => return e,
    };
    
    if let Some(proc) = crate::process::current_process() {
        // Resolve path relative to current CWD
        let new_cwd = crate::vfs::resolve_path(&proc.cwd, &path);
        
        // Validate that the directory exists
        if crate::fs::exists(&new_cwd) {
            // Check if it's actually a directory
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

/// Helper: write data to the current process's ProcessChannel (stdout buffer)
fn write_to_process_channel(data: &[u8]) -> u64 {
    let proc_channel = match crate::process::current_channel() {
        Some(channel) => channel,
        None => return (-libc_errno::ENOMEM as i64) as u64,
    };
    proc_channel.write(data);
    data.len() as u64
}

/// sys_set_terminal_attributes - Sets terminal control attributes
fn sys_set_terminal_attributes(fd: u64, action: u64, mode_flags_arg: u64) -> u64 {
    let term_state_lock = match crate::process::current_terminal_state() {
        Some(state) => state,
        None => return (-libc_errno::ENOMEM as i64) as u64,
    };

    let mut term_state = term_state_lock.lock();
    term_state.mode_flags = mode_flags_arg;

    // Standard Linux-style flags for compatibility
    if (mode_flags_arg & mode_flags::RAW_MODE_ENABLE) != 0 {
        // Save the current flags so we can restore them exactly when raw mode ends.
        term_state.saved_iflag = Some(term_state.iflag);
        term_state.saved_oflag = Some(term_state.oflag);
        term_state.saved_lflag = Some(term_state.lflag);
        term_state.iflag &= !(0x00000100 | 0x00000040); // IGNBRK | ICRNL
        term_state.oflag &= !mode_flags::OPOST;
        term_state.lflag &= !(mode_flags::ECHO | mode_flags::ICANON);
    } else {
        // Restore whatever flags were in place before raw mode was enabled.
        // This correctly handles the case where the parent (e.g. dash) had already
        // configured the terminal (raw mode, no echo, etc.) before the child (meow)
        // temporarily enabled its own raw mode.
        if let Some(saved) = term_state.saved_iflag.take() {
            term_state.iflag = saved;
        }
        if let Some(saved) = term_state.saved_oflag.take() {
            term_state.oflag = saved;
        }
        if let Some(saved) = term_state.saved_lflag.take() {
            term_state.lflag = saved;
        } else {
            // No saved state (raw mode was never enabled via this syscall).
            // Fall back to restoring a sane default.
            term_state.oflag |= mode_flags::OPOST | mode_flags::ONLCR;
            term_state.lflag |= mode_flags::ECHO | mode_flags::ICANON;
        }
    }

    // Propagate raw mode setting to the ProcessChannel
    let proc_channel = match crate::process::current_channel() {
        Some(channel) => channel,
        None => return (-libc_errno::ENOMEM as i64) as u64,
    };
    proc_channel.set_raw_mode((term_state.lflag & mode_flags::ICANON) == 0);

    // Handle action: TCSAFLUSH (2) flushes input
    if action == 2 {
        proc_channel.flush_stdin();
    }

    0
}

/// sys_get_terminal_attributes - Retrieves current terminal control attributes
fn sys_get_terminal_attributes(fd: u64, attr_ptr: u64) -> u64 {
    if attr_ptr == 0 {
        return (-libc_errno::EINVAL as i64) as u64;
    }
    if !validate_user_ptr(attr_ptr, 8) { return EFAULT; }

    let term_state_lock = match crate::process::current_terminal_state() {
        Some(state) => state,
        None => return (-libc_errno::ENOMEM as i64) as u64,
    };

    let term_state = term_state_lock.lock();

    unsafe {
        *(attr_ptr as *mut u64) = term_state.mode_flags;
    }

    0
}

/// sys_set_cursor_position - Sets the cursor position via ANSI escape sequence
fn sys_set_cursor_position(col: u64, row: u64) -> u64 {
    if config::SYSCALL_DEBUG_INFO_ENABLED {
        crate::safe_print!(64, "[syscall] sys_set_cursor_position({}, {})\n", col, row);
    }
    // VT100/ANSI escape sequence: ESC[{row};{col}H (1-indexed)
    let row_1 = row + 1;
    let col_1 = col + 1;
    let sequence = alloc::format!("\x1b[{};{}H", row_1, col_1);
    write_to_process_channel(sequence.as_bytes())
}

/// sys_hide_cursor - Hides the terminal cursor
fn sys_hide_cursor() -> u64 {
    if config::SYSCALL_DEBUG_INFO_ENABLED {
        crate::safe_print!(64, "[syscall] sys_hide_cursor()\n");
    }
    write_to_process_channel(b"\x1b[?25l")
}

/// sys_show_cursor - Shows the terminal cursor
fn sys_show_cursor() -> u64 {
    if config::SYSCALL_DEBUG_INFO_ENABLED {
        crate::safe_print!(64, "[syscall] sys_show_cursor()\n");
    }
    write_to_process_channel(b"\x1b[?25h")
}

/// sys_clear_screen - Clears the entire terminal screen
fn sys_clear_screen() -> u64 {
    if config::SYSCALL_DEBUG_INFO_ENABLED {
        crate::safe_print!(64, "[syscall] sys_clear_screen()\n");
    }
    write_to_process_channel(b"\x1b[2J")
}

fn sys_poll_input_event(buf_ptr: u64, buf_len: usize, timeout_us: u64) -> u64 {
    if buf_ptr == 0 || buf_len == 0 {
        return (-libc_errno::EINVAL as i64) as u64;
    }
    if !validate_user_ptr(buf_ptr, buf_len) { return EFAULT; }

    if config::SYSCALL_DEBUG_INFO_ENABLED && timeout_us > 0 && timeout_us != u64::MAX {
        // Only print for non-infinite timeouts to avoid noise
        // crate::safe_print!(64, "[syscall] poll_input_event: timeout={}us\n", timeout_us);
    }

    let proc_channel = match crate::process::current_channel() {
        Some(channel) => channel,
        None => return (-libc_errno::ENOMEM as i64) as u64,
    };

    let term_state_lock = match crate::process::current_terminal_state() {
        Some(state) => state,
        None => return (-libc_errno::EBADF as i64) as u64,
    };

    let mut kernel_buf = alloc::vec![0u8; buf_len];
    let bytes_read;

    if timeout_us == 0 {
        // Non-blocking read
        bytes_read = proc_channel.read_stdin(&mut kernel_buf);
    } else {
        // Blocking or timed read
        let deadline = if timeout_us == u64::MAX {
            u64::MAX
        } else {
            crate::timer::uptime_us().saturating_add(timeout_us)
        };

        loop {
            // CRITICAL: Register waker BEFORE checking for data to avoid lost wake-up race
            {
                crate::threading::disable_preemption();
                let mut term_state = term_state_lock.lock();
                let thread_id = crate::threading::current_thread_id();
                term_state.set_input_waker(crate::threading::get_waker_for_thread(thread_id));
                crate::threading::enable_preemption();
            }

            // Check for data AFTER registering waker
            let n = proc_channel.read_stdin(&mut kernel_buf);
            if n > 0 {
                bytes_read = n;
                break;
            }

            if crate::process::is_current_interrupted() {
                return (-libc_errno::EINTR as i64) as u64;
            }

            if crate::timer::uptime_us() >= deadline {
                bytes_read = 0;
                break;
            }

            // Yield, will be woken by SSH if input arrives (calling waker.wake())
            crate::threading::schedule_blocking(deadline);

            // Clear waker after being woken up or timeout
            {
                crate::threading::disable_preemption();
                let mut term_state = term_state_lock.lock();
                term_state.input_waker.lock().take();
                crate::threading::enable_preemption();
            }
        }
    }

    if bytes_read > 0 {
        unsafe {
            core::ptr::copy_nonoverlapping(kernel_buf.as_ptr(), buf_ptr as *mut u8, bytes_read);
        }
        bytes_read as u64
    } else {
        0
    }
}

fn sys_get_cpu_stats(ptr: u64, max: usize) -> u64 {
    if !validate_user_ptr(ptr, max * core::mem::size_of::<ThreadCpuStat>()) { return EFAULT; }
    let count = max.min(config::MAX_THREADS);
    for i in 0..count {
        let mut stat = ThreadCpuStat {
            tid: i as u32,
            total_time_us: crate::threading::get_thread_cpu_time(i),
            state: crate::threading::get_thread_state(i),
            ..Default::default()
        };

        // Lookup PID and name from process table
        if let Some(pid) = crate::process::find_pid_by_thread(i) {
            stat.pid = pid;
            if let Some(proc) = crate::process::lookup_process(pid) {
                stat.box_id = proc.box_id;
                let name_bytes = proc.name.as_bytes();
                // Ensure name is clean (already zeroed by Default::default(), but being explicit)
                let to_copy = name_bytes.len().min(stat.name.len());
                stat.name[..to_copy].copy_from_slice(&name_bytes[..to_copy]);
                if to_copy < stat.name.len() {
                    for b in &mut stat.name[to_copy..] { *b = 0; }
                }
            }
        } else if i == 0 {
            // Thread 0 is special (Kernel/Idle)
            stat.name[..6].copy_from_slice(b"kernel");
            for b in &mut stat.name[6..] { *b = 0; }
        } else {
            // Ensure name is empty if no process found
            for b in &mut stat.name { *b = 0; }
        }

        unsafe { core::ptr::write_volatile((ptr as *mut ThreadCpuStat).add(i), stat); }
    }
    count as u64
}

// ============================================================================
// Framebuffer Syscalls
// ============================================================================

/// sys_fb_init - Initialize the ramfb framebuffer
///
/// # Arguments
/// * `width` - Desired framebuffer width in pixels
/// * `height` - Desired framebuffer height in pixels
///
/// # Returns
/// 0 on success, negative errno on failure
fn sys_fb_init(width: u32, height: u32) -> u64 {
    if width == 0 || height == 0 || width > 1920 || height > 1080 {
        return (-libc_errno::EINVAL as i64) as u64;
    }

    match crate::ramfb::init(width, height) {
        Ok(()) => 0,
        Err(_) => (-libc_errno::EIO as i64) as u64,
    }
}

/// sys_fb_draw - Copy pixel data from userspace buffer to framebuffer
///
/// # Arguments
/// * `buf_ptr` - Pointer to userspace XRGB8888 pixel buffer
/// * `buf_len` - Length of the buffer in bytes
///
/// # Returns
/// Number of bytes copied on success, negative errno on failure
fn sys_fb_draw(buf_ptr: u64, buf_len: usize) -> u64 {
    if buf_ptr == 0 || buf_len == 0 {
        return (-libc_errno::EINVAL as i64) as u64;
    }

    if !crate::ramfb::is_initialized() {
        return (-libc_errno::EIO as i64) as u64;
    }

    // Read pixels from userspace buffer
    let src = unsafe { core::slice::from_raw_parts(buf_ptr as *const u8, buf_len) };

    let copied = crate::ramfb::draw(src);
    if copied == 0 {
        (-libc_errno::EIO as i64) as u64
    } else {
        copied as u64
    }
}

/// sys_fb_info - Get framebuffer information
///
/// # Arguments
/// * `info_ptr` - Pointer to userspace FBInfo struct to fill
///
/// # Returns
/// 0 on success, negative errno on failure
fn sys_fb_info(info_ptr: u64) -> u64 {
    if info_ptr == 0 {
        return (-libc_errno::EINVAL as i64) as u64;
    }

    match crate::ramfb::info() {
        Some(info) => {
            unsafe {
                core::ptr::write(info_ptr as *mut crate::ramfb::FBInfo, info);
            }
            0
        }
        None => (-libc_errno::EIO as i64) as u64,
    }
}

fn sys_getpid() -> u64 {
    crate::process::read_current_pid().map_or(!0u64, |pid| pid as u64)
}

fn sys_getppid() -> u64 {
    if let Some(proc) = crate::process::current_process() {
        proc.parent_pid as u64
    } else {
        !0u64 // Return error if no current process
    }
}

fn sys_geteuid() -> u64 {
    0 // Return 0 for root/default user, as Akuma does not have robust user management yet.
}
