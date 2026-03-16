//! Pure data types for the process subsystem.
//!
//! These types have no architecture-specific or runtime dependencies
//! and can be compiled and tested on the host.

#![allow(dead_code)]

use alloc::string::String;
use alloc::vec::Vec;
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};

/// Default environment variables for new processes when none are provided.
pub const DEFAULT_ENV: &[&str] = &[
    "PATH=/usr/bin:/bin",
    "HOME=/",
    "TERM=xterm",
];

/// A future that yields once then completes
pub struct YieldOnce(bool);

impl YieldOnce {
    pub fn new() -> Self {
        YieldOnce(false)
    }
}

impl Future for YieldOnce {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<()> {
        if self.0 {
            Poll::Ready(())
        } else {
            self.0 = true;
            Poll::Pending
        }
    }
}

/// Fixed address for process info page (read-only from userspace)
pub const PROCESS_INFO_ADDR: usize = 0x1000;

/// Maximum size of argument data in ProcessInfo
pub const ARGV_DATA_SIZE: usize = 744;

/// Maximum size of cwd data in ProcessInfo
pub const CWD_DATA_SIZE: usize = 256;

/// Process info structure shared between kernel and userspace
///
/// The kernel writes this, userspace reads it (read-only mapping).
/// Layout must match libakuma exactly.
#[repr(C)]
pub struct ProcessInfo {
    pub pid: u32,
    pub ppid: u32,
    pub box_id: u64,
    pub _reserved: [u8; 1008],
}

impl ProcessInfo {
    pub const fn new(pid: u32, ppid: u32, box_id: u64) -> Self {
        Self { pid, ppid, box_id, _reserved: [0u8; 1008] }
    }
}

const _: () = assert!(core::mem::size_of::<ProcessInfo>() == 1024);

/// Process ID type
pub type Pid = u32;

/// Stdio buffer for procfs visibility
pub struct StdioBuffer {
    pub data: Vec<u8>,
    pub pos: usize,
}

impl StdioBuffer {
    pub fn new() -> Self {
        Self { data: Vec::new(), pos: 0 }
    }

    pub fn len(&self) -> usize {
        self.data.len()
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    pub fn clear(&mut self) {
        self.data.clear();
        self.pos = 0;
    }

    pub fn write_with_limit(&mut self, data: &[u8], max_size: usize) {
        if self.data.len() + data.len() > max_size {
            self.data.clear();
        }
        self.data.extend_from_slice(data);
    }

    pub fn set_with_limit(&mut self, data: &[u8], max_size: usize) {
        self.data.clear();
        self.pos = 0;
        if data.len() <= max_size {
            self.data.extend_from_slice(data);
        } else {
            self.data.extend_from_slice(&data[data.len() - max_size..]);
        }
    }

    pub fn read(&mut self, buf: &mut [u8]) -> usize {
        let remaining = &self.data[self.pos..];
        let to_read = buf.len().min(remaining.len());
        buf[..to_read].copy_from_slice(&remaining[..to_read]);
        self.pos += to_read;
        to_read
    }

    pub fn clone_data(&self) -> Vec<u8> {
        self.data.clone()
    }
}

impl Default for StdioBuffer {
    fn default() -> Self {
        Self::new()
    }
}

/// File descriptor types for the per-process FD table
#[derive(Debug, Clone)]
pub enum FileDescriptor {
    Stdin,
    Stdout,
    Stderr,
    Socket(usize),
    File(KernelFile),
    ChildStdout(Pid),
    PipeRead(u32),
    PipeWrite(u32),
    EventFd(u32),
    DevNull,
    DevUrandom,
    TimerFd(u32),
    EpollFd(u32),
    PidFd(u32),
}

/// Kernel file handle for open files
#[derive(Debug, Clone)]
pub struct KernelFile {
    pub path: String,
    pub position: usize,
    pub flags: u32,
}

impl KernelFile {
    pub fn new(path: String, flags: u32) -> Self {
        Self { path, position: 0, flags }
    }
}

/// File open flags (Linux compatible)
pub mod open_flags {
    pub const O_RDONLY: u32 = 0;
    pub const O_WRONLY: u32 = 1;
    pub const O_RDWR: u32 = 2;
    pub const O_CREAT: u32 = 0o100;
    pub const O_TRUNC: u32 = 0o1000;
    pub const O_APPEND: u32 = 0o2000;
    pub const O_CLOEXEC: u32 = 0o2000000;
}

/// Source of data for a lazy region page.
#[derive(Clone)]
pub enum LazySource {
    Zero,
    File {
        path: String,
        inode: u32,
        file_offset: usize,
        filesz: usize,
        segment_va: usize,
    },
}

/// A lazily-backed virtual memory region.
#[derive(Clone)]
pub struct LazyRegion {
    pub start_va: usize,
    pub size: usize,
    pub flags: u64,
    pub source: LazySource,
}

/// Process state
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessState {
    Ready,
    Running,
    Blocked,
    Zombie(i32),
}

/// User context saved during kernel entry
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct UserContext {
    pub x0: u64, pub x1: u64, pub x2: u64, pub x3: u64,
    pub x4: u64, pub x5: u64, pub x6: u64, pub x7: u64,
    pub x8: u64, pub x9: u64, pub x10: u64, pub x11: u64,
    pub x12: u64, pub x13: u64, pub x14: u64, pub x15: u64,
    pub x16: u64, pub x17: u64, pub x18: u64, pub x19: u64,
    pub x20: u64, pub x21: u64, pub x22: u64, pub x23: u64,
    pub x24: u64, pub x25: u64, pub x26: u64, pub x27: u64,
    pub x28: u64, pub x29: u64, pub x30: u64,
    pub sp: u64,
    pub pc: u64,
    pub spsr: u64,
    pub tpidr: u64,
    pub ttbr0: u64,
}

impl UserContext {
    pub fn new(entry_point: usize, stack_pointer: usize) -> Self {
        Self {
            x0: 0, x1: 0, x2: 0, x3: 0, x4: 0, x5: 0, x6: 0, x7: 0,
            x8: 0, x9: 0, x10: 0, x11: 0, x12: 0, x13: 0, x14: 0, x15: 0,
            x16: 0, x17: 0, x18: 0, x19: 0, x20: 0, x21: 0, x22: 0, x23: 0,
            x24: 0, x25: 0, x26: 0, x27: 0, x28: 0, x29: 0, x30: 0,
            sp: stack_pointer as u64,
            pc: entry_point as u64,
            spsr: 0,
            tpidr: 0,
            ttbr0: 0,
        }
    }

    pub fn default() -> Self {
        Self::new(0, 0)
    }
}

pub const MAX_SIGNALS: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SignalHandler {
    Default,
    Ignore,
    UserFn(usize),
}

#[derive(Debug, Clone, Copy)]
pub struct SignalAction {
    pub handler: SignalHandler,
    pub flags: u64,
    pub mask: u64,
    pub restorer: usize,
}

impl SignalAction {
    pub const fn default() -> Self {
        Self {
            handler: SignalHandler::Default,
            flags: 0,
            mask: 0,
            restorer: 0,
        }
    }
}

/// Memory regions for a process
#[derive(Debug, Clone)]
pub struct ProcessMemory {
    pub code_end: usize,
    pub brk: usize,
    pub stack_bottom: usize,
    pub stack_top: usize,
    pub next_mmap: usize,
    pub mmap_limit: usize,
    pub free_regions: Vec<(usize, usize)>,
}

impl ProcessMemory {
    pub fn new(code_end: usize, stack_bottom: usize, stack_top: usize, mmap_floor: usize) -> Self {
        let base = (code_end + 0x1000_0000) & !0xFFFF;
        let mmap_start = core::cmp::max(base, mmap_floor);
        let mmap_limit = stack_bottom.saturating_sub(0x10_0000);

        Self {
            code_end,
            brk: code_end,
            stack_bottom,
            stack_top,
            next_mmap: mmap_start,
            mmap_limit,
            free_regions: Vec::new(),
        }
    }

    pub fn overlaps_stack(&self, addr: usize, size: usize) -> bool {
        let end = addr.saturating_add(size);
        addr < self.stack_top && end > self.stack_bottom
    }

    const KERNEL_VA_START: usize = 0x4000_0000;
    const KERNEL_VA_END: usize   = 0x8000_0000;

    pub fn alloc_mmap(&mut self, size: usize) -> Option<usize> {
        for i in 0..self.free_regions.len() {
            let (start, f_size) = self.free_regions[i];
            if f_size >= size {
                if start >= Self::KERNEL_VA_START && start < Self::KERNEL_VA_END {
                    continue;
                }
                self.free_regions.remove(i);
                if f_size > size {
                    self.free_regions.push((start + size, f_size - size));
                }
                return Some(start);
            }
        }

        let addr = self.next_mmap;
        let mut candidate = addr;

        if candidate >= Self::KERNEL_VA_START && candidate < Self::KERNEL_VA_END {
            candidate = Self::KERNEL_VA_END;
        }
        if candidate + size > Self::KERNEL_VA_START && candidate < Self::KERNEL_VA_START {
            candidate = Self::KERNEL_VA_END;
        }

        if self.overlaps_stack(candidate, size) {
            return None;
        }
        if candidate + size > self.mmap_limit {
            return None;
        }

        self.next_mmap = candidate + size;
        Some(candidate)
    }

    pub fn free_mmap(&mut self, start: usize, size: usize) {
        self.free_regions.push((start, size));
    }
}

/// Process info for display (used by ps command)
#[derive(Debug, Clone)]
pub struct ProcessInfo2 {
    pub pid: Pid,
    pub ppid: Pid,
    pub box_id: u64,
    pub name: String,
    pub state: &'static str,
    pub last_syscall: u64,
    pub args: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::task::{RawWaker, RawWakerVTable, Waker};

    #[test]
    fn process_info_size() {
        assert_eq!(core::mem::size_of::<ProcessInfo>(), 1024);
    }

    #[test]
    fn process_info_new() {
        let info = ProcessInfo::new(42, 1, 7);
        assert_eq!(info.pid, 42);
        assert_eq!(info.ppid, 1);
        assert_eq!(info.box_id, 7);
    }

    #[test]
    fn stdio_buffer_write_and_read() {
        let mut buf = StdioBuffer::new();
        assert!(buf.is_empty());
        buf.write_with_limit(b"hello", 1024);
        assert_eq!(buf.len(), 5);
        let mut out = [0u8; 3];
        let n = buf.read(&mut out);
        assert_eq!(n, 3);
        assert_eq!(&out, b"hel");
        let n = buf.read(&mut out);
        assert_eq!(n, 2);
        assert_eq!(&out[..2], b"lo");
    }

    #[test]
    fn stdio_buffer_write_over_limit_clears() {
        let mut buf = StdioBuffer::new();
        buf.write_with_limit(b"hello", 8);
        buf.write_with_limit(b"world!", 8);
        assert_eq!(buf.len(), 6);
    }

    #[test]
    fn stdio_buffer_set_with_limit_truncates() {
        let mut buf = StdioBuffer::new();
        buf.set_with_limit(b"abcdefghij", 5);
        assert_eq!(buf.len(), 5);
        assert_eq!(buf.clone_data(), b"fghij");
    }

    #[test]
    fn stdio_buffer_clear() {
        let mut buf = StdioBuffer::new();
        buf.write_with_limit(b"data", 1024);
        buf.clear();
        assert!(buf.is_empty());
    }

    #[test]
    fn kernel_file_new() {
        let f = KernelFile::new(String::from("/etc/test"), 0o100);
        assert_eq!(f.path, "/etc/test");
        assert_eq!(f.position, 0);
        assert_eq!(f.flags, 0o100);
    }

    #[test]
    fn user_context_new() {
        let ctx = UserContext::new(0x1000, 0x2000);
        assert_eq!(ctx.pc, 0x1000);
        assert_eq!(ctx.sp, 0x2000);
        assert_eq!(ctx.x0, 0);
    }

    #[test]
    fn signal_action_default() {
        let sa = SignalAction::default();
        assert_eq!(sa.handler, SignalHandler::Default);
        assert_eq!(sa.flags, 0);
    }

    #[test]
    fn process_memory_new() {
        let mem = ProcessMemory::new(0x10000, 0x7FFF_0000, 0x8000_0000, 0);
        assert_eq!(mem.code_end, 0x10000);
        assert_eq!(mem.brk, 0x10000);
        assert_eq!(mem.stack_bottom, 0x7FFF_0000);
        assert_eq!(mem.stack_top, 0x8000_0000);
    }

    #[test]
    fn process_memory_overlaps_stack() {
        let mem = ProcessMemory::new(0x10000, 0x7FFF_0000, 0x8000_0000, 0);
        assert!(mem.overlaps_stack(0x7FFF_0000, 0x1000));
        assert!(!mem.overlaps_stack(0x1000, 0x1000));
    }

    #[test]
    fn process_memory_alloc_mmap_sequential() {
        let mut mem = ProcessMemory::new(0x10000, 0x3000_0000, 0x3010_0000, 0);
        let a1 = mem.alloc_mmap(0x1000);
        let a2 = mem.alloc_mmap(0x1000);
        assert!(a1.is_some());
        assert!(a2.is_some());
        assert_ne!(a1, a2);
    }

    #[test]
    fn process_memory_alloc_mmap_skips_kernel_va() {
        let mut mem = ProcessMemory::new(0x3FFF_0000, 0x6000_0000, 0x6010_0000, 0);
        let addr = mem.alloc_mmap(0x1000);
        if let Some(a) = addr {
            assert!(a < 0x4000_0000 || a >= 0x5000_0000);
        }
    }

    #[test]
    fn process_memory_free_and_reuse() {
        let mut mem = ProcessMemory::new(0x10000, 0x3000_0000, 0x3010_0000, 0);
        let a1 = mem.alloc_mmap(0x1000).unwrap();
        mem.free_mmap(a1, 0x1000);
        let a2 = mem.alloc_mmap(0x1000).unwrap();
        assert_eq!(a2, a1);
    }

    fn noop_waker() -> Waker {
        fn noop(_: *const ()) {}
        fn clone(p: *const ()) -> RawWaker { RawWaker::new(p, &VTABLE) }
        static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, noop, noop, noop);
        unsafe { Waker::from_raw(RawWaker::new(core::ptr::null(), &VTABLE)) }
    }

    #[test]
    fn yield_once_future() {
        let waker = noop_waker();
        let mut cx = core::task::Context::from_waker(&waker);
        let mut y = YieldOnce::new();
        let pinned = core::pin::Pin::new(&mut y);
        assert_eq!(pinned.poll(&mut cx), core::task::Poll::Pending);
        let pinned = core::pin::Pin::new(&mut y);
        assert_eq!(pinned.poll(&mut cx), core::task::Poll::Ready(()));
    }
}

