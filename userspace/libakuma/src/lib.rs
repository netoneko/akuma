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
}

/// File descriptors
pub mod fd {
    pub const STDIN: u64 = 0;
    pub const STDOUT: u64 = 1;
    pub const STDERR: u64 = 2;
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
// Global Allocator using brk syscall
// ============================================================================

mod allocator {
    use core::alloc::{GlobalAlloc, Layout};
    use core::cell::UnsafeCell;
    use core::ptr;

    /// Simple bump allocator using brk syscall
    /// 
    /// This is a very simple allocator that only grows the heap.
    /// Deallocation is a no-op (memory is only freed when process exits).
    pub struct BrkAllocator {
        /// Current allocation pointer (next free address)
        head: UnsafeCell<usize>,
        /// End of currently allocated heap pages
        end: UnsafeCell<usize>,
    }

    unsafe impl Sync for BrkAllocator {}

    impl BrkAllocator {
        pub const fn new() -> Self {
            Self {
                head: UnsafeCell::new(0),
                end: UnsafeCell::new(0),
            }
        }

        /// Initialize the allocator by getting the current brk
        fn init(&self) {
            unsafe {
                let head = self.head.get();
                if *head == 0 {
                    // First allocation - get initial brk from kernel
                    let initial_brk = super::brk(0);
                    *head = initial_brk;
                    
                    // Request 64KB of heap space (kernel may have pre-allocated this)
                    let requested_end = initial_brk + 0x10000;
                    let actual_end = super::brk(requested_end);
                    *self.end.get() = actual_end;
                }
            }
        }

        /// Expand the heap if needed
        fn expand(&self, needed: usize) -> bool {
            unsafe {
                let end = self.end.get();
                // Request more memory from kernel
                // Add some extra to reduce syscall frequency
                let grow_by = ((needed + 0xFFF) & !0xFFF).max(4096);
                let new_end = *end + grow_by;
                let result = super::brk(new_end);
                if result >= new_end {
                    *end = result;
                    true
                } else {
                    false
                }
            }
        }
    }

    unsafe impl GlobalAlloc for BrkAllocator {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            self.init();

            let head = self.head.get();
            let end = self.end.get();

            // Align the head pointer
            let align = layout.align();
            let aligned = (*head + align - 1) & !(align - 1);
            let new_head = aligned + layout.size();

            // Check if we need more heap space
            if new_head > *end {
                let needed = new_head - *end;
                if !self.expand(needed) {
                    return ptr::null_mut();
                }
            }

            *head = new_head;
            aligned as *mut u8
        }

        unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {
            // Bump allocator - no-op for deallocation
            // Memory is reclaimed when process exits
        }

        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            // Simple implementation: allocate new, copy, don't free old
            let new_layout = match Layout::from_size_align(new_size, layout.align()) {
                Ok(l) => l,
                Err(_) => return ptr::null_mut(),
            };
            
            let new_ptr = self.alloc(new_layout);
            if !new_ptr.is_null() && !ptr.is_null() {
                let copy_size = layout.size().min(new_size);
                ptr::copy_nonoverlapping(ptr, new_ptr, copy_size);
            }
            new_ptr
        }
    }

    #[global_allocator]
    pub static ALLOCATOR: BrkAllocator = BrkAllocator::new();
}

/// Panic handler for user programs
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    eprint("PANIC!\n");
    exit(1);
}
