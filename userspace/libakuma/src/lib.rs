//! Akuma User Space Library
//!
//! Provides syscall wrappers and runtime support for user programs.

#![no_std]
#![feature(alloc_error_handler)]

extern crate alloc;

pub mod net;

use core::arch::asm;

/// Syscall numbers
pub mod syscall {
    pub const EXIT: u64 = 0;
    pub const READ: u64 = 1;
    pub const WRITE: u64 = 2;
    pub const BRK: u64 = 3;
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
    pub const UPTIME: u64 = 216;
    pub const MMAP: u64 = 222;
    pub const GETDENTS64: u64 = 61;
    pub const MKDIRAT: u64 = 34;
    // Custom syscalls
    pub const RESOLVE_HOST: u64 = 300;
    pub const SPAWN: u64 = 301;
    pub const KILL: u64 = 302;
    pub const WAITPID: u64 = 303;
    pub const GETRANDOM: u64 = 304;
    pub const TIME: u64 = 305;
    pub const CHDIR: u64 = 306;
    // New Terminal Control Syscalls
    pub const SET_TERMINAL_ATTRIBUTES: u64 = 307;
    pub const GET_TERMINAL_ATTRIBUTES: u64 = 308;
    pub const SET_CURSOR_POSITION: u64 = 309;
    pub const HIDE_CURSOR: u64 = 310;
    pub const SHOW_CURSOR: u64 = 311;
    pub const CLEAR_SCREEN: u64 = 312;
    pub const POLL_INPUT_EVENT: u64 = 313;
    // Framebuffer Syscalls
    pub const FB_INIT: u64 = 314;
    pub const FB_DRAW: u64 = 315;
    pub const FB_INFO: u64 = 316;
}

/// File descriptors
pub mod fd {
    pub const STDIN: u64 = 0;
    pub const STDOUT: u64 = 1;
    pub const STDERR: u64 = 2;
}

/// Fixed address for process info page (read-only, set by kernel)
///
/// The kernel maps this page read-only and writes process information
/// before the process starts. Userspace can read but not modify.
pub const PROCESS_INFO_ADDR: usize = 0x1000;

/// Maximum size of argument data in ProcessInfo
pub const ARGV_DATA_SIZE: usize = 744;

/// Maximum size of cwd data in ProcessInfo
pub const CWD_DATA_SIZE: usize = 256;

// ============================================================================
// Memory Layout Constants
// ============================================================================

/// User process stack size (must match kernel's config::USER_STACK_SIZE)
///
/// The kernel allocates this much stack space for each userspace process.
/// A guard page is placed below the stack to detect overflow.
///
/// WARNING: This value must be kept in sync with src/config.rs USER_STACK_SIZE.
pub const USER_STACK_SIZE: usize = 128 * 1024;

/// Top of userspace address space (stack grows down from here)
pub const STACK_TOP: usize = 0x4000_0000;

/// Page size used by the kernel
pub const PAGE_SIZE: usize = 4096;

/// Process info structure shared between kernel and userspace
///
/// This is mapped read-only at PROCESS_INFO_ADDR.
/// The kernel writes it, userspace reads it.
///
/// WARNING: Must match kernel's ProcessInfo struct exactly!
/// Layout:
///   - pid: 4 bytes
///   - ppid: 4 bytes
///   - argc: 4 bytes
///   - argv_len: 4 bytes (total bytes used in argv_data)
///   - cwd_len: 4 bytes
///   - _reserved: 4 bytes (alignment padding)
///   - cwd_data: 256 bytes (current working directory)
///   - argv_data: 744 bytes (null-separated argument strings)
/// Total: 24 + 256 + 744 = 1024 bytes
#[repr(C)]
pub struct ProcessInfo {
    /// Process ID
    pub pid: u32,
    /// Parent process ID  
    pub ppid: u32,
    /// Number of command line arguments
    pub argc: u32,
    /// Total bytes used in argv_data
    pub argv_len: u32,
    /// Length of cwd string (not including null terminator)
    pub cwd_len: u32,
    /// Reserved for alignment
    pub _reserved: u32,
    /// Current working directory (null-terminated string)
    pub cwd_data: [u8; CWD_DATA_SIZE],
    /// Null-separated argument strings
    pub argv_data: [u8; ARGV_DATA_SIZE],
}

/// Get the current process ID
///
/// Reads from the kernel-provided process info page.
#[inline]
pub fn getpid() -> u32 {
    unsafe { (*(PROCESS_INFO_ADDR as *const ProcessInfo)).pid }
}

/// Get the parent process ID
///
/// Reads from the kernel-provided process info page.
#[inline]
pub fn getppid() -> u32 {
    unsafe { (*(PROCESS_INFO_ADDR as *const ProcessInfo)).ppid }
}

/// Get the current working directory
///
/// Reads from the kernel-provided process info page.
/// Returns "/" if cwd is not set.
pub fn getcwd() -> &'static str {
    unsafe {
        let info = &*(PROCESS_INFO_ADDR as *const ProcessInfo);
        let len = info.cwd_len as usize;
        if len == 0 {
            "/"
        } else {
            core::str::from_utf8_unchecked(&info.cwd_data[..len])
        }
    }
}

/// Change the current working directory
///
/// Updates the process's cwd in the kernel and ProcessInfo page.
/// Returns 0 on success, negative errno on failure.
pub fn chdir(path: &str) -> i32 {
    let result: i64;
    unsafe {
        asm!(
            "svc #0",
            in("x8") syscall::CHDIR,
            in("x0") path.as_ptr(),
            in("x1") path.len(),
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

    pub fn lock(&self) -> SpinlockGuard<T> {
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
// Command Line Arguments
// ============================================================================

/// Get the number of command line arguments
///
/// Returns the argc value set by the kernel.
#[inline]
pub fn argc() -> u32 {
    unsafe { (*(PROCESS_INFO_ADDR as *const ProcessInfo)).argc }
}

/// Get a command line argument by index
///
/// Returns `Some(&str)` if the index is valid, `None` otherwise.
/// Index 0 is conventionally the program name/path.
pub fn arg(index: u32) -> Option<&'static str> {
    let info = unsafe { &*(PROCESS_INFO_ADDR as *const ProcessInfo) };
    
    if index >= info.argc {
        return None;
    }
    
    // Parse through null-separated strings to find the requested index
    let data = &info.argv_data[..info.argv_len as usize];
    let mut current_index = 0u32;
    let mut start = 0;
    
    for (i, &byte) in data.iter().enumerate() {
        if byte == 0 {
            if current_index == index {
                // Found the argument
                return core::str::from_utf8(&data[start..i]).ok();
            }
            current_index += 1;
            start = i + 1;
        }
    }
    
    None
}

/// Iterator over command line arguments
pub struct Args {
    data: &'static [u8],
    pos: usize,
    remaining: u32,
}

impl Iterator for Args {
    type Item = &'static str;
    
    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 || self.pos >= self.data.len() {
            return None;
        }
        
        // Find the next null terminator
        let start = self.pos;
        while self.pos < self.data.len() && self.data[self.pos] != 0 {
            self.pos += 1;
        }
        
        let arg = core::str::from_utf8(&self.data[start..self.pos]).ok();
        
        // Skip past the null terminator
        if self.pos < self.data.len() {
            self.pos += 1;
        }
        
        self.remaining -= 1;
        arg
    }
}

/// Get an iterator over all command line arguments
///
/// Returns an iterator that yields each argument as a `&str`.
pub fn args() -> Args {
    let info = unsafe { &*(PROCESS_INFO_ADDR as *const ProcessInfo) };
    Args {
        data: unsafe { 
            core::slice::from_raw_parts(
                info.argv_data.as_ptr(),
                info.argv_len as usize
            )
        },
        pos: 0,
        remaining: info.argc,
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
#[inline(always)]
pub fn exit(code: i32) -> ! {
    syscall(syscall::EXIT, code as u64, 0, 0, 0, 0, 0);
    // Should not reach here, but just in case
    loop {
        unsafe { asm!("wfi") };
    }
}

/// Read from a file descriptor
///
/// Returns the number of bytes read, or negative on error
#[inline(always)]
pub fn read(fd: u64, buf: &mut [u8]) -> isize {
    syscall(
        syscall::READ,
        fd,
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
pub fn write(fd: u64, buf: &[u8]) -> isize {
    syscall(
        syscall::WRITE,
        fd,
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
    let nanos = milliseconds * 1_000_000;
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

    /// Parse from "ip:port" string
    pub fn parse(s: &str) -> Option<Self> {
        let mut parts = s.split(':');
        let ip_str = parts.next()?;
        let port_str = parts.next()?;

        // Parse IP
        let mut ip = [0u8; 4];
        let mut octets = ip_str.split('.');
        for i in 0..4 {
            let octet_str = octets.next()?;
            ip[i] = parse_u8(octet_str)?;
        }

        // Parse port
        let port = parse_u16(port_str)?;

        Some(Self { ip, port })
    }
}

fn parse_u8(s: &str) -> Option<u8> {
    let mut result: u8 = 0;
    for c in s.bytes() {
        if c < b'0' || c > b'9' {
            return None;
        }
        result = result.checked_mul(10)?.checked_add(c - b'0')?;
    }
    Some(result)
}

fn parse_u16(s: &str) -> Option<u16> {
    let mut result: u16 = 0;
    for c in s.bytes() {
        if c < b'0' || c > b'9' {
            return None;
        }
        result = result.checked_mul(10)?.checked_add((c - b'0') as u16)?;
    }
    Some(result)
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
            sin_addr: u32::from_be_bytes(addr.ip),
            sin_zero: [0u8; 8],
        }
    }

    pub fn to_addr(&self) -> SocketAddrV4 {
        SocketAddrV4 {
            ip: self.sin_addr.to_be_bytes(),
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
    let mut sockaddr = SockAddrIn {
        sin_family: 0,
        sin_port: 0,
        sin_addr: 0,
        sin_zero: [0u8; 8],
    };
    let mut addrlen: u32 = SockAddrIn::SIZE as u32;
    syscall(
        syscall::ACCEPT,
        fd as u64,
        &mut sockaddr as *mut _ as u64,
        &mut addrlen as *mut _ as u64,
        0, 0, 0,
    ) as i32
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
    syscall(
        syscall::OPENAT,
        -100i32 as u64, // AT_FDCWD
        path.as_ptr() as u64,
        path.len() as u64,
        flags as u64,
        0o644u64, // mode
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
    syscall(
        syscall::MKDIRAT,
        -100i32 as u64, // AT_FDCWD
        path.as_ptr() as u64,
        path.len() as u64,
        0o755u64, // mode
        0, 0,
    ) as i32
}

/// Create a directory and all parent directories
///
/// Returns true on success (directory exists or was created).
pub fn mkdir_p(path: &str) -> bool {
    // First check if it already exists by trying to open it
    let fd = open(path, open_flags::O_RDONLY);
    if fd >= 0 {
        close(fd);
        return true; // Already exists
    }

    // Try to create parent directories
    let mut current = alloc::string::String::new();
    for component in path.split('/') {
        if component.is_empty() {
            current.push('/');
            continue;
        }
        if !current.is_empty() && !current.ends_with('/') {
            current.push('/');
        }
        current.push_str(component);
        
        // Try to create this directory (ignore errors for existing dirs)
        let _ = mkdir(&current);
    }

    // Check if final path exists now
    let fd = open(path, open_flags::O_RDONLY);
    if fd >= 0 {
        close(fd);
        true
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

/// Spawn a child process with stdin data
///
/// Returns SpawnResult on success with child PID and stdout FD.
/// Returns None on error.
/// 
/// If stdin is provided, it will be available to the child process
/// when reading from stdin (fd 0).
pub fn spawn_with_stdin(path: &str, args: Option<&[&str]>, stdin: Option<&[u8]>) -> Option<SpawnResult> {
    // Build null-separated args string
    let mut args_buf = alloc::vec::Vec::new();
    if let Some(args_slice) = args {
        for arg in args_slice {
            args_buf.extend_from_slice(arg.as_bytes());
            args_buf.push(0);
        }
    }

    let args_ptr = if args_buf.is_empty() { 0 } else { args_buf.as_ptr() as u64 };
    let args_len = args_buf.len();
    
    let stdin_ptr = stdin.map(|s| s.as_ptr() as u64).unwrap_or(0);
    let stdin_len = stdin.map(|s| s.len() as u64).unwrap_or(0);

    let result = syscall(
        syscall::SPAWN,
        path.as_ptr() as u64,
        path.len() as u64,
        args_ptr,
        args_len as u64,
        stdin_ptr,
        stdin_len,
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

/// Kill a process by PID
///
/// Returns 0 on success, negative errno on error.
pub fn kill(pid: u32) -> i32 {
    syscall(syscall::KILL, pid as u64, 0, 0, 0, 0, 0) as i32
}

/// Wait for a child process (non-blocking)
///
/// Returns:
/// - Some((pid, exit_code)) if child has exited
/// - None if child is still running or not found
pub fn waitpid(pid: u32) -> Option<(u32, i32)> {
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
        // Child exited, extract exit code from Linux-style status
        let exit_code = ((status >> 8) & 0xFF) as i32;
        Some((result as u32, exit_code))
    }
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
    let ret = syscall(
        syscall::POLL_INPUT_EVENT,
        timeout_ms,
        event_buf.as_mut_ptr() as u64,
        event_buf.len() as u64,
        0, 0, 0,
    ) as i64;

    if ret < 0 {
        ret as isize // Return negative errno
    } else {
        ret as isize // Return bytes read
    }
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
                    .map(|s| alloc::string::String::from(s))
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
// Global Allocator with mmap/brk switch
// ============================================================================

/// Set to true to use mmap-based allocation, false for brk-based
pub const USE_MMAP_ALLOCATOR: bool = true;

mod allocator {
    use super::USE_MMAP_ALLOCATOR;
    use core::alloc::{GlobalAlloc, Layout};
    use core::ptr;
    use core::sync::atomic::{AtomicUsize, Ordering};

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
        brk_head: AtomicUsize,
        brk_end: AtomicUsize,
        _padding: [u8; 128],
    }

    #[cfg(not(feature = "chunked-allocator"))]
    #[repr(C, align(256))]
    pub struct HybridAllocator {
        brk_head: AtomicUsize,
        brk_end: AtomicUsize,
        _padding: [u8; 240],
    }

    unsafe impl Sync for HybridAllocator {}

    impl HybridAllocator {
        #[cfg(feature = "chunked-allocator")]
        pub const fn new() -> Self {
            Self {
                talc: super::Spinlock::new(talc::Talc::new(talc::ErrOnOom)),
                brk_head: AtomicUsize::new(0),
                brk_end: AtomicUsize::new(0),
                _padding: [0u8; 128],
            }
        }

        #[cfg(not(feature = "chunked-allocator"))]
        pub const fn new() -> Self {
            Self {
                brk_head: AtomicUsize::new(0),
                brk_end: AtomicUsize::new(0),
                _padding: [0u8; 240],
            }
        }

        pub fn head_addr(&self) -> usize {
            &self.brk_head as *const _ as usize
        }

        pub fn head_value(&self) -> usize {
            self.brk_head.load(Ordering::SeqCst)
        }

        pub fn end_value(&self) -> usize {
            self.brk_end.load(Ordering::SeqCst)
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

        // =====================================================================
        // brk-based allocation (fallback)
        // =====================================================================
        
        fn brk_init(&self) {
            if self.brk_head.load(Ordering::SeqCst) == 0 {
                let initial_brk = super::brk(0);
                if self
                    .brk_head
                    .compare_exchange(0, initial_brk, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
                {
                    let requested_end = initial_brk + 0x10000;
                    let actual_end = super::brk(requested_end);
                    self.brk_end.store(actual_end, Ordering::SeqCst);
                }
            }
        }

        fn brk_expand(&self, needed: usize) -> bool {
            let current_end = self.brk_end.load(Ordering::SeqCst);
            let grow_by = ((needed + 0xFFF) & !0xFFF).max(4096);
            let new_end = current_end + grow_by;
            let result = super::brk(new_end);
            if result >= new_end {
                self.brk_end.store(result, Ordering::SeqCst);
                true
            } else {
                false
            }
        }

        unsafe fn brk_alloc(&self, layout: Layout) -> *mut u8 {
            self.brk_init();
            let current_head = self.brk_head.load(Ordering::SeqCst);
            let current_end = self.brk_end.load(Ordering::SeqCst);
            let align = layout.align();
            let aligned = (current_head + align - 1) & !(align - 1);
            let new_head = aligned + layout.size();
            if new_head > current_end {
                let needed = new_head - current_end;
                if !self.brk_expand(needed) {
                    return ptr::null_mut();
                }
            }
            self.brk_head.store(new_head, Ordering::SeqCst);
            aligned as *mut u8
        }
    }

    unsafe impl GlobalAlloc for HybridAllocator {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            if USE_MMAP_ALLOCATOR {
                self.mmap_alloc(layout)
            } else {
                self.brk_alloc(layout)
            }
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            if USE_MMAP_ALLOCATOR {
                self.mmap_dealloc(ptr, layout);
            }
        }

        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            if !USE_MMAP_ALLOCATOR {
                let new_layout = match Layout::from_size_align(new_size, layout.align()) {
                    Ok(l) => l,
                    Err(_) => return ptr::null_mut(),
                };
                let new_ptr = self.brk_alloc(new_layout);
                if !new_ptr.is_null() && !ptr.is_null() {
                    let copy_size = layout.size().min(new_size);
                    ptr::copy_nonoverlapping(ptr, new_ptr, copy_size);
                }
                return new_ptr;
            }

            let new_layout = match Layout::from_size_align(new_size, layout.align()) {
                Ok(l) => l,
                Err(_) => return ptr::null_mut(),
            };

            let new_ptr = self.mmap_alloc(new_layout);
            if !new_ptr.is_null() {
                if !ptr.is_null() {
                    let copy_size = layout.size().min(new_size);
                    ptr::copy_nonoverlapping(ptr, new_ptr, copy_size);
                    self.mmap_dealloc(ptr, layout);
                }
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

/// Print allocator debug info (addresses and values)
pub fn print_allocator_info() {
    print("  Allocator head addr: 0x");
    print_hex(allocator::ALLOCATOR.head_addr());
    print("\n  Allocator head value: 0x");
    print_hex(allocator::ALLOCATOR.head_value());
    print("\n  Allocator end value: 0x");
    print_hex(allocator::ALLOCATOR.end_value());
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

// ============================================================================
// Framebuffer Syscalls
// ============================================================================

/// Framebuffer information structure (matches kernel's ramfb::FBInfo)
#[repr(C)]
pub struct FBInfo {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub format: u32,
}

/// Initialize the ramfb framebuffer with the given resolution.
///
/// Returns 0 on success, negative errno on failure.
pub fn fb_init(width: u32, height: u32) -> i64 {
    syscall(
        syscall::FB_INIT,
        width as u64,
        height as u64,
        0, 0, 0, 0,
    ) as i64
}

/// Copy an XRGB8888 pixel buffer to the framebuffer.
///
/// `buf` must contain exactly `width * height * 4` bytes of pixel data.
/// Returns number of bytes copied on success, negative errno on failure.
pub fn fb_draw(buf: &[u8]) -> i64 {
    syscall(
        syscall::FB_DRAW,
        buf.as_ptr() as u64,
        buf.len() as u64,
        0, 0, 0, 0,
    ) as i64
}

/// Query framebuffer information.
///
/// Returns 0 on success and fills `info`, negative errno on failure.
pub fn fb_info(info: &mut FBInfo) -> i64 {
    syscall(
        syscall::FB_INFO,
        info as *mut FBInfo as u64,
        0, 0, 0, 0, 0,
    ) as i64
}

/// Panic handler for user programs
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    eprint("PANIC!\n");
    exit(1);
}
