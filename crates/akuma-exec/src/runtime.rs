#![allow(clippy::missing_safety_doc)]

/// Single-shot, lock-free cell for boot-registered `Copy` values.
///
/// Re-exported from `akuma-primitives`, where it now lives so that crates
/// wanting it (`akuma-ext2`'s thread hooks) don't have to depend on the whole
/// execution crate to get it. Kept as a re-export so existing
/// `akuma_exec::runtime::OnceCopy` imports keep working.
pub use akuma_primitives::OnceCopy;

use akuma_primitives::Registered;

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

/// Direct call into `akuma_pmm::track_frame`, converting [`FrameSource`] at the
/// boundary — this crate's enum and the crate's `akuma_pmm::FrameSource` are
/// separate types (the crate sits below `akuma-exec` and cannot name this one).
/// Mirrors `src/pmm.rs`'s identical wrapper for `src/`'s own call sites.
pub fn track_frame(frame: PhysFrame, source: FrameSource) {
    let src = match source {
        FrameSource::Kernel => akuma_pmm::FrameSource::Kernel,
        FrameSource::UserPageTable => akuma_pmm::FrameSource::UserPageTable,
        FrameSource::UserData => akuma_pmm::FrameSource::UserData,
        FrameSource::ElfLoader => akuma_pmm::FrameSource::ElfLoader,
        FrameSource::Unknown => akuma_pmm::FrameSource::Unknown,
    };
    akuma_pmm::track_frame(frame.addr, src);
}

/// Kernel-provided callbacks for the exec crate.
///
/// Registered once during init. All function pointers must remain valid
/// for the lifetime of the kernel (they are plain `fn` pointers, not closures).
///
/// **The 12 PMM fields this used to carry are gone** (`docs/archive/PMM_EXTRACT.md`
/// §7 Step 5, 2026-08-14): `akuma-pmm` is a crate now, this crate depends on it
/// directly, and every call site that used to read one of these fields off
/// `runtime()` calls `akuma_pmm::*` directly instead, converting `PhysFrame` at
/// the boundary the same way `src/pmm.rs` does for `src/`'s own call sites.
///
/// `is_memory_low` is the one PMM-shaped field the plan's inventory counted as
/// a 13th but that could not move with the rest: its implementation
/// (`allocator::is_memory_low`) lives in `src/`, not in `akuma_pmm` — it checks
/// `pmm::free_count()` once the PMM is up, but falls back to the kernel heap's
/// own byte accounting before that (`is_pmm_ready()`), which is `src/allocator.rs`
/// state this crate has no way to reach. Turning it into a direct call would mean
/// dropping that pre-init fallback, a real behaviour change this
/// behaviour-preserving step must not make. It stays a hook, correctly — this is
/// the ordinary direction for `ExecRuntime`, a hook down into `src/`, not the
/// leftover-from-when-PMM-lived-in-`src/` indirection the other 12 were.
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
    /// Ring one idle peer core's scheduler SGI so it can pick up READY work.
    /// Returns `true` if an idle peer was found and rung, `false` if every peer
    /// is busy (callers use this to decide whether displacing the current
    /// thread is the only way to run queued work). Off shared-SMP: always
    /// `false`.
    pub wake_remote_idle: fn() -> bool,
    // Real shared-kernel SMP: send a scheduler SGI to a specific core (the woken
    // thread's last-known core) so its scheduler picks up the just-READY thread
    // without waiting for the ~10 ms timer tick. No-op on single-core / non-SMP.
    pub wake_core: fn(u8),

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
    /// Takes the path as well as the inode: the VFS is multi-root (`with_fs`
    /// dispatches on the path prefix), so an inode alone does not name a file.
    /// The prefault fill in `mmu/user_access.rs` calls this for every
    /// inode-backed lazy file page — a stub registration here turns those pages
    /// into silent zeros (`[FILL-SHORT/prefault]`, the self-host ICE).
    pub read_at_by_inode: fn(&str, u32, usize, &mut [u8]) -> Result<usize, i32>,

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
    /// ITIMER_REAL / alarm() expiry check + SIGALRM delivery, riding the tick
    /// ISR (called from `alarms::on_timer_interrupt`). A hook because itimer
    /// state lives in the bin crate's syscall layer.
    pub check_itimers: fn(),
    pub pipe_close_write: fn(u32),
    pub pipe_close_read: fn(u32),
    pub pipe_clone_ref: fn(u32, bool),
    pub eventfd_close: fn(u32),
    pub eventfd_clone_ref: fn(u32),
    /// Release one reference to an AF_UNIX table entry
    /// (`FileDescriptor::UnixSocket`'s `sock` field), tearing it down at zero.
    ///
    /// Needed as a callback for the same reason `pipe_close_read` is: the fd
    /// table lives here and the socket table lives in `akuma-net`, and this
    /// crate cannot call into it directly. Skipping it would leak a table
    /// entry — and, for a listener, every server-side endpoint still queued in
    /// its backlog — on every close. A `sock` of 0 is the "no entry" sentinel
    /// and the implementation ignores it.
    pub unix_sock_close: fn(u32),
    /// Take one reference to an AF_UNIX table entry. `dup`, `dup2`, `F_DUPFD`
    /// and `fork` each produce a real second reference; without this the first
    /// close destroys the entry underneath the other fd, exactly as it would
    /// for a pipe or a socket.
    pub unix_sock_clone_ref: fn(u32),
    pub epoll_destroy: fn(u32),
    pub pidfd_close: fn(u32),
    /// Release whatever `flock(2)` lock `(holder, fd)` — the `usize` is the
    /// calling process's `SharedFdTable` `Arc` pointer, see `src/syscall/flock.rs`
    /// — holds on `path`, if any. A no-op if it holds none.
    pub flock_release: fn(&str, usize, u32),

    // VFS helpers
    pub resolve_symlinks: fn(&str) -> alloc::string::String,
    pub file_size: fn(&str) -> Result<u64, &'static str>,

    // Namespace lookup (for container spawn)
    pub get_box_namespace: fn(u64) -> Option<alloc::sync::Arc<akuma_isolation::Namespace>>,
    pub set_spawn_namespace: fn(alloc::sync::Arc<akuma_isolation::Namespace>),
    pub clear_spawn_namespace: fn(),

    // Console fallback
    pub print_str: fn(&str),
}

/// Compile-time kernel configuration, passed once at init.
///
/// **`pmm_uaf_quarantine` is gone** (`docs/archive/PMM_EXTRACT.md` §7 Step 6):
/// its only reader was `memmath::poison_word_frame`'s gate, and that function
/// moved to `src/pmm.rs` and gates on `akuma_pmm::config().pmm_uaf_quarantine`
/// instead — the crate's own copy of the same kernel constant
/// (`config::PMM_UAF_QUARANTINE` feeds both; `src/main.rs` only sets one now).
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

    /// Share physical frames between read-only file-backed mappings. Gates
    /// [`crate::memmath::is_shareable_mapping`]. See
    /// `config::SHARED_FILE_PAGES_ENABLED`.
    pub shared_file_pages_enabled: bool,
}

// Lock-free single-shot cells: must be safe to read from IRQ context.
// A spinlock here causes a self-deadlock if any IRQ handler (e.g. the
// preemption watchdog) reads while EL1 code is mid-critical-section.
// `Registered` is the shared form of that; see `akuma_primitives::once`.
static RUNTIME: Registered<ExecRuntime> =
    Registered::new("akuma-exec: ExecRuntime not registered — call akuma_exec::init() first");
static CONFIG: Registered<ExecConfig> =
    Registered::new("akuma-exec: ExecConfig not registered — call akuma_exec::init() first");

/// Register the kernel runtime callbacks. Must be called exactly once,
/// before any other crate function (including from IRQ handlers).
pub fn register(rt: ExecRuntime, cfg: ExecConfig) {
    RUNTIME.register(rt);
    CONFIG.register(cfg);
    // Point the shared `safe_print!` at the same sink `runtime()` hands out, so
    // every crate's heap-free console output lights up at exactly this moment —
    // which is when it lit up before, back when each crate's own writer called
    // `(runtime().print_str)(…)` directly. Before this call those macros are
    // silent rather than panicking; see `akuma_primitives::console`.
    akuma_primitives::console::set_print_hook(rt.print_str);
    // Same for the uptime clock. `akuma-primitives`' preemption bookkeeping wants
    // it for one diagnostic timestamp, and it degrades to 0 before this point —
    // which is exactly what `disable_preemption` already did behind an
    // `is_registered()` check. See `akuma_primitives::clock`.
    akuma_primitives::clock::set_clock_hook(rt.uptime_us);
}

/// Access the registered runtime. Panics if not yet registered.
/// Safe to call from IRQ context — never blocks.
#[must_use]
pub fn runtime() -> ExecRuntime {
    RUNTIME.require()
}

/// Whether the runtime + config have been registered (non-panicking probe). Lets code
/// that might run before `init()` (e.g. an early BKL-stuck log or timestamp) degrade
/// gracefully instead of panicking.
#[must_use]
pub fn is_registered() -> bool {
    RUNTIME.is_registered() && CONFIG.is_registered()
}

/// Access the registered config. Panics if not yet registered.
/// Safe to call from IRQ context — never blocks.
#[must_use]
pub fn config() -> ExecConfig {
    CONFIG.require()
}

/// Register **only** the config half, for host unit tests.
///
/// Host tests cannot call [`crate::init`]: that also wants an [`ExecRuntime`],
/// which is 27 kernel function pointers with no meaningful stub. But plenty of
/// this crate's pure logic reads `config()` and nothing else, and it should not
/// have to grow a production-side "is anything registered?" branch just to be
/// reachable from a test — inject the dependency instead of teaching the code
/// to live without it.
///
/// `OnceCopy::set` is idempotent (a second `set` is a silent no-op), so every
/// test can call this unconditionally even though `cargo test` runs them in
/// parallel threads of one process. First writer wins; they all write
/// [`ExecConfig::for_test`], so there is nothing to race over.
#[cfg(test)]
pub(crate) fn register_config_for_test() {
    CONFIG.register(ExecConfig::for_test());
}

#[cfg(test)]
impl ExecConfig {
    /// Plausible host-test config. Deliberately a full struct literal with no
    /// `..Default::default()`: adding a field to `ExecConfig` should break this
    /// and make someone choose a test value, rather than silently defaulting to
    /// zero for something a test then depends on.
    ///
    /// `syscall_debug_info_enabled` is **on** so the tracing paths are actually
    /// executed rather than skipped by their own gate. On the host
    /// `safe_print!` writes to an unregistered console hook, which discards.
    pub(crate) fn for_test() -> Self {
        Self {
            max_threads: 256,
            reserved_threads: 8,
            kernel_stack_size: 64 * 1024,
            boot_stack_base: 0,
            boot_stack_top: 0,
            default_thread_stack_size: 64 * 1024,
            system_thread_stack_size: 96 * 1024,
            user_thread_stack_size: 64 * 1024,
            user_stack_size: 1024 * 1024,
            enable_stack_canaries: false,
            stack_canary: 0xDEAD_BEEF_CAFE_BABE,
            canary_words: 2,
            network_thread_ratio: 4,
            prioritize_never_scheduled: false,
            deferred_thread_cleanup: false,
            thread_cleanup_cooldown_us: 0,
            process_reclaim_cooldown_us: 0,
            syscall_debug_info_enabled: true,
            fork_brk_serial_progress: false,
            enable_sgi_debug_prints: false,
            proc_stdin_max_size: 64 * 1024,
            proc_stdout_max_size: 64 * 1024,
            cow_fork_enabled: true,
            vfork_fastpath_enabled: true,
            pthread_kill_eintr_enabled: true,
            // **On**, for the same reason as `syscall_debug_info_enabled` above:
            // a gate left off makes every test of the gated path skip the one
            // branch it exists to cover. `memmath`'s tests rely on this.
            shared_file_pages_enabled: true,
        }
    }
}

/// Local-IRQ masking, re-exported from `akuma_primitives::irq`.
///
/// This crate carried its own `IrqGuard` — one of two under that name, the other
/// in the bin crate at `src/irq.rs:12`, plus a third barrier-less DAIF
/// implementation in this crate's `sync.rs`. All three are now one; see
/// `akuma_primitives::irq` for the census and for why the `isb` difference
/// between the guard and `irq_save_mask` is preserved rather than resolved.
///
/// Kept as re-exports so `akuma_exec::runtime::{IrqGuard, with_irqs_disabled}`
/// keeps resolving for the ~34 call sites across this crate and the bin crate.
pub use akuma_primitives::irq::{IrqGuard, with_irqs_disabled};

// `OnceCopy`'s unit tests moved with it to `akuma-primitives::once`.
