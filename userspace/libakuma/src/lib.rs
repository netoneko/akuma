//! Akuma User Space Library
//!
//! Provides syscall wrappers and runtime support for user programs.

#![no_std]
#![feature(alloc_error_handler)]
#![deny(warnings)]

extern crate alloc;

pub mod fs;
pub mod net;

use core::arch::asm;
use core::sync::atomic::{AtomicUsize, Ordering};

core::arch::global_asm!(
    r#"
    .section .text._start
    .global _start
    _start:
        // sp points to argc, followed by argv pointers
        mov x0, sp
        bl libakuma_init
        bl main
        // If main returns, exit with 0
        mov x0, 0
        bl exit
    "#
);

/// Initial stack pointer passed from the kernel
static INITIAL_SP: AtomicUsize = AtomicUsize::new(0);

/// Initialize libakuma with the stack pointer from the kernel
///
/// # Safety
///
/// Must be called exactly once, by `_start`, before any other libakuma function.
/// `sp` must be the stack pointer as received from the kernel at entry.
#[no_mangle]
pub unsafe extern "C" fn libakuma_init(sp: usize) {
    INITIAL_SP.store(sp, Ordering::SeqCst);
}

#[allow(dead_code)] // called from _start assembly, not visible to Rust
extern "C" {
    fn main();
}

/// Syscall numbers
pub mod syscall {
    pub const EXIT: u64 = 93;
    pub const READ: u64 = 63;
    pub const WRITE: u64 = 64;
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
    pub const SHUTDOWN: u64 = 210;
    pub const MUNMAP: u64 = 215;
    pub const MMAP: u64 = 222;
    pub const GETDENTS64: u64 = 61;
    pub const MKDIRAT: u64 = 34;
    pub const STATFS: u64 = 43;
    pub const UNLINKAT: u64 = 35;
    pub const SYMLINKAT: u64 = 36;
    pub const RENAMEAT: u64 = 38;
    pub const GETRANDOM: u64 = 278;
    // Custom syscalls
    pub const RESOLVE_HOST: u64 = 300;
    pub const SPAWN: u64 = 301;
    pub const KILL: u64 = 302;
    pub const WAITPID: u64 = 303;
    pub const TIME: u64 = 305;
    pub const CLOSE_CHILD_STDIN: u64 = 326;
    pub const CHDIR: u64 = 49;
    pub const GETCWD: u64 = 17;
    pub const FCNTL: u64 = 25;
    pub const PIPE2: u64 = 59;
    pub const FACCESSAT: u64 = 48;
    pub const NEWFSTATAT: u64 = 79;
    pub const CLOCK_GETTIME: u64 = 113;
    pub const FACCESSAT2: u64 = 439;
    // New Terminal Control Syscalls
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
    pub const SET_TID_ADDRESS: u64 = 96;
    pub const EXIT_GROUP: u64 = 94;
    pub const SET_TPIDR_EL0: u64 = 320;
    // 321-323 were the framebuffer syscalls (FB_INIT/FB_DRAW/FB_INFO), removed
    // 2026-08-31 with the whole ramfb path (docs/archive/FRAMEBUFFER_REMOVED.md).
    // The numbers stay RESERVED kernel-side; do not reuse them here either.
    pub const GETEUID: u64 = 175;
    pub const MOUNT: u64 = 40;
    pub const UMOUNT2: u64 = 39;
    pub const MOUNT_IN_NS: u64 = 325;
    pub const FCHMODAT: u64 = 53;
    pub const CLONE: u64 = 220;
    pub const WAIT4: u64 = 260;
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
    /// Last core the thread ran on (MPIDR aff0). 0xFF = never scheduled.
    pub last_core: u8,
    pub _reserved: [u8; 6],
    pub name: [u8; 16],
}

/// File descriptors
///
/// `i32` to match every other fd-taking wrapper in this crate (`read_fd`,
/// `write_fd`, `close`, `fstat`, `lseek`, `recv`, ...) — `read`/`write` used
/// to be the lone `u64` exception, forcing an `as u64` at every call site
/// that held a real fd. `docs/archive/LIBAKUMA_AUDIT.md` item 10.
pub mod fd {
    pub const STDIN: i32 = 0;
    pub const STDOUT: i32 = 1;
    pub const STDERR: i32 = 2;
}

/// Fixed address for process info page (read-only, set by kernel)
///
/// The kernel maps this page read-only and writes process information
/// before the process starts. Userspace can read but not modify.
pub const PROCESS_INFO_ADDR: usize = 0x1000;

// ============================================================================
// Memory Layout Constants
// ============================================================================

/// User process stack size (must match kernel's config::USER_STACK_SIZE)
///
/// The kernel allocates this much stack space for each userspace process.
/// A guard page is placed below the stack to detect overflow.
///
/// WARNING: This value must be kept in sync with src/config.rs USER_STACK_SIZE.
pub const USER_STACK_SIZE: usize = 2 * 1024 * 1024;

/// Top of userspace address space (stack grows down from here)
pub const STACK_TOP: usize = 0x4000_0000;

/// Page size used by the kernel
pub const PAGE_SIZE: usize = 4096;

/// Process info structure shared between kernel and userspace
///
/// Mapped read-only at [`PROCESS_INFO_ADDR`]. The kernel writes it via
/// `ProcessInfo::new(pid, ppid, box_id)`; userspace reads `pid`/`ppid`/`box_id`.
/// The `_reserved` tail (1008 bytes) stays zeroed — argv and cwd are *not*
/// communicated through this page (argv comes from the entry stack, cwd from
/// the `GETCWD` syscall).
///
/// Must match `crates/akuma-exec/src/process/types.rs::ProcessInfo` exactly
/// (asserted on the kernel side: `size_of == 1024`).
#[repr(C)]
pub struct ProcessInfo {
    /// Process ID
    pub pid: u32,
    /// Parent process ID
    pub ppid: u32,
    /// Box ID
    pub box_id: u64,
    pub _reserved: [u8; 1008],
}

/// Get the current process ID
///
/// Reads from the kernel-provided process info page.
/// With the `linux-abi` feature, uses the Linux getpid syscall (172) instead,
/// because the Akuma process-info page at 0x1000 is unmapped on standard Linux.
#[inline]
pub fn getpid() -> u32 {
    #[cfg(feature = "linux-abi")]
    { syscall(172, 0, 0, 0, 0, 0, 0) as u32 }
    #[cfg(not(feature = "linux-abi"))]
    unsafe { (*(PROCESS_INFO_ADDR as *const ProcessInfo)).pid }
}

/// Get the parent process ID
///
/// Reads from the kernel-provided process info page.
#[inline]
pub fn getppid() -> u32 {
    unsafe { (*(PROCESS_INFO_ADDR as *const ProcessInfo)).ppid }
}

/// Get the effective user ID
///
/// Makes a syscall to the kernel.
#[inline]
pub fn geteuid() -> u32 {
    syscall(syscall::GETEUID, 0, 0, 0, 0, 0, 0) as u32
}

/// Get the current working directory
///
/// The syscall and buffer validation happen under `CWD_LOCK`, so two threads
/// calling `getcwd()` concurrently can no longer race the write to the
/// shared buffer (the previous `static mut` was UB under concurrent access).
/// The returned `&'static str` still outlives the lock, so a later
/// `getcwd()` call from another thread can overwrite the bytes a caller is
/// still holding a reference to — same caveat as before, just no longer UB.
pub fn getcwd() -> &'static str {
    static CWD_LOCK: Spinlock<[u8; 256]> = Spinlock::new([0u8; 256]);
    let mut guard = CWD_LOCK.lock();
    let buf_ptr = guard.as_mut_ptr();
    let result: i64;
    unsafe {
        asm!(
            "svc #0",
            in("x8") syscall::GETCWD,
            in("x0") buf_ptr,
            in("x1") 256usize,
            lateout("x0") result,
            options(nostack)
        );
    }
    if result <= 0 {
        return "/";
    }
    let len = (result as usize - 1).min(guard.len());
    // SAFETY: points into the static CWD_LOCK buffer, which lives for the
    // program's duration; the lock only needs to cover the write above.
    let bytes: &'static [u8] = unsafe { core::slice::from_raw_parts(buf_ptr as *const u8, len) };
    drop(guard);
    core::str::from_utf8(bytes).unwrap_or("/")
}

/// Change the current working directory
///
/// Updates the process's cwd in the kernel and ProcessInfo page.
/// Returns 0 on success, negative errno on failure.
pub fn chdir(path: &str) -> i32 {
    let path_c = alloc::format!("{}\0", path);
    let result: i64;
    unsafe {
        asm!(
            "svc #0",
            in("x8") syscall::CHDIR,
            in("x0") path_c.as_ptr(),
            lateout("x0") result,
            options(nostack)
        );
    }
    result as i32
}

// ============================================================================
// Sync Primitives for Userspace
// ============================================================================

pub struct Spinlock<T> {
    locked: core::sync::atomic::AtomicBool,
    data: core::cell::UnsafeCell<T>,
}

unsafe impl<T: Send> Sync for Spinlock<T> {}

impl<T> Spinlock<T> {
    pub const fn new(data: T) -> Self {
        Self {
            locked: core::sync::atomic::AtomicBool::new(false),
            data: core::cell::UnsafeCell::new(data),
        }
    }

    pub fn lock(&self) -> SpinlockGuard<'_, T> {
        while self.locked.compare_exchange_weak(
            false,
            true,
            core::sync::atomic::Ordering::Acquire,
            core::sync::atomic::Ordering::Relaxed,
        ).is_err() {
            core::hint::spin_loop();
        }
        SpinlockGuard { lock: self }
    }
}

pub struct SpinlockGuard<'a, T> {
    lock: &'a Spinlock<T>,
}

impl<'a, T> core::ops::Deref for SpinlockGuard<'a, T> {
    type Target = T;
    fn deref(&self) -> &Self::Target {
        unsafe { &*self.lock.data.get() }
    }
}

impl<'a, T> core::ops::DerefMut for SpinlockGuard<'a, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut *self.lock.data.get() }
    }
}

impl<'a, T> Drop for SpinlockGuard<'a, T> {
    fn drop(&mut self) {
        self.lock.locked.store(false, core::sync::atomic::Ordering::Release);
    }
}

// ============================================================================
// Command Line Arguments & Environment
// ============================================================================

/// Get the number of command line arguments
#[inline]
pub fn argc() -> u32 {
    let sp = INITIAL_SP.load(Ordering::Acquire);
    if sp == 0 { return 0; }
    unsafe { *(sp as *const u64) as u32 }
}

/// Get a command line argument by index
pub fn arg(index: u32) -> Option<&'static str> {
    let sp = INITIAL_SP.load(Ordering::Acquire);
    if sp == 0 { return None; }
    
    unsafe {
        let argc = *(sp as *const u64);
        if index as u64 >= argc { return None; }
        
        // argv starts at sp + 8
        let argv = (sp + 8) as *const *const u8;
        let arg_ptr = *argv.add(index as usize);
        if arg_ptr.is_null() { return None; }
        
        // Calculate length
        let mut len = 0;
        while *arg_ptr.add(len) != 0 {
            len += 1;
        }
        
        core::str::from_utf8(core::slice::from_raw_parts(arg_ptr, len)).ok()
    }
}

/// Iterator over command line arguments
pub struct Args {
    current: u32,
    count: u32,
}

impl Iterator for Args {
    type Item = &'static str;
    
    fn next(&mut self) -> Option<Self::Item> {
        if self.current >= self.count {
            return None;
        }
        let res = arg(self.current);
        self.current += 1;
        res
    }
}

/// Get an iterator over all command line arguments
pub fn args() -> Args {
    Args {
        current: 0,
        count: argc(),
    }
}

/// Get an environment variable by name
pub fn env(name: &str) -> Option<&'static str> {
    let sp = INITIAL_SP.load(Ordering::Acquire);
    if sp == 0 { return None; }
    
    unsafe {
        let argc = *(sp as *const usize);
        // argv is [sp+8 ... sp+8+argc*8], followed by a NULL pointer (8 bytes)
        // envp starts after that NULL
        let envp_start = sp + 8 + (argc + 1) * 8;
        let mut envp = envp_start as *const *const u8;
        
        while !(*envp).is_null() {
            let entry_ptr = *envp;
            let mut len = 0;
            while *entry_ptr.add(len) != 0 {
                len += 1;
            }
            
            if let Ok(entry) = core::str::from_utf8(core::slice::from_raw_parts(entry_ptr, len)) {
                if let Some((k, v)) = entry.split_once('=') {
                    if k == name {
                        return Some(v);
                    }
                }
            }
            envp = envp.add(1);
        }
    }
    None
}

/// Iterator over environment variables
pub struct EnvVars {
    ptr: *const *const u8,
}

impl Iterator for EnvVars {
    type Item = &'static str;
    fn next(&mut self) -> Option<Self::Item> {
        unsafe {
            if self.ptr.is_null() || (*self.ptr).is_null() { return None; }
            let entry_ptr = *self.ptr;
            let mut len = 0;
            while *entry_ptr.add(len) != 0 {
                len += 1;
            }
            let res = core::str::from_utf8(core::slice::from_raw_parts(entry_ptr, len)).ok();
            self.ptr = self.ptr.add(1);
            res
        }
    }
}

/// Get an iterator over all environment variables (strings formatted as "KEY=VALUE")
pub fn env_all() -> EnvVars {
    let sp = INITIAL_SP.load(Ordering::Acquire);
    if sp == 0 { return EnvVars { ptr: core::ptr::null() }; }
    
    unsafe {
        let argc = *(sp as *const usize);
        let envp_start = sp + 8 + (argc + 1) * 8;
        EnvVars { ptr: envp_start as *const *const u8 }
    }
}

/// Perform a syscall with up to 6 arguments
///
/// Uses the Linux AArch64 syscall ABI:
/// - x8: syscall number
/// - x0-x5: arguments
/// - x0: return value
#[inline(always)]
pub fn syscall(num: u64, a0: u64, a1: u64, a2: u64, a3: u64, a4: u64, a5: u64) -> u64 {
    let ret: u64;
    unsafe {
        asm!(
            "svc #0",
            in("x8") num,
            inout("x0") a0 => ret,
            in("x1") a1,
            in("x2") a2,
            in("x3") a3,
            in("x4") a4,
            in("x5") a5,
            options(nostack)
        );
    }
    ret
}

/// Exit the program with the given status code
#[no_mangle]
pub extern "C" fn exit(code: i32) -> ! {
    syscall(syscall::EXIT, code as u64, 0, 0, 0, 0, 0);
    // Should not reach here, but just in case
    loop {
        unsafe { asm!("wfi") };
    }
}

/// Abort the program
#[no_mangle]
pub extern "C" fn abort() -> ! {
    print("ABORT\n");
    exit(134); // 128 + SIGABRT(6)
}

/// Read from a file descriptor
///
/// Returns the number of bytes read, or negative on error
#[inline(always)]
pub fn read(fd: i32, buf: &mut [u8]) -> isize {
    syscall(
        syscall::READ,
        fd as u64,
        buf.as_mut_ptr() as u64,
        buf.len() as u64,
        0,
        0,
        0,
    ) as isize
}

/// Write to a file descriptor
///
/// Returns the number of bytes written, or negative on error
#[inline(always)]
pub fn write(fd: i32, buf: &[u8]) -> isize {
    syscall(
        syscall::WRITE,
        fd as u64,
        buf.as_ptr() as u64,
        buf.len() as u64,
        0,
        0,
        0,
    ) as isize
}

/// Change the program break (heap end)
///
/// # Arguments
/// * `addr` - New break address, or 0 to query current
///
/// # Returns
/// Current (or new) break address
#[inline(always)]
pub fn brk(addr: usize) -> usize {
    syscall(syscall::BRK, addr as u64, 0, 0, 0, 0, 0) as usize
}

/// mmap flags
pub mod mmap_flags {
    pub const PROT_READ: u32 = 0x1;
    pub const PROT_WRITE: u32 = 0x2;
    pub const MAP_PRIVATE: u32 = 0x02;
    pub const MAP_ANONYMOUS: u32 = 0x20;
}

/// Map memory pages
///
/// Returns the mapped address, or usize::MAX on failure.
#[inline(always)]
pub fn mmap(addr: usize, len: usize, prot: u32, flags: u32) -> usize {
    let result = syscall(
        syscall::MMAP,
        addr as u64,
        len as u64,
        prot as u64,
        flags as u64,
        0,
        0,
    );
    result as usize
}

/// Map a **file** into memory.
///
/// [`mmap`] hardcodes `fd = 0, offset = 0`, so it can only make anonymous
/// mappings. This variant passes both through, which is what a probe needs to
/// hold an inode open: a file-backed mapping takes an `InodePin`, and that pin
/// is what makes an `unlink` of the file *deferred* rather than immediate — the
/// precondition for the ext2 unlink leak
/// (`docs/archive/EXT2_UNLINK_INODE_BLOCK_LEAK.md`).
///
/// The fd may be closed immediately after this returns; the mapping, and
/// therefore the pin, outlives it.
///
/// Returns the mapped address, or `usize::MAX` on failure.
#[inline(always)]
pub fn mmap_file(addr: usize, len: usize, prot: u32, flags: u32, fd: i32, offset: usize) -> usize {
    syscall(
        syscall::MMAP,
        addr as u64,
        len as u64,
        prot as u64,
        flags as u64,
        fd as u64,
        offset as u64,
    ) as usize
}

/// Unmap memory pages
#[inline(always)]
pub fn munmap(addr: usize, len: usize) -> isize {
    syscall(syscall::MUNMAP, addr as u64, len as u64, 0, 0, 0, 0) as isize
}

/// Unmap memory pages (version that properly marks x0 as clobbered)
/// Used by dealloc to ensure compiler saves any important values in x0
///
/// CRITICAL: We use mov+svc to avoid inout on x0, which ensures the compiler
/// knows x0 is clobbered and will save/restore any important values.
#[cfg(not(feature = "chunked-allocator"))]
#[inline(never)] // Prevent inlining to ensure proper call/return semantics
fn munmap_void(addr: usize, len: usize) {
    unsafe {
        let _ret: u64;
        core::arch::asm!(
            "mov x0, {addr}",
            "mov x1, {len}",
            "mov x2, #0",
            "mov x3, #0",
            "mov x4, #0",
            "mov x5, #0",
            "svc #0",
            addr = in(reg) addr as u64,
            len = in(reg) len as u64,
            in("x8") syscall::MUNMAP,
            lateout("x0") _ret,  // x0 is clobbered by syscall return
            out("x1") _,
            out("x2") _,
            out("x3") _,
            out("x4") _,
            out("x5") _,
            options(nostack)
        );
    }
}

/// Sleep for the specified number of seconds
#[inline(never)]
pub fn sleep(seconds: u64) {
    syscall(syscall::NANOSLEEP, seconds, 0, 0, 0, 0, 0);
}

/// Sleep for the specified number of milliseconds
#[inline(never)]
pub fn sleep_ms(milliseconds: u64) {
    let nanos = milliseconds.saturating_mul(1_000_000);
    syscall(syscall::NANOSLEEP, 0, nanos, 0, 0, 0, 0);
}

// returns microseconds, not milliseconds
#[inline(never)]
pub fn uptime() -> u64 {
    syscall(syscall::UPTIME, 0, 0, 0, 0, 0, 0)
}

/// Get current Unix timestamp (microseconds since 1970-01-01 00:00:00 UTC)
///
/// Returns 0 if the RTC is not available.
#[inline(never)]
pub fn time() -> u64 {
    syscall(syscall::TIME, 0, 0, 0, 0, 0, 0)
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Timespec {
    pub tv_sec: i64,
    pub tv_nsec: i64,
}

pub const CLOCK_REALTIME: u32 = 0;
pub const CLOCK_MONOTONIC: u32 = 1;

/// Get the time of the specified clock
pub fn clock_gettime(clock_id: u32, tp: &mut Timespec) -> i32 {
    syscall(
        syscall::CLOCK_GETTIME,
        clock_id as u64,
        tp as *mut Timespec as u64,
        0, 0, 0, 0,
    ) as i32
}

// ============================================================================
// Socket Syscalls
// ============================================================================

/// Socket address families
pub mod socket_const {
    pub const AF_INET: i32 = 2;
    pub const SOCK_STREAM: i32 = 1;
    pub const SOCK_DGRAM: i32 = 2;
    pub const IPPROTO_TCP: i32 = 6;
    pub const SHUT_RD: i32 = 0;
    pub const SHUT_WR: i32 = 1;
    pub const SHUT_RDWR: i32 = 2;
}

/// IPv4 socket address
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SocketAddrV4 {
    pub ip: [u8; 4],
    pub port: u16,
}

impl SocketAddrV4 {
    pub const fn new(ip: [u8; 4], port: u16) -> Self {
        Self { ip, port }
    }

    /// Parse the four dot-separated octets of an IPv4 address. Rejects a 5th
    /// octet rather than silently ignoring it (`splitn(5, '.')` + a check
    /// that the 5th slot is empty), unlike a plain `split('.')` over a
    /// fixed-size array, which would take the first four and drop the rest.
    pub fn parse_ip(s: &str) -> Option<[u8; 4]> {
        let mut parts = s.splitn(5, '.');
        let mut ip = [0u8; 4];
        for byte in &mut ip {
            *byte = parts.next()?.parse().ok()?;
        }
        if parts.next().is_some() {
            return None;
        }
        Some(ip)
    }

    /// Parse from "ip:port" string
    pub fn parse(s: &str) -> Option<Self> {
        let mut parts = s.split(':');
        let ip_str = parts.next()?;
        let port_str = parts.next()?;
        let ip = Self::parse_ip(ip_str)?;
        let port = port_str.parse().ok()?;
        Some(Self { ip, port })
    }
}

/// Linux sockaddr_in structure
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct SockAddrIn {
    pub sin_family: u16,
    pub sin_port: u16,     // Network byte order
    pub sin_addr: u32,     // Network byte order
    pub sin_zero: [u8; 8],
}

impl SockAddrIn {
    pub const SIZE: usize = 16;

    pub fn from_addr(addr: &SocketAddrV4) -> Self {
        Self {
            sin_family: 2, // AF_INET
            sin_port: addr.port.to_be(),
            sin_addr: u32::from_ne_bytes(addr.ip),
            sin_zero: [0u8; 8],
        }
    }

    pub fn to_addr(&self) -> SocketAddrV4 {
        SocketAddrV4 {
            ip: self.sin_addr.to_ne_bytes(),
            port: u16::from_be(self.sin_port),
        }
    }
}

/// Create a socket
pub fn socket(domain: i32, sock_type: i32, protocol: i32) -> i32 {
    syscall(
        syscall::SOCKET,
        domain as u64,
        sock_type as u64,
        protocol as u64,
        0, 0, 0,
    ) as i32
}

/// Bind a socket to an address
pub fn bind(fd: i32, addr: &SocketAddrV4) -> i32 {
    let sockaddr = SockAddrIn::from_addr(addr);
    syscall(
        syscall::BIND,
        fd as u64,
        &sockaddr as *const _ as u64,
        SockAddrIn::SIZE as u64,
        0, 0, 0,
    ) as i32
}

/// Listen for connections
pub fn listen(fd: i32, backlog: i32) -> i32 {
    syscall(
        syscall::LISTEN,
        fd as u64,
        backlog as u64,
        0, 0, 0, 0,
    ) as i32
}

/// Accept a connection
pub fn accept(fd: i32) -> i32 {
    accept_addr(fd).0
}

/// Accept a connection, also returning the peer address the kernel filled
/// into the `sockaddr` out-parameter (dropped by the plain `accept` above).
/// On failure the returned `SocketAddrV4` is `0.0.0.0:0` and must be ignored.
pub fn accept_addr(fd: i32) -> (i32, SocketAddrV4) {
    let mut sockaddr = SockAddrIn {
        sin_family: 0,
        sin_port: 0,
        sin_addr: 0,
        sin_zero: [0u8; 8],
    };
    let mut addrlen: u32 = SockAddrIn::SIZE as u32;
    let ret = syscall(
        syscall::ACCEPT,
        fd as u64,
        &mut sockaddr as *mut _ as u64,
        &mut addrlen as *mut _ as u64,
        0, 0, 0,
    ) as i32;
    if ret < 0 {
        return (ret, SocketAddrV4::new([0, 0, 0, 0], 0));
    }
    (ret, sockaddr.to_addr())
}

/// Connect to a remote address
pub fn connect(fd: i32, addr: &SocketAddrV4) -> i32 {
    let sockaddr = SockAddrIn::from_addr(addr);
    syscall(
        syscall::CONNECT,
        fd as u64,
        &sockaddr as *const _ as u64,
        SockAddrIn::SIZE as u64,
        0, 0, 0,
    ) as i32
}

/// Send data on a socket
pub fn send(fd: i32, buf: &[u8], flags: i32) -> isize {
    syscall(
        syscall::SENDTO,
        fd as u64,
        buf.as_ptr() as u64,
        buf.len() as u64,
        flags as u64,
        0, 0,
    ) as isize
}

/// Receive data from a socket
pub fn recv(fd: i32, buf: &mut [u8], flags: i32) -> isize {
    syscall(
        syscall::RECVFROM,
        fd as u64,
        buf.as_mut_ptr() as u64,
        buf.len() as u64,
        flags as u64,
        0, 0,
    ) as isize
}

/// Shutdown a socket
pub fn shutdown(fd: i32, how: i32) -> i32 {
    syscall(
        syscall::SHUTDOWN,
        fd as u64,
        how as u64,
        0, 0, 0, 0,
    ) as i32
}

/// Close a file descriptor
pub fn close(fd: i32) -> i32 {
    syscall(
        syscall::CLOSE,
        fd as u64,
        0, 0, 0, 0, 0,
    ) as i32
}

// ============================================================================
// DNS Syscall
// ============================================================================

/// Resolve a hostname to an IPv4 address
///
/// Returns Ok([a, b, c, d]) on success, Err(errno) on failure.
pub fn resolve_host(hostname: &str) -> Result<[u8; 4], i32> {
    let mut result = [0u8; 4];
    let ret = syscall(
        syscall::RESOLVE_HOST,
        hostname.as_ptr() as u64,
        hostname.len() as u64,
        result.as_mut_ptr() as u64,
        0, 0, 0,
    ) as i64;

    if ret < 0 {
        Err((-ret) as i32)
    } else {
        Ok(result)
    }
}

/// Fill a buffer with cryptographically secure random bytes
///
/// Uses the kernel's VirtIO RNG device to generate random bytes.
///
/// # Arguments
/// * `buf` - Buffer to fill with random bytes (max 256 bytes per call)
///
/// # Returns
/// * `Ok(n)` - Number of bytes written
/// * `Err(errno)` - Error code on failure
pub fn getrandom(buf: &mut [u8]) -> Result<usize, i32> {
    if buf.is_empty() {
        return Ok(0);
    }

    let ret = syscall(
        syscall::GETRANDOM,
        buf.as_mut_ptr() as u64,
        buf.len() as u64,
        0, 0, 0, 0,
    ) as i64;

    if ret < 0 {
        Err((-ret) as i32)
    } else {
        Ok(ret as usize)
    }
}

// ============================================================================
// File I/O Syscalls
// ============================================================================

/// Open flags
pub mod open_flags {
    pub const O_RDONLY: u32 = 0;
    pub const O_WRONLY: u32 = 1;
    pub const O_RDWR: u32 = 2;
    pub const O_CREAT: u32 = 0o100;
    pub const O_TRUNC: u32 = 0o1000;
    pub const O_APPEND: u32 = 0o2000;
}

/// Seek modes
pub mod seek_mode {
    pub const SEEK_SET: i32 = 0;
    pub const SEEK_CUR: i32 = 1;
    pub const SEEK_END: i32 = 2;
}

/// File stat structure (simplified)
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

/// Open a file
///
/// Returns file descriptor on success, negative errno on failure.
pub fn open(path: &str, flags: u32) -> i32 {
    let path_c = alloc::format!("{}\0", path);
    syscall(
        syscall::OPENAT,
        -100i32 as u64, // AT_FDCWD
        path_c.as_ptr() as u64,
        flags as u64,
        0o644u64, // mode
        0,
        0,
    ) as i32
}

/// Get file status
pub fn fstat(fd: i32) -> Result<Stat, i32> {
    let mut stat = Stat::default();
    let ret = syscall(
        syscall::FSTAT,
        fd as u64,
        &mut stat as *mut _ as u64,
        0, 0, 0, 0,
    ) as i64;

    if ret < 0 {
        Err((-ret) as i32)
    } else {
        Ok(stat)
    }
}

/// Get file status relative to directory
pub fn fstatat(dirfd: i32, path: &str, flags: u32) -> Result<Stat, i32> {
    let path_c = alloc::format!("{}\0", path);
    let mut stat = Stat::default();
    let ret = syscall(
        syscall::NEWFSTATAT,
        dirfd as u64,
        path_c.as_ptr() as u64,
        &mut stat as *mut _ as u64,
        flags as u64,
        0, 0,
    ) as i64;

    if ret < 0 {
        Err((-ret) as i32)
    } else {
        Ok(stat)
    }
}

/// Create a pipe
pub fn pipe(fds: &mut [i32; 2]) -> i32 {
    syscall(
        syscall::PIPE2,
        fds.as_mut_ptr() as u64,
        0,
        0, 0, 0, 0,
    ) as i32
}

/// Set or clear the `O_NONBLOCK` flag on a file descriptor via `fcntl(F_SETFL)`.
///
/// The kernel's `F_SETFL` only inspects the `O_NONBLOCK` bit (see
/// `src/syscall/fs.rs::sys_fcntl`), so we pass it directly. Returns 0 on
/// success, negative errno on failure.
pub fn set_nonblocking(fd: i32, nonblocking: bool) -> i32 {
    const F_SETFL: u64 = 4;
    const O_NONBLOCK: u64 = 0x800;
    let arg = if nonblocking { O_NONBLOCK } else { 0 };
    syscall(syscall::FCNTL, fd as u64, F_SETFL, arg, 0, 0, 0) as i32
}

/// Set a PTY's terminal window size via `ioctl(TIOCSWINSZ)`.
///
/// The kernel routes this to the child's shared `TerminalState` when `fd` is a
/// `ChildStdout(pid)` (the handle a spawner like sshd holds for a spawned login
/// shell); for fd 0-2 it updates the caller's own state. `width`/`height` are in
/// columns/rows. Returns 0 on success, negative errno otherwise.
pub fn set_terminal_size(fd: i32, width: u16, height: u16) -> i32 {
    const TIOCSWINSZ: u64 = 0x5414;
    // struct winsize { u16 ws_row, ws_col, ws_xpixel, ws_ypixel }
    let winsz: [u16; 4] = [height, width, 0, 0];
    syscall(
        syscall::IOCTL,
        fd as u64,
        TIOCSWINSZ,
        winsz.as_ptr() as u64,
        0, 0, 0,
    ) as i32
}

/// Deliver EOF to a spawned child's stdin (`CLOSE_CHILD_STDIN`). A shell reading
/// a piped script (busybox `sh`) blocks for more input until it sees EOF; the
/// SSH-into-box bridge calls this on the client's CHANNEL_EOF so the shell
/// finishes reading and runs to completion. Returns 0 on success, negative
/// errno otherwise. Only the spawner of `pid` may close its stdin.
pub fn close_child_stdin(pid: u32) -> i32 {
    syscall(syscall::CLOSE_CHILD_STDIN, pid as u64, 0, 0, 0, 0, 0) as i32
}

/// Check file access permissions
pub fn access(path: &str, mode: u32) -> i32 {
    let path_c = alloc::format!("{}\0", path);
    syscall(
        syscall::FACCESSAT,
        -100i32 as u64, // AT_FDCWD
        path_c.as_ptr() as u64,
        mode as u64,
        0, 0, 0,
    ) as i32
}

/// Check file access permissions relative to directory
pub fn accessat(dirfd: i32, path: &str, mode: u32, flags: u32) -> i32 {
    let path_c = alloc::format!("{}\0", path);
    syscall(
        syscall::FACCESSAT2,
        dirfd as u64,
        path_c.as_ptr() as u64,
        mode as u64,
        flags as u64,
        0, 0,
    ) as i32
}

/// Seek in a file
///
/// Returns new position on success, negative errno on failure.
pub fn lseek(fd: i32, offset: i64, whence: i32) -> i64 {
    syscall(
        syscall::LSEEK,
        fd as u64,
        offset as u64,
        whence as u64,
        0, 0, 0,
    ) as i64
}

/// Read from a file descriptor (generic version)
pub fn read_fd(fd: i32, buf: &mut [u8]) -> isize {
    syscall(
        syscall::READ,
        fd as u64,
        buf.as_mut_ptr() as u64,
        buf.len() as u64,
        0, 0, 0,
    ) as isize
}

/// Write to a file descriptor (generic version)
pub fn write_fd(fd: i32, buf: &[u8]) -> isize {
    syscall(
        syscall::WRITE,
        fd as u64,
        buf.as_ptr() as u64,
        buf.len() as u64,
        0, 0, 0,
    ) as isize
}

/// Create a directory
///
/// Returns 0 on success, negative errno on failure.
pub fn mkdir(path: &str) -> i32 {
    let path_c = alloc::format!("{}\0", path);
    syscall(
        syscall::MKDIRAT,
        -100i32 as u64, // AT_FDCWD
        path_c.as_ptr() as u64,
        0o755u64, // mode
        0,
        0, 0,
    ) as i32
}

/// Delete a file
/// Filesystem free space, as `(block_size, total_blocks, free_blocks)`.
///
/// Exists for `ext2probe`'s space-reclamation phase: the ext2 unlink leak
/// (`docs/archive/EXT2_UNLINK_INODE_BLOCK_LEAK.md`) is only observable as "bytes
/// deleted but never returned", so a probe for it has to read the filesystem's
/// own free count rather than trust `du`.
///
/// Reads only the first three `i64`s it needs out of the 120-byte
/// `struct statfs` (`akuma_syscalls_linux::stat::Statfs`): `f_bsize` at +8,
/// `f_blocks` at +16, `f_bfree` at +24. Returns `None` on error.
#[must_use]
pub fn statfs_free(path: &str) -> Option<(u64, u64, u64)> {
    let path_c = alloc::format!("{}\0", path);
    // 120-byte buffer, 8-byte aligned so the i64 reads are aligned.
    let mut buf = [0u64; 15];
    let r = syscall(
        syscall::STATFS,
        path_c.as_ptr() as u64,
        buf.as_mut_ptr() as u64,
        0, 0, 0, 0,
    ) as i64;
    if r < 0 {
        return None;
    }
    // buf[1] = f_bsize, buf[2] = f_blocks, buf[3] = f_bfree
    Some((buf[1], buf[2], buf[3]))
}

pub fn unlink(path: &str) -> i32 {
    let path_c = alloc::format!("{}\0", path);
    syscall(
        syscall::UNLINKAT,
        -100i32 as u64, // AT_FDCWD
        path_c.as_ptr() as u64,
        0, // flags
        0,
        0, 0,
    ) as i32
}

/// Mount a filesystem.
///
/// Supported fstypes: "proc", "tmpfs".
pub fn mount(source: &str, target: &str, fstype: &str) -> i32 {
    let source_c = alloc::format!("{}\0", source);
    let target_c = alloc::format!("{}\0", target);
    let fstype_c = alloc::format!("{}\0", fstype);
    syscall(
        syscall::MOUNT,
        source_c.as_ptr() as u64,
        target_c.as_ptr() as u64,
        fstype_c.as_ptr() as u64,
        0, // flags
        0, 0,
    ) as i32
}

/// Change a file's permission bits.
pub fn chmod(path: &str, mode: u32) -> i32 {
    let path_c = alloc::format!("{}\0", path);
    syscall(
        syscall::FCHMODAT,
        -100i32 as u64, // AT_FDCWD
        path_c.as_ptr() as u64,
        u64::from(mode),
        0,
        0, 0,
    ) as i32
}

/// Remove an empty directory.
pub fn rmdir(path: &str) -> i32 {
    const AT_REMOVEDIR: u64 = 0x200;
    let path_c = alloc::format!("{}\0", path);
    syscall(
        syscall::UNLINKAT,
        -100i32 as u64, // AT_FDCWD
        path_c.as_ptr() as u64,
        AT_REMOVEDIR,
        0,
        0, 0,
    ) as i32
}

/// Mount a filesystem into another box's mount namespace.
///
/// Callable only from box 0 — the kernel returns `EPERM` otherwise. `fstype`
/// takes the same values as [`mount`] plus `"overlay"`, which is the only one
/// that reads `data`.
pub fn mount_in_ns(box_id: u64, target: &str, fstype: &str, data: Option<&str>) -> i32 {
    let target_c = alloc::format!("{}\0", target);
    let fstype_c = alloc::format!("{}\0", fstype);
    let data_c = data.map(|d| alloc::format!("{}\0", d));
    syscall(
        syscall::MOUNT_IN_NS,
        box_id,
        target_c.as_ptr() as u64,
        target.len() as u64,
        fstype_c.as_ptr() as u64,
        fstype.len() as u64,
        data_c.as_ref().map_or(0, |d| d.as_ptr() as u64),
    ) as i32
}

/// Mount an OCI-style overlay as a box's root: read-only image layers
/// (topmost-first) with a writable container directory on top.
pub fn mount_overlay_root(box_id: u64, lowerdirs: &[alloc::string::String], upperdir: &str) -> i32 {
    let mut data = alloc::string::String::from("lowerdir=");
    for (i, dir) in lowerdirs.iter().enumerate() {
        if i > 0 {
            data.push(':');
        }
        data.push_str(dir);
    }
    data.push_str(",upperdir=");
    data.push_str(upperdir);
    mount_in_ns(box_id, "/", "overlay", Some(&data))
}

/// Unmount a filesystem.
pub fn umount(target: &str) -> i32 {
    let target_c = alloc::format!("{}\0", target);
    syscall(
        syscall::UMOUNT2,
        target_c.as_ptr() as u64,
        0, // flags
        0, 0, 0, 0,
    ) as i32
}

/// Create a symbolic link
pub fn symlink(target: &str, link_path: &str) -> i32 {
    let target_c = alloc::format!("{}\0", target);
    let link_c = alloc::format!("{}\0", link_path);
    syscall(
        syscall::SYMLINKAT,
        target_c.as_ptr() as u64,
        -100i32 as u64, // AT_FDCWD
        link_c.as_ptr() as u64,
        0, 0, 0,
    ) as i32
}

/// Rename/move a file or directory
pub fn rename(old_path: &str, new_path: &str) -> i32 {
    let old_path_c = alloc::format!("{}\0", old_path);
    let new_path_c = alloc::format!("{}\0", new_path);
    syscall(
        syscall::RENAMEAT,
        -100i32 as u64, // AT_FDCWD
        old_path_c.as_ptr() as u64,
        -100i32 as u64, // AT_FDCWD
        new_path_c.as_ptr() as u64,
        0,
        0,
    ) as i32
}

/// Create a directory and all parent directories
///
/// Returns true on success (directory exists or was created).
pub fn mkdir_p(path: &str) -> bool {
    // If path is empty, we're done
    if path.is_empty() {
        return true;
    }

    // Try to create parent directories
    let mut current = alloc::string::String::new();
    let components: alloc::vec::Vec<&str> = path.split('/').collect();
    
    for (i, component) in components.iter().enumerate() {
        if component.is_empty() {
            if i == 0 {
                current.push('/');
            }
            continue;
        }
        
        if !current.is_empty() && !current.ends_with('/') {
            current.push('/');
        }
        current.push_str(component);
        
        // Try to create this directory
        let _ = mkdir(&current);
    }

    // Check if the final path exists and is a directory
    // We use fstat to check if it's a directory
    let fd = open(path, open_flags::O_RDONLY);
    if fd >= 0 {
        let mut success = false;
        if let Ok(stat) = fstat(fd) {
            // S_IFDIR is 0x4000
            if (stat.st_mode & 0xF000) == 0x4000 {
                success = true;
            }
        }
        close(fd);
        success
    } else {
        false
    }
}

/// Print a string to stdout
#[inline(always)]
pub fn print(s: &str) {
    write(fd::STDOUT, s.as_bytes());
}

/// Print a string to stdout with a newline
#[inline(always)]
pub fn println(s: &str) {
    print(s);
    print("\n");
}

/// Print a string to stderr
#[inline(always)]
pub fn eprint(s: &str) {
    write(fd::STDERR, s.as_bytes());
}

/// Print a string to stderr with a newline
#[inline(always)]
pub fn eprintln(s: &str) {
    eprint(s);
    eprint("\n");
}

// ============================================================================
// Output Abstraction for Libraries
// ============================================================================

/// Trait for output operations in Akuma userspace
///
/// This trait provides a standard interface for output that libraries can use.
/// It mirrors common output patterns (print, println, eprint, eprintln) and
/// can be implemented by different output backends.
///
/// # Example
///
/// ```ignore
/// use libakuma::{Output, Stdout};
///
/// fn greet(out: &impl Output) {
///     out.println("Hello, Akuma!");
/// }
///
/// // Use the default stdout implementation
/// greet(&Stdout);
/// ```
pub trait Output {
    /// Print a string without newline
    fn print(&self, s: &str);
    
    /// Print a string with newline
    fn println(&self, s: &str) {
        self.print(s);
        self.print("\n");
    }
    
    /// Print to stderr without newline
    fn eprint(&self, s: &str);
    
    /// Print to stderr with newline
    fn eprintln(&self, s: &str) {
        self.eprint(s);
        self.eprint("\n");
    }
}

/// Standard output implementation
///
/// Routes output to stdout/stderr via libakuma's syscall wrappers.
/// This is the default output implementation for Akuma userspace.
///
/// # Example
///
/// ```ignore
/// use libakuma::{Output, Stdout};
///
/// let out = Stdout;
/// out.println("Hello, world!");
/// out.eprintln("Error message");
/// ```
#[derive(Debug, Clone, Copy, Default)]
pub struct Stdout;

impl Stdout {
    /// Create a new Stdout instance
    pub const fn new() -> Self {
        Self
    }
}

impl Output for Stdout {
    #[inline(always)]
    fn print(&self, s: &str) {
        print(s);
    }
    
    #[inline(always)]
    fn eprint(&self, s: &str) {
        eprint(s);
    }
}

/// Backwards compatibility alias for Stdout
#[deprecated(since = "0.2.0", note = "use Stdout instead")]
pub type AkumaOutput = Stdout;

// ============================================================================
// Process Management Syscalls
// ============================================================================

/// Result of spawning a child process
pub struct SpawnResult {
    /// Child process ID
    pub pid: u32,
    /// File descriptor to read child's stdout
    pub stdout_fd: u32,
}

/// Spawn a child process
///
/// Returns SpawnResult on success with child PID and stdout FD.
/// Returns None on error.
pub fn spawn(path: &str, args: Option<&[&str]>) -> Option<SpawnResult> {
    spawn_with_stdin(path, args, None)
}

/// Spawn flag bits passed in the SPAWN syscall's 6th argument (Akuma's own
/// SPAWN ABI). Keep in sync with the kernel's `SPAWN_FLAG_PTY` in
/// `src/syscall/proc.rs`.
pub const SPAWN_FLAG_PTY: u64 = 1;

/// Spawn a child whose stdin/stdout is a terminal (pty), not a raw pipe.
///
/// Marks the child's channel as a tty so `isatty()` reports true and the kernel
/// runs its canonical line discipline (ICRNL CR->NL, echo, line editing) on the
/// child's stdin. Used by sshd when a client requests an interactive login shell
/// (`pty-req` / `ssh -tt`) so typed input is cooked like a real terminal.
pub fn spawn_pty(path: &str, args: Option<&[&str]>) -> Option<SpawnResult> {
    let path_terminated = alloc::format!("{}\0", path);
    let mut argv = alloc::vec::Vec::new();
    argv.push(path_terminated.as_ptr());

    let mut args_terminated = alloc::vec::Vec::new();
    if let Some(slice) = args {
        for a in slice {
            args_terminated.push(alloc::format!("{}\0", a));
        }
    }
    for s in &args_terminated {
        argv.push(s.as_ptr());
    }
    argv.push(core::ptr::null());

    let result = syscall(
        syscall::SPAWN,
        path_terminated.as_ptr() as u64,
        argv.as_ptr() as u64,
        0, // NULL envp
        0, // no stdin seed
        0,
        SPAWN_FLAG_PTY,
    );

    if (result as i64) < 0 {
        return None;
    }

    let pid = (result & 0xFFFF_FFFF) as u32;
    let stdout_fd = ((result >> 32) & 0xFFFF_FFFF) as u32;
    Some(SpawnResult { pid, stdout_fd })
}

/// Spawn a child process with stdin data
///
/// Returns SpawnResult on success with child PID and stdout FD.
/// Returns None on error.
/// 
/// If stdin is provided, it will be available to the child process
/// when reading from stdin (fd 0).
pub fn spawn_with_stdin(path: &str, args: Option<&[&str]>, stdin: Option<&[u8]>) -> Option<SpawnResult> {
    // 1. Build argv array: [path, args..., NULL]
    let mut argv = alloc::vec::Vec::new();
    // Use raw pointers to strings. Strings must stay alive during syscall!
    // In libakuma::spawn, the caller provides &str which usually live long enough.
    // However, we need null-terminated strings for the kernel.
    
    // For simplicity and safety in this wrapper, we'll convert all to String
    let path_terminated = alloc::format!("{}\0", path);
    argv.push(path_terminated.as_ptr());
    
    let mut args_terminated = alloc::vec::Vec::new();
    if let Some(slice) = args {
        for a in slice {
            let s = alloc::format!("{}\0", a);
            args_terminated.push(s);
        }
    }
    
    for s in &args_terminated {
        argv.push(s.as_ptr());
    }
    argv.push(core::ptr::null());

    let stdin_ptr = stdin.map(|s| s.as_ptr() as u64).unwrap_or(0);
    let stdin_len = stdin.map(|s| s.len() as u64).unwrap_or(0);

    let result = syscall(
        syscall::SPAWN,
        path_terminated.as_ptr() as u64,
        argv.as_ptr() as u64,
        0, // NULL envp
        stdin_ptr,
        stdin_len,
        0,
    );

    // Check for error (negative value)
    if (result as i64) < 0 {
        return None;
    }

    // Extract PID (low 32 bits) and stdout_fd (high 32 bits)
    let pid = (result & 0xFFFF_FFFF) as u32;
    let stdout_fd = ((result >> 32) & 0xFFFF_FFFF) as u32;

    Some(SpawnResult { pid, stdout_fd })
}

/// Spawn a child process with stdin data and extra environment variables.
/// env is a list of "KEY=VALUE" strings to inject into the child's environment.
pub fn spawn_with_env(path: &str, args: Option<&[&str]>, stdin: Option<&[u8]>, env: &[&str]) -> Option<SpawnResult> {
    let path_terminated = alloc::format!("{}\0", path);
    let mut argv = alloc::vec::Vec::new();
    argv.push(path_terminated.as_ptr());

    let mut args_terminated = alloc::vec::Vec::new();
    if let Some(slice) = args {
        for a in slice {
            let s = alloc::format!("{}\0", a);
            args_terminated.push(s);
        }
    }
    for s in &args_terminated {
        argv.push(s.as_ptr());
    }
    argv.push(core::ptr::null());

    let mut envp = alloc::vec::Vec::new();
    let mut env_terminated = alloc::vec::Vec::new();
    for e in env {
        env_terminated.push(alloc::format!("{}\0", e));
    }
    for s in &env_terminated {
        envp.push(s.as_ptr());
    }
    envp.push(core::ptr::null());

    let stdin_ptr = stdin.map(|s| s.as_ptr() as u64).unwrap_or(0);
    let stdin_len = stdin.map(|s| s.len() as u64).unwrap_or(0);

    let result = syscall(
        syscall::SPAWN,
        path_terminated.as_ptr() as u64,
        argv.as_ptr() as u64,
        envp.as_ptr() as u64,
        stdin_ptr,
        stdin_len,
        0,
    );

    if (result as i64) < 0 {
        return None;
    }

    let pid = (result & 0xFFFF_FFFF) as u32;
    let stdout_fd = ((result >> 32) & 0xFFFF_FFFF) as u32;
    Some(SpawnResult { pid, stdout_fd })
}

/// Kill a process by PID
///
/// Returns 0 on success, negative errno on error.
///
/// **This sends signal 0**, which the kernel treats as a "does the process
/// exist?" probe and never delivers (`sys_kill` in `src/syscall/proc.rs`). It
/// checks for the process; it does not stop it. Use [`kill_signal`] to actually
/// terminate something. The behaviour is kept as-is because call sites outside
/// herd rely on the probe.
#[deprecated(note = "sends signal 0 (a liveness probe), not a real signal — \
    use kill_signal(pid, sig) to actually terminate a process; this name has \
    already caused at least 3 call sites to believe they were killing a \
    child when they were not (docs/archive/LIBAKUMA_AUDIT.md item 11)")]
pub fn kill(pid: u32) -> i32 {
    syscall(syscall::KILL, pid as u64, 0, 0, 0, 0, 0) as i32
}

/// Send signal `sig` to `pid`. Returns 0 on success, negative errno on error.
///
/// `sig = 0` only probes for the process's existence without delivering
/// anything — the same contract as Linux, and what [`kill`] hardcodes.
pub fn kill_signal(pid: u32, sig: u32) -> i32 {
    syscall(syscall::KILL, pid as u64, sig as u64, 0, 0, 0, 0) as i32
}

/// `SIGINT` — terminal interrupt (Ctrl-C). Catchable.
pub const SIGINT: u32 = 2;

/// `SIGTERM` — the polite stop. Catchable, so a service may exit cleanly in
/// response rather than showing up as a signal death.
pub const SIGTERM: u32 = 15;

/// `SIGKILL` — uncatchable.
pub const SIGKILL: u32 = 9;

/// Reattach I/O to a target process. `force` mirrors `screen -d`: if the
/// target already has a live holder (a previous `reattach` caller that hasn't
/// exited), a non-`force` call fails with `EBUSY` instead of silently
/// stealing its channel; `force` detaches that previous holder (delivers it
/// `SIGTERM`) and proceeds.
pub fn reattach(pid: u32, force: bool) -> i32 {
    syscall(syscall::REATTACH, pid as u64, force as u64, 0, 0, 0, 0) as i32
}

/// What [`fork`] returned, from the perspective of whoever is asking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForkResult {
    /// We are the parent; the child's PID is inside.
    Parent(u32),
    /// We are the child.
    Child,
}

/// `fork(2)` — duplicate this process, copy-on-write.
///
/// Issued as `clone(SIGCHLD)`, which `sys_clone` (`src/syscall/proc.rs`) routes
/// to `fork_process` on the `flags & 0xFF == 0x11` arm. There is no `libakuma`
/// `spawn`-style alternative that does what this does: **the child inherits the
/// parent's whole fd table**, sockets included, refcounted per-fd
/// (`FdTable::clone_deep_for_fork` → `socket_clone_ref`). That is the only way
/// in this OS to hand an already-`accept()`ed connection to another process —
/// `sys_spawn` has no fd argument, and there is no `SCM_RIGHTS`
/// (`docs/MISSING_SOCKET_MACHINERY.md`).
///
/// Both sides return: the parent gets [`ForkResult::Parent`] with the child's
/// PID, the child gets [`ForkResult::Child`]. The child runs on its own
/// copy-on-write address space (`COW_FORK_ENABLED`, `src/config.rs`), so writes
/// after this point are private to whoever made them.
///
/// Returns `Err(errno)` (negative) if the kernel refused — `ENOMEM` when memory
/// is low or the process table is full.
///
/// # Reaping
///
/// A forked child becomes a zombie until reaped. `fork_process` registers an
/// exit channel keyed on the child PID, so both [`waitpid`] (poll one known
/// PID, non-blocking) and [`wait_any`] (reap whatever finished) work on it.
pub fn fork() -> Result<ForkResult, i32> {
    // SIGCHLD (17) in the low byte is the standard fork flag combination; every
    // other bit clear means "new address space, new fd table, new thread group".
    const SIGCHLD: u64 = 17;
    let ret = syscall(syscall::CLONE, SIGCHLD, 0, 0, 0, 0, 0) as i64;
    match ret {
        0 => Ok(ForkResult::Child),
        n if n > 0 => Ok(ForkResult::Parent(n as u32)),
        e => Err(e as i32),
    }
}

/// Reap any one exited child, without blocking.
///
/// `wait4(-1, WNOHANG)`. Returns `None` when no child has exited (including the
/// "no children at all" case — the two are not distinguished, matching
/// [`waitpid`]'s existing contract).
///
/// Prefer this over [`waitpid`] when children are anonymous — a server that
/// forks per connection does not want to track and poll every outstanding PID
/// individually just to keep zombies from accumulating.
pub fn wait_any() -> Option<WaitStatus> {
    const WNOHANG: u64 = 1;
    let mut status: u32 = 0;
    let ret = syscall(
        syscall::WAIT4,
        u64::MAX, // pid = -1: any child
        &mut status as *mut u32 as u64,
        WNOHANG,
        0, // rusage
        0,
        0,
    ) as i64;

    if ret > 0 {
        Some(WaitStatus { pid: ret as u32, raw: status })
    } else {
        // 0 = children exist but none exited; negative = ECHILD or similar.
        None
    }
}

/// How a reaped child ended: the raw Linux-style wait status plus the accessors
/// needed to interpret it.
///
/// Exists because a bare exit code cannot express "killed by a signal". The
/// kernel already distinguishes the two cases (`encode_wait_status` in
/// `src/syscall/proc.rs`: a clean exit goes in the high byte, a signal death in
/// the low 7 bits), but [`waitpid`] reads only the high byte and so reports a
/// signal-killed child as exit code 0 — indistinguishable from success. Callers
/// that need the difference should use [`waitpid_status`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WaitStatus {
    /// PID of the reaped child.
    pub pid: u32,
    /// The status word exactly as the kernel wrote it, for callers that want to
    /// apply their own macros.
    pub raw: u32,
}

impl WaitStatus {
    /// `WIFEXITED`: the child terminated normally (low 7 bits clear).
    pub fn exited(&self) -> bool {
        self.raw & 0x7F == 0
    }

    /// `WEXITSTATUS`: the exit code, meaningful only when [`Self::exited`].
    /// Returns 0 for a signal death, which is exactly the ambiguity
    /// [`Self::signaled`] exists to resolve.
    pub fn exit_code(&self) -> i32 {
        ((self.raw >> 8) & 0xFF) as i32
    }

    /// `WIFSIGNALED`: the child was killed by a signal.
    ///
    /// 0x7F in the low byte is Linux's `WIFSTOPPED` marker, not a termination.
    /// Akuma never encodes a stopped status today, but it is excluded here so
    /// that adding one later cannot make this silently claim a kill.
    pub fn signaled(&self) -> bool {
        let low = self.raw & 0x7F;
        low != 0 && low != 0x7F
    }

    /// `WTERMSIG`: the signal that killed the child, or `None` if it exited
    /// normally.
    pub fn term_signal(&self) -> Option<u8> {
        if self.signaled() {
            Some((self.raw & 0x7F) as u8)
        } else {
            None
        }
    }

    /// Exit code in the shell/`$?` convention: `128 + signal` for a signal
    /// death, otherwise the plain exit code. What a shell would have reported.
    pub fn shell_code(&self) -> i32 {
        match self.term_signal() {
            Some(sig) => 128 + sig as i32,
            None => self.exit_code(),
        }
    }
}

/// Wait for a child process (non-blocking), returning the full wait status.
///
/// Prefer this over [`waitpid`] whenever a signal death must be distinguished
/// from a clean exit — a crashed child, a `kill`ed one, and a child that exited
/// 0 are all `exit_code() == 0` and only [`WaitStatus::signaled`] tells them
/// apart.
///
/// Returns `None` if the child is still running or does not exist (the same
/// two-cases-in-one-value limitation [`waitpid`] has always had).
pub fn waitpid_status(pid: u32) -> Option<WaitStatus> {
    let mut status: u32 = 0;
    let result = syscall(
        syscall::WAITPID,
        pid as u64,
        &mut status as *mut u32 as u64,
        0, 0, 0, 0,
    );

    if result == 0 {
        // Child still running
        None
    } else if (result as i64) < 0 {
        // Error (e.g., no such child)
        None
    } else {
        Some(WaitStatus { pid: result as u32, raw: status })
    }
}

/// Wait for a child process (non-blocking)
///
/// Returns:
/// - Some((pid, exit_code)) if child has exited
/// - None if child is still running or not found
///
/// The exit code is `WEXITSTATUS` only: a child **killed by a signal** reports
/// 0 here and is indistinguishable from a clean success. Use
/// [`waitpid_status`] if that distinction matters.
#[deprecated(note = "a signal-killed child and a clean exit-0 child are both \
    reported as exit_code 0 — use waitpid_status(pid) (or wait_any()) when \
    that distinction matters (docs/archive/LIBAKUMA_AUDIT.md item 11)")]
pub fn waitpid(pid: u32) -> Option<(u32, i32)> {
    waitpid_status(pid).map(|st| (st.pid, st.exit_code()))
}

// ============================================================================
// Terminal Syscall Wrappers
// ============================================================================

/// Sets terminal control attributes.
pub fn set_terminal_attributes(fd: u64, action: u64, mode_flags: u64) -> i32 {
    syscall(
        syscall::SET_TERMINAL_ATTRIBUTES,
        fd,
        action,
        mode_flags,
        0, 0, 0,
    ) as i32
}

/// Retrieves current terminal control attributes.
pub fn get_terminal_attributes(fd: u64, attr_ptr: u64) -> i32 {
    syscall(
        syscall::GET_TERMINAL_ATTRIBUTES,
        fd,
        attr_ptr,
        0, 0, 0, 0,
    ) as i32
}

/// Sets the cursor position (col, row).
pub fn set_cursor_position(col: u64, row: u64) -> i32 {
    syscall(
        syscall::SET_CURSOR_POSITION,
        col,
        row,
        0, 0, 0, 0,
    ) as i32
}

/// Hides the terminal cursor.
pub fn hide_cursor() -> i32 {
    syscall(
        syscall::HIDE_CURSOR,
        0, 0, 0, 0, 0, 0,
    ) as i32
}

/// Shows the terminal cursor.
pub fn show_cursor() -> i32 {
    syscall(
        syscall::SHOW_CURSOR,
        0, 0, 0, 0, 0, 0,
    ) as i32
}

/// Clears the entire terminal screen.
pub fn clear_screen() -> i32 {
    syscall(
        syscall::CLEAR_SCREEN,
        0, 0, 0, 0, 0, 0,
    ) as i32
}

/// Checks for and returns pending input events.
pub fn poll_input_event(timeout_ms: u64, event_buf: &mut [u8]) -> isize {
    let timeout_us = if timeout_ms == u64::MAX {
        u64::MAX
    } else {
        timeout_ms.saturating_mul(1000)
    };

    let ret = syscall(
        syscall::POLL_INPUT_EVENT,
        event_buf.as_mut_ptr() as u64,
        event_buf.len() as u64,
        timeout_us,
        0, 0, 0,
    ) as i64;

    ret as isize
}

/// Get CPU statistics for all threads.
/// 
/// Populates the provided slice with statistics. Returns the number of threads.
pub fn get_cpu_stats(stats: &mut [ThreadCpuStat]) -> usize {
    syscall(
        syscall::GET_CPU_STATS,
        stats.as_mut_ptr() as u64,
        stats.len() as u64,
        0, 0, 0, 0,
    ) as usize
}

/// Directory entry from getdents64
#[repr(C)]
pub struct DirEntry64 {
    pub d_ino: u64,
    pub d_off: i64,
    pub d_reclen: u16,
    pub d_type: u8,
    // d_name follows (variable length, null-terminated)
}

/// File types from d_type
pub mod file_type {
    pub const DT_REG: u8 = 8;  // Regular file
    pub const DT_DIR: u8 = 4;  // Directory
}

/// Read directory entries
///
/// Returns number of bytes read, or negative errno on error.
/// 0 means end of directory.
pub fn getdents64(fd: i32, buf: &mut [u8]) -> isize {
    syscall(
        syscall::GETDENTS64,
        fd as u64,
        buf.as_mut_ptr() as u64,
        buf.len() as u64,
        0, 0, 0,
    ) as isize
}

/// Iterator over directory entries
pub struct ReadDir {
    fd: i32,
    buf: [u8; 1024],
    pos: usize,
    len: usize,
    done: bool,
}

impl ReadDir {
    /// Open a directory for reading
    pub fn open(path: &str) -> Option<Self> {
        let fd = open(path, open_flags::O_RDONLY);
        if fd < 0 {
            return None;
        }
        
        // Check if this is actually a directory using fstat
        // S_IFMT = 0o170000, S_IFDIR = 0o040000
        const S_IFMT: u32 = 0o170000;
        const S_IFDIR: u32 = 0o040000;
        
        if let Ok(stat) = fstat(fd) {
            if (stat.st_mode & S_IFMT) != S_IFDIR {
                // Not a directory - close and return None
                close(fd);
                return None;
            }
        } else {
            // fstat failed - close and return None
            close(fd);
            return None;
        }
        
        Some(Self {
            fd,
            buf: [0u8; 1024],
            pos: 0,
            len: 0,
            done: false,
        })
    }
}

impl Drop for ReadDir {
    fn drop(&mut self) {
        close(self.fd);
    }
}

/// Directory entry info
pub struct DirEntryInfo {
    pub name: alloc::string::String,
    pub is_dir: bool,
}

impl Iterator for ReadDir {
    type Item = DirEntryInfo;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            // If we have buffered data, parse the next entry
            if self.pos < self.len {
                let entry = unsafe {
                    &*(self.buf.as_ptr().add(self.pos) as *const DirEntry64)
                };
                let reclen = entry.d_reclen as usize;
                
                // Extract name (null-terminated string after header)
                let name_ptr = unsafe { self.buf.as_ptr().add(self.pos + 19) }; // header is 19 bytes
                let mut name_len = 0;
                while name_len < reclen - 19 {
                    if unsafe { *name_ptr.add(name_len) } == 0 {
                        break;
                    }
                    name_len += 1;
                }
                let name_bytes = unsafe { core::slice::from_raw_parts(name_ptr, name_len) };
                let name = core::str::from_utf8(name_bytes)
                    .map(alloc::string::String::from)
                    .unwrap_or_default();
                
                let is_dir = entry.d_type == file_type::DT_DIR;
                
                self.pos += reclen;
                return Some(DirEntryInfo { name, is_dir });
            }

            // Need to read more entries
            if self.done {
                return None;
            }

            let n = getdents64(self.fd, &mut self.buf);
            if n <= 0 {
                self.done = true;
                return None;
            }
            self.pos = 0;
            self.len = n as usize;
        }
    }
}

/// List directory contents
pub fn read_dir(path: &str) -> Option<ReadDir> {
    ReadDir::open(path)
}

// ============================================================================
// Global Allocator (mmap-backed)
// ============================================================================
//
// This used to have a second, brk-based arm selected by a `USE_MMAP_ALLOCATOR`
// source constant. That arm's `brk_alloc` did non-atomic
// load-head/load-end/compute/store-head, so two threads racing it could both
// read the same head and return overlapping memory — silent heap corruption,
// latent only because the constant was always `true`. Deleted rather than
// fixed: nothing sets the constant to `false`, so there was no live use case
// to preserve. See `docs/archive/LIBAKUMA_AUDIT.md` item 13.

mod allocator {
    use core::alloc::{GlobalAlloc, Layout};
    use core::ptr;
    use core::sync::atomic::{AtomicUsize, Ordering};

    #[cfg(not(feature = "chunked-allocator"))]
    const PAGE_SIZE: usize = 4096;
    const CHUNK_SIZE: usize = 64 * 1024; // 64 KB chunks
    const MAP_FAILED: usize = usize::MAX;

    /// Track total bytes allocated from kernel
    static ALLOCATED_BYTES: AtomicUsize = AtomicUsize::new(0);
    /// Track total bytes freed
    static FREED_BYTES: AtomicUsize = AtomicUsize::new(0);
    /// Track number of user-level allocations
    static ALLOC_COUNT: AtomicUsize = AtomicUsize::new(0);
    /// Track actual bytes currently used by user
    static USER_USED_BYTES: AtomicUsize = AtomicUsize::new(0);

    #[cfg(feature = "chunked-allocator")]
    #[repr(C, align(256))]
    pub struct HybridAllocator {
        talc: super::Spinlock<talc::Talc<talc::ErrOnOom>>,
        _padding: [u8; 144],
    }

    #[cfg(not(feature = "chunked-allocator"))]
    #[repr(C, align(256))]
    pub struct HybridAllocator {
        _padding: [u8; 256],
    }

    unsafe impl Sync for HybridAllocator {}

    impl HybridAllocator {
        #[cfg(feature = "chunked-allocator")]
        pub const fn new() -> Self {
            Self {
                talc: super::Spinlock::new(talc::Talc::new(talc::ErrOnOom)),
                _padding: [0u8; 144],
            }
        }

        #[cfg(not(feature = "chunked-allocator"))]
        pub const fn new() -> Self {
            Self {
                _padding: [0u8; 256],
            }
        }

        // =====================================================================
        // mmap-based allocation
        // =====================================================================

        #[cfg(feature = "chunked-allocator")]
        unsafe fn mmap_alloc(&self, layout: Layout) -> *mut u8 {
            let mut talc = self.talc.lock();

            match talc.malloc(layout) {
                Ok(ptr) => {
                    ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
                    USER_USED_BYTES.fetch_add(layout.size(), Ordering::Relaxed);
                    ptr.as_ptr()
                }
                Err(_) => {
                    use super::mmap_flags::*;
                    let request_size = if layout.size() + 1024 > CHUNK_SIZE {
                        (layout.size() + 1024 + 4095) & !4095
                    } else {
                        CHUNK_SIZE
                    };

                    let addr = super::mmap(
                        0,
                        request_size,
                        PROT_READ | PROT_WRITE,
                        MAP_PRIVATE | MAP_ANONYMOUS,
                    );

                    if addr == MAP_FAILED || addr == 0 {
                        return ptr::null_mut();
                    }

                    let span = talc::Span::from_base_size(addr as *mut u8, request_size);
                    if talc.claim(span).is_err() {
                        return ptr::null_mut();
                    }

                    ALLOCATED_BYTES.fetch_add(request_size, Ordering::Relaxed);

                    match talc.malloc(layout) {
                        Ok(ptr) => {
                            ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
                            USER_USED_BYTES.fetch_add(layout.size(), Ordering::Relaxed);
                            ptr.as_ptr()
                        }
                        Err(_) => ptr::null_mut(),
                    }
                }
            }
        }

        #[cfg(not(feature = "chunked-allocator"))]
        unsafe fn mmap_alloc(&self, layout: Layout) -> *mut u8 {
            use super::mmap_flags::*;
            let size = layout.size().max(layout.align());
            let alloc_size = (size + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);

            let addr = super::mmap(
                0,
                alloc_size,
                PROT_READ | PROT_WRITE,
                MAP_PRIVATE | MAP_ANONYMOUS,
            );

            if addr == MAP_FAILED || addr == 0 {
                ptr::null_mut()
            } else {
                ALLOCATED_BYTES.fetch_add(alloc_size, Ordering::Relaxed);
                USER_USED_BYTES.fetch_add(alloc_size, Ordering::Relaxed);
                ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
                addr as *mut u8
            }
        }

        #[cfg(feature = "chunked-allocator")]
        unsafe fn mmap_dealloc(&self, ptr: *mut u8, layout: Layout) {
            if ptr.is_null() {
                return;
            }
            let mut talc = self.talc.lock();
            talc.free(ptr::NonNull::new_unchecked(ptr), layout);
            USER_USED_BYTES.fetch_sub(layout.size(), Ordering::Relaxed);
        }

        #[cfg(not(feature = "chunked-allocator"))]
        unsafe fn mmap_dealloc(&self, ptr: *mut u8, layout: Layout) {
            let size = layout.size().max(layout.align());
            let alloc_size = (size + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
            USER_USED_BYTES.fetch_sub(alloc_size, Ordering::Relaxed);
            FREED_BYTES.fetch_add(alloc_size, Ordering::Relaxed);
            super::munmap_void(ptr as usize, alloc_size);
        }

    }

    unsafe impl GlobalAlloc for HybridAllocator {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            self.mmap_alloc(layout)
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            self.mmap_dealloc(ptr, layout);
        }

        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            let new_layout = match Layout::from_size_align(new_size, layout.align()) {
                Ok(l) => l,
                Err(_) => return ptr::null_mut(),
            };

            let new_ptr = self.mmap_alloc(new_layout);
            if !new_ptr.is_null() && !ptr.is_null() {
                let copy_size = layout.size().min(new_size);
                ptr::copy_nonoverlapping(ptr, new_ptr, copy_size);
                self.mmap_dealloc(ptr, layout);
            }
            new_ptr
        }
    }

    #[global_allocator]
    pub static ALLOCATOR: HybridAllocator = HybridAllocator::new();

    pub fn total_allocated_bytes() -> usize {
        ALLOCATED_BYTES.load(Ordering::Relaxed)
    }

    pub fn total_freed_bytes() -> usize {
        FREED_BYTES.load(Ordering::Relaxed)
    }

    pub fn net_memory() -> usize {
        USER_USED_BYTES.load(Ordering::Relaxed)
    }

    pub fn alloc_count() -> usize {
        ALLOC_COUNT.load(Ordering::Relaxed)
    }
}

/// Get current net memory usage in bytes (user-level)
pub fn memory_usage() -> usize {
    allocator::net_memory()
}

/// Get total bytes requested from kernel
pub fn total_allocated() -> usize {
    allocator::total_allocated_bytes()
}

/// Get total freed bytes (actual unmaps)
pub fn total_freed() -> usize {
    allocator::total_freed_bytes()
}

/// Get number of allocations made
pub fn allocation_count() -> usize {
    allocator::alloc_count()
}

/// Custom allocation error handler - prints stats and exits
#[alloc_error_handler]
fn alloc_error(_layout: core::alloc::Layout) -> ! {
    // Print OOM message and memory stats using stack-based formatting
    eprint("OUT OF MEMORY!\n");
    eprint("  Net memory: ");
    print_dec(memory_usage());
    eprint(" bytes (");
    print_dec(memory_usage() / 1024);
    eprint(" KB)\n");
    eprint("  Total allocated: ");
    print_dec(total_allocated());
    eprint(" bytes\n");
    eprint("  Total freed: ");
    print_dec(total_freed());
    eprint(" bytes\n");
    eprint("  Allocation count: ");
    print_dec(allocation_count());
    eprint("\n");
    exit(-1);
}

/// Print allocator debug info (mmap byte/allocation counters, plus the raw
/// process break for reference — the allocator itself is mmap-only and never
/// moves the break).
pub fn print_allocator_info() {
    print("  Total allocated: 0x");
    print_hex(total_allocated());
    print("\n  Total freed: 0x");
    print_hex(total_freed());
    print("\n  Net memory: 0x");
    print_hex(memory_usage());
    print("\n  brk(0) = 0x");
    print_hex(brk(0));
    print("\n");
}

/// Print a usize as hex
pub fn print_hex(val: usize) {
    const HEX_CHARS: &[u8; 16] = b"0123456789abcdef";
    let mut buf = [0u8; 16];
    let mut v = val;
    let mut i = 15;

    if v == 0 {
        print("0");
        return;
    }

    while v > 0 {
        buf[i] = HEX_CHARS[v & 0xF];
        v >>= 4;
        if i == 0 {
            break;
        }
        i -= 1;
    }

    // Safety: we only write valid ASCII hex digits
    if let Ok(s) = core::str::from_utf8(&buf[i..]) {
        print(s);
    }
}

/// Print a usize as decimal
pub fn print_dec(val: usize) {
    const DEC_CHARS: &[u8; 10] = b"0123456789";
    let mut buf = [0u8; 20];
    let mut v = val;
    let mut i = 19;

    if v == 0 {
        print("0");
        return;
    }

    while v > 0 {
        buf[i] = DEC_CHARS[v % 10];
        v /= 10;
        if i == 0 {
            break;
        }
        i -= 1;
    }

    if let Ok(s) = core::str::from_utf8(&buf[i..]) {
        print(s);
    }
}

/// Panic handler for user programs
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    eprint("PANIC!\n");
    exit(1);
}
