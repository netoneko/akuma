//! System Call Handlers
//!
//! Implements the syscall interface for user programs.
//! Uses Linux-compatible ABI: syscall number in x8, arguments in x0-x5.

use alloc::string::String;
use alloc::vec::Vec;
use alloc::collections::BTreeMap;
use alloc::format;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use spinning_top::Spinlock;

mod container;
mod eventfd;
mod fb;
mod fs;
mod mem;
mod net;
mod pipe;
mod poll;
mod proc;
mod signal;
mod sync;
mod term;
mod time;
mod timerfd;

pub use sync::futex_wake;
pub use mem::membarrier_cmd;
pub(crate) use fs::sys_close_range;

pub static CURRENT_SYSCALL_NR: AtomicU64 = AtomicU64::new(9999);
pub fn current_syscall_nr() -> u64 { CURRENT_SYSCALL_NR.load(Ordering::Relaxed) }

pub mod syscall_counters {
    use core::sync::atomic::{AtomicU64, Ordering};
    static MMAP_COUNT: AtomicU64 = AtomicU64::new(0);
    static MMAP_PAGES: AtomicU64 = AtomicU64::new(0);
    static MUNMAP_COUNT: AtomicU64 = AtomicU64::new(0);
    static BRK_COUNT: AtomicU64 = AtomicU64::new(0);
    static READ_COUNT: AtomicU64 = AtomicU64::new(0);
    static WRITE_COUNT: AtomicU64 = AtomicU64::new(0);
    static OPENAT_COUNT: AtomicU64 = AtomicU64::new(0);
    static CLOSE_COUNT: AtomicU64 = AtomicU64::new(0);
    static MPROTECT_COUNT: AtomicU64 = AtomicU64::new(0);
    static FUTEX_COUNT: AtomicU64 = AtomicU64::new(0);
    static SIGPROCMASK_COUNT: AtomicU64 = AtomicU64::new(0);
    static SIGACTION_COUNT: AtomicU64 = AtomicU64::new(0);
    static CLOCK_COUNT: AtomicU64 = AtomicU64::new(0);
    static IOCTL_COUNT: AtomicU64 = AtomicU64::new(0);
    static FSTAT_COUNT: AtomicU64 = AtomicU64::new(0);
    static YIELD_COUNT: AtomicU64 = AtomicU64::new(0);
    static MADVISE_COUNT: AtomicU64 = AtomicU64::new(0);
    static MREMAP_COUNT: AtomicU64 = AtomicU64::new(0);
    static LSEEK_COUNT: AtomicU64 = AtomicU64::new(0);
    static GETRANDOM_COUNT: AtomicU64 = AtomicU64::new(0);
    static GETPID_COUNT: AtomicU64 = AtomicU64::new(0);
    static FCNTL_COUNT: AtomicU64 = AtomicU64::new(0);
    static TOTAL_COUNT: AtomicU64 = AtomicU64::new(0);
    static PAGEFAULT_COUNT: AtomicU64 = AtomicU64::new(0);
    static PAGEFAULT_PAGES: AtomicU64 = AtomicU64::new(0);
    static OTHER_LAST_NR: AtomicU64 = AtomicU64::new(0);
    static OTHER_COUNT: AtomicU64 = AtomicU64::new(0);

    pub fn inc_mmap(pages: usize) { MMAP_COUNT.fetch_add(1, Ordering::Relaxed); MMAP_PAGES.fetch_add(pages as u64, Ordering::Relaxed); }
    pub fn inc_munmap() { MUNMAP_COUNT.fetch_add(1, Ordering::Relaxed); }
    pub fn inc_brk() { BRK_COUNT.fetch_add(1, Ordering::Relaxed); }
    pub fn inc_read() { READ_COUNT.fetch_add(1, Ordering::Relaxed); }
    pub fn inc_write() { WRITE_COUNT.fetch_add(1, Ordering::Relaxed); }
    pub fn inc_openat() { OPENAT_COUNT.fetch_add(1, Ordering::Relaxed); }
    pub fn inc_close() { CLOSE_COUNT.fetch_add(1, Ordering::Relaxed); }
    pub fn inc_mprotect() { MPROTECT_COUNT.fetch_add(1, Ordering::Relaxed); }
    pub fn inc_futex() { FUTEX_COUNT.fetch_add(1, Ordering::Relaxed); }
    pub fn inc_sigprocmask() { SIGPROCMASK_COUNT.fetch_add(1, Ordering::Relaxed); }
    pub fn inc_sigaction() { SIGACTION_COUNT.fetch_add(1, Ordering::Relaxed); }
    pub fn inc_clock() { CLOCK_COUNT.fetch_add(1, Ordering::Relaxed); }
    pub fn inc_ioctl() { IOCTL_COUNT.fetch_add(1, Ordering::Relaxed); }
    pub fn inc_fstat() { FSTAT_COUNT.fetch_add(1, Ordering::Relaxed); }
    pub fn inc_yield() { YIELD_COUNT.fetch_add(1, Ordering::Relaxed); }
    pub fn inc_madvise() { MADVISE_COUNT.fetch_add(1, Ordering::Relaxed); }
    pub fn inc_mremap() { MREMAP_COUNT.fetch_add(1, Ordering::Relaxed); }
    pub fn inc_lseek() { LSEEK_COUNT.fetch_add(1, Ordering::Relaxed); }
    pub fn inc_getrandom() { GETRANDOM_COUNT.fetch_add(1, Ordering::Relaxed); }
    pub fn inc_getpid() { GETPID_COUNT.fetch_add(1, Ordering::Relaxed); }
    pub fn inc_fcntl() { FCNTL_COUNT.fetch_add(1, Ordering::Relaxed); }
    pub fn inc_other(nr: u64) { OTHER_COUNT.fetch_add(1, Ordering::Relaxed); OTHER_LAST_NR.store(nr, Ordering::Relaxed); }
    pub fn inc_total() { TOTAL_COUNT.fetch_add(1, Ordering::Relaxed); }
    pub fn inc_pagefault(pages_mapped: u64) { PAGEFAULT_COUNT.fetch_add(1, Ordering::Relaxed); PAGEFAULT_PAGES.fetch_add(pages_mapped, Ordering::Relaxed); }

    pub fn dump() {
        let total = TOTAL_COUNT.load(Ordering::Relaxed);
        let other = OTHER_COUNT.load(Ordering::Relaxed);
        let last_nr = OTHER_LAST_NR.load(Ordering::Relaxed);
        crate::safe_print!(512,
            "[SC-STATS] total={} madvise={} mremap={} lseek={} rnd={} pid={} fcntl={} other={}(last_nr={})\n",
            total,
            MADVISE_COUNT.load(Ordering::Relaxed),
            MREMAP_COUNT.load(Ordering::Relaxed),
            LSEEK_COUNT.load(Ordering::Relaxed),
            GETRANDOM_COUNT.load(Ordering::Relaxed),
            GETPID_COUNT.load(Ordering::Relaxed),
            FCNTL_COUNT.load(Ordering::Relaxed),
            other, last_nr,
        );
        crate::safe_print!(512,
            "[SC-STATS] futex={} sigmask={} sigact={} clk={} ioctl={} fstat={} yield={}\n",
            FUTEX_COUNT.load(Ordering::Relaxed),
            SIGPROCMASK_COUNT.load(Ordering::Relaxed),
            SIGACTION_COUNT.load(Ordering::Relaxed),
            CLOCK_COUNT.load(Ordering::Relaxed),
            IOCTL_COUNT.load(Ordering::Relaxed),
            FSTAT_COUNT.load(Ordering::Relaxed),
            YIELD_COUNT.load(Ordering::Relaxed),
        );
        crate::safe_print!(384,
            "[SC-STATS] mmap={}({}pg) munmap={} brk={} read={} write={} open={} close={} mprot={} pgfault={}({}pg)\n",
            MMAP_COUNT.load(Ordering::Relaxed),
            MMAP_PAGES.load(Ordering::Relaxed),
            MUNMAP_COUNT.load(Ordering::Relaxed),
            BRK_COUNT.load(Ordering::Relaxed),
            READ_COUNT.load(Ordering::Relaxed),
            WRITE_COUNT.load(Ordering::Relaxed),
            OPENAT_COUNT.load(Ordering::Relaxed),
            CLOSE_COUNT.load(Ordering::Relaxed),
            MPROTECT_COUNT.load(Ordering::Relaxed),
            PAGEFAULT_COUNT.load(Ordering::Relaxed),
            PAGEFAULT_PAGES.load(Ordering::Relaxed),
        );
    }
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
    pub const NANOSLEEP: u64 = 101;
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
    pub const MUNMAP: u64 = 215;
    pub const MREMAP: u64 = 216;
    pub const MMAP: u64 = 222;
    pub const GETDENTS64: u64 = 61;
    pub const PSELECT6: u64 = 72;
    pub const PPOLL: u64 = 73;
    pub const MKDIRAT: u64 = 34;
    pub const UNLINKAT: u64 = 35;
    pub const SYMLINKAT: u64 = 36;
    pub const LINKAT: u64 = 37;
    pub const RENAMEAT: u64 = 38;
    pub const READLINKAT: u64 = 78;
    pub const SET_TID_ADDRESS: u64 = 96;
    pub const EXIT_GROUP: u64 = 94;
    pub const RT_SIGPROCMASK: u64 = 135;
    pub const RT_SIGACTION: u64 = 134;
    pub const RT_SIGRETURN: u64 = 139;
    pub const RT_SIGSUSPEND: u64 = 133;
    pub const GETRANDOM: u64 = 278;
    pub const GETCWD: u64 = 17;
    pub const FCNTL: u64 = 25;
    pub const DUP: u64 = 23;
    pub const FSTATFS: u64 = 44;
    pub const DUP3: u64 = 24;
    pub const PIPE2: u64 = 59;
    pub const NEWFSTATAT: u64 = 79;
    pub const FACCESSAT: u64 = 48;
    pub const CLOCK_GETTIME: u64 = 113;
    pub const CLONE3: u64 = 435;
    pub const FACCESSAT2: u64 = 439;
    pub const WAIT4: u64 = 260;
    pub const RESOLVE_HOST: u64 = 300;
    pub const SPAWN: u64 = 301;
    pub const KILL: u64 = 302;
    pub const WAITPID: u64 = 303;
    pub const TIME: u64 = 305;
    pub const CHDIR: u64 = 49;
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
    pub const MPROTECT: u64 = 226;
    pub const FUTEX: u64 = 98;
    pub const SET_ROBUST_LIST: u64 = 99;
    pub const SIGALTSTACK: u64 = 132;
    pub const GETRLIMIT: u64 = 163;
    pub const PRLIMIT64: u64 = 261;
    pub const EVENTFD2: u64 = 19;
    pub const PREAD64: u64 = 67;
    pub const PWRITE64: u64 = 68;
    pub const SETITIMER: u64 = 103;
    pub const MEMBARRIER: u64 = 283;
    pub const PRCTL: u64 = 167;
    pub const GETRUSAGE: u64 = 165;
    pub const MSYNC: u64 = 227;
    pub const PROCESS_VM_READV: u64 = 270;
    pub const SCHED_SETAFFINITY: u64 = 122;
    pub const SCHED_GETAFFINITY: u64 = 123;
    pub const TKILL: u64 = 130;
    pub const PIDFD_OPEN: u64 = 434;
    pub const CLOSE_RANGE: u64 = 436;
    pub const SYSINFO: u64 = 179;
    pub const CLOCK_GETRES: u64 = 114;
    pub const EPOLL_CREATE1: u64 = 20;
    pub const EPOLL_CTL: u64 = 21;
    pub const EPOLL_PWAIT: u64 = 22;
    pub const TIMERFD_CREATE: u64 = 85;
    pub const TIMERFD_SETTIME: u64 = 86;
    pub const TIMERFD_GETTIME: u64 = 87;
    pub const CAPGET: u64 = 90;
    pub const IO_URING_SETUP: u64 = 425;
    pub const IO_URING_ENTER: u64 = 426;
    pub const IO_URING_REGISTER: u64 = 427;
    pub const INOTIFY_INIT1: u64 = 26;
    pub const INOTIFY_ADD_WATCH: u64 = 27;
    pub const INOTIFY_RM_WATCH: u64 = 28;
    pub const ACCEPT4: u64 = 242;
    pub const TIMES: u64 = 153;
    pub const MOUNT: u64 = 40;
    pub const UMOUNT2: u64 = 39;
    pub const MOUNT_IN_NS: u64 = 325;
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

const EINTR: u64 = (-4i64) as u64;
const ENOENT: u64 = (-2i64) as u64;
const EFAULT: u64 = (-14i64) as u64;
const EINVAL: u64 = (-22i64) as u64;
const EBADF: u64 = (-9i64) as u64;
const ENOSYS: u64 = (-38i64) as u64;
const EPERM: u64 = (-1i64) as u64;
const ENOTDIR: u64 = (-20i64) as u64;
const EISDIR: u64 = (-21i64) as u64;
const ENOTEMPTY: u64 = (-39i64) as u64;
const EEXIST: u64 = (-17i64) as u64;
const ENOSPC: u64 = (-28i64) as u64;
const EAGAIN: u64 = (-11i64) as u64;
const ENOMEM: u64 = (-12i64) as u64;
const EAFNOSUPPORT: u64 = (-97i64) as u64;
const EINPROGRESS: u64 = (-115i64) as u64;
const ETIMEDOUT: u64 = (-110i64) as u64;
const ENODEV: u64 = (-19i64) as u64;

#[repr(C)]
struct Timespec {
    tv_sec: i64,
    tv_nsec: i64,
}

fn user_va_limit() -> u64 {
    if let Some(proc) = akuma_exec::process::current_process() {
        proc.memory.stack_top as u64
    } else {
        0x4000_0000
    }
}

fn validate_user_ptr(ptr: u64, len: usize) -> bool {
    if BYPASS_VALIDATION.load(Ordering::Acquire) { return true; }
    if ptr < 0x1000 { return false; }
    let end = match ptr.checked_add(len as u64) {
        Some(e) => e,
        None => return false,
    };
    if end > user_va_limit() { return false; }

    if !akuma_exec::mmu::is_current_user_range_mapped(ptr as usize, len) {
        if !ensure_user_pages_mapped(ptr as usize, len) {
            return false;
        }
    }

    true
}

fn ensure_user_pages_mapped(start: usize, len: usize) -> bool {
    let page_start = start & !0xFFF;
    let page_end = (start + len + 0xFFF) & !0xFFF;
    let mut va = page_start;
    while va < page_end {
        if !akuma_exec::mmu::is_current_user_page_mapped(va) {
            if let Some((flags, source, _region_start, _region_size)) = akuma_exec::process::lazy_region_lookup(va) {
                let map_flags = match &source {
                    akuma_exec::process::LazySource::File { .. } => {
                        if flags != 0 { flags } else { akuma_exec::mmu::user_flags::RW_NO_EXEC }
                    }
                    _ => akuma_exec::mmu::user_flags::RW_NO_EXEC,
                };
                if let Some(page_frame) = crate::pmm::alloc_page_zeroed() {
                    if let akuma_exec::process::LazySource::File { ref path, inode, file_offset, filesz, segment_va } = source {
                        let pg_data_start = core::cmp::max(va, segment_va);
                        let pg_data_end = core::cmp::min(va + 0x1000, segment_va + filesz);
                        if pg_data_start < pg_data_end {
                            let dst_off = pg_data_start - va;
                            let file_off = file_offset + (pg_data_start - segment_va);
                            let read_len = pg_data_end - pg_data_start;
                            let page_ptr = akuma_exec::mmu::phys_to_virt(page_frame.addr);
                            let page_buf = unsafe {
                                core::slice::from_raw_parts_mut((page_ptr as *mut u8).add(dst_off), read_len)
                            };
                            if inode != 0 {
                                let _ = crate::vfs::read_at_by_inode(path, inode, file_off, page_buf);
                            } else {
                                let _ = crate::vfs::read_at(path, file_off, page_buf);
                            }
                        }
                    }
                    let (table_frames, installed) = unsafe {
                        akuma_exec::mmu::map_user_page(va, page_frame.addr, map_flags)
                    };
                    let owner_pid = akuma_exec::process::read_current_pid().unwrap_or(0);
                    if let Some(owner) = akuma_exec::process::lookup_process(owner_pid) {
                        if installed {
                            owner.address_space.track_user_frame(page_frame);
                        } else {
                            crate::pmm::free_page(page_frame);
                        }
                        for tf in table_frames {
                            owner.address_space.track_page_table_frame(tf);
                        }
                    } else {
                        if installed {
                            crate::pmm::free_page(page_frame);
                        }
                        for tf in table_frames { crate::pmm::free_page(tf); }
                    }
                } else {
                    return false;
                }
            } else {
                return false;
            }
        }
        va += 4096;
    }
    true
}

fn copy_from_user_str(ptr: u64, max_len: usize) -> Result<String, u64> {
    let limit = user_va_limit();
    if !BYPASS_VALIDATION.load(Ordering::Acquire) {
        if ptr < 0x1000 || ptr >= limit { return Err(EFAULT); }
        if !akuma_exec::mmu::is_current_user_range_mapped(ptr as usize, 1) { return Err(EFAULT); }
    }
    let mut len = 0;
    while len < max_len {
        let addr = ptr + len as u64;
        if !BYPASS_VALIDATION.load(Ordering::Acquire) {
            if addr >= limit { return Err(EFAULT); }
            if addr % 4096 == 0 {
                if !akuma_exec::mmu::is_current_user_range_mapped(addr as usize, 1) { return Err(EFAULT); }
            }
        }
        let c = unsafe { *(addr as *const u8) };
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

pub fn handle_syscall(syscall_num: u64, args: &[u64; 6]) -> u64 {
    CURRENT_SYSCALL_NR.store(syscall_num, Ordering::Relaxed);

    if akuma_exec::process::is_current_interrupted() {
        if let Some(proc) = akuma_exec::process::current_process() {
            proc.exited = true;
            proc.exit_code = 130;
            proc.state = akuma_exec::process::ProcessState::Zombie(130);
        }
        return EINTR;
    }

    if crate::config::SYSCALL_DEBUG_IO_ENABLED && syscall_num != nr::WRITE && syscall_num != nr::READ && syscall_num != nr::READV && syscall_num != nr::WRITEV && syscall_num != nr::IOCTL && syscall_num != nr::PSELECT6 && syscall_num != nr::PPOLL && syscall_num != nr::BRK && syscall_num != nr::MMAP && syscall_num != nr::MUNMAP && syscall_num != nr::MREMAP && syscall_num != nr::CLOSE && syscall_num != nr::FSTAT && syscall_num != nr::LSEEK && syscall_num != nr::RT_SIGPROCMASK && syscall_num != nr::NANOSLEEP && syscall_num != nr::WAITPID && syscall_num != nr::UPTIME && syscall_num != nr::FUTEX && syscall_num != nr::MEMBARRIER && syscall_num != nr::RT_SIGACTION && syscall_num != nr::SCHED_SETAFFINITY && syscall_num != nr::SCHED_GETAFFINITY {
        crate::safe_print!(128, "[SC] nr={} a0=0x{:x} a1=0x{:x} a2=0x{:x}\n", syscall_num, args[0], args[1], args[2]);
    }

    syscall_counters::inc_total();
    match syscall_num {
        nr::MMAP => { syscall_counters::inc_mmap(((args[1] as usize) + 4095) / 4096); }
        nr::MUNMAP => { syscall_counters::inc_munmap(); }
        nr::BRK => { syscall_counters::inc_brk(); }
        nr::READ | nr::READV | nr::PREAD64 => { syscall_counters::inc_read(); }
        nr::WRITE | nr::WRITEV | nr::PWRITE64 => { syscall_counters::inc_write(); }
        nr::OPENAT => { syscall_counters::inc_openat(); }
        nr::CLOSE => { syscall_counters::inc_close(); }
        nr::MPROTECT => { syscall_counters::inc_mprotect(); }
        nr::FUTEX => { syscall_counters::inc_futex(); }
        nr::RT_SIGPROCMASK => { syscall_counters::inc_sigprocmask(); }
        nr::RT_SIGACTION => { syscall_counters::inc_sigaction(); }
        nr::CLOCK_GETTIME => { syscall_counters::inc_clock(); }
        nr::IOCTL => { syscall_counters::inc_ioctl(); }
        nr::FSTAT | nr::NEWFSTATAT => { syscall_counters::inc_fstat(); }
        124 => { syscall_counters::inc_yield(); }
        nr::MADVISE => { syscall_counters::inc_madvise(); }
        nr::MREMAP => { syscall_counters::inc_mremap(); }
        nr::LSEEK => { syscall_counters::inc_lseek(); }
        nr::GETRANDOM => { syscall_counters::inc_getrandom(); }
        nr::GETPID => { syscall_counters::inc_getpid(); }
        nr::FCNTL => { syscall_counters::inc_fcntl(); }
        _ => { syscall_counters::inc_other(syscall_num); }
    }

    let track_time = crate::config::PROCESS_SYSCALL_STATS;
    if track_time {
        let owner_pid = akuma_exec::process::read_current_pid().unwrap_or(0);
        if let Some(proc) = akuma_exec::process::lookup_process(owner_pid) {
            proc.syscall_stats.inc(syscall_num);
        }
    }

    let t0 = if track_time { crate::timer::uptime_us() } else { 0 };

    let result = match syscall_num {
        nr::EXIT => proc::sys_exit(args[0] as i32),
        nr::READ => fs::sys_read(args[0], args[1], args[2] as usize),
        nr::WRITE => fs::sys_write(args[0], args[1], args[2] as usize),
        nr::READV => fs::sys_readv(args[0], args[1], args[2] as usize),
        nr::WRITEV => fs::sys_writev(args[0], args[1], args[2] as usize),
        nr::IOCTL => term::sys_ioctl(args[0] as u32, args[1] as u32, args[2]),
        nr::DUP => fs::sys_dup(args[0] as u32),
        nr::FSTATFS => fs::sys_fstatfs(args[0] as u32, args[1]),
        nr::DUP3 => fs::sys_dup3(args[0] as u32, args[1] as u32, args[2] as u32),
        nr::PIPE2 => pipe::sys_pipe2(args[0], args[1] as u32),
        nr::BRK => mem::sys_brk(args[0] as usize),
        nr::OPENAT => fs::sys_openat(args[0] as i32, args[1], args[2] as u32, args[3] as u32),
        nr::CLOSE => fs::sys_close(args[0] as u32),
        nr::LSEEK => fs::sys_lseek(args[0] as u32, args[1] as i64, args[2] as i32),
        nr::FSTAT => fs::sys_fstat(args[0] as u32, args[1]),
        nr::NANOSLEEP => time::sys_nanosleep(args[0], args[1]),
        nr::SOCKET => net::sys_socket(args[0] as i32, args[1] as i32, args[2] as i32),
        nr::BIND => net::sys_bind(args[0] as u32, args[1], args[2] as usize),
        nr::LISTEN => net::sys_listen(args[0] as u32, args[1] as i32),
        nr::ACCEPT => net::sys_accept(args[0] as u32, args[1], args[2]),
        nr::ACCEPT4 => net::sys_accept4(args[0] as u32, args[1], args[2], args[3] as u32),
        nr::CONNECT => net::sys_connect(args[0] as u32, args[1], args[2] as usize),
        nr::SENDTO => net::sys_sendto(args[0] as u32, args[1], args[2] as usize, args[3] as i32, args[4], args[5] as usize),
        nr::RECVFROM => net::sys_recvfrom(args[0] as u32, args[1], args[2] as usize, args[3] as i32, args[4], args[5]),
        nr::GETSOCKNAME => net::sys_getsockname(args[0] as u32, args[1], args[2]),
        nr::GETPEERNAME => net::sys_getpeername(args[0] as u32, args[1], args[2]),
        nr::SETSOCKOPT => net::sys_setsockopt(args[0] as u32, args[1] as i32, args[2] as i32, args[3], args[4] as u32),
        nr::GETSOCKOPT => net::sys_getsockopt(args[0] as u32, args[1] as i32, args[2] as i32, args[3], args[4]),
        nr::SHUTDOWN => net::sys_shutdown(args[0] as u32, args[1] as i32),
        nr::SENDMSG => net::sys_sendmsg(args[0] as u32, args[1], args[2] as i32),
        nr::RECVMSG => net::sys_recvmsg(args[0] as u32, args[1], args[2] as i32),
        nr::MREMAP => mem::sys_mremap(args[0] as usize, args[1] as usize, args[2] as usize, args[3] as u32),
        nr::MMAP => mem::sys_mmap(args[0] as usize, args[1] as usize, args[2] as u32, args[3] as u32, args[4] as i32, args[5] as usize),
        nr::MUNMAP => mem::sys_munmap(args[0] as usize, args[1] as usize),
        nr::CLONE => proc::sys_clone(args[0], args[1], args[2], args[3], args[4]),
        nr::CLONE3 => proc::sys_clone3(args[0], args[1] as usize),
        nr::EXECVE => proc::sys_execve(args[0], args[1], args[2]),
        nr::UPTIME => time::sys_uptime(),
        nr::RESOLVE_HOST => net::sys_resolve_host(args[0], args[1] as usize, args[2]),
        nr::GETDENTS64 => fs::sys_getdents64(args[0] as u32, args[1], args[2] as usize),
        nr::PSELECT6 => poll::sys_pselect6(args[0] as usize, args[1], args[2], args[3], args[4], args[5]),
        nr::PPOLL => poll::sys_ppoll(args[0], args[1] as usize, args[2], args[3]),
        nr::MKDIRAT => fs::sys_mkdirat(args[0] as i32, args[1], args[2] as u32),
        nr::UNLINKAT => fs::sys_unlinkat(args[0] as i32, args[1], args[2] as u32),
        nr::SYMLINKAT => fs::sys_symlinkat(args[0], args[1] as i32, args[2]),
        nr::LINKAT => fs::sys_linkat(args[0] as i32, args[1], args[2] as i32, args[3], args[4] as u32),
        nr::RENAMEAT => fs::sys_renameat(args[0] as i32, args[1], args[2] as i32, args[3]),
        nr::READLINKAT => fs::sys_readlinkat(args[0] as i32, args[1], args[2], args[3] as usize),
        nr::SPAWN => proc::sys_spawn(args[0], args[1], args[2], args[3], args[4] as usize, args[5]),
        nr::KILL => proc::sys_kill(args[0] as u32, args[1] as u32),
        nr::WAITPID => proc::sys_waitpid(args[0] as u32, args[1]),
        nr::GETRANDOM => proc::sys_getrandom(args[0], args[1] as usize),
        nr::TIME => time::sys_time(),
        nr::CHDIR => fs::sys_chdir(args[0]),
        nr::FCHDIR => fs::sys_fchdir(args[0] as u32),
        nr::SET_TERMINAL_ATTRIBUTES => term::sys_set_terminal_attributes(args[0], args[1], args[2]),
        nr::GET_TERMINAL_ATTRIBUTES => term::sys_get_terminal_attributes(args[0], args[1]),
        nr::SET_CURSOR_POSITION => term::sys_set_cursor_position(args[0], args[1]),
        nr::HIDE_CURSOR => term::sys_hide_cursor(),
        nr::SHOW_CURSOR => term::sys_show_cursor(),
        nr::CLEAR_SCREEN => term::sys_clear_screen(),
        nr::POLL_INPUT_EVENT => term::sys_poll_input_event(args[0], args[1] as usize, args[2]),
        nr::GET_CPU_STATS => term::sys_get_cpu_stats(args[0], args[1] as usize),
        nr::SPAWN_EXT => proc::sys_spawn_ext(args[0], args[1], args[2], args[3], args[4], args[5]),
        nr::REGISTER_BOX => container::sys_register_box(args[0] as u64, args[1], args[2] as usize, args[3], args[4] as usize, args[5] as u32),
        nr::KILL_BOX => container::sys_kill_box(args[0] as u64),
        nr::REATTACH => container::sys_reattach(args[0] as u32),
        nr::SET_TID_ADDRESS => proc::sys_set_tid_address(args[0]),
        nr::EXIT_GROUP => proc::sys_exit_group(args[0] as i32),
        nr::RT_SIGPROCMASK => signal::sys_rt_sigprocmask(args[0] as u32, args[1], args[2], args[3] as usize),
        nr::RT_SIGSUSPEND => 0,
        nr::RT_SIGRETURN => 0,
        nr::RT_SIGACTION => signal::sys_rt_sigaction(args[0] as u32, args[1] as usize, args[2] as usize, args[3] as usize),
        nr::GETCWD => fs::sys_getcwd(args[0], args[1] as usize),
        nr::FCNTL => fs::sys_fcntl(args[0] as u32, args[1] as u32, args[2]),
        nr::NEWFSTATAT => fs::sys_newfstatat(args[0] as i32, args[1], args[2], args[3] as u32),
        nr::FACCESSAT => fs::sys_faccessat2(args[0] as i32, args[1], args[2] as u32, 0),
        nr::CLOCK_GETTIME => time::sys_clock_gettime(args[0] as u32, args[1]),
        nr::FACCESSAT2 => fs::sys_faccessat2(args[0] as i32, args[1], args[2] as u32, args[3] as u32),
        nr::WAIT4 => proc::sys_wait4(args[0] as i32, args[1], args[2] as i32, args[3]),
        nr::SET_TPIDR_EL0 => proc::sys_set_tpidr_el0(args[0]),
        nr::FB_INIT => fb::sys_fb_init(args[0] as u32, args[1] as u32),
        nr::FB_DRAW => fb::sys_fb_draw(args[0], args[1] as usize),
        nr::FB_INFO => fb::sys_fb_info(args[0]),
        nr::GETPID => proc::sys_getpid(),
        nr::GETPPID => proc::sys_getppid(),
        nr::GETUID => 0,
        nr::GETEUID => proc::sys_geteuid(),
        nr::GETGID => 0,
        nr::GETEGID => 0,
        nr::GETTID => akuma_exec::threading::current_thread_id() as u64,
        nr::KILL_LINUX => proc::sys_kill(args[0] as u32, args[1] as u32),
        nr::SETPGID => proc::sys_setpgid(args[0] as u32, args[1] as u32),
        nr::GETPGID => proc::sys_getpgid(args[0] as u32),
        nr::SETSID => proc::sys_setsid(),
        nr::UNAME => proc::sys_uname(args[0]),
        nr::FLOCK => 0,
        nr::UMASK => 0o022,
        nr::UTIMENSAT => 0,
        nr::FDATASYNC => 0,
        nr::FSYNC => 0,
        nr::FCHMOD => fs::sys_fchmod(args[0] as u32, args[1] as u32),
        nr::FCHMODAT => fs::sys_fchmodat(args[0] as i32, args[1], args[2] as u32),
        nr::FCHOWNAT => 0,
        nr::MADVISE => mem::sys_madvise(args[0] as usize, args[1] as usize, args[2] as i32),
        nr::MPROTECT => mem::sys_mprotect(args[0] as usize, args[1] as usize, args[2] as u32),
        nr::FUTEX => sync::sys_futex(args[0] as usize, args[1] as i32, args[2] as u32, args[3], args[4] as usize, args[5] as u32),
        nr::SET_ROBUST_LIST => proc::sys_set_robust_list(args[0], args[1] as usize),
        nr::SIGALTSTACK => signal::sys_sigaltstack(args[0], args[1]),
        nr::GETRLIMIT => proc::sys_prlimit64(0, args[0] as u32, 0, args[1]),
        nr::PRLIMIT64 => proc::sys_prlimit64(args[0] as u32, args[1] as u32, args[2], args[3]),
        nr::EVENTFD2 => eventfd::sys_eventfd2(args[0] as u32, args[1] as u32),
        nr::PREAD64 => fs::sys_pread64(args[0] as u32, args[1], args[2] as usize, args[3] as i64),
        nr::PWRITE64 => fs::sys_pwrite64(args[0] as u32, args[1], args[2] as usize, args[3] as i64),
        nr::SETITIMER => {
            crate::tprint!(128, "[stub] setitimer(which={}, new_value={:#x}, old_value={:#x})\n",
                args[0], args[1], args[2]);
            0
        }
        nr::MEMBARRIER => mem::membarrier_cmd(args[0] as u32),
        nr::PRCTL => proc::sys_prctl(args[0] as i32, args[1], args[2], args[3], args[4]),
        nr::TIMES => time::sys_times(args[0] as usize),
        nr::GETRUSAGE => time::sys_getrusage(args[0] as i32, args[1] as usize),
        nr::MSYNC => 0,
        nr::PROCESS_VM_READV => ENOSYS,
        nr::SCHED_SETAFFINITY => 0,
        118 => { 0 }
        119 => {
            let param_ptr = args[1] as usize;
            if param_ptr != 0 && validate_user_ptr(param_ptr as u64, 4) {
                unsafe { core::ptr::write(param_ptr as *mut i32, 0); }
            }
            0
        }
        124 => {
            akuma_exec::threading::yield_now();
            0
        }
        nr::SCHED_GETAFFINITY => {
            let mask_ptr = args[2] as usize;
            let cpusetsize = args[1] as usize;
            if cpusetsize >= 8 && validate_user_ptr(mask_ptr as u64, cpusetsize) {
                unsafe {
                    core::ptr::write_bytes(mask_ptr as *mut u8, 0, cpusetsize);
                    core::ptr::write(mask_ptr as *mut u64, 1);
                }
            }
            0
        }
        nr::TKILL => signal::sys_tkill(args[0] as u32, args[1] as u32),
        nr::PIDFD_OPEN => {
            let target_pid = args[0] as u32;
            if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
                crate::safe_print!(64, "[syscall] pidfd_open(pid={})\n", target_pid);
            }
            ENOSYS
        }
        nr::CLOSE_RANGE => {
            fs::sys_close_range(args[0] as u32, args[1] as u32, args[2] as u32)
        }
        nr::CAPGET => {
            let data_ptr = args[1] as usize;
            if data_ptr != 0 && validate_user_ptr(args[1], 24) {
                unsafe { core::ptr::write_bytes(data_ptr as *mut u8, 0, 24); }
            }
            0
        }
        nr::SYSINFO => proc::sys_sysinfo(args[0] as usize),
        nr::CLOCK_GETRES => time::sys_clock_getres(args[0] as u32, args[1] as usize),
        nr::EPOLL_CREATE1 => poll::sys_epoll_create1(args[0] as u32),
        nr::EPOLL_CTL => poll::sys_epoll_ctl(args[0] as u32, args[1] as i32, args[2] as u32, args[3] as usize),
        nr::EPOLL_PWAIT => poll::sys_epoll_pwait(args[0] as u32, args[1] as usize, args[2] as i32, args[3] as i32),
        nr::TIMERFD_CREATE => timerfd::sys_timerfd_create(args[0] as i32, args[1] as i32),
        nr::TIMERFD_SETTIME => timerfd::sys_timerfd_settime(args[0] as u32, args[1] as i32, args[2] as usize, args[3] as usize),
        nr::TIMERFD_GETTIME => timerfd::sys_timerfd_gettime(args[0], args[1]),
        nr::IO_URING_SETUP | nr::IO_URING_ENTER | nr::IO_URING_REGISTER => ENOSYS,
        // Linux AIO syscalls (io_setup=0, io_destroy=1, io_submit=2, io_cancel=3, io_getevents=4)
        0 | 1 | 2 | 3 | 4 => {
            // Bun probes for AIO support - return ENOSYS to make it fall back
            ENOSYS
        }
        // Extended attributes syscalls (5-16) - return ENOTSUP (not supported on this fs)
        5 | 6 | 7 | 8 | 9 | 10 | 11 | 12 | 13 | 14 | 15 | 16 => {
            // setxattr, lsetxattr, fsetxattr, getxattr, lgetxattr, fgetxattr
            // listxattr, llistxattr, flistxattr, removexattr, lremovexattr, fremovexattr
            const ENOTSUP: u64 = (!95i64) as u64; // Operation not supported
            ENOTSUP
        }
        nr::INOTIFY_INIT1 | nr::INOTIFY_ADD_WATCH | nr::INOTIFY_RM_WATCH => ENOSYS,
        nr::MOUNT => container::sys_mount(args[0], args[1], args[2], args[3] as u64, args[4]),
        nr::UMOUNT2 => container::sys_umount2(args[0], args[1] as i32),
        nr::MOUNT_IN_NS => container::sys_mount_in_ns(args[0], args[1], args[2] as usize, args[3], args[4] as usize),
        _ => {
            crate::safe_print!(128, "[syscall] Unknown syscall: {} (args: [0x{:x}, 0x{:x}, 0x{:x}, 0x{:x}, 0x{:x}, 0x{:x}])\n",
                syscall_num, args[0], args[1], args[2], args[3], args[4], args[5]);
            ENOSYS
        }
    };

    if track_time {
        let elapsed = crate::timer::uptime_us().saturating_sub(t0);
        let owner_pid = akuma_exec::process::read_current_pid().unwrap_or(0);
        if let Some(proc) = akuma_exec::process::lookup_process(owner_pid) {
            proc.syscall_stats.add_time_us(syscall_num, elapsed);
        }
    }

    result
}
