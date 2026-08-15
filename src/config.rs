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
///
/// This is the **compile-time ceiling**, not the working limit: it sizes the per-slot
/// static arrays (`THREAD_STATES`, `THREAD_CONTEXTS`, the signal/preemption registers,
/// `ThreadPool::{states,sps,stacks}`), which are BSS whether or not a slot is
/// ever used. The *working* limit is chosen at boot from actual RAM by
/// `compute_thread_limit` → `threading::set_thread_limit`, which takes ¼ of user pages
/// and divides by `USER_THREAD_STACK_SIZE` — so thread capacity already scales with
/// memory, and this constant is only the cap that scaling clamps against.
///
/// 64 was that cap until 2026-08-04, and on a `MEMORY=8192` devbox it was the *binding*
/// constraint, not RAM: `compute_thread_limit` produced a far larger figure and clamped.
/// Measured effect — one process could hold 51-52 threads (56 user slots minus herd/
/// httpd/sshd/the SSH session), and a 16-way `pthread_create` load hit genuine
/// exhaustion, because a slot also stays TERMINATED for `THREAD_CLEANUP_COOLDOWN_US`
/// after its thread dies (see `docs/reference/subsystems/thread-lifecycle.md`).
///
/// Raised to 256 for the non-`size` profiles, where a few hundred KB of BSS is free
/// relative to an 8 GB box. `size`/`extreme-size` keep 64: those target a 4 MB RAM floor
/// where per-slot BSS is a real cost and nothing spawns hundreds of threads anyway.
///
/// **Re-exported, not redeclared.** This and `akuma_exec`'s copy were independent literals
/// carrying a "must match" comment; they silently diverged the first time one was raised
/// (boot logged the new limit, `set_thread_limit` clamped to the crate's old one, and the
/// ceiling never moved). The profile split now lives at the definition.
pub use akuma_exec::threading::types::MAX_THREADS;

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
// release: 512 KB (was 64 KB). Bumped while investigating the intermittent
// self-host register/memory corruption (docs/AKUMA_SELF_HOSTING.md §7k): the SSH
// system thread overflowed 64 KB on a deep streaming path (ELR=0x0 / x29→.text
// corruption, §7k.2). 512 KB makes stack overflow implausible on any release path,
// so a *persisting* crash decisively rules overflow out. Lazily allocated — only
// touched pages cost RAM. (extreme keeps its measured tighter size below.)
#[cfg(not(kernel_profile_extreme))]
pub const SYSTEM_THREAD_STACK_SIZE: usize = 512 * 1024;
// extreme: 96 KB. The stack high-water probe (threading::report_stack_high_water)
// measured a true peak of 79 KB across the SSH exec / busybox spawn paths at the
// 6 MB floor, so 96 KB keeps a 17 KB (~21%) margin above observed worst-case while
// reclaiming 32 KB per live system thread (~64 KB at the idle 2-thread floor). The
// stack canary at the base trips first if a deeper path ever exceeds it.
#[cfg(kernel_profile_extreme)]
pub const SYSTEM_THREAD_STACK_SIZE: usize = 96 * 1024;

/// Stack size for user process threads.
///
/// Used for threads RESERVED_THREADS through MAX_THREADS-1.
/// User processes have their own user-space stack; this is for kernel-side
/// syscall handling only.
///
/// Halving the per-slot cost doubles how many user-thread slots fit the same
/// PMM budget, paying for the `reserved + 6` floor in compute_thread_limit.
// release: 512 KB (was 128 KB). Bumped for the §7k stack-overflow experiment —
// rustc worker threads do a deep demand-paging→ext2→readahead chain plus a nested
// IRQ trap frame; 512 KB removes overflow as a variable. Lazily allocated.
#[cfg(not(kernel_profile_extreme))]
pub const USER_THREAD_STACK_SIZE: usize = 512 * 1024;
// extreme: 128 KB (was 64 KB). 64 KB was sized for tcc's shallow syscall depth
// (open/read/write/mmap/brk) back when the deep path — SSH exec / shell spawn —
// ran on an SSH *system* thread and was therefore covered by the 96 KB
// `SYSTEM_THREAD_STACK_SIZE` measured for it. Moving sshd to userspace with
// process-per-session (`userspace/sshd`, `fork-sessions`) moved that same path
// onto a *user* thread: each session is a forked process whose syscalls run on a
// user-thread kernel stack. `report_stack_high_water` measures that path at
// 74 KB (the same probe measured 79 KB when it drove the system stack), so 64 KB
// overflowed it by ~10 KB on every session.
//
// Nothing caught the overflow, because the stack pool comes from the PMM and the
// pages below a stack are ordinary allocations: the run-off wrote into whatever
// happened to sit there. On the extreme profile that was the session process's
// own L3 page table — three PTEs zeroed mid-`sys_spawn` (inside
// `vfs::resolve_symlinks`), unmapping the child's malloc arena, so every ssh
// session died instantly with a SIGSEGV whose faulting address had nothing to do
// with the corruption. That is the failure
// `docs/archive/TRIM_FAT_EMBARASSING_DUPLICATIONS.md` recorded as "ssh
// sessions die instantly on extreme-size, degradation of unknown origin"; see
// `docs/archive/EXTREME_SSHD_KERNEL_STACK_OVERFLOW.md`.
//
// 128 KB rather than 96 KB: the system stack's 96 KB leaves only a 17 KB margin
// over the same measurement, and a user thread additionally carries a nested IRQ
// trap frame. Stacks are allocated on demand here (`WARM_FREE_USER == 0`), so the
// cost is per *live* user thread, not per slot — at the 4 MB floor that is one or
// two threads.
#[cfg(kernel_profile_extreme)]
pub const USER_THREAD_STACK_SIZE: usize = 128 * 1024;

/// Maximum length (bytes) of a single `argv`/`envp` string copied in by
/// `execve`/`spawn`. Linux's `MAX_ARG_STRLEN` is 32 pages (128 KB); cargo/rustc
/// routinely pass multi-KB single arguments (e.g. smoltcp's build-script
/// `--check-cfg 'cfg(feature, values(...))'` is ~5 KB), so the self-host build
/// needs the full Linux cap. The small-memory profiles keep a tight cap to bound
/// the per-arg heap copy — they never run a host toolchain.
///
/// NOTE: exceeding this is a hard `E2BIG` failure of the whole `execve` (matching
/// Linux); it must NOT silently truncate the argument list (a too-short cap used
/// to drop the over-long arg and every arg after it, exec'ing a corrupt argv).
#[cfg(not(kernel_profile_extreme))]
pub const MAX_ARG_STRLEN: usize = 128 * 1024; // Linux MAX_ARG_STRLEN
#[cfg(kernel_profile_extreme)]
pub const MAX_ARG_STRLEN: usize = 4 * 1024;

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
///
/// Default **false**: every futex op otherwise prints to the (slow) serial UART,
/// which is real overhead under futex-heavy workloads — llama.cpp's ggml thread
/// pool issues thousands of wait/wake ops during inference. Flip to `true` only
/// when actively debugging futex wait/wake pairing. (Verified separately that
/// wake delivery is correct and prompt — ~401 µs — see `test_futex_wake_latency_prompt`.)
pub const FUTEX_DBG_ENABLED: bool = false;

/// Track, per thread, the last 16 `FUTEX_WAITERS` transitions it took part in
/// (enqueue / self-remove / popped-by-wake / purged / requeued / park / unpark),
/// and report any thread parked inside `sys_futex` that is **not** queued anywhere
/// (`[FUTEX-ORPHAN]`, printed by `futex_dump`).
///
/// That invariant — "parked in FUTEX_WAIT ⇒ present in `FUTEX_WAITERS`" — is what
/// separates a lost wakeup from an ordinary userspace deadlock, and `[FUTEX-DUMP]`
/// alone cannot check it: it shows who *is* queued, never who *should* be. The
/// per-tid history then names the path that removed the orphan, which is the whole
/// diagnosis (see `docs/runbooks/debug-futex-lost-wakeup.md`).
///
/// Cost when true: two relaxed stores per futex table op, no printing. Cheap enough
/// to leave on — a rustc self-host run issues ~10M futex ops and the added stores do
/// not show up against the syscall entry cost. Compiles out entirely when false.
pub const FUTEX_ORPHAN_DIAG: bool = true;

/// When true, the Thread-0 heartbeat periodically dumps every non-idle thread's
/// saved kernel/user resume point (`[THR-DUMP]`) once `>= 2` threads are WAITING.
/// A deadlock-hunt aid (docs §7g) for locating where parked threads are stuck
/// without SSH (which can itself wedge). Off by default — noisy under normal load.
pub const DEADLOCK_THREAD_DUMP_ENABLED: bool = true;

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

/// EXPERIMENTAL diagnostic (Failure D investigation, 2026-08-07 —
/// docs/archive/J4_WRITE_PERM_FAULT_AND_HALF_WRITTEN_LINKER_OUTPUT.md §7): when
/// true, `schedule_indices` unconditionally prefers any READY thread that has
/// **never once been scheduled** (`LAST_CORE == 0xFF`) over the wakeup-locality
/// hint and the normal round-robin scan. This is a falsification test, not a
/// real fix: if flipping it on makes the `jobserver_stress` barrier hang under
/// `SMP=4` + CPU-hog contention disappear, some newly-`clone()`d threads can be
/// starved indefinitely by the normal scheduler under this load pattern — a
/// thread-admission/fairness bug, not the futex wake-loss bug the rest of that
/// investigation assumed. If the hang persists unchanged, this rules the theory
/// out cleanly. Bypasses `round_robin_idx` bookkeeping on the fast path, so it
/// is not itself fair — leave `false` outside this experiment.
pub const PRIORITIZE_NEVER_SCHEDULED: bool = false;

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

/// Run `test_forktest_parent_mmap` in the process-tests boot suite.
///
/// Off by default: the test runs for up to 60s (regresses the Go
/// mmap-under-fork SIGSEGV), too slow for every boot. Flip to `true` to run
/// it deliberately (e.g. before/after touching CoW fork or mmap-region code).
pub const RUN_SLOW_FORKTEST_PARENT_MMAP: bool = false;

/// Enable DHCP for automatic IP configuration
pub const ENABLE_DHCP: bool = true;

/// Skip filesystem initialization (for debugging crashes)
///
/// When enabled, skips block device and filesystem init.
/// Use this to isolate whether crashes are caused by fs code.
pub const SKIP_FILESYSTEM_INIT: bool = false;


pub const MEM_MONITOR_PERIOD_SECONDS: u64 = 3;
pub const MEM_MONITOR_ENABLED: bool = false;

/// Gate the per-fault demand-paging trace — today the `[IA-DP] file region:` line
/// on the instruction-abort path, which fires on every file-backed demand page.
///
/// This const had **no reader anywhere in the tree** until 2026-08-08: it was
/// defined here and documented in `config-flags.md`, and gated nothing. Meanwhile
/// the line above printed unconditionally and was the single largest source of
/// serial traffic under load — 34.7k lines in one `-j4` self-host build sample.
///
/// Its old docstring claimed to gate `[DA-DP]` / `[DP]` / `[DP-eager]`. Those do
/// exist, but they are *anomaly* lines (readahead pool exhausted, single-page
/// fallback OOM, anon alloc failed, lazy/eager region miss, kernel-VA fault), and
/// anomaly lines should not be switchable off — they stay unconditional. Only the
/// success-path trace is gated here.
pub const DEMAND_PAGE_LOG_ENABLED: bool = false;

/// Gate the routine pipe lifecycle trace (`[pipe] create` / `clone_ref` /
/// `close_write` / `close_read`).
///
/// Several lines per pipe, and a parallel build makes thousands of them — 6.6k
/// lines in one `-j4` build sample, second only to the demand-paging trace. The
/// refcount lines are how the SIGPIPE/close-ordering deadlocks were cracked, so
/// they stay one flag away rather than deleted. `WARN` and `DESTROY` are NOT gated.
pub const PIPE_TRACE_ENABLED: bool = false;

/// Gate the per-call memory-syscall trace (`[mmap]` / `[mprotect]` / `[munmap]`).
///
/// One line per call, on every call. That is affordable for a single process and
/// ruinous for a parallel build: an in-VM `-j4` self-host build emitted **68 MB of
/// serial output in 20 minutes** (~270 KB/s) through the one shared console, which
/// turned a ~10-minute build into well over an hour and put four cores in
/// contention for the console lock. The trace was unconditional until 2026-08-08.
///
/// Turn it on when working on mmap/mprotect/munmap themselves; leave it off for
/// anything throughput- or timing-sensitive. Failures and anomalies (`EINVAL`,
/// region-bookkeeping complaints) are NOT gated by this — they stay visible.
pub const MEM_SYSCALL_TRACE_ENABLED: bool = false;

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
///
/// Also gates the fork/exec/thread-spawn `[FORK-DBG]`/`[TRAMP]` lifecycle
/// traces (`akuma_exec::process::lifecycle_trace`). Those were unconditional
/// and cost ~20 serial lines per `fork()`, 5 per `execve`, and 2 per thread
/// spawn — enough to dominate the log and shift the timing of the paths they
/// trace under an in-VM `-j4` build.
pub const SYSCALL_DEBUG_INFO_ENABLED: bool = false;

/// Print a `[TMR] t=… T=… p=… f=…` scheduler heartbeat from the timer IRQ every
/// 1000 ticks — and every **100** while any fork is in progress. Left over from
/// the fork-wedge investigations; `[Heartbeat] … SmolNet Active` already gives
/// liveness, and under `-j4` a fork is almost always in progress, so the ramped
/// rate is pure noise on the serial line.
pub const TIMER_TICK_HEARTBEAT: bool = false;

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

/// Share one physical frame between every read-only mapper of the same file page,
/// keyed on `(inode, file_offset)`, instead of giving each process a private copy.
///
/// This is the fix for "`-j4` is slower than `-j1`" on the self-host build: four
/// concurrent `rustc`s mapping the same 295 MB `librustc_driver.so` used to hold
/// four physical copies filled by four separate `read_at` sweeps, which pushed the
/// PMM into `reclaim_clean_file_pages` and turned every eviction into a re-read.
///
/// Only mappings that give EL0 no write access are shared, so writable data
/// segments keep their private-copy semantics untouched. Set to false to restore
/// per-process private file pages (clean kill switch — use it to A/B).
/// See `crate::file_page_cache`.
pub const SHARED_FILE_PAGES_ENABLED: bool = true;

/// **Diagnostic, OFF by default.** On every `file_page_cache` *hit*, re-read the page
/// from disk and compare it against the cached frame before mapping it, printing
/// `[FPC-BAD]` with the `(inode, file_off)` and the first differing byte on mismatch.
///
/// This is the decisive instrument for "a mapped file reads back as zeros": it answers
/// *is the cache serving bytes that do not match the file* directly, rather than by
/// inference. It costs a block read per hit — i.e. it throws away the entire point of
/// the cache — so it is for investigation runs only, never a shipping default.
///
/// A mismatch names the poisoned key. **No mismatch across a failing build is equally
/// decisive**: it clears the cache and moves the search to the install pass or the
/// mapping itself. Counter: `pmm::DP_FILE_CACHE_MISMATCH`.
pub const FPCACHE_VERIFY_HITS: bool = false;

/// Let a `pthread_kill` (`tkill`/`tgkill`) signal interrupt a blocking syscall
/// with `EINTR`, per Linux semantics.
///
/// Before this, `EINTR` was reported only from `ProcessChannel::is_interrupted`,
/// which is set solely by Ctrl-C and `sys_kill` — so a signal pended on a single
/// thread woke it but could never break it out of a blocking loop.
/// `SA_RESTART` handlers are still never interrupted (the loop just takes another
/// pass, which is exactly a restart), so Go's SIGURG preemption is unaffected.
///
/// Set to false to restore the old Ctrl-C-only behavior (clean kill switch —
/// use it to A/B any regression in blocking-syscall behavior).
/// See `akuma_exec::process::should_interrupt_blocking_syscall`.
pub const PTHREAD_KILL_EINTR_ENABLED: bool = true;

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
/// Demand-page file-backed `mmap` regions (1 MB readahead per fault) instead of
/// eagerly reading every mapped page up front.
///
/// **`true`** on all profiles. `size`/`extreme` always needed it (eagerly mapping
/// even a 600 KB library exhausts user pages at 8 MB PMM). `release` was eager
/// until an A/B on the apk rustc toolchain showed lazy is **1.8× faster** full
/// compile / **6.9× faster** process startup: eager read all ~240 MB of
/// `libLLVM`+`librustc_driver` at `mmap()` time even though rustc touches only a
/// fraction (docs/AKUMA_SELF_HOSTING.md §7a). Eager would only win for workloads
/// that touch *every* mapped page (e.g. fully-read model weights).
pub const MMAP_FILE_BACKED_LAZY: bool = true;

/// Kernel heap size override, in **MiB**. `0` = auto-size from detected RAM
/// (see `compute_heap_size` in `src/main.rs`). Set a fixed value to pin the heap
/// — useful for squeezing onto very small machines or reproducing a layout.
/// The auto heuristic already scales down for RAM < 256 MB, so an override is
/// rarely needed.
pub const KERNEL_HEAP_SIZE_MB: usize = 0;

/// Clamp (in **MiB**) on the RAM size used to compute the kernel's *own*
/// reserves — `code_and_stack` (the `ram/16` term) and the auto heap size. The
/// user-page pool, PMM, thread limit and user-stack sizing always use the REAL
/// detected RAM. `0` = no clamp (reserves scale with real RAM, the historical
/// behaviour).
///
/// On the **extreme** profile we clamp to 4 MiB: the kernel boots and idles in
/// ~2 MB of code+stack and a 512 KB seed heap (which grows on demand from PMM),
/// so giving the box more RAM no longer inflates kernel overhead — the surplus
/// all lands in the user-page pool. This is what lets a 64 MB box hand ~62 MB
/// (instead of 52 MB) to userspace for LLM weights (e.g. llama.cpp models).
/// The kernel still *sees* and uses the real RAM; only its internal reserve math
/// is pinned to the small-machine numbers it was tuned for.
#[cfg(kernel_profile_extreme)]
pub const MEM_CALC_CLAMP_MB: usize = 4;
#[cfg(not(kernel_profile_extreme))]
pub const MEM_CALC_CLAMP_MB: usize = 0;

/// Floor (bytes) on the `code_and_stack` region — guarantees the kernel binary
/// always fits even when `ram/16` and the boot-stack cover are both small. On
/// **extreme** this is 0: the boot-stack cover is the effective floor, handing
/// the slack back to the user-page pool (lets repeated tcc fit below 6 MB).
/// Consumed by `crate::compute_memory_layout`.
#[cfg(kernel_profile_extreme)]
pub const MIN_CODE_AND_STACK_BYTES: usize = 0;
#[cfg(not(kernel_profile_extreme))]
pub const MIN_CODE_AND_STACK_BYTES: usize = 4 * 1024 * 1024;

/// Slack (bytes) reserved ABOVE the boot-stack top before the heap. The boot
/// stack grows DOWN (away from the heap), so this is pure paranoia — the boot
/// layout sanity guard enforces `boot_stack_top <= heap_start` unconditionally.
/// **extreme** trims it to 64 KB (vs 1 MB) to free user pages.
#[cfg(kernel_profile_extreme)]
pub const STACK_GUARD_BYTES: usize = 64 * 1024;
#[cfg(not(kernel_profile_extreme))]
pub const STACK_GUARD_BYTES: usize = 1024 * 1024;

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

/// Poison-and-quarantine freed physical frames to catch use-after-free writes
/// (`pmm::quarantine_push`).
///
/// On for the cargo null-`Rc` investigation (docs/archive/CARGO_HEAP_NULL_RC.md):
/// that defect's signature is a heap qword reading back as zero, which is what a
/// frame handed to the PMM while a process still maps it looks like once the next
/// `alloc_page_zeroed` wipes it. With this on, the still-mapped owner's next write
/// lands on poison instead, and `[PMM-UAF]` names the frame and the pid that freed
/// it — at a bounded distance from the cause instead of minutes downstream.
///
/// Costs a 4 KiB store per `free_page` and holds back 512 frames (2 MiB); the
/// hold-back is surrendered on the first allocation failure
/// (`pmm::quarantine_drain_all` sits on the pressure ladder), so it cannot cause
/// an OOM. Forced `false` on the low-RAM profiles, where 2 MiB is a real fraction
/// of RAM and the debug cost is not wanted in a shipped image.
#[cfg(not(kernel_profile_extreme))]
pub const PMM_UAF_QUARANTINE: bool = true;
#[cfg(kernel_profile_extreme)]
pub const PMM_UAF_QUARANTINE: bool = false;

/// At every `free_page` that drops the **last** reference, check whether a live
/// address space still tracks the frame (`pmm::report_premature_free`).
///
/// This is the direct form of the question `PMM_UAF_QUARANTINE` can only answer
/// indirectly. The quarantine catches a *write* through a mapping that outlived
/// its free; the null-`Rc` defect's fatal access is a **read** — a poisoned qword
/// loaded as a pointer — which leaves contents intact and is invisible to it
/// (`docs/archive/CARGO_NULL_RC_MEMORY_REFERENCE_AUDIT.md` §13.8.2). This asks
/// instead, at the moment of the free, whether anyone still holds the frame, and
/// reports the freeing thread, the surviving process and the CoW history together.
///
/// **Off by default because it perturbs the race it hunts.** It costs a scan of
/// the process table plus a `BTreeMap` lookup per active address space, on every
/// frame freed — milliseconds added to each teardown. Armed, the cold-build loop
/// went 10/10 green against a 25 % crash baseline (`p ≈ 0.056` of happening by
/// chance), i.e. the instrument most likely hid the defect.
///
/// Prefer `pmm::report_poison_value`, wired into the fault path: it decodes a
/// faulting value that is a quarantine poison word back to the frame it belonged
/// to and names the thread that freed it, for zero steady-state cost. Turn this on
/// only to catch a premature free whose victim never faults.
pub const PMM_PREMATURE_FREE_CHECK: bool = false;

/// Record every CoW/share refcount increment and decrement in a ring, so an
/// anomaly can print a frame's whole reference history (`pmm::print_cow_history`).
///
/// `COW_REFCOUNTS` decides when a frame is freed, and the `EAGER-UPGRADE` anomaly
/// is a page whose count reached **0** while its owner still maps it read-only —
/// one decrement more than there were shares. The counter alone cannot say who
/// over-decremented it; the history can, and it names the thread behind each
/// event. See docs/archive/CARGO_HEAP_NULL_RC.md.
///
/// ~98 KB of BSS and two relaxed stores per refcount operation, so it is gated off
/// on the low-RAM profiles alongside the quarantine.
#[cfg(not(kernel_profile_extreme))]
pub const COW_REF_LEDGER: bool = true;
#[cfg(kernel_profile_extreme)]
pub const COW_REF_LEDGER: bool = false;

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

/// Master switch for the EFAULT/ENOSYS/EINVAL dangerous-errno log line itself
/// (see `SYSCALL_ERRNO_DIAG_EXTRA` above for its format, not whether it fires).
///
/// Some callers legitimately hit these errnos at high volume with no bug
/// involved — e.g. `readlinkat` on a real (non-symlink) path correctly
/// returns `EINVAL` per POSIX, and cargo/rustc probe "is this a symlink?" on
/// every file of every extracted crate during a build, which floods this at
/// tens of thousands of lines/build (docs/archive/SELFHOST_DEVBOX_SMOLTCP.md).
/// Flip to `false` to silence it (e.g. for a quieter self-host build run);
/// leave `true` to keep the WILD-DA-crash diagnostic live for the general case.
pub const SYSCALL_ERRNO_DIAG_ENABLED: bool = false;

/// Stale-instruction-cache **spurious-SVC** guard (§7k.4 root cause).
///
/// At an `EC_SVC64` trap the CPU sets `ELR_EL1` to the instruction *after* the
/// `svc`, so the instruction at `ELR-4` **must** be an `svc`
/// (`0xD4000001` under mask `0xFFE0001F`). Reads of user memory are
/// cache-coherent (they see the real bytes, not the I-cache), so if `ELR-4`
/// reads back as anything *other* than `svc`, the CPU executed a **stale
/// I-cache `svc`** at a PC whose backing memory is no longer an `svc` — a
/// spurious syscall that, when dispatched, returns an errno into `x0` and
/// clobbers the live register the real (non-`svc`) instruction expected. This
/// is the long-open intermittent rustc self-host `[WILD-DA]`
/// (`wait4(95)`/`futex` with pointer args → `EFAULT`/`ENOSYS` → `str [x0]`).
///
/// When `true`, such a spurious SVC is detected at entry, the I-cache is
/// flushed (`ic iallu`), `ELR` is backed up by 4, and the syscall is **not**
/// dispatched — so the real instruction re-executes with registers intact.
/// Reads 4 bytes of user code per syscall; forced `false` on the low-RAM
/// `extreme` profile.
#[cfg(not(kernel_profile_extreme))]
pub const VERIFY_SVC_AT_ENTRY: bool = true;
#[cfg(kernel_profile_extreme)]
pub const VERIFY_SVC_AT_ENTRY: bool = false;

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

/// Minimum time (microseconds) a reaped process's memory sits RETIRED before
/// `process::reclaim_retired_processes` frees it. Must outlast any BKL-dropped
/// window (no-bkl-vfs/no-bkl-mm/no-bkl-process) that could still hold a raw
/// pointer to it — see `unregister_process`'s doc comment and
/// docs/archive/BKL_PHASE7E_PROCESS_TABLE_RECLAIM.md. Same order of magnitude as
/// THREAD_CLEANUP_COOLDOWN_US since the windows being outlasted are the same kind.
pub const PROCESS_RECLAIM_COOLDOWN_US: u64 = 10_000; // 10ms

pub const THREADING_HEARTBEAT_INTERVAL: u64 = 50_000_000; // BSP idle heartbeat print cadence (raised to keep serial from throttling the guest)

// ============================================================================
// Herd Process Supervisor Configuration
// ============================================================================

/// Auto-start the herd process supervisor at boot
///
/// When enabled, the kernel will spawn /bin/herd as a userspace process
/// after the network stack is initialized. Herd manages background services
/// defined in /etc/herd/enabled/.
/// On by default. The one exception is the 4 MB extreme profile once the
/// built-in SSH server is compiled out (`userspace-sshd`): there herd plus its
/// service tree costs more RAM than the box has to spare, and the only service
/// it would start is `/bin/sshd`, which [`AUTO_START_SSHD`] starts directly.
pub const AUTO_START_HERD: bool =
    !(cfg!(kernel_profile_extreme) && cfg!(feature = "userspace-sshd"));

/// Login shell handed to the kernel-spawned `/bin/sshd`.
///
/// `/bin/paws` is the first-party mini-shell (`userspace/paws`, 7 mapped pages)
/// rather than busybox's `/bin/sh` (265 pages): on a 4 MB box busybox alone is
/// ~1.8x the shared file-page dedup cache, so concurrent shells stop sharing
/// text. See docs/archive/FPCACHE_UNDERSIZED_AT_LOW_RAM.md.
pub const USERSPACE_SSHD_SHELL: &str = "/bin/paws";

/// Spawn `/bin/sshd` straight from the kernel, with no supervisor.
///
/// Only meaningful when there is no built-in SSH server (`userspace-sshd`) and
/// herd is not running — otherwise herd owns service startup and starting a
/// second sshd would collide on the listening port. This is what keeps a
/// herd-less image reachable.
pub const AUTO_START_SSHD: bool = cfg!(feature = "userspace-sshd") && !AUTO_START_HERD;

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
///
/// Driven by the `userspace-sshd` cargo feature (off by default → the normal
/// image keeps its built-in SSH). The devbox turns it on: with rump the default
/// stack, the built-in server (smoltcp-only) would be the sole thing left on the
/// native stack, so it must not start.
pub const ENABLE_USERSPACE_SSHD: bool = cfg!(feature = "userspace-sshd");

/// Prioritize built-in shell commands over external binaries in SSH shell.
///
/// When false (default), external binaries in /usr/bin and /bin are searched
/// before trying built-in commands. When true, built-ins take precedence.
pub const SSH_BUILT_INS_FIRST: bool = false;

// ============================================================================
// Rump sysproxy (networking) Configuration
// ============================================================================

/// Emit the verbose per-syscall `[RUMP-SP]` proxy trace (the `route …`, per-op
/// latency, `connect`/`accept`/`recvmsg` lines in `rump_proxy.rs`).
///
/// Off by default: it prints a line for every proxied socket syscall, which floods
/// the console under any real network load (e.g. an SSH session or `curl`). The
/// one-time lifecycle lines (`box … marked stack=rump`, `proxy ready`/`handshake
/// failed`, the rump-default bring-up) print regardless. Flip to `true` to debug
/// the sysproxy path.
pub const RUMP_SP_TRACE: bool = false;

// ============================================================================
// Syscall trace prints (high-volume; off by default outside active debugging)
// ============================================================================

/// Emit the `[munmap] pid=… addr=… (N pages, M owned, base=…)` trace on every
/// `munmap()` syscall (`src/syscall/mem.rs`), plus its shared-writeback
/// counterpart.
///
/// Off by default: a single process can issue thousands of small `munmap()`
/// calls during exit/teardown (observed ~2,900/exit during a self-host cargo
/// build), each paying a `tprint!` — see
/// docs/archive/SELFHOST_DEVBOX_SMOLTCP.md, where this was 35% of
/// total log volume. Flip to `true` to debug region unmap/frame-free issues.
pub const TRACE_MUNMAP: bool = false;

/// Emit the `[signal] tkill(tid=…, sig=…)` trace on every `tkill()` syscall
/// (`src/syscall/signal.rs`).
///
/// Off by default: some userspace runtimes (observed with rustc/musl on
/// thread/process startup) call `tkill` on themselves in fixed-size retry
/// bursts (100 calls/spawn, harmless but noisy) — see
/// docs/archive/SELFHOST_DEVBOX_SMOLTCP.md. Flip to `true` to
/// debug signal delivery.
pub const TRACE_TKILL: bool = false;

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
