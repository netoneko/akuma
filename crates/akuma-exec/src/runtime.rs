#![allow(clippy::missing_safety_doc)]

use core::cell::UnsafeCell;
use core::mem::MaybeUninit;
use core::sync::atomic::{AtomicBool, Ordering};

/// Single-shot, lock-free cell for `Copy` types.
///
/// Set once at init, then read freely from any context (including IRQ
/// handlers). No spinlock — readers must never block on writers, because
/// reading `RUNTIME`/`CONFIG` from inside an IRQ that interrupted code
/// holding the same lock would self-deadlock on a single CPU.
struct OnceCopy<T: Copy> {
    initialized: AtomicBool,
    value: UnsafeCell<MaybeUninit<T>>,
}

unsafe impl<T: Copy + Send + Sync> Sync for OnceCopy<T> {}

impl<T: Copy> OnceCopy<T> {
    const fn new() -> Self {
        Self {
            initialized: AtomicBool::new(false),
            value: UnsafeCell::new(MaybeUninit::uninit()),
        }
    }

    /// Write the value. Must be called exactly once before any `get()`.
    /// Second call is silently ignored — callers shouldn't rely on that.
    fn set(&self, v: T) {
        if self.initialized.load(Ordering::Acquire) {
            return;
        }
        // SAFETY: we are the only writer (single-shot at boot); readers
        // observe the value only after the Release store below.
        unsafe { (*self.value.get()).write(v) };
        self.initialized.store(true, Ordering::Release);
    }

    fn get(&self) -> Option<T> {
        if self.initialized.load(Ordering::Acquire) {
            // SAFETY: initialized=true means the value was fully written
            // before the Release store; T: Copy lets us read a copy.
            Some(unsafe { (*self.value.get()).assume_init_read() })
        } else {
            None
        }
    }
}

/// Physical page frame (mirrors kernel pmm::PhysFrame).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PhysFrame {
    pub addr: usize,
}

impl PhysFrame {
    pub const fn new(addr: usize) -> Self {
        Self {
            addr: addr & !(4096 - 1),
        }
    }

    pub fn containing_address(addr: usize) -> Self {
        Self::new(addr)
    }

    pub fn start_address(&self) -> usize {
        self.addr
    }
}

/// Allocation source for debug frame tracking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameSource {
    Kernel,
    UserPageTable,
    UserData,
    ElfLoader,
    Unknown,
}

/// Kernel-provided callbacks for the exec crate.
///
/// Registered once during init. All function pointers must remain valid
/// for the lifetime of the kernel (they are plain `fn` pointers, not closures).
#[derive(Clone, Copy)]
pub struct ExecRuntime {
    // Timer
    pub uptime_us: fn() -> u64,

    // IRQ control
    pub disable_irqs: fn(),
    pub enable_irqs: fn(),

    // GIC
    pub end_of_interrupt: fn(u32),
    pub trigger_sgi: fn(u32),

    // PMM
    pub alloc_page_zeroed: fn() -> Option<PhysFrame>,
    pub alloc_page: fn() -> Option<PhysFrame>,
    pub free_page: fn(PhysFrame),
    pub pmm_stats: fn() -> (usize, usize, usize),
    pub track_frame: fn(PhysFrame, FrameSource),
    pub free_count: fn() -> usize,
    pub total_count: fn() -> usize,
    pub alloc_pages_contiguous_zeroed: fn(usize) -> Option<PhysFrame>,
    pub free_pages_contiguous: fn(PhysFrame, usize),

    // Allocator
    pub heap_stats: fn() -> (usize, usize),
    pub is_memory_low: fn() -> bool,

    // VFS (for elf_loader)
    pub read_file: fn(&str) -> Result<alloc::vec::Vec<u8>, i32>,
    pub read_at: fn(&str, usize, &mut [u8]) -> Result<usize, i32>,
    pub resolve_inode: fn(&str) -> Result<u32, i32>,
    pub read_at_by_inode: fn(u32, usize, &mut [u8]) -> Result<usize, i32>,

    // Process exit hook (e.g. socket cleanup)
    pub on_process_exit: fn(u32),

    // Socket cleanup (per-FD)
    pub remove_socket: fn(usize),

    // Syscall helpers
    pub futex_wake: fn(u32, usize, i32),
    pub pipe_close_write: fn(u32),
    pub pipe_close_read: fn(u32),
    pub pipe_clone_ref: fn(u32, bool),
    pub eventfd_close: fn(u32),
    pub eventfd_clone_ref: fn(u32),
    pub epoll_destroy: fn(u32),
    pub pidfd_close: fn(u32),

    // VFS helpers
    pub resolve_symlinks: fn(&str) -> alloc::string::String,
    pub file_size: fn(&str) -> Result<u64, &'static str>,

    // Namespace lookup (for container spawn)
    pub get_box_namespace: fn(u64) -> Option<alloc::sync::Arc<akuma_isolation::Namespace>>,
    pub set_spawn_namespace: fn(alloc::sync::Arc<akuma_isolation::Namespace>),
    pub clear_spawn_namespace: fn(),

    // Console fallback
    pub print_str: fn(&str),

    // Copy-on-Write fork
    pub cow_ref_inc: fn(usize),
    pub cow_ref_dec: fn(usize) -> bool,
    pub cow_ref_get: fn(usize) -> u16,
}

/// Compile-time kernel configuration, passed once at init.
#[derive(Clone, Copy)]
pub struct ExecConfig {
    pub max_threads: usize,
    pub reserved_threads: usize,
    pub kernel_stack_size: usize,
    pub default_thread_stack_size: usize,
    pub system_thread_stack_size: usize,
    pub user_thread_stack_size: usize,
    pub user_stack_size: usize,
    pub enable_stack_canaries: bool,
    pub stack_canary: u64,
    pub canary_words: usize,
    pub network_thread_ratio: u32,
    pub deferred_thread_cleanup: bool,
    pub thread_cleanup_cooldown_us: u64,
    pub syscall_debug_info_enabled: bool,
    /// Print a line to serial every N pages while copying brk during fork (slow on large heaps).
    pub fork_brk_serial_progress: bool,
    pub enable_sgi_debug_prints: bool,
    pub proc_stdin_max_size: usize,
    pub proc_stdout_max_size: usize,
    pub cow_fork_enabled: bool,
}

// Lock-free single-shot cells: must be safe to read from IRQ context.
// A spinlock here causes a self-deadlock if any IRQ handler (e.g. the
// preemption watchdog) reads while EL1 code is mid-critical-section.
static RUNTIME: OnceCopy<ExecRuntime> = OnceCopy::new();
static CONFIG: OnceCopy<ExecConfig> = OnceCopy::new();

/// Register the kernel runtime callbacks. Must be called exactly once,
/// before any other crate function (including from IRQ handlers).
pub fn register(rt: ExecRuntime, cfg: ExecConfig) {
    RUNTIME.set(rt);
    CONFIG.set(cfg);
}

/// Access the registered runtime. Panics if not yet registered.
/// Safe to call from IRQ context — never blocks.
#[must_use]
pub fn runtime() -> ExecRuntime {
    RUNTIME
        .get()
        .expect("akuma-exec: ExecRuntime not registered — call akuma_exec::init() first")
}

/// Access the registered config. Panics if not yet registered.
/// Safe to call from IRQ context — never blocks.
#[must_use]
pub fn config() -> ExecConfig {
    CONFIG
        .get()
        .expect("akuma-exec: ExecConfig not registered — call akuma_exec::init() first")
}

/// Run a closure with IRQs disabled, properly saving and restoring DAIF.
#[inline]
pub fn with_irqs_disabled<T, F: FnOnce() -> T>(f: F) -> T {
    let _guard = IrqGuard::new();
    f()
}

/// RAII guard that saves DAIF on creation and restores on drop.
///
/// On non-aarch64 targets (host testing), this is a no-op.
pub struct IrqGuard {
    #[cfg(target_os = "none")]
    saved_daif: u64,
}

impl IrqGuard {
    #[inline]
    pub fn new() -> Self {
        #[cfg(target_os = "none")]
        {
            let daif: u64;
            unsafe {
                core::arch::asm!("mrs {}, daif", out(reg) daif, options(nomem, nostack));
                core::arch::asm!("msr daifset, #2", options(nomem, nostack));
                core::arch::asm!("isb", options(nomem, nostack));
            }
            Self { saved_daif: daif }
        }
        #[cfg(not(target_os = "none"))]
        {
            Self {}
        }
    }
}

impl Drop for IrqGuard {
    #[inline]
    fn drop(&mut self) {
        #[cfg(target_os = "none")]
        unsafe {
            core::arch::asm!("msr daif, {}", in(reg) self.saved_daif, options(nomem, nostack));
        }
    }
}

#[cfg(test)]
mod once_copy_tests {
    use super::OnceCopy;

    #[test]
    fn get_returns_none_before_set() {
        let cell: OnceCopy<u32> = OnceCopy::new();
        assert!(cell.get().is_none());
    }

    #[test]
    fn get_returns_value_after_set() {
        let cell: OnceCopy<u32> = OnceCopy::new();
        cell.set(0xc0ffee);
        assert_eq!(cell.get(), Some(0xc0ffee));
    }

    #[test]
    fn second_set_is_ignored() {
        let cell: OnceCopy<u32> = OnceCopy::new();
        cell.set(1);
        cell.set(2);
        assert_eq!(cell.get(), Some(1));
    }

    #[test]
    fn many_reads_return_same_value() {
        let cell: OnceCopy<u64> = OnceCopy::new();
        cell.set(0xdead_beef_cafe_babe);
        for _ in 0..10_000 {
            assert_eq!(cell.get(), Some(0xdead_beef_cafe_babe));
        }
    }

    #[test]
    fn concurrent_readers_after_set_never_block() {
        // Lock-free contract: many threads reading concurrently must each
        // observe the value with no spinning, no panics. If anyone ever
        // reintroduced a Spinlock-on-read, this would still pass (no
        // contention), but combined with the "called from IRQ" kernel
        // test it nails down the invariant.
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::thread;

        let cell: Arc<OnceCopy<u32>> = Arc::new(OnceCopy::new());
        cell.set(42);

        let hits = Arc::new(AtomicUsize::new(0));
        let mut handles = vec![];
        for _ in 0..8 {
            let cell = Arc::clone(&cell);
            let hits = Arc::clone(&hits);
            handles.push(thread::spawn(move || {
                for _ in 0..10_000 {
                    if cell.get() == Some(42) {
                        hits.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(hits.load(Ordering::Relaxed), 8 * 10_000);
    }
}
