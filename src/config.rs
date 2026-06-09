//! Kernel configuration constants
//!
//! This module contains tunable parameters for the kernel.
//! Modify these values to adjust kernel behavior.
//!
//! # Stack Size Warnings
//!
//! Stack sizes may be insufficient for certain workloads:
//! - Deep async call chains (SSH, HTTP) may need larger stacks
//! - Recursive algorithms can overflow smaller stacks
//! - Complex shell commands may need more stack space
//!
//! See `docs/THREAD_STACK_ANALYSIS.md` for detailed analysis and guidance.

#![allow(dead_code)]

/// Physical address where the kernel binary is loaded.
///
/// QEMU virt ARM64 Image boot: text_offset in the Image header controls load
/// address. text_offset = 1 MB (≥ 4 KB, so QEMU does NOT add 2 MB) →
/// kernel at RAM_BASE + 1 MB = 0x40100000.
///
/// DTB is placed at ALIGN_UP(KERNEL_PHYS_BASE + image_size, 2MB) = 0x40200000,
/// which fits in 4 MB RAM (DTB end 0x40300000 < 0x40400000).
///
/// Must match `KERNEL_PHYS_BASE` in linker.ld and `text_offset` in boot.rs.
pub const KERNEL_PHYS_BASE: usize = 0x4010_0000;

/// Pre-kernel gap size: bytes from RAM_BASE to KERNEL_PHYS_BASE.
/// This region is reclaimed to the PMM pool after early boot.
pub const KERNEL_PHYS_OFFSET: usize = 0x10_0000; // 1 MB (= text_offset)

/// Boot/kernel stack size (1MB default)
///
/// Used by thread 0 (boot thread) and exception handlers.
/// This stack is placed at a fixed address (0x40800000) in boot.rs.
pub const KERNEL_STACK_SIZE: usize = 1024 * 1024;

/// Default per-thread stack size (32KB)
///
/// Used for kernel threads spawned without a custom stack size.
/// WARNING: May overflow with deep async polling or recursion.
/// Consider using `ASYNC_THREAD_STACK_SIZE` for network-heavy threads.
pub const DEFAULT_THREAD_STACK_SIZE: usize = 32 * 1024;

/// Stack size for networking/async thread (512KB)
///
/// Larger stack to handle deep SSH/HTTP async call chains.
/// Use this for threads that run the async executor or network services.
/// Note: Increased from 256KB due to stack exhaustion during long-running sessions.
pub const ASYNC_THREAD_STACK_SIZE: usize = 512 * 1024;

/// User process stack size override (0 = auto-scale based on RAM)
///
/// Stack allocated for user-space ELF processes.
/// When set to 0, the stack size is automatically computed based on available RAM:
///   - 256 MB RAM → 128 KB stack (minimum for basic apps)
///   - 512 MB RAM → 256 KB stack
///   - 1 GB RAM   → 512 KB stack
///   - 2 GB RAM   → 1 MB stack
///   - 4 GB+ RAM  → 2 MB stack (maximum, needed for heavy runtimes like bun/JSC)
///
/// Set to a non-zero value to override automatic scaling.
/// Bun's JSC initialization uses ~600KB of stack, and complex dependency
/// resolution (like @google/gemini-cli with 263 packages) may need more.
///
/// On the `size` profile (small-RAM targets) we let the RAM-scaling run — the
/// stack is eagerly committed from PMM, so pinning it to 8 MB would consume
/// 2048 pages per process before any work is done.  Auto-scaling gives 128 KB
/// (the minimum) on ≤ 256 MB boxes, which is sufficient for tcc and dash.
pub const USER_STACK_SIZE_OVERRIDE: usize = 0; // auto-scale; set to e.g. 8MB to debug crush/bun/JSC stack depth

/// Maximum kernel threads
///
/// Total number of thread slots in the thread pool.
/// Thread 0 is reserved for the boot/idle thread.
/// Actual usable threads = MAX_THREADS - 1
pub const MAX_THREADS: usize = 64;

/// Number of kernel threads reserved for system services
///
/// Threads 0 to RESERVED_THREADS-1 are reserved for:
/// - Thread 0: Boot/async main loop
/// - Threads 1-7: Shell, SSH sessions, internal services
///
/// User processes can only spawn on threads RESERVED_THREADS through MAX_THREADS-1.
pub const RESERVED_THREADS: usize = 8;

/// Maximum number of user processes
pub const MAX_PROCESSES: usize = 64;

/// Stack size for reserved system threads (256KB)
///
/// Used for threads 1 through RESERVED_THREADS-1.
/// Handles SSH/HTTP async call chains and the async main loop.
/// 64 KB is sufficient on release (opt-level=3 inlines aggressively, shallow frames).
/// Size profile (opt-level=z, inlining off) has deeper frames on the SSH exec path —
/// observed ELR=0x0 crash (stack overflow → corrupted return addr) at 64 KB.
#[cfg(not(kernel_profile_size))]
pub const SYSTEM_THREAD_STACK_SIZE: usize = 64 * 1024;
// size (non-extreme): 128 KB — the SSH exec path overflowed 64 KB (ELR=0x0).
#[cfg(all(kernel_profile_size, not(kernel_profile_extreme)))]
pub const SYSTEM_THREAD_STACK_SIZE: usize = 128 * 1024;
// extreme: 96 KB. The stack high-water probe (threading::report_stack_high_water)
// measured a true peak of 79 KB across the SSH exec / busybox spawn paths at the
// 6 MB floor, so 96 KB keeps a 17 KB (~21%) margin above observed worst-case while
// reclaiming 32 KB per live system thread (~64 KB at the idle 2-thread floor). The
// stack canary at the base trips first if a deeper path ever exceeds it.
#[cfg(kernel_profile_extreme)]
pub const SYSTEM_THREAD_STACK_SIZE: usize = 96 * 1024;

/// Stack size for user process threads (128KB release, 64KB size profile)
///
/// Used for threads RESERVED_THREADS through MAX_THREADS-1.
/// User processes have their own user-space stack; this is for kernel-side
/// syscall handling only.  tcc's syscall depth is shallow (open/read/write/
/// mmap/brk); 64 KB is sufficient.  Keep `ENABLE_STACK_CANARIES` true so an
/// undersized stack trips a canary rather than corrupting silently.
///
/// Halving the per-slot cost doubles how many user-thread slots fit the same
/// PMM budget, paying for the `reserved + 6` floor in compute_thread_limit.
#[cfg(not(kernel_profile_size))]
pub const USER_THREAD_STACK_SIZE: usize = 128 * 1024;
#[cfg(kernel_profile_size)]
pub const USER_THREAD_STACK_SIZE: usize = 64 * 1024;

/// Enable stack canary checking
///
/// When enabled, canary values are written at the bottom of each thread stack
/// and periodically checked to detect stack overflow.
/// Disable for slightly better performance in production.
pub const ENABLE_STACK_CANARIES: bool = true; // enabled for debugging stack corruption

/// Stack canary value
///
/// Magic value written at the bottom of each stack.
/// If this value is corrupted, stack overflow has occurred.
pub const STACK_CANARY: u64 = 0xDEAD_BEEF_CAFE_BABE;

/// Number of canary words at stack bottom
///
/// More canary words = better detection but more wasted stack space.
/// 8 words = 64 bytes of canary.
pub const CANARY_WORDS: usize = 8;

/// Enable [futex-dbg] trace logging for wait/wake pairs with timestamps.
/// Zero cost when false (LLVM eliminates const-false branches).
pub const FUTEX_DBG_ENABLED: bool = true;

/// Fail tests if test binaries are missing
///
/// When enabled, tests that require binaries (elftest, stdcheck, hello, echo2)
/// will fail if the binary is not found on the filesystem.
/// When disabled, these tests will be skipped with a warning.
///
/// Set to `true` for CI/production builds where all binaries should be present.
/// Set to `false` for development when testing without a fully populated disk.
pub const FAIL_TESTS_IF_TEST_BINARY_MISSING: bool = false;

/// Use cooperative main thread
///
/// When enabled, the main thread (thread 0) runs the async loop directly on the
/// 1MB boot stack. When disabled, it runs on a system thread with 512KB stack.
///
/// RECOMMENDATION: Set to `true` if experiencing stack exhaustion issues.
/// The async main loop pins 6 complex futures (SSH, HTTP, network) which can
/// require significant stack space for deep async call chains.
///
pub const MAIN_THREAD_PRIORITY_BOOST: bool = false; // legacy option, now using proportional scheduler

/// Network polling thread scheduling ratio.
/// The network thread (run_async_main) gets boosted every N scheduler ticks where N = this value.
///
/// Examples:
/// - 2: network thread gets 50% of slots (every other tick) - too aggressive
/// - 4: network thread gets 25% of slots (every 4th tick) - good balance
/// - 8: network thread gets 12.5% of slots - more CPU for userspace
///
/// With 4 concurrent SSH sessions, each userspace thread gets:
/// - ratio=4: (75% / 4) = ~19% CPU each
/// - ratio=8: (87.5% / 4) = ~22% CPU each
///
/// Lower values = better network responsiveness, higher = more CPU for downloads
pub const NETWORK_THREAD_RATIO: u32 = 4;

pub const IGNORE_THREADING_TESTS: bool = false;

/// Disable all tests at boot
///
/// When enabled, skips memory tests, threading tests, filesystem tests,
/// process tests, and shell tests. Use this to debug crashes that might
/// be caused by test-induced heap corruption or thread scheduling issues.
pub const DISABLE_ALL_TESTS: bool = false;

/// Minimal idle loop (for debugging EC=0xe crashes)
///
/// When enabled, the idle loop does nothing but yield. No cleanup, no stats,
/// no prints. Use this to isolate whether the crash is caused by something
/// in the cleanup or stats code vs the core timer/ERET path.
pub const MINIMAL_IDLE_LOOP: bool = false;

/// Skip async network initialization (for debugging crashes)
///
/// When enabled, skips the async network stack and services (SSH, HTTP, etc.).
/// Use this to isolate whether crashes are caused by network code.
pub const SKIP_ASYNC_NETWORK: bool = false;

/// Run network self-tests after initialization
pub const RUN_NETWORK_TESTS: bool = false;

/// Run container isolation tests after initialization
pub const RUN_CONTAINER_TESTS: bool = false;

/// Enable DHCP for automatic IP configuration
pub const ENABLE_DHCP: bool = true;

/// Skip filesystem initialization (for debugging crashes)
///
/// When enabled, skips block device and filesystem init.
/// Use this to isolate whether crashes are caused by fs code.
pub const SKIP_FILESYSTEM_INIT: bool = false;


pub const MEM_MONITOR_PERIOD_SECONDS: u64 = 3;
pub const MEM_MONITOR_ENABLED: bool = true;

/// Enable preemption watchdog
///
/// When enabled, the timer IRQ handler checks if any thread has held
/// preemption disabled for too long and logs a warning.
/// Disable to rule out watchdog as a source of issues.
pub const ENABLE_PREEMPTION_WATCHDOG: bool = true;


/// Enable async process execution with streaming output over SSH
///
/// When enabled, external binaries stream output in real-time to the SSH client
/// instead of buffering all output until command completion. This provides
/// better user experience for long-running commands.
///
/// The streaming implementation uses proper yielding to allow the network runner
/// to transmit packets while the process is running.
pub const ENABLE_SSH_ASYNC_EXEC: bool = true;

// Option to disable copying stdout to kernel log
pub const STDOUT_TO_KERNEL_LOG_COPY_ENABLED: bool = false;

/// Option to disable [syscall] debug prints to the kernel log.
pub const SYSCALL_DEBUG_INFO_ENABLED: bool = false;

/// During `fork`, print a short line to **serial** every 8192 brk pages copied (Go heaps are huge).
/// Independent of `SYSCALL_DEBUG_INFO_ENABLED` so QEMU logs show liveness without log::debug routing.
pub const FORK_BRK_SERIAL_PROGRESS: bool = true;

/// Enable Copy-on-Write fork.  When true, fork shares physical pages read-only
/// instead of copying them.  Write faults allocate new pages on demand.
/// Set to false to fall back to the old eager-copy fork if regressions appear.
pub const COW_FORK_ENABLED: bool = true;

/// Enable the `vfork` fast-path (docs/COW_OPTIMIZATIONS.md Fix B).  When true, a
/// `CLONE_VFORK` child SHARES the parent's address space (no CoW copy, no
/// demote) instead of routing through the full `fork_process` replication — the
/// parent is suspended until the child execs/_exits, so sharing is safe and the
/// child's immediate `exec` discards the shared view without ever copying.
/// Also makes `read_current_pid` resolve identity via THREAD_PID_MAP→tgid so a
/// child sharing the parent's ProcessInfo page still reports its own pid.
/// Set to false to fall back to copy-fork for vfork (clean kill switch).
pub const VFORK_FASTPATH_ENABLED: bool = true;

/// Eager/lazy threshold for **anonymous private** `mmap` (docs/COW_OPTIMIZATIONS.md,
/// "lazy/zero-on-demand population").  An anonymous mapping of more than this many
/// pages is registered as a lazy region and demand-paged (zero-fill on first touch)
/// instead of eagerly allocating + zeroing + mapping every page in the syscall.
///
/// Why a threshold rather than always-lazy (Linux's behaviour): each demand fault
/// is an EL0→EL1 round-trip + `fault_mutex` + a single-page TLB flush, so for a
/// *fully-touched* region eager batching (one PMM-lock alloc, `no_flush` maps, one
/// range TLB flush) is cheaper.  Keeping small mappings eager avoids per-fault
/// overhead on the common 1–8 page case (which dominates by count and frees little
/// memory if deferred); deferring the larger mappings is where the
/// physical-footprint win is — the rustc trace ended at ~3% free RAM because eager
/// mmap commits pages that may never be touched.
///
/// Set high (e.g. 256) to restore the old mostly-eager behaviour.
pub const MMAP_EAGER_MAX_PAGES: usize = 16;

/// Demand-page file-backed `mmap` regions instead of eagerly allocating all
/// pages up front.  When `true`, `mmap(fd, ...)` creates a `LazySource::File`
/// region; pages are faulted in one at a time via `read_at`.
///
/// Default **`false`** on `release` (eager batching is cheaper when pages are
/// all touched).  Default **`true`** on the `size` profile — at 8 MB PMM,
/// eagerly mapping a 600 KB shared library exhausts user pages before the
/// process even starts.
#[cfg(not(kernel_profile_size))]
pub const MMAP_FILE_BACKED_LAZY: bool = false;
#[cfg(kernel_profile_size)]
pub const MMAP_FILE_BACKED_LAZY: bool = true;

/// Kernel heap size override, in **MiB**. `0` = auto-size from detected RAM
/// (see `compute_heap_size` in `src/main.rs`). Set a fixed value to pin the heap
/// — useful for squeezing onto very small machines or reproducing a layout.
/// The auto heuristic already scales down for RAM < 256 MB, so an override is
/// rarely needed.
pub const KERNEL_HEAP_SIZE_MB: usize = 0;

/// Below this detected-RAM threshold (MiB), skip the resource-heavy boot
/// self-tests (parallel multi-process / FP-across-preemption) that need to spawn
/// several processes at once — they can't fit on tiny machines and would halt the
/// boot. Core tests still run. `0` disables the skip (always run everything).
/// See docs/LOW_MEMORY_ENVIRONMENT.md.
pub const LOW_MEM_TEST_SKIP_MB: usize = 32;

/// Override for the number of thread slots that get a stack allocated at boot.
/// `0` = auto-scale from RAM (see `compute_thread_limit` in src/main.rs and
/// docs/LOW_MEMORY_ENVIRONMENT.md). Capped at `MAX_THREADS`. The thread-stack
/// pool comes from PMM, so on tiny machines fewer slots = more usable RAM.
pub const THREAD_LIMIT_OVERRIDE: usize = 0;

/// Emit per-process syscall stats on exit (total + breakdown by category).
///
/// Debug instrumentation: forced `false` on `kernel_profile_extreme`. With both
/// this and `PROC_SYSCALL_LOG_ENABLED` off, `handle_syscall` also skips the
/// per-syscall timing read (see `need_timing` in `src/syscall/mod.rs`).
#[cfg(not(kernel_profile_extreme))]
pub const PROCESS_SYSCALL_STATS: bool = true;
#[cfg(kernel_profile_extreme)]
pub const PROCESS_SYSCALL_STATS: bool = false;

/// Enable per-process syscall ring-buffer log in procfs (/proc/<pid>/syscalls).
///
/// This is the real heap cost in the debug-instrumentation group: a per-process
/// `VecDeque` of up to `PROC_SYSCALL_LOG_MAX_ENTRIES` entries, retained
/// `PROC_SYSCALL_LOG_RETAIN_MS` after the process exits, scaling with process
/// count. Forced `false` on `kernel_profile_extreme` — the recording call in
/// `src/syscall/mod.rs` is gated on this, so the `SYSCALL_LOG` map is never
/// populated (the MAX_ENTRIES / RETAIN_MS knobs below become inert).
#[cfg(not(kernel_profile_extreme))]
pub const PROC_SYSCALL_LOG_ENABLED: bool = true;
#[cfg(kernel_profile_extreme)]
pub const PROC_SYSCALL_LOG_ENABLED: bool = false;

/// Number of most-recent syscall entries to retain per process. Each entry is
/// 32 B, so this caps the ring buffer at `N × 32 B` of heap per live/recently-dead
/// process. 64 keeps the last ~64 syscalls — enough to see the lead-up to a fault
/// — for ~2 KB/process (was 500 → ~16 KB/process, far more history than debugging
/// needs). Only allocated when `PROC_SYSCALL_LOG_ENABLED` (off on extreme).
pub const PROC_SYSCALL_LOG_MAX_ENTRIES: usize = 64;

/// How long (ms) to keep a dead process's log after it exits.
pub const PROC_SYSCALL_LOG_RETAIN_MS: u64 = 10_000; // 10 s

/// Expose SysV IPC message queue state at /proc/sysvipc/msg.
///
/// Forced `false` on `kernel_profile_extreme` — that profile also gates out the
/// `sc-sysv-ipc` syscall family, so the procfs view has nothing to show.
#[cfg(not(kernel_profile_extreme))]
pub const PROC_SYSVIPC_ENABLED: bool = true;
#[cfg(kernel_profile_extreme)]
pub const PROC_SYSVIPC_ENABLED: bool = false;

/// Verbose file I/O logging (openat, read, readv, fstat paths + sizes).
pub const SYSCALL_DEBUG_IO_ENABLED: bool = false;

/// Extended diagnostics for syscalls that return EFAULT/ENOSYS/EINVAL.
///
/// When enabled, the dangerous-errno log line in `handle_syscall` includes
/// the calling thread id, ELR_EL1 of the SVC, and all six argument registers.
/// For `mmap` (nr=222) failures, also decodes the flag bitmask and prints a
/// short reason hint (`len==0`, `fixed+unaligned`, `kernel_va`, or `other`).
///
/// Default `true` while the forktest mmap-stress investigation is active
/// (see docs/GO_FORKTEST_DEBUG.md §E). Set to `false` to revert to the
/// compact one-line format. Forced `false` on `kernel_profile_extreme`
/// (debug instrumentation — trims image size on the shipped low-RAM build).
#[cfg(not(kernel_profile_extreme))]
pub const SYSCALL_ERRNO_DIAG_EXTRA: bool = true;
#[cfg(kernel_profile_extreme)]
pub const SYSCALL_ERRNO_DIAG_EXTRA: bool = false;

/// Log **`read()`** on **pipe reader FDs** (`PipeRead`): pid, tid, fd, pipe id, user
/// buffer pointer and count on each syscall, plus **`validate_user_ptr`** /
/// **`copy_to_user`** failures. Uses **`tprint`** timestamps so serial correlates with
/// **`[signal]`** / mmap lines (`GO_FORKTEST_DEBUG.md`, parent Pattern 2).
///
/// Can be very chatty during **`forktest_parent` + epoll**; set **`false`** once done,
/// or pair with a shorter **`--duration`**.
pub const SYSCALL_DEBUG_PIPE_READ: bool = false;

/// Log one **`[pipe-read]`** line every **N** matching syscalls (1 = every call).
/// Ignored when **`SYSCALL_DEBUG_PIPE_READ`** is **`false`**.
pub const SYSCALL_DEBUG_PIPE_READ_SAMPLE: u64 = 1;

/// When **`true`**, a fatal SIGSEGV whose **`ELR_EL1`** falls in the inclusive range
/// **`DEBUG_SIGSEGV_SYSCALL_STUB_ELIR_MIN`..=`DEBUG_SIGSEGV_SYSCALL_STUB_ELIR_MAX`**
/// logs **`[sigsegv-syscall]`** with **`x8`** (syscall number) and **`x0`–`x5`** — disambiguates
/// **`read`** vs **`epoll_ctl`** vs other syscalls when Go reports **`PC≈0x13060`** (shared syscall
/// trampoline). See **`docs/GO_FORKTEST_DEBUG.md`** (Pattern 2, Agent handoff).
/// Forced `false` on `kernel_profile_extreme` (debug instrumentation).
#[cfg(not(kernel_profile_extreme))]
pub const DEBUG_SIGSEGV_SYSCALL_STUB: bool = true;
#[cfg(kernel_profile_extreme)]
pub const DEBUG_SIGSEGV_SYSCALL_STUB: bool = false;

/// Inclusive minimum user **`ELR_EL1`** for **`[sigsegv-syscall]`** (static **`forktest_parent`**
/// trampoline ~**`0x13060`**; widen if your binary's text mapping differs).
pub const DEBUG_SIGSEGV_SYSCALL_STUB_ELIR_MIN: u64 = 0x10000;

/// Inclusive maximum user **`ELR_EL1`** for **`[sigsegv-syscall]`**.
pub const DEBUG_SIGSEGV_SYSCALL_STUB_ELIR_MAX: u64 = 0x20000;

/// When **`true`**, log **`[pattern2-stub]`** / **`[pattern2-sigreturn]`** only when user **`ELR`**
/// is inside **`DEBUG_SIGSEGV_SYSCALL_STUB_ELIR_*`** (shared Go syscall trampoline window).
/// Chatty if many **`SIGURG`**s hit the stub — enable only while correlating signal delivery vs
/// **`rt_sigreturn`** (`docs/GO_FORKTEST_DEBUG.md` Phase D).
pub const DEBUG_PATTERN2_TRAP_TRACE: bool = false;

/// Verbose network/epoll debugging for bun resolution issues.
/// Logs epoll_pwait returns (compact; see `EPOLL_ZERO_SAMPLE_INTERVAL`), UDP recv/send, and DNS traffic.
pub const SYSCALL_DEBUG_NET_ENABLED: bool = false;

/// Log every Nth `epoll_pwait` with **timeout=0** and **nready=0** (hot spin). Others are suppressed
/// to avoid serial floods; increase for quieter traces, decrease (e.g. 512) while debugging.
pub const EPOLL_ZERO_SAMPLE_INTERVAL: u64 = 64;

/// Option to disable [ext2] debug prints to the kernel log.
pub const DEBUG_EXT2: bool = false;

// ============================================================================
// Network TX Queue Configuration
// ============================================================================

/// Enable TX packet queueing when virtio lock is contended
///
/// When the main network loop can't acquire the virtio lock (held by an SSH
/// session thread), packets would normally be dropped. With this enabled,
/// packets are copied to a pending queue and sent on the next successful
/// lock acquisition.
///
/// This prevents packet loss during lock contention but uses additional memory.
pub const ENABLE_TX_QUEUE: bool = true;

/// Number of pending TX packet slots
///
/// Maximum number of packets that can be queued when the virtio lock is busy.
/// Each slot uses TX_PACKET_BUFFER_SIZE bytes of static memory.
/// Total memory usage: TX_QUEUE_SLOTS * TX_PACKET_BUFFER_SIZE bytes
pub const TX_QUEUE_SLOTS: usize = 8;

/// Size of each TX packet buffer in bytes
///
/// Must be large enough to hold the largest Ethernet frame (1514 bytes)
/// plus any virtio headers. 2048 is a safe default that matches virtio
/// buffer sizes.
pub const TX_PACKET_BUFFER_SIZE: usize = 2048;

// Debug prints
// WARNING: SGI debug prints use format! which can deadlock if the
// allocator lock is held when timer fires. Keep disabled unless debugging.
pub const ENABLE_SGI_DEBUG_PRINTS: bool = false;
pub const ENABLE_IRQ_DEBUG_PRINTS: bool = false;

/// Serial traces around the in-kernel `ps` builtin (`list_processes`) for diagnosing hangs.
pub const SHELL_PS_DEBUG: bool = false;

// Timer interval in microseconds
pub const TIMER_INTERVAL_US: u64 = 10_000;

/// Deferred thread cleanup mode
///
/// When enabled, cleanup_terminated() becomes a no-op except when called from
/// thread 0 (main/boot thread). This serializes all cleanup to a single point,
/// avoiding potential races between cleanup and spawn operations.
///
/// Enable this to debug thread slot synchronization issues.
pub const DEFERRED_THREAD_CLEANUP: bool = true;

/// Minimum time (microseconds) a thread must be TERMINATED before cleanup
///
/// This adds a "cooldown" period after termination to ensure exception handlers
/// and context switches have fully completed before the slot is recycled.
/// Only applies when DEFERRED_THREAD_CLEANUP is enabled.
///
/// 10ms is enough for context switches to complete while not blocking tests.
pub const THREAD_CLEANUP_COOLDOWN_US: u64 = 10_000; // 10ms

pub const THREADING_HEARTBEAT_INTERVAL: u64 = 100000; // 1000 iterations

// ============================================================================
// Herd Process Supervisor Configuration
// ============================================================================

/// Auto-start the herd process supervisor at boot
///
/// When enabled, the kernel will spawn /bin/herd as a userspace process
/// after the network stack is initialized. Herd manages background services
/// defined in /etc/herd/enabled/.
pub const AUTO_START_HERD: bool = true;

// ============================================================================
// Procfs Buffer Size Limits
// ============================================================================

/// Maximum size for per-process stdin buffer in procfs
///
/// When a write to /proc/<pid>/fd/0 would cause the buffer to exceed this
/// limit, the entire buffer is replaced with the new write data.
/// This prevents OOM from runaway stdin input while keeping the most recent data.
///
/// Note: A single write larger than this limit is still accepted in full.
pub const PROC_STDIN_MAX_SIZE: usize = 8 * 1024; // 8KB

/// Maximum size for per-process stdout buffer in procfs
///
/// When a write to /proc/<pid>/fd/1 would cause the buffer to exceed this
/// limit, the entire buffer is replaced with the new write data.
/// This prevents OOM from verbose process output (e.g., CGI scripts) while
/// keeping the most recent output available for reading.
///
/// Note: A single write larger than this limit is still accepted in full.
pub const PROC_STDOUT_MAX_SIZE: usize = 8 * 1024; // 8KB

// ============================================================================
// SSH Server Configuration
// ============================================================================

/// Port for the built-in kernel SSH server
///
/// Default is 22. Set to a different port (e.g., 2222) if running a userspace
/// SSH server like Dropbear on port 22.
pub const SSH_PORT: u16 = 22;

/// Enable userspace SSHD instead of the built-in kernel SSH server.
///
/// When enabled, the kernel will not spawn its internal SSH server thread.
/// The userspace /bin/sshd should be started by /bin/herd instead.
pub const ENABLE_USERSPACE_SSHD: bool = false;

/// Prioritize built-in shell commands over external binaries in SSH shell.
///
/// When false (default), external binaries in /usr/bin and /bin are searched
/// before trying built-in commands. When true, built-ins take precedence.
pub const SSH_BUILT_INS_FIRST: bool = false;

// ============================================================================
// Dynamic Configuration Functions
// ============================================================================

/// Compute user process stack size based on available RAM.
///
/// Returns `USER_STACK_SIZE_OVERRIDE` if non-zero, otherwise scales:
///   - 256 MB RAM → 128 KB (minimum)
///   - 512 MB RAM → 256 KB
///   - 1 GB RAM   → 512 KB  
///   - 2 GB RAM   → 1 MB
///   - 4 GB+ RAM  → 2 MB (maximum)
///
/// The formula is: stack_size = RAM / 2048, clamped to [128KB, 2MB]
pub const fn compute_user_stack_size(ram_size_bytes: usize) -> usize {
    if USER_STACK_SIZE_OVERRIDE != 0 {
        return USER_STACK_SIZE_OVERRIDE;
    }
    
    const MIN_STACK: usize = 128 * 1024;  // 128 KB minimum
    const MAX_STACK: usize = 8 * 1024 * 1024;  // 8 MB maximum
    
    // RAM / 2048 gives us nice scaling:
    // 256 MB / 2048 = 128 KB
    // 512 MB / 2048 = 256 KB
    // 1 GB / 2048 = 512 KB
    // 2 GB / 2048 = 1 MB
    // 4 GB / 2048 = 2 MB
    // 8 GB / 2048 = 4 MB
    // 16 GB / 2048 = 8 MB
    let computed = ram_size_bytes / 2048;
    
    // Clamp to [MIN_STACK, MAX_STACK]
    if computed < MIN_STACK {
        MIN_STACK
    } else if computed > MAX_STACK {
        MAX_STACK
    } else {
        // Round up to nearest 4KB page boundary
        (computed + 0xFFF) & !0xFFF
    }
}
