#![allow(clippy::missing_safety_doc)]

/// Single-shot, lock-free cell for boot-registered `Copy` values.
///
/// Re-exported from `akuma-primitives`, where it now lives so that crates
/// wanting it (`akuma-ext2`'s thread hooks) don't have to depend on the whole
/// execution crate to get it. Kept as a re-export so existing
/// `akuma_exec::runtime::OnceCopy` imports keep working.
pub use akuma_primitives::OnceCopy;

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
    // Real shared-kernel SMP: nudge one idle peer core to reschedule so a just-woken
    // thread runs there promptly instead of waiting for that core's next timer tick.
    // No-op on single-core / non-SMP builds.
    pub wake_remote_idle: fn(),
    // Real shared-kernel SMP: send a scheduler SGI to a specific core (the woken
    // thread's last-known core) so its scheduler picks up the just-READY thread
    // without waiting for the ~10 ms timer tick. No-op on single-core / non-SMP.
    pub wake_core: fn(u8),

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

    /// Whether the shared-kernel SMP execve/ELF-load BKL-drop is enabled (mirrors
    /// `smp_shared::exec_bkl_drop_enabled`). The ELF loader (in this crate) consults it to
    /// decide whether to drop the BKL around the dynamic-interpreter whole-file read; always
    /// `false` off `smp-shared`, where the `bkl` calls are no-ops anyway.
    pub exec_bkl_drop_enabled: fn() -> bool,

    // VFS (for elf_loader)
    pub read_file: fn(&str) -> Result<alloc::vec::Vec<u8>, i32>,
    pub read_at: fn(&str, usize, &mut [u8]) -> Result<usize, i32>,
    pub resolve_inode: fn(&str) -> Result<u32, i32>,
    pub read_at_by_inode: fn(u32, usize, &mut [u8]) -> Result<usize, i32>,

    // Process exit hook (e.g. socket cleanup)
    pub on_process_exit: fn(u32),

    // Socket cleanup (per-FD)
    pub remove_socket: fn(usize),

    // Socket fd-reference bump (fork's fd-table deep copy). Mirrors
    // `pipe_clone_ref`: without it the first close of a fork-inherited socket fd
    // destroys the socket under the other holders.
    pub socket_clone_ref: fn(usize),
    /// Take one reference on a `stack=rump` box's socket, keyed `(box_id, rump_fd)`.
    /// The rump counterpart of `socket_clone_ref`: `fork` duplicates the descriptor,
    /// so the rump-side socket must outlive the first `close` of the two.
    pub rump_socket_clone_ref: fn(u64, i32),

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
    pub cow_fault_lock: fn(usize),
    pub cow_fault_unlock: fn(usize),

}

/// Compile-time kernel configuration, passed once at init.
#[derive(Clone, Copy)]
pub struct ExecConfig {
    pub max_threads: usize,
    pub reserved_threads: usize,
    pub kernel_stack_size: usize,
    /// Boot (thread 0) stack bounds, supplied by the kernel because they are
    /// profile-dependent (see boot.rs / build.rs / linker.ld). `boot_stack_base`
    /// is the lowest address (where the stack canary is written); `boot_stack_top`
    /// is the highest. Hardcoding these in the crate caused the canary to be
    /// stamped into the kernel heap once the boot stack was relocated — see
    /// docs/LOW_MEMORY_ENVIRONMENT.md "Known bug".
    pub boot_stack_base: usize,
    pub boot_stack_top: usize,
    pub default_thread_stack_size: usize,
    pub system_thread_stack_size: usize,
    pub user_thread_stack_size: usize,
    pub user_stack_size: usize,
    pub enable_stack_canaries: bool,
    pub stack_canary: u64,
    pub canary_words: usize,
    pub network_thread_ratio: u32,
    /// See `config::PRIORITIZE_NEVER_SCHEDULED`.
    pub prioritize_never_scheduled: bool,
    pub deferred_thread_cleanup: bool,
    pub thread_cleanup_cooldown_us: u64,
    /// Cooldown before `process::reclaim_retired_processes` actually frees a
    /// retired (reaped) process's memory. See its doc comment.
    pub process_reclaim_cooldown_us: u64,
    pub syscall_debug_info_enabled: bool,
    /// Print a line to serial every N pages while copying brk during fork (slow on large heaps).
    pub fork_brk_serial_progress: bool,
    pub enable_sgi_debug_prints: bool,
    pub proc_stdin_max_size: usize,
    pub proc_stdout_max_size: usize,
    pub cow_fork_enabled: bool,
    /// Enable the vfork fast-path (shared-AS child for CLONE_VFORK). See
    /// `config::VFORK_FASTPATH_ENABLED`.
    pub vfork_fastpath_enabled: bool,

    /// Let a `tkill`/`tgkill` (`pthread_kill`) signal interrupt a blocking
    /// syscall with `EINTR`. See `config::PTHREAD_KILL_EINTR_ENABLED`.
    pub pthread_kill_eintr_enabled: bool,
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
    // Point the shared `safe_print!` at the same sink `runtime()` hands out, so
    // every crate's heap-free console output lights up at exactly this moment —
    // which is when it lit up before, back when each crate's own writer called
    // `(runtime().print_str)(…)` directly. Before this call those macros are
    // silent rather than panicking; see `akuma_primitives::console`.
    akuma_primitives::console::set_print_hook(rt.print_str);
}

/// Access the registered runtime. Panics if not yet registered.
/// Safe to call from IRQ context — never blocks.
#[must_use]
pub fn runtime() -> ExecRuntime {
    RUNTIME
        .get()
        .expect("akuma-exec: ExecRuntime not registered — call akuma_exec::init() first")
}

/// Whether the runtime + config have been registered (non-panicking probe). Lets code
/// that might run before `init()` (e.g. an early BKL-stuck log or timestamp) degrade
/// gracefully instead of panicking.
#[must_use]
pub fn is_registered() -> bool {
    RUNTIME.get().is_some() && CONFIG.get().is_some()
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

// `OnceCopy`'s unit tests moved with it to `akuma-primitives::once`.
