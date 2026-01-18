//! Akuma User Space Library
//!
//! Provides syscall wrappers and runtime support for user programs.

#![no_std]

extern crate alloc;

use core::arch::asm;

/// Syscall numbers
pub mod syscall {
    pub const EXIT: u64 = 0;
    pub const READ: u64 = 1;
    pub const WRITE: u64 = 2;
    pub const BRK: u64 = 3;
    pub const NANOSLEEP: u64 = 101;
    pub const MMAP: u64 = 222;
    pub const MUNMAP: u64 = 215;
    pub const UPTIME: u64 = 216;
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
pub const ARGV_DATA_SIZE: usize = 1024 - 16;

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
///   - argv_data: 1008 bytes (null-separated argument strings)
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

/// Print a string to stdout
#[inline(always)]
pub fn print(s: &str) {
    write(fd::STDOUT, s.as_bytes());
}

/// Print a string to stderr
#[inline(always)]
pub fn eprint(s: &str) {
    write(fd::STDERR, s.as_bytes());
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
    const MAP_FAILED: usize = usize::MAX;

    /// Hybrid allocator that can use either mmap or brk
    /// WORKAROUND: Large padding to work around layout-sensitive heap corruption bug.
    /// The bug causes String::push_str to fail when the binary is a certain size.
    /// Adding padding changes the binary layout and makes the bug go away.
    #[repr(C, align(256))]
    pub struct HybridAllocator {
        /// For brk mode: current allocation pointer
        brk_head: AtomicUsize,
        /// For brk mode: end of allocated heap
        brk_end: AtomicUsize,
        /// Padding to work around layout-sensitive bug (see docs/HEAP_CORRUPTION_ANALYSIS.md)
        _padding: [u8; 240],
    }

    unsafe impl Sync for HybridAllocator {}

    impl HybridAllocator {
        pub const fn new() -> Self {
            Self {
                brk_head: AtomicUsize::new(0),
                brk_end: AtomicUsize::new(0),
                _padding: [0u8; 240],
            }
        }

        /// Get allocator info for debugging
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

        #[inline(never)]
        unsafe fn mmap_alloc(&self, layout: Layout) -> *mut u8 {
            use super::mmap_flags::*;

            // Round up to page size for mmap
            let size = layout.size().max(layout.align());
            let alloc_size = (size + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);

            let addr = super::mmap(
                0, // Let kernel choose address
                alloc_size,
                PROT_READ | PROT_WRITE,
                MAP_PRIVATE | MAP_ANONYMOUS,
            );

            if addr == MAP_FAILED || addr == 0 {
                ptr::null_mut()
            } else {
                addr as *mut u8
            }
        }

        unsafe fn mmap_dealloc(&self, ptr: *mut u8, layout: Layout) {
            let size = layout.size().max(layout.align());
            let alloc_size = (size + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
            // Use munmap_void which properly marks x0 as clobbered
            // to prevent corrupting function return values when called from Drop
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

        unsafe fn brk_realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            let new_layout = match Layout::from_size_align(new_size, layout.align()) {
                Ok(l) => l,
                Err(_) => return ptr::null_mut(),
            };

            let new_ptr = self.brk_alloc(new_layout);
            if !new_ptr.is_null() && !ptr.is_null() {
                let copy_size = layout.size().min(new_size);
                ptr::copy_nonoverlapping(ptr, new_ptr, copy_size);
            }
            new_ptr
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
            // brk mode: no-op (bump allocator)
        }

        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            // Extract layout fields HERE where they're correct
            let old_size = layout.size();
            let old_align = layout.align();

            if !USE_MMAP_ALLOCATOR {
                return self.brk_realloc(ptr, layout, new_size);
            }

            // INLINE realloc logic - no function call to avoid ABI issues
            // (see docs/STDCHECK_DEBUG.md for why this is necessary)

            // Use safe alignment
            let safe_align = if old_align == 0 || (old_align & (old_align - 1)) != 0 {
                1
            } else {
                old_align
            };

            // Allocate new buffer
            let new_layout = match Layout::from_size_align(new_size, safe_align) {
                Ok(l) => l,
                Err(_) => return ptr::null_mut(),
            };

            let new_ptr = self.mmap_alloc(new_layout);
            if new_ptr.is_null() {
                return ptr::null_mut();
            }

            // Copy old data
            if !ptr.is_null() && old_size > 0 {
                let copy_size = old_size.min(new_size);
                ptr::copy_nonoverlapping(ptr, new_ptr, copy_size);
            }

            new_ptr
        }
    }

    #[global_allocator]
    pub static ALLOCATOR: HybridAllocator = HybridAllocator::new();
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

/// Panic handler for user programs
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    eprint("PANIC!\n");
    exit(1);
}
