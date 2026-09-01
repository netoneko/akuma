#![no_std]
// This crate carries `#![forbid(unsafe_code)]` as of 2026-09-01.
//
// It was created as "the unsafe half of the bin crate" and held four things the
// `unsafe_code` lint rejects. Each went to the crate that owns what it pokes,
// which is the same move `src/syscall/` made:
//
//   * boot assembly + the secondary trampoline + the `#[unsafe(no_mangle)]`
//     symbols assembly branches to  ->  `akuma-entry`
//   * `linker.ld`'s absolute image/stack symbols  ->  `akuma-entry::linker_syms`,
//     as safe accessors (reading a linker symbol's *address* never needed
//     `unsafe`; only naming it did)
//   * `akuma_fdt::locate`'s raw boot-time read  ->  `akuma_mmu::with_boot_identity_fdt`,
//     which maps and then *checks* the block instead of vouching for it
//
// `console.rs` and `platform.rs` moved DOWN to `akuma-kernel-core` at the same
// time. Neither held any `unsafe`; `console.rs` carried its own module-level
// `#![forbid(unsafe_code)]`, which is why `scripts/cloc_akuma.py` used to report
// this crate as forbidding while it still contained boot assembly (the script
// marks a crate when ANY file in it carries the attribute).
//
// `forbid`, not `deny`: it cannot be switched back off by a local
// `#[allow(unsafe_code)]`. If something here needs a genuinely unsafe operation,
// put it behind a named function in the crate that owns the state it touches and
// state the obligation there — do not reach for an allow.
#![forbid(unsafe_code)]
//! Extracted from `src/main.rs`, `src/boot.rs`, `src/console.rs`,
//! `src/platform.rs`, `src/smp_shared.rs`, `src/rump_proxy.rs`,
//! `src/syscall.rs` and `src/vfs.rs` on 2026-09-01 — the second half of the
//! `src/` extraction `akuma-kernel-core` started. `src/main.rs` keeps only
//! what has to live in the actual bin crate: the `#![no_std]`/`#![no_main]`
//! attributes, `#[global_allocator]`, `#[alloc_error_handler]`,
//! `#[panic_handler]`, and the `rust_start` entry stub the boot assembly
//! `bl`s — plus the boot self-test suite, which stays in `src/` per the
//! original request.
//!
//! Every module below is re-exported back into `src/main.rs`'s crate root
//! (`pub use akuma_kernel_glue::x;`), the same trick `akuma-kernel-core` uses,
//! so the ~thousands of `crate::x::y` call sites in the test files — which
//! are descendants of the BIN crate's root, not this one — resolve unchanged.
//! `kernel_main`, `halt`, and the memory-layout helpers
//! (`compute_heap_size`/`compute_memory_layout`/`reserve_calc_ram`/
//! `compute_thread_limit`/`MemoryLayout`) were `pub(crate)` or private in
//! `main.rs`, where crate-root privacy made them visible to the test modules
//! for free; moved here they need to be `pub` and explicitly re-exported by
//! name, since this crate's `pub(crate)` means "private to
//! `akuma-kernel-glue`", not "private to the bin".

extern crate alloc;

pub use akuma_primitives::{safe_print, tprint};

// Same aliasing `src/main.rs` does for its own copy: `#[global_allocator]`
// stays in the bin crate (a library installing it would fight std in a host
// test binary), but `kernel_main` and friends still need `allocator::stats()`
// &c. under the name they were spelled with as crate-root siblings.
pub use akuma_alloc as allocator;
// Ditto for the exception-path registration calls in `kernel_main`.
#[cfg(target_os = "none")]
use akuma_exceptions as exceptions;

// The leaf re-exports `akuma-kernel-core` itself provides (config, fs, vfs
// glue, timer, etc.) plus the crate-root items only `akuma-kernel-core`
// needed bumped to `pub` for (`akuma`, `bkl_profile`, ...). Blanket
// re-exported here too so `crate::x` inside the modules below — moved
// verbatim from `main.rs`, where they were already spelled that way — keeps
// resolving without editing every call site.
pub use akuma_kernel_core::akuma;
#[cfg(feature = "bkl-profile")]
pub use akuma_kernel_core::bkl_profile;
pub use akuma_kernel_core::config;
pub use akuma_kernel_core::file_page_cache;
pub use akuma_kernel_core::fs;
pub use akuma_kernel_core::irq;
pub use akuma_kernel_core::klog;
#[cfg(feature = "net-profile")]
pub use akuma_kernel_core::nic_profile;
pub use akuma_kernel_core::ntp_boot;
pub use akuma_kernel_core::pmm;
// `akuma-timer`'s hardware-register functions are `#[cfg(target_os = "none")]`
// (see `akuma-kernel-core`'s own matching gate on this same module), so this
// re-export — and everything below it that touches real hardware — has to be
// gated the same way for a host `cargo test`/`cargo clippy` of this crate to
// compile at all.
#[cfg(target_os = "none")]
pub use akuma_kernel_core::timer;
#[cfg(target_os = "none")]
pub use akuma_kernel_core::timer::GLOBAL_POLL_STEP;

// akuma-virtio re-exports. `block`/`rng`/`audio` are ALSO re-exported
// separately at the bin crate's own root (`src/main.rs`) because
// `process_tests.rs` reaches them via `crate::block`/`crate::rng`/
// `crate::audio` as a descendant of the bin crate, not this one — two
// independent bindings to the same upstream crate, not a conflict.
pub use akuma_virtio::{audio, block, rng};

// `boot`/`smp_shared` carry `global_asm!` blocks using GNU-as/ELF section
// directives (`.section .text.boot`, `#[unsafe(link_section = ".data.boot")]`)
// that a Mach-O host toolchain rejects outright, and `console`/`platform`/
// `rump_proxy`/`syscall`/`vfs` all reach `boot`/`smp_shared`/`timer` (directly
// or transitively), so the whole group has to move together behind
// `target_os = "none"`. `akuma-gic`, by contrast, compiles fine on host: raw
// MMIO reads/writes are just pointer dereferences, portable to any target —
// it is specifically the asm section directives here that are not.
#[cfg(target_os = "none")]
pub use akuma_entry::boot;
#[cfg(target_os = "none")]
pub use akuma_entry::linker_syms;
#[cfg(target_os = "none")]
pub use akuma_kernel_core::console;
#[cfg(target_os = "none")]
pub use akuma_kernel_core::platform;
#[cfg(all(target_os = "none", feature = "rump"))]
pub mod rump_proxy;
#[cfg(all(target_os = "none", kernel_smp_shared))]
pub use akuma_entry::smp_shared;
#[cfg(target_os = "none")]
pub mod syscall;
#[cfg(target_os = "none")]
pub mod vfs;

#[cfg(target_os = "none")]
use akuma_exec::{mmu, process, threading};

#[cfg(target_os = "none")]
pub fn halt() -> ! {
    // Discarded deliberately: on success this does not return, so a status can
    // only mean "no PSCI conduit" — which the `wfi` below already handles.
    let _ = akuma_psci::call(akuma_psci::SYSTEM_OFF, 0, 0, 0);
    loop {
        akuma_cpu::park::wfi();
    }
}
#[cfg(target_os = "none")]
fn detect_memory(fdt: Option<&akuma_fdt::Fdt<'_>>) -> (usize, usize) {
    const DEFAULT_RAM_BASE: usize = 0x4000_0000; // QEMU virt: 1 GB

    const DEFAULT_RAM_SIZE: usize = 256 * 1024 * 1024;
    const DTB_RESERVE: usize = 2 * 1024 * 1024; // 2 MB

    let Some(fdt) = fdt else {
        console::print("[Memory] No DTB found, using default 256MB\n");
        return (DEFAULT_RAM_BASE, DEFAULT_RAM_SIZE - DTB_RESERVE);
    };

    // Get memory regions from DTB
    let memory = fdt.memory();
    if let Some(region) = memory.regions().next() {
        let base = region.starting_address as usize;
        let ram_size = region.size.unwrap_or(DEFAULT_RAM_SIZE);
        
        console::print("[Memory] Detected from DTB: base=0x");
        console::print_hex(base as u64);
        console::print(", size=");
        console::print_dec(ram_size / 1024 / 1024);
        console::print(" MB\n");
        (base, ram_size)
    } else {
        console::print("[Memory] No memory region in DTB, using defaults\n");
        (DEFAULT_RAM_BASE, DEFAULT_RAM_SIZE - DTB_RESERVE)
    }
}

/// Decide the kernel heap size (bytes) for a given RAM size and code+stack reserve.
///
/// Pure function so it can be unit-tested without booting (see
/// `tests::test_compute_heap_size`).
///
/// - `config::KERNEL_HEAP_SIZE_MB != 0` → use that fixed value (manual override).
/// - **RAM ≥ 256 MB** → the historical generous heap: `1/8 of RAM, clamped to
///   [64 MB, 256 MB]`. Unchanged so the common 256 MB+ configs and memory-hungry
///   workloads (go build, bun, rustc metadata) behave exactly as before.
/// - **RAM < 256 MB** → scale down: target `1/8 of RAM` with an 8 MB floor (the
///   kernel boots using only ~2 MB of heap), but **never more than half of the
///   memory left after code+stack**, so user pages always survive. The old code
///   used a flat 64 MB floor here, which left 0 user pages below ~72 MB (no boot)
///   and starved user RAM at 128 MB.
///   Kernel physical-RAM layout: three contiguous regions starting at `ram_base`
///   — `[.. heap_start)` code+boot-stack, `[heap_start ..)` heap, then user pages.
pub struct MemoryLayout {
    pub code_and_stack: usize,
    pub heap_start: usize,
    pub heap_size: usize,
    pub user_pages_start: usize,
    pub user_pages_size: usize,
}

/// Compute the kernel memory layout for a detected RAM region.
///
/// All profile-specific policy lives in `config` (`MIN_CODE_AND_STACK_BYTES`,
/// `STACK_GUARD_BYTES`, `MEM_CALC_CLAMP_MB`) rather than inline `#[cfg]` in the
/// boot path, so the layout is one pure, unit-testable function
/// (`tests::test_compute_memory_layout`).
///
/// CRITICAL: `code_and_stack` must cover the *boot stack* (`boot_stack_top` =
/// the STACK_TOP linker symbol, absolute VA of the initial SP), or the heap is
/// placed atop the live boot stack and the allocator hands out the stack's own
/// pages under pressure — kernel-corrupts-kernel (the EC=0x21/0x22 crash; see
/// docs/STACK_CORRUPTION_ANALYSIS.md). The boot layout sanity guard in
/// `kernel_main` re-verifies the result before any allocation.
///
/// `calc_ram` (see `reserve_calc_ram`) sizes only the kernel's OWN reserves;
/// `user_pages_size` is always carved from the REAL `ram_size`.
#[must_use]
pub fn compute_memory_layout(
    ram_base: usize,
    ram_size: usize,
    boot_stack_top: usize,
) -> MemoryLayout {
    let stack_cover = (boot_stack_top - ram_base) + config::STACK_GUARD_BYTES;
    let calc_ram = reserve_calc_ram(ram_size, config::MEM_CALC_CLAMP_MB);
    let code_and_stack = core::cmp::max(
        core::cmp::max(calc_ram / 16, config::MIN_CODE_AND_STACK_BYTES),
        stack_cover,
    );
    let heap_start = ram_base + code_and_stack;
    let heap_size = compute_heap_size(calc_ram, code_and_stack);
    let user_pages_start = heap_start + heap_size;
    let user_pages_size = ram_size.saturating_sub(code_and_stack + heap_size);
    MemoryLayout {
        code_and_stack,
        heap_start,
        heap_size,
        user_pages_start,
        user_pages_size,
    }
}

/// RAM size used to compute the kernel's own reserves (code+stack, heap).
///
/// `clamp_mb == 0` → return the real `ram_size` (historical behaviour, used on
/// release/size). Otherwise cap at `clamp_mb` MiB, so on a big box the kernel's
/// reserve math stays pinned to the small-machine numbers it was tuned for and
/// the surplus RAM flows to the user-page pool. See `config::MEM_CALC_CLAMP_MB`.
/// Pure so it can be unit-tested (see `tests::test_reserve_calc_ram`).
#[must_use]
pub fn reserve_calc_ram(ram_size: usize, clamp_mb: usize) -> usize {
    if clamp_mb != 0 {
        core::cmp::min(ram_size, clamp_mb * 1024 * 1024)
    } else {
        ram_size
    }
}

#[must_use]
pub fn compute_heap_size(ram_size: usize, code_and_stack: usize) -> usize {
    const MB: usize = 1024 * 1024;
    if config::KERNEL_HEAP_SIZE_MB != 0 {
        return config::KERNEL_HEAP_SIZE_MB * MB;
    }
    if ram_size >= 256 * MB {
        (ram_size / 8).clamp(64 * MB, 256 * MB)
    } else {
        // Small RAM. The kernel boots on ~2.2 MB of heap. Thread stacks are NOT
        // in the heap (they come from PMM), so the heap doesn't have to cover
        // them — keeping it small leaves more user pages for the thread pool +
        // processes.
        //
        // On the `size` profile (small-RAM target) we drop the floor to 4 MB:
        // that frees 4 MB that would otherwise be wasted on heap that the kernel
        // doesn't use, and on a 24 MB box nearly doubles the user-page pool
        // (5 MB → 9 MB), which is the difference between tcc's ELF load
        // failing and fitting.  On release we keep the 8 MB floor for headroom.
        //
        // For RAM >= 128 MB, ram/8 dominates the floor (16 MB+), so this only
        // shrinks the heap below 128 MB.
        // On the size profile, seed the heap with only 512 KB — the PmmOomHandler
        // grows it on demand from PMM.  On release keep 4 MB (was 8 MB) for headroom.
        #[cfg(kernel_profile_extreme)]
        const SMALL_FLOOR: usize = 512 * 1024;
        #[cfg(not(kernel_profile_extreme))]
        const SMALL_FLOOR: usize = 4 * MB;
        const MIN_USER: usize = 4 * MB;
        let cap = ram_size
            .saturating_sub(code_and_stack)
            .saturating_sub(MIN_USER);
        core::cmp::min(
            core::cmp::max(ram_size / 8, SMALL_FLOOR),
            core::cmp::max(cap, MB),
        )
    }
}

/// Decide how many thread slots get a stack allocated (`thread_limit`, capped at `MAX_THREADS`).
///
/// Thread stacks come from PMM (the user-pages pool), so on a small machine
/// the full 64-thread pool (~9 MB) is the real boot floor. Give the pool at
/// most ~half of user pages (leaving the rest for processes), keeping the
/// `reserved` system threads plus at least a couple of user threads. See
/// docs/LOW_MEMORY_ENVIRONMENT.md.
#[must_use]
pub fn compute_thread_limit(user_pages_size: usize) -> usize {
    if config::THREAD_LIMIT_OVERRIDE != 0 {
        return config::THREAD_LIMIT_OVERRIDE;
    }
    let reserved = config::RESERVED_THREADS;
    let sys_total = reserved.saturating_sub(1) * config::SYSTEM_THREAD_STACK_SIZE;
    // The pool gets at most 1/4 of user pages — processes (their ELF images,
    // heaps, page tables) need the rest, and one process ELF load OOMs if the
    // pool is too greedy (observed at MEMORY=32M when the pool took half).
    let stack_budget = user_pages_size / 4;
    let user_budget = stack_budget.saturating_sub(sys_total);
    let n_user = user_budget / config::USER_THREAD_STACK_SIZE;
    // Floor: reserved + 6 so a minimal session (shell + SSH thread + tcc +
    // a couple of sub-processes) can coexist without hitting "no free user
    // thread slots".  Cost at 64 KB/slot: 4 × 64 KB = 256 KB extra pool.
    (reserved + n_user).clamp(reserved + 6, config::MAX_THREADS)
}

/// Build the `akuma-exec` runtime callbacks + config from the kernel's functions and
/// `config::` constants.
#[cfg(target_os = "none")]
pub(crate) fn build_exec_runtime(
    boot_stack_base: usize,
    boot_stack_top: usize,
    user_stack_size: usize,
    enable_stack_canaries: bool,
) -> (akuma_exec::ExecRuntime, akuma_exec::ExecConfig) {
    // No-op shim for gated-out Tier 2 FD-teardown callbacks (see ExecRuntime below).
    #[cfg(not(all(feature = "sc-eventfd", feature = "sc-epoll", feature = "sc-pidfd")))]
    fn noop_u32(_id: u32) {}
    #[cfg(not(feature = "rump"))]
    fn noop_u64_i32(_box_id: u64, _rump_fd: i32) {}

    let rt = akuma_exec::ExecRuntime {
        uptime_us: timer::uptime_us,
        disable_irqs: irq::disable_irqs,
        enable_irqs: irq::enable_irqs,
        end_of_interrupt: akuma_gic::end_of_interrupt,
        // Real shared-kernel SMP: voluntary reschedules (yield_now / schedule_blocking)
        // must ring THIS core's scheduler SGI, not the hardcoded PE0 that `trigger_sgi`
        // targets — otherwise a secondary's yield/block pokes the BSP and never
        // reschedules itself. On the BSP (aff0 = 0) `trigger_sgi_self` is equivalent.
        #[cfg(kernel_smp_shared)]
        trigger_sgi: akuma_gic::trigger_sgi_self,
        #[cfg(not(kernel_smp_shared))]
        trigger_sgi: akuma_gic::trigger_sgi,
        // Cross-core wakeup (M4): nudge an idle peer to run a just-woken thread. No-op
        // off shared-SMP.
        #[cfg(kernel_smp_shared)]
        wake_remote_idle: smp_shared::wake_remote_idle,
        #[cfg(not(kernel_smp_shared))]
        wake_remote_idle: || false,
        // Direct a scheduler SGI at the woken thread's last-known core so its
        // scheduler picks up the READY thread promptly. No-op off shared-SMP.
        #[cfg(kernel_smp_shared)]
        wake_core: smp_shared::wake_core,
        #[cfg(not(kernel_smp_shared))]
        wake_core: |_| {},
        heap_stats: || {
            let s = allocator::stats();
            (s.heap_size, s.allocated)
        },
        is_memory_low: allocator::is_memory_low,
        // Whether the execve/ELF-load BKL-drop is enabled (M5c hold-shortening). Lets the
        // ELF loader drop the BKL around the dynamic-interpreter read. No-op off shared-SMP.
        #[cfg(kernel_smp_shared)]
        exec_bkl_drop_enabled: smp_shared::exec_bkl_drop_enabled,
        #[cfg(not(kernel_smp_shared))]
        exec_bkl_drop_enabled: || false,
        read_file: |path| crate::fs::read_file(path).map_err(|_| -1),
        read_at: |path, off, buf| crate::vfs::read_at(path, off, buf).map_err(|_| -1),
        resolve_file_id: |path| crate::vfs::resolve_file_id(path).ok_or(-1),
        // Real implementation, not a stub: `prefault_user_range` fills every
        // inode-backed lazy file page through this hook (the path is required —
        // `with_fs` dispatches on its prefix). A previous `Err(-1)` stub made
        // every such prefault install a silent zero page, which is the
        // `[0,0,0,0]` metadata ICE in the self-host build. See
        // `akuma_pmm::DP_PREFAULT_FILL_SHORT` and
        // `docs/archive/PREFAULT_INODE_STUB_ZERO_PAGES.md`.
        read_at_by_inode: |path, inode, off, buf| {
            crate::vfs::read_at_by_inode(path, inode, off, buf).map_err(|_| -1)
        },
        on_process_exit: |_pid| {},
        remove_socket: akuma_net::socket::remove_socket,
        socket_clone_ref: akuma_net::socket::socket_clone_ref,
        // Only ever invoked for a `FileDescriptor::RumpSocket`, which no build
        // without the rump proxy can construct — same "exists so the struct
        // compiles" arrangement as the Tier 2 callbacks below.
        #[cfg(feature = "rump")]
        rump_socket_clone_ref: crate::rump_proxy::rump_fd_ref_clone,
        #[cfg(not(feature = "rump"))]
        rump_socket_clone_ref: noop_u64_i32,
        futex_wake: crate::syscall::futex_wake,
        // ITIMER_REAL/alarm() expiry check, riding the tick ISR (alarms module).
        check_itimers: crate::syscall::check_itimers,
        pipe_close_write: crate::syscall::pipe::pipe_close_write,
        pipe_close_read: crate::syscall::pipe::pipe_close_read,
        pipe_clone_ref: crate::syscall::pipe::pipe_clone_ref,
        // Tier 2 FD-teardown callbacks. akuma-exec calls these unconditionally
        // during FD drop, but when a family is gated out its FileDescriptor
        // variant is never constructed, so the no-op is never actually invoked
        // — it only has to exist so the runtime struct compiles.
        #[cfg(feature = "sc-eventfd")]
        eventfd_close: crate::syscall::eventfd::eventfd_close,
        #[cfg(not(feature = "sc-eventfd"))]
        eventfd_close: noop_u32,
        #[cfg(feature = "sc-eventfd")]
        eventfd_clone_ref: crate::syscall::eventfd::eventfd_clone_ref,
        #[cfg(not(feature = "sc-eventfd"))]
        eventfd_clone_ref: noop_u32,
        // AF_UNIX table refcounting. Not behind an `sc-*` gate: `socketpair`
        // is unconditional (rustc's linker spawn and box 0's rump sysproxy both
        // need it), so the descriptor variant is always constructible and these
        // must always be real.
        unix_sock_close: crate::syscall::unixsock::unix_sock_close,
        unix_sock_clone_ref: crate::syscall::unixsock::unix_sock_clone_ref,
        #[cfg(feature = "sc-epoll")]
        epoll_destroy: crate::syscall::poll::epoll_destroy,
        #[cfg(not(feature = "sc-epoll"))]
        epoll_destroy: noop_u32,
        #[cfg(feature = "sc-pidfd")]
        pidfd_close: crate::syscall::pidfd::pidfd_close,
        #[cfg(not(feature = "sc-pidfd"))]
        pidfd_close: noop_u32,
        flock_release: crate::syscall::flock::flock_release,
        resolve_symlinks: |path| crate::vfs::resolve_symlinks(path),
        file_size: |path| crate::fs::file_size(path).map_err(|_| "fs error"),
        get_box_namespace: |box_id| crate::vfs::get_box_namespace(box_id),
        set_spawn_namespace: crate::vfs::set_spawn_namespace,
        clear_spawn_namespace: crate::vfs::clear_spawn_namespace,
        print_str: console::print,
    };
    let cfg = akuma_exec::ExecConfig {
        max_threads: config::MAX_THREADS,
        reserved_threads: config::RESERVED_THREADS,
        // Derive the boot-stack size from the linker symbols (the single
        // source of truth — BOOT_STACK_SIZE via --defsym, profile-dependent)
        // rather than config::KERNEL_STACK_SIZE, so slot-0's StackInfo bounds
        // and canary placement always match the actual reservation even when
        // the extreme profile shrinks it. See linker.ld / build.rs.
        kernel_stack_size: boot_stack_top - boot_stack_base,
        // Real boot-stack bounds, read from the linker-derived STACK_BOTTOM /
        // STACK_TOP symbols above. The threading crate must NOT hardcode these
        // — when the boot stack was relocated, a stale constant stamped the
        // stack canary into the kernel heap at low RAM (release boot floor
        // jumped to 128 MB). See docs/LOW_MEMORY_ENVIRONMENT.md "Known bug".
        boot_stack_base,
        boot_stack_top,
        default_thread_stack_size: config::DEFAULT_THREAD_STACK_SIZE,
        system_thread_stack_size: config::SYSTEM_THREAD_STACK_SIZE,
        user_thread_stack_size: config::USER_THREAD_STACK_SIZE,
        user_stack_size,
        enable_stack_canaries,
        stack_canary: config::STACK_CANARY,
        canary_words: config::CANARY_WORDS,
        network_thread_ratio: config::NETWORK_THREAD_RATIO,
        prioritize_never_scheduled: config::PRIORITIZE_NEVER_SCHEDULED,
        deferred_thread_cleanup: config::DEFERRED_THREAD_CLEANUP,
        thread_cleanup_cooldown_us: config::THREAD_CLEANUP_COOLDOWN_US,
        process_reclaim_cooldown_us: config::PROCESS_RECLAIM_COOLDOWN_US,
        syscall_debug_info_enabled: config::SYSCALL_DEBUG_INFO_ENABLED,
        fork_brk_serial_progress: config::FORK_BRK_SERIAL_PROGRESS,
        enable_sgi_debug_prints: config::ENABLE_SGI_DEBUG_PRINTS,
        proc_stdin_max_size: config::PROC_STDIN_MAX_SIZE,
        proc_stdout_max_size: config::PROC_STDOUT_MAX_SIZE,
        cow_fork_enabled: config::COW_FORK_ENABLED,
        vfork_fastpath_enabled: config::VFORK_FASTPATH_ENABLED,
        pthread_kill_eintr_enabled: config::PTHREAD_KILL_EINTR_ENABLED,
        shared_file_pages_enabled: config::SHARED_FILE_PAGES_ENABLED,
    };
    (rt, cfg)
}

/// Boot self-test suite entry points, registered once from `src/main.rs`'s
/// `rust_start` before it calls [`kernel_main`].
///
/// The test files (`tests.rs`, `daif_tests.rs`, ...) stay in `src/` per the
/// original request — they are the ONE thing this crate cannot depend on,
/// since `src/` is the bin crate and depends on THIS crate, not the other way
/// around. `kernel_main`/`run_async_main` used to call them by bare module
/// name when they were crate-root siblings in `main.rs`; now they go through
/// this hook, the same `OnceCopy`-backed boot-registered-function pattern
/// `akuma_primitives::console::set_print_hook` already uses for `console::print`.
#[cfg(all(target_os = "none", kernel_tests))]
#[derive(Clone, Copy)]
pub struct BootTestHooks {
    pub daif_tests: fn(),
    pub memory_tests: fn() -> bool,
    pub async_tests: fn() -> bool,
    pub fs_tests: fn(),
    pub threading_tests: fn() -> bool,
    pub sync_tests: fn(),
    pub pthread_tests: fn(),
    pub process_tests: fn(),
    pub benchmarks: fn() -> bool,
    #[cfg(feature = "smoltcp")]
    pub network_tests: fn(),
    #[cfg(feature = "smoltcp")]
    pub process_network_tests: fn(),
}

/// `Registered`, not a bare `OnceCopy`: this table MUST exist by the time
/// `kernel_main` reaches the suite, so absence is a boot-order bug rather than
/// a condition to handle. See `akuma_not_even_once`'s "Which of the two to
/// reach for" — the accessor below was a hand-rolled `.expect()` on an
/// `OnceCopy` until 2026-09-01, which is the same policy spelled the long way
/// and the one hook in the tree that did not match its class.
#[cfg(all(target_os = "none", kernel_tests))]
static BOOT_TEST_HOOKS: akuma_primitives::Registered<BootTestHooks> =
    akuma_primitives::Registered::new(
        "akuma-kernel-glue: boot test hooks not registered — \
         call akuma_kernel_glue::set_boot_test_hooks() from rust_start first",
    );

/// Install the boot self-test hooks. Idempotent by `Registered`'s contract — a
/// second call is ignored.
#[cfg(all(target_os = "none", kernel_tests))]
pub fn set_boot_test_hooks(hooks: BootTestHooks) {
    BOOT_TEST_HOOKS.register(hooks);
}

#[cfg(all(target_os = "none", kernel_tests))]
fn boot_test_hooks() -> BootTestHooks {
    BOOT_TEST_HOOKS.require()
}

/// The rump regression-guard suite (`src/rump_tests.rs`) is gated
/// independently of `kernel_tests` — it can run on a `no-tests` build when
/// `rump-tests` is explicitly on — so it gets its own hook rather than a
/// field on [`BootTestHooks`].
#[cfg(all(
    target_os = "none",
    not(kernel_profile_extreme),
    feature = "rump",
    any(not(feature = "no-tests"), feature = "rump-tests"),
))]
static RUMP_TESTS_HOOK: akuma_primitives::Registered<fn()> = akuma_primitives::Registered::new(
    "akuma-kernel-glue: rump tests hook not registered — \
     call akuma_kernel_glue::set_rump_tests_hook() from rust_start first",
);

#[cfg(all(
    target_os = "none",
    not(kernel_profile_extreme),
    feature = "rump",
    any(not(feature = "no-tests"), feature = "rump-tests"),
))]
pub fn set_rump_tests_hook(f: fn()) {
    RUMP_TESTS_HOOK.register(f);
}

/// Main kernel initialization - all safe code
#[cfg(target_os = "none")]
pub fn kernel_main(dtb_ptr: usize) -> ! {
    // Detect memory from DTB (must be done before heap init, so print first)
    console::print("Akuma Kernel starting...\n");

    // =========================================================================
    // Device map: install before ANY GIC or virtio MMIO access
    // =========================================================================
    // `boot.rs` maps only the console UART, because that is the only device whose
    // address can be a compile-time literal — Firecracker's GIC redistributor
    // base moves with vCPU count, so the rest has to be discovered. Install the
    // compile-time bootstrap map now and rewrite the boot table's device L3 from
    // it, so the GIC and virtio pages exist before `akuma_gic::init()` runs.
    //
    // This is the bootstrap map, not the authority: `platform::machine`'s
    // redistributor address assumes one vCPU. The FDT-derived refinement runs
    // below, as soon as `ensure_boot_identity_covers` has mapped the blob.
    // Tell the exception/scheduler tripwires where kernel code lives. Must happen
    // before the first IRQ; a wrong window makes every legitimate frame look
    // poisoned (see `mmu::set_kernel_text_window`).
    mmu::set_kernel_text_window(config::KERNEL_PHYS_BASE, config::KERNEL_TEXT_END);
    // Route the `log` facade into the console before anything that uses it.
    // `akuma-net`/smoltcp report progress exclusively through `log::info!`, and
    // with no sink installed `smoltcp_net::init` was silent — see src/klog.rs.
    klog::init();

    // Diagnostic: SCTLR_EL1 as it stands after boot.rs ORed its bits into the
    // RESET value. The architecture leaves several SCTLR_EL1 fields UNKNOWN at
    // reset, and KVM stamps UNKNOWN-reset registers with a poison pattern, so
    // "or into whatever was there" inherits different behaviour per hypervisor.
    // SA0 (bit 4) in particular enables EL0 SP-alignment checking.
    {
        let sctlr = akuma_cpu::sysreg::sctlr_el1();
        safe_print!(96, "[SCTLR] EL1=0x{:x} SA={} SA0={}\n",
            sctlr, (sctlr >> 3) & 1, (sctlr >> 4) & 1);
    }

    platform::install_bootstrap_device_map();
    // Still on the boot page table built by `boot.rs`, single-threaded, before any
    // user address space exists — which is what `rebuild_boot_device_table` now
    // checks for itself. It also finds the device L3 by walking that table, so
    // there is no L3 address to pass and no way to pass the wrong one.
    mmu::rebuild_boot_device_table();
    console::print("[Platform] ");
    console::print(platform::machine::NAME);
    console::print(" device map installed\n");

    // =========================================================================
    // CRITICAL: Verify kernel binary doesn't overlap with boot stack
    // =========================================================================
    // Boot stack is placed immediately above the kernel image by linker.ld, which
    // derives the reservation from the actual linked size and exports it as the
    // absolute symbols STACK_BOTTOM (first page of the 1 MB stack) and STACK_TOP
    // (initial SP). There is no per-profile IMAGE_SIZE/STACK_BOTTOM constant to
    // keep in lockstep anymore: boot.rs (asm SP + Image header), this file (overlap
    // guard + heap reserve + ExecConfig bounds) and exceptions.rs all read the same
    // linker symbols. Reading a symbol's address yields its absolute value (the
    // same trick used for _kernel_phys_end), so the layout auto-tracks the binary.
    //
    // QEMU virt loads flat binary with ARM64 Image header at RAM_BASE + 1MB
    // (text_offset = 1MB >= 4KB so QEMU does not add 2MB).
    // DTB goes to ALIGN_UP(kernel_load + image_size, 2MB) = 0x40200000.
    const KERNEL_BASE: usize = config::KERNEL_PHYS_BASE;

    let kernel_end = akuma_entry::linker_syms::kernel_phys_end();
    let stack_bottom = akuma_entry::linker_syms::stack_bottom();
    let boot_stack_top = akuma_entry::linker_syms::stack_top();
    let kernel_size = kernel_end - KERNEL_BASE;

    // Stack high-water probe: paint the boot stack's unused lower region so the
    // memory monitor can later report thread 0's true peak (drives whether the
    // 1 MB boot stack can be trimmed). No-op unless the probe const is on.
    akuma_exec::threading::paint_boot_stack(stack_bottom, boot_stack_top);

    console::print("Kernel binary: ");
    console::print_dec(kernel_size / 1024);
    console::print(" KB (0x");
    console::print_hex(KERNEL_BASE as u64);
    console::print(" - 0x");
    console::print_hex(kernel_end as u64);
    console::print(")\n");

    if kernel_end >= stack_bottom {
        console::print("\n!!! FATAL: Kernel binary overlaps with boot stack !!!\n");
        console::print("Kernel end:   0x");
        console::print_hex(kernel_end as u64);
        console::print("\nStack bottom: 0x");
        console::print_hex(stack_bottom as u64);
        console::print("\n\nThe kernel has grown too large. Options:\n");
        console::print("  1. Increase STACK_TOP in boot.rs (move stack higher)\n");
        console::print("  2. Reduce kernel size (remove unused features)\n");
        console::print("  3. Move to dynamic stack allocation\n");
        console::print("\nHALTING.\n");
        halt();
    }

    // Safety margin check - warn if kernel is getting close to stack
    let margin = stack_bottom - kernel_end;
    if margin < 4 * 1024 * 1024 {
        // Less than 4MB margin
        console::print("WARNING: Kernel is within 4MB of stack! (");
        console::print_dec(margin / 1024);
        console::print(" KB margin)\n");
    }

    // =========================================================================
    // Device map, take two: from the machine, not from a literal
    // =========================================================================
    // The bootstrap map installed above holds `platform::machine::GICR_PA`, which
    // is the *single-vCPU* redistributor address. Firecracker stacks the
    // redistributors downward from the distributor, so CPU0's frames sit at
    // `GICD - vcpu_count * 0x2_0000` and move with the configured vCPU count.
    // Keeping the literal at `SMP=2` points the boot core at CPU1's frames: it
    // clears the wrong `GICR_WAKER` and enables the virtual timer on the wrong
    // frame, losing its own scheduler tick with no build or boot error. So this
    // has to run before `akuma_gic::init()`, and the DTB has to be mapped first — hence
    // its position immediately after `ensure_boot_identity_covers`.
    //
    // A failure here is not fatal: the bootstrap map stays, which is correct on
    // QEMU virt at every `SMP=N` and correct on a single-vCPU Firecracker. Print
    // the outcome either way — it is the only record of which map the GIC was
    // configured from. See docs/reference/firecracker/README.md §3.3.
    //
    // This block is also where the DTB is materialised, once, for all three of
    // its consumers — the device map here, `detect_memory` and
    // `smp_shared::probe_dtb` below. It is a block and not three statements for
    // a reason: the blob is NOT valid for the rest of the boot, because on
    // large-RAM configs the heap can be placed on top of it. Scoping the borrow
    // is what makes the borrow checker enforce "read the DTB before heap init",
    // which was previously only a comment on `probe_dtb`.
    // `with_boot_identity_fdt` maps the DTB's block, confirms the mapping took,
    // and only then reads — the FDT may live outside boot.rs's static [0, 3 GiB)
    // identity map (Firecracker places it in the last 2 MiB of guest RAM, so a
    // 4 GiB microVM has it at ~6 GiB), and an unchecked read faults before the
    // console has said anything about memory. It is also what bounds the blob's
    // lifetime to this closure: `akuma_fdt::locate` hands back an unbounded one,
    // and the bytes stop being valid at heap init below.
    let (ram_base, ram_size) = mmu::with_boot_identity_fdt(dtb_ptr, |dtb| {
        if let Some(d) = dtb {
            safe_print!(64, "[DTB] found at 0x{:x} ({} bytes)\n", d.base(), d.len());
        } else {
            safe_print!(64, "[DTB] none at 0x{:x}\n", akuma_fdt::resolve(dtb_ptr));
        }
        let fdt = dtb.and_then(akuma_fdt::Dtb::parse);

        let outcome = platform::install_fdt_device_map(fdt.as_ref());
        match outcome {
            platform::FdtMapOutcome::Installed { gicr_pa, moved } => {
                mmu::rebuild_boot_device_table();
                safe_print!(96, "[Platform] FDT device map: GICR=0x{:x}{}\n",
                    gicr_pa, if moved { " (moved from bootstrap literal)" } else { "" });
            }
            platform::FdtMapOutcome::NoFdt => {
                safe_print!(80, "[Platform] no FDT; keeping bootstrap device map\n");
            }
            platform::FdtMapOutcome::Rejected(e) => {
                safe_print!(96, "[Platform] FDT rejected ({:?}); keeping bootstrap map\n", e);
            }
        }

        let ram = detect_memory(fdt.as_ref());

        // Real (shared-kernel) SMP: snapshot CPU/PSCI info from the DTB NOW, before the
        // heap allocator (which can be placed exactly at the DTB's address on
        // large-RAM configs) overwrites it. No-op without the `smp-shared` feature.
        #[cfg(kernel_smp_shared)]
        smp_shared::probe_dtb(fdt.as_ref());

        ram
    });

    // Memory layout. All the policy (boot-stack cover, code+stack floor, the
    // extreme reserve-RAM clamp) lives in `compute_memory_layout` + `config`, so
    // the boot path here is just "compute, then verify". The sanity guard below
    // re-checks the result before any allocation — a wrong reserve constant must
    // refuse to boot rather than silently corrupt kernel memory under load.
    let MemoryLayout {
        code_and_stack,
        heap_start,
        heap_size,
        user_pages_start,
        user_pages_size,
    } = compute_memory_layout(ram_base, ram_size, boot_stack_top);

    // ---- Layout sanity guard (runs AFTER all region calculations) ----
    // The kernel address space is laid out as three contiguous regions:
    //   [ram_base .. heap_start)            code + boot stack
    //   [heap_start .. heap_end)            kernel heap
    //   [user_pages_start .. user_end)      user pages (PMM pool)
    // Verify they are contiguous, non-overlapping, in-bounds, and that NONE
    // collides with the fixed boot stack [BOOT_STACK_TOP-1MB, BOOT_STACK_TOP).
    // A failure means a memory-calc constant is wrong; refuse to boot rather
    // than silently corrupt kernel memory under load — that is exactly the
    // MEMORY=64 Thread0 EC=0x21/0x22 crash (heap overlapped the boot stack
    // because the reserve forgot the 2 MB KERNEL_BASE offset). The MMU cannot
    // protect the kernel from its own allocator, so this check must be explicit.
    let ram_end = ram_base + ram_size;
    let heap_end = heap_start + heap_size;
    let user_end = user_pages_start + user_pages_size;
    // The actual boot-stack bottom is the STACK_BOTTOM linker symbol, not a
    // hardcoded `top - 1 MB` — the extreme profile shrinks BOOT_STACK_SIZE, so
    // assuming 1 MB here would compute a bogus bottom (below the kernel image)
    // and the overlap guard below would be checking the wrong region.
    let boot_stack_bottom = stack_bottom;
    let layout_ok =
        kernel_end <= heap_start &&                // kernel binary fits in code+stack
        boot_stack_bottom >= ram_base &&           // boot stack starts within RAM
        boot_stack_top <= heap_start &&            // boot stack ends at/before heap (no overlap)
        heap_size > 0 &&
        heap_start == ram_base + code_and_stack && // contiguous: code+stack -> heap
        user_pages_start == heap_end &&            // contiguous: heap -> user pages
        user_pages_size > 0 &&
        user_end <= ram_end;                       // everything fits in RAM
    if !layout_ok {
        console::print("\n!!! FATAL: kernel memory layout invalid (overlap / out of bounds) !!!\n");
        console::print("  ram:        0x"); console::print_hex(ram_base as u64);
        console::print(" - 0x"); console::print_hex(ram_end as u64); console::print("\n");
        console::print("  code+stack: 0x"); console::print_hex(ram_base as u64);
        console::print(" - 0x"); console::print_hex(heap_start as u64); console::print("\n");
        console::print("  boot stack: 0x"); console::print_hex(boot_stack_bottom as u64);
        console::print(" - 0x"); console::print_hex(boot_stack_top as u64); console::print("\n");
        console::print("  heap:       0x"); console::print_hex(heap_start as u64);
        console::print(" - 0x"); console::print_hex(heap_end as u64); console::print("\n");
        console::print("  user pages: 0x"); console::print_hex(user_pages_start as u64);
        console::print(" - 0x"); console::print_hex(user_end as u64); console::print("\n");
        console::print("  kernel_end: 0x"); console::print_hex(kernel_end as u64); console::print("\n");
        console::print("HALTING.\n");
        halt();
    }

    // Log memory layout decisions (using print_hex/print_dec since heap not yet initialized)
    console::print("\n=== Memory Layout ===\n");
    console::print("Total RAM: ");
    console::print_dec(ram_size / 1024 / 1024);
    console::print(" MB at 0x");
    console::print_hex(ram_base as u64);
    console::print("\n");

    console::print("Code+Stack: ");
    console::print_dec(code_and_stack / 1024 / 1024);
    console::print(" MB (0x");
    console::print_hex(ram_base as u64);
    console::print(" - 0x");
    console::print_hex(heap_start as u64);
    console::print(") [stack-cover + guard]\n");

    console::print("Heap:       ");
    console::print_dec(heap_size / 1024 / 1024);
    console::print(" MB (0x");
    console::print_hex(heap_start as u64);
    console::print(" - 0x");
    console::print_hex(user_pages_start as u64);
    console::print(") [auto]\n");

    console::print("User pages: ");
    console::print_dec(user_pages_size / 1024 / 1024);
    console::print(" MB (0x");
    console::print_hex(user_pages_start as u64);
    console::print(" - 0x");
    console::print_hex((ram_base + ram_size) as u64);
    console::print(") [remaining]\n");

    // Compute user stack size based on RAM
    let user_stack_size = config::compute_user_stack_size(ram_size);
    console::print("User stack: ");
    console::print_dec(user_stack_size / 1024);
    console::print(" KB");
    if config::USER_STACK_SIZE_OVERRIDE == 0 {
        console::print(" (auto-scaled from RAM)\n");
    } else {
        console::print(" (override)\n");
    }

    console::print("=====================\n\n");

    // Ensure we have enough for heap
    if heap_size == 0 {
        console::print("FATAL: Not enough RAM for heap\n");
        halt();
    }

    // Initialize allocator first (uses talc until PMM is ready)
    if let Err(e) = allocator::init(heap_start, heap_size) {
        console::print("Allocator init failed: ");
        console::print(e);
        console::print("\n");
        halt();
    }
    console::print("Allocator initialized (talc mode)\n");

    // Register the PMM's config + reclaim hooks before `pmm::init`, which reads
    // the config immediately (the CoW-ever bitset's COW_REF_LEDGER gate). The
    // hooks themselves are plain fn items — safe to register this early, since
    // registering costs nothing and they are only invoked later, under user
    // memory pressure, which cannot occur before userspace exists.
    pmm::register_config(pmm::PmmConfig {
        cow_ref_ledger: config::COW_REF_LEDGER,
        pmm_uaf_quarantine: config::PMM_UAF_QUARANTINE,
        pmm_premature_free_check: config::PMM_PREMATURE_FREE_CHECK,
    });
    pmm::register_hooks(pmm::PmmHooks {
        heap_reclaim: allocator::reclaim_to_pmm,
        drain_retired: akuma_exec::process::reclaim::drain_retired_under_pressure,
        evict_clean_file_pages: akuma_exec::process::reclaim_clean_file_pages,
        shrink_page_cache: file_page_cache::shrink,
    });
    // The permanent surviving-mapper bridge `akuma-pmm` needs moved to
    // `akuma_exec::process::reclaim` on 2026-09-01 (the fn only ever needed the
    // process table, which akuma-exec owns) and registers itself from
    // `akuma_exec::init`. The `cow_ref_get` bridge Step 2 needed here too is
    // gone as of Step 3 — `COW_REFCOUNTS` is crate-native now.

    // Initialize Physical Memory Manager
    // After this, the allocator can switch to page-based allocation
    let kernel_end = heap_start + heap_size;
    console::print("Initializing PMM...\n");
    pmm::init(ram_base, ram_size, kernel_end);

    // Signal that PMM is ready - allocator will switch to page mode
    allocator::mark_pmm_ready();
    console::print("PMM initialized, allocator switched to page mode\n");

    // Reclaim the pre-kernel region.  KERNEL_PHYS_OFFSET (1 MB) bytes before the
    // kernel are unused space — fully consumed by detect_memory() before PMM init
    // and safe to give back.  Hands ~256 pages (1 MB) to the user-page pool.
    {
        let pages = config::KERNEL_PHYS_OFFSET / 4096;
        pmm::free_pages_contiguous(pmm::PhysFrame::new(ram_base), pages);
        console::print("[PMM] Reclaimed pre-kernel region: 1 MB\n");
    }

    // Initialize MMU with identity mapping for kernel
    console::print("Initializing MMU...\n");
    mmu::init(ram_base, ram_size);

    // Register exec runtime before init_shared_device_tables, which needs
    // the PMM callbacks via runtime(). The function pointers are just stored
    // here — subsystems like GIC/timer don't need to be initialized yet.
    console::print("Initializing exec subsystem...\n");

    let (exec_rt, exec_cfg) = build_exec_runtime(
        stack_bottom,
        boot_stack_top,
        user_stack_size,
        config::ENABLE_STACK_CANARIES,
    );
    akuma_exec::init(exec_rt, exec_cfg);
    akuma_exec::process::enable_process_syscall_stats(config::PROCESS_SYSCALL_STATS);
    console::print("Exec subsystem initialized\n");

    mmu::init_shared_device_tables();
    console::print("MMU enabled with identity mapping\n");

    console::print("Enabling kernel code protection...\n");
    mmu::protect_kernel_code();
    console::print("Kernel code protection enabled\n");

    // Print PMM stats
    let (total, allocated, free) = pmm::stats();
    console::print("PMM stats: ");
    console::print_dec(total);
    console::print(" total, ");
    console::print_dec(allocated);
    console::print(" allocated, ");
    console::print_dec(free);
    console::print(" free\n");

    // Initialize GIC (Generic Interrupt Controller)
    akuma_gic::init();
    console::print("GIC initialized\n");

    // Real (shared-kernel) SMP: wake secondary cores onto the SHARED boot page tables,
    // PMM, and heap. Each secondary adopts an idle thread and joins the one shared
    // scheduler. No-op with a single QEMU CPU. (docs/archive/SMP_SHARED.md)
    //
    // Bringup TIMING depends on the image, because the secondary immediately calls
    // `adopt_current_as_core_idle` to claim a thread-pool slot:
    //
    //  * Boot SELF-TEST image (tests compiled in): bring up HERE, before `threading::init`,
    //    so the M0–M4 `test_smp_shared_*` self-tests find the secondaries already online.
    //    `threading::init` later resets the slot allocator's free map, so the secondaries'
    //    adopted slots are re-marked free — a latent collision — but it stays masked because
    //    a secondary only WFI-idles its slot and the tests run+finish on the BSP. Placing
    //    bringup after `init` instead makes the secondaries join the scheduler during the
    //    spawn-heavy suites and storm the coarse BKL (~2900 `[BKL] stuck`) — an M5
    //    fine-graining problem. So the self-test image keeps the pre-init order.
    //
    //  * RUNTIME image (`no-tests`, e.g. devbox-smoltcp): there are no self-tests to run
    //    against the secondaries, but the full boot DOES schedule the async-main thread —
    //    which, under the pre-init order, collides with a secondary's adopted idle slot and
    //    stalls the boot. So the runtime image brings secondaries up AFTER `threading::init`
    //    (below), where the slot allocator is live and hands out distinct slots. See
    //    docs/archive/SMP_SHARED.md M4 "open item".
    #[cfg(all(kernel_smp_shared, not(feature = "no-tests")))]
    smp_shared::bringup_secondaries();

    // Exception-path hooks: the dispatcher + the EL0-trap diagnostic the IRQ
    // and SVC entry points call out to. Registered BEFORE `exceptions::init()`
    // — no exception can be handled before it installs VBAR and unmasks IRQs,
    // which is what keeps the hooks' `require()` unreachable-empty.
    exceptions::register_config(exceptions::ExceptionsConfig {
        verify_svc_at_entry: config::VERIFY_SVC_AT_ENTRY,
        process_syscall_stats: config::PROCESS_SYSCALL_STATS,
        demand_page_log_enabled: config::DEMAND_PAGE_LOG_ENABLED,
        fpcache_verify_hits: config::FPCACHE_VERIFY_HITS,
        signal_trace_enabled: config::SIGNAL_TRACE_ENABLED,
        trace_tkill: config::TRACE_TKILL,
        debug_sigsegv_syscall_stub: config::DEBUG_SIGSEGV_SYSCALL_STUB,
        debug_sigsegv_syscall_stub_elir_min: config::DEBUG_SIGSEGV_SYSCALL_STUB_ELIR_MIN,
        debug_sigsegv_syscall_stub_elir_max: config::DEBUG_SIGSEGV_SYSCALL_STUB_ELIR_MAX,
        debug_pattern2_trap_trace: config::DEBUG_PATTERN2_TRAP_TRACE,
    });
    #[cfg(kernel_smp_shared)]
    exceptions::register_hooks(exceptions::ExceptionHooks {
        dispatch_irq: irq::dispatch_irq,
        record_el0_trap: smp_shared::record_el0_trap,
        report_poison_value: akuma_exec::process::reclaim::report_poison_value,
        dp_counters_line: pmm::dp_counters_line,
        handle_syscall: syscall::handle_syscall,
        current_syscall_nr: syscall::current_syscall_nr,
        inc_pagefault: syscall::syscall_counters::inc_pagefault,
        inc_qemu_dc_zva_ec15: syscall::syscall_counters::inc_qemu_dc_zva_ec15,
        inc_qemu_stp_xzr_ec15: syscall::syscall_counters::inc_qemu_stp_xzr_ec15,
        sys_exit_group: syscall::proc::sys_exit_group_pub,
        notify_child_channel_exited: syscall::proc::notify_child_channel_exited_pub,
        vfork_complete: syscall::proc::vfork_complete,
        signal_is_fatal_default: syscall::signal::signal_is_fatal_default,
        syscall_log_formatted: syscall::log::get_formatted,
        read_profile_span_new: syscall::utils::read_profile::exception_span_start,
        read_profile_span_end: syscall::utils::read_profile::exception_span_end,
    });
    #[cfg(not(kernel_smp_shared))]
    exceptions::register_hooks(exceptions::ExceptionHooks {
        dispatch_irq: irq::dispatch_irq,
        report_poison_value: akuma_exec::process::reclaim::report_poison_value,
        dp_counters_line: pmm::dp_counters_line,
        handle_syscall: syscall::handle_syscall,
        current_syscall_nr: syscall::current_syscall_nr,
        inc_pagefault: syscall::syscall_counters::inc_pagefault,
        inc_qemu_dc_zva_ec15: syscall::syscall_counters::inc_qemu_dc_zva_ec15,
        inc_qemu_stp_xzr_ec15: syscall::syscall_counters::inc_qemu_stp_xzr_ec15,
        sys_exit_group: syscall::proc::sys_exit_group_pub,
        notify_child_channel_exited: syscall::proc::notify_child_channel_exited_pub,
        vfork_complete: syscall::proc::vfork_complete,
        signal_is_fatal_default: syscall::signal::signal_is_fatal_default,
        syscall_log_formatted: syscall::log::get_formatted,
        read_profile_span_new: syscall::utils::read_profile::exception_span_start,
        read_profile_span_end: syscall::utils::read_profile::exception_span_end,
    });

    // Set up exception vectors and enable IRQs
    exceptions::init();
    console::print("IRQ handling enabled\n");

    // Initialize timer
    timer::init();
    console::print("Timer initialized\n");

    // =========================================================================
    // Hardware RNG initialization
    // =========================================================================
    match rng::init() {
        Ok(()) => {
            console::print("[RNG] Hardware RNG initialized\n");
        }
        Err(_e) => {
            console::print("[RNG] Hardware RNG not available\n");
        }
    }

    // =========================================================================
    // VirtIO sound output initialization (non-fatal; /dev/dsp gated on success)
    // =========================================================================
    // Skipped entirely where the machine has no sound device: probing all eight
    // virtio slots to report "not available" every boot is noise, not diagnosis.
    #[cfg(kernel_audio)]
    match audio::init() {
        Ok(()) => console::print("[SND] virtio-sound ready (/dev/dsp)\n"),
        Err(_e) => console::print("[SND] virtio-sound not available\n"),
    }


    // Initialize kernel timer (CNTV alarm queue for async timeouts)
    akuma_exec::alarms::init();

    // Check timer hardware
    let freq = timer::read_frequency();
    console::print("Timer frequency: ");
    safe_print!(32, "{}", freq);
    console::print(" Hz\n");

    // Read UTC time from PL031 RTC hardware
    if timer::init_utc_from_rtc() {
        console::print("UTC time initialized from RTC\n");
    } else {
        console::print("Warning: RTC not available, UTC time not set\n");
    }

    console::print("Current UTC time: ");
    console::print(&timer::utc_iso8601());
    console::print("\n");

    console::print("Uptime: ");
    safe_print!(32, "{}", timer::uptime_us() / 1_000_000);
    console::print(" seconds\n");

    // Scale the thread-stack pool to RAM before threading allocates it from PMM.
    let tl = compute_thread_limit(user_pages_size);
    threading::set_thread_limit(tl);
    crate::safe_print!(96, "Thread limit: {} slots (stack pool from PMM)\n", tl);

    // Initialize threading (but don't enable timer yet!)
    console::print("Initializing threading...\n");
    threading::init();
    process::init(); // Initialize process subsystem (registers cleanup callback)
    // Config + the four `/proc` facts only the binary can answer. Must precede
    // the root mount below, which is `akuma_vfs_glue::init`.
    vfs::register();
    syscall::register();
    // Identity-cache hit counting rides with the epilogue audit: both are
    // measurement, and the hit count is only meaningful next to the miss
    // breakdown. See `config::IDENTITY_AUDIT`.
    akuma_exec::process::table::IDENTITY_STATS
        .store(config::IDENTITY_AUDIT, core::sync::atomic::Ordering::Relaxed);
    // Per-tid state owned by THIS crate, dropped when a thread slot is recycled. The
    // threading crate scrubs its own per-slot arrays but cannot reach kernel tables that
    // are keyed by tid — chiefly `FUTEX_WAITERS`, where a tid left queued by a thread that
    // died while parked is inherited by the slot's next occupant and silently absorbs that
    // address's next wake.
    threading::set_slot_purge_callback(syscall::futex_purge_tid);
    process::init_box_registry(); // Init Box 0
    console::print("Threading system initialized\n");

    // =========================================================================
    // Now enable preemptive scheduling (timer interrupts)
    // =========================================================================
    console::print("Configuring scheduler SGI...\n");
    akuma_gic::enable_irq(akuma_gic::SGI_SCHEDULER);

    console::print("Registering timer IRQ...\n");
    // Single hardware timer: the virtual timer (CNTV) fires PPI 27. Its handler
    // drives preemption AND services the async alarm queue (kernel_timer). The
    // physical timer (CNTP/PPI 30) is not used — it is inaccessible to the guest
    // under QEMU HVF (programming it faults with EC=0x0).
    //
    // Boot order: a NOP handler is registered first so the host WFI probe
    // (timer::probe_host_tick) can fire one-shots into a harmless handler;
    // the real ISR is swapped in below before the periodic tick is armed.
    irq::register_handler(27, timer::probe_irq_nop);
    akuma_gic::enable_irq(27); // Enable virtual timer interrupt

    console::print("Enabling timer...\n");
    let tick_us = timer::probe_host_tick();
    irq::register_handler(27, timer::timer_irq_handler);
    timer::enable_timer_interrupts(tick_us);
    safe_print!(96, "Preemptive scheduling enabled ({}us timer -> SGI)\n", tick_us);

    // Enable IRQ-safe allocations now that preemption is active
    allocator::enable_preemption_safe_alloc();

    // RUNTIME image (`no-tests`): bring secondaries up HERE — after the thread pool and
    // preemptive scheduler are live — so each secondary's `adopt_current_as_core_idle`
    // claims a distinct slot from the initialized allocator and never collides with the
    // async-main thread spawned below. (The self-test image brought them up earlier, before
    // `threading::init`; see the note there.) Combined with the network path's cross-core
    // lock discipline (`PreemptGuard` in akuma-net), this lets the devbox boot to userspace
    // sshd under SMP>=2. See docs/archive/SMP_SHARED.md M4 "open item".
    #[cfg(all(kernel_smp_shared, feature = "no-tests"))]
    smp_shared::bringup_secondaries();

    // The boot self-test suite spawns many concurrent threads/processes. On a
    // tiny machine there aren't enough user thread slots (the pool is scaled to
    // RAM) or user pages, so spawn-based tests panic ("No free user thread
    // slots") and halt the boot. At or below LOW_MEM_TEST_SKIP_MB, skip the whole
    // suite so small RAM boots to SSH — the heuristics are still covered by the
    // pure compute_heap_size/compute_thread_limit unit tests on larger configs,
    // and production uses DISABLE_ALL_TESTS anyway. See docs/LOW_MEMORY_ENVIRONMENT.md.
    //
    // Both the decision and its message live under the same cfg as the suite
    // itself: on a `no-tests`/size image there is no suite to skip, and printing
    // that we skipped one is a lie the extreme boot log told for months.
    #[cfg(kernel_tests)]
    let boot_tests_enabled = {
        let low_mem_skip_tests = config::LOW_MEM_TEST_SKIP_MB != 0
            && ram_size <= config::LOW_MEM_TEST_SKIP_MB * 1024 * 1024;
        if low_mem_skip_tests {
            crate::safe_print!(128,
                "[TESTS] low-mem ({} MB <= {} MB): skipping boot self-test suite\n",
                ram_size / 1024 / 1024, config::LOW_MEM_TEST_SKIP_MB);
        }
        !config::DISABLE_ALL_TESTS && !low_mem_skip_tests
    };

    // Run DAIF / IRQ-mask tests first — these verify the foundational
    // invariants that every later subsystem relies on. See
    // docs/STABILITY_URGENT_ISSUES.md issue #1.
    #[cfg(kernel_tests)]
    if boot_tests_enabled {
        (boot_test_hooks().daif_tests)();
    }

    // Run memory tests (no filesystem dependency)
    #[cfg(kernel_tests)]
    {
        if boot_tests_enabled {
            if !(boot_test_hooks().memory_tests)() {
                console::print("\n!!! MEMORY TESTS FAILED - HALTING !!!\n");
                halt();
            }

            // =========================================================================
            // Run async tests (before network takes over the main loop)
            // =========================================================================
            if !(boot_test_hooks().async_tests)() {
                console::print("\n!!! ASYNC TESTS FAILED - HALTING !!!\n");
                halt();
            }
        } else {
            console::print("[TESTS] All tests DISABLED via config::DISABLE_ALL_TESTS\n");
        }
    }

    // =========================================================================
    // Filesystem initialization
    // =========================================================================
    if !config::SKIP_FILESYSTEM_INIT {
        console::print("\n--- Filesystem Initialization ---\n");

        // Initialize block device first
        match block::init() {
            Ok(()) => {
                console::print("[Block] Block device initialized successfully\n");

                // Now initialize filesystem
                match fs::init() {
                    Ok(()) => {
                        console::print("[FS] Filesystem mounted successfully\n");

                        // List root directory contents
                        if let Ok(entries) = fs::list_dir("/") {
                            console::print("[FS] Root directory contents:\n");
                            for entry in entries {
                                if entry.is_dir {
                                    crate::safe_print!(64, "  [DIR]  {}\n", entry.name);
                                } else {
                                    crate::safe_print!(64, 
                                        "  [FILE] {} ({} bytes)\n",
                                        entry.name,
                                        entry.size
                                    );
                                }
                            }
                        }

                        #[cfg(kernel_tests)]
                        if boot_tests_enabled {
                            // Run filesystem tests
                            (boot_test_hooks().fs_tests)();

                            // Run threading tests (requires fs for parallel process tests)
                            if !(boot_test_hooks().threading_tests)() {
                                console::print("\n!!! THREADING TESTS FAILED - HALTING !!!\n");
                                if !config::IGNORE_THREADING_TESTS {
                                    halt();
                                }
                                console::print("WARNING: Threading tests failed but continuing...\n");
                            }

                            // Spawn-heavy suites (futex spawns, process exec,
                            // shell pipelines) need several concurrent threads /
                            // processes and panic on tiny machines. Skip them at
                            // or below LOW_MEM_TEST_SKIP_MB so small RAM boots to
                            // SSH (docs/LOW_MEMORY_ENVIRONMENT.md).
                            let low_mem = config::LOW_MEM_TEST_SKIP_MB != 0
                                && ram_size <= config::LOW_MEM_TEST_SKIP_MB * 1024 * 1024;
                            if low_mem {
                                crate::safe_print!(128,
                                    "[TESTS] low-mem ({} MB <= {} MB): skipping sync/process/shell suites\n",
                                    ram_size / 1024 / 1024, config::LOW_MEM_TEST_SKIP_MB);
                            } else {
                                // Run futex sync tests
                                (boot_test_hooks().sync_tests)();

                                // Run pthread / threading-API conformance tests
                                // (per-thread signal mask, sigaltstack, tkill,
                                // gettid — the §7k.3 regression class).
                                (boot_test_hooks().pthread_tests)();

                                // Run process execution tests
                                (boot_test_hooks().process_tests)();

                            }

                            // Run memory benchmarks (always prints, never fails)
                            (boot_test_hooks().benchmarks)();
                        }
                    }
                    Err(e) => {
                        console::print("[FS] Filesystem init failed: ");
                        crate::safe_print!(32, "{}\n", e);
                        console::print("[FS] Continuing without filesystem...\n");
                    }
                }
            }
            Err(e) => {
                console::print("[Block] Block device not found: ");
                crate::safe_print!(32, "{}\n", e);
                console::print("[Block] Continuing without filesystem...\n");
            }
        }

        console::print("--- Filesystem Initialization Done ---\n\n");
    } else {
        console::print("[FS] Filesystem SKIPPED via config::SKIP_FILESYSTEM_INIT\n");
    }

    run_async_main_preemptive();
}

#[cfg(target_os = "none")]
fn run_async_main_preemptive() -> ! {
    // Use spawn_system_thread_fn - it uses SYSTEM_THREAD_STACK_SIZE (256KB)
    // which equals ASYNC_THREAD_STACK_SIZE, so no custom size needed
    let thread_result = akuma_exec::threading::spawn_system_thread_fn(|| {
        run_async_main();
    });

    match thread_result {
        Ok(thread_id) => {
            let mut loop_counter = 0u64;
            loop {
                if threading::is_thread_terminated(thread_id) {
                    break;
                }
                
                if config::MINIMAL_IDLE_LOOP {
                    // Minimal loop for debugging - just yield, no cleanup/stats/prints
                    threading::yield_now();
                    continue;
                }
                
                loop_counter = loop_counter.wrapping_add(1);
                
                // Thread 0 is responsible for cleanup when DEFERRED_THREAD_CLEANUP is enabled
                // Clean up every 10 iterations (not too frequent to avoid overhead)
                if loop_counter.is_multiple_of(10) {
                    let cleaned = threading::cleanup_terminated();
                    // Per cleanup pass, i.e. continuously under any fork-heavy
                    // workload — 39 lines in a plain boot-suite run. The COUNT is
                    // the whole content, and `[PSTATS]` already reports thread
                    // churn, so this is a trace and belongs behind the flag.
                    if cleaned > 0 && config::SYSCALL_DEBUG_INFO_ENABLED {
                        // Safe print without heap allocation to prevent panics
                        console::print("[Thread0] Cleaned ");
                        console::print_dec(cleaned);
                        console::print(" terminated threads\n");
                    }
                    // Second of `process::reclaim`'s vetted drain sites, and the one
                    // that covers the regime where netpoll_maint starves: if memory
                    // pressure is bad enough to block the maintenance thread, something
                    // is blocked, and the idle loop is what runs. No drop-path lock is
                    // held here — this is the same context that already runs
                    // `cleanup_terminated`'s kernel-stack frees.
                    akuma_exec::process::reclaim::drain_retired_if_requested();
                    // Announce any live thread that has overrun its kernel stack.
                    // Latched per stack, so this is one canary read per allocated
                    // slot and at most one line per overflow. The idle loop is the
                    // right home: an overflow whose damage hangs or panics the box
                    // never reaches the teardown check in `free_stack_for_slot`.
                    threading::report_overrun_stack_canaries();
                }
                
                // Heartbeat every 1000 iterations to show thread 0 is alive
                static HEARTBEAT_DUMP_CTR: core::sync::atomic::AtomicU64 =
                    core::sync::atomic::AtomicU64::new(0);
                if loop_counter.is_multiple_of(crate::config::THREADING_HEARTBEAT_INTERVAL) {
                    // Safe print without heap allocation to prevent panics
                    let stats = threading::thread_stats_full();
                    console::print("[Thread0] loop=");
                    console::print_u64(loop_counter);
                    console::print(" | run=");
                    console::print_dec(stats.running);
                    console::print(" rdy=");
                    console::print_dec(stats.ready);
                    console::print(" wait=");
                    console::print_dec(stats.waiting);
                    console::print(" term=");
                    console::print_dec(stats.terminated);
                    console::print(" init=");
                    console::print_dec(stats.initializing);
                    console::print(" free=");
                    console::print_dec(stats.free);
                    console::print("\n");

                    // Deadlock-hunt aid: every ~8th heartbeat, dump where each
                    // non-idle thread is parked in kernel code. Only when there
                    // are blocked threads (a hang signature), to keep it quiet.
                    if crate::config::DEADLOCK_THREAD_DUMP_ENABLED {
                        HEARTBEAT_DUMP_CTR.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                        if HEARTBEAT_DUMP_CTR.load(core::sync::atomic::Ordering::Relaxed).is_multiple_of(8)
                            && stats.waiting >= 2
                        {
                            threading::dump_thread_resume_points();
                            crate::syscall::pipe::pipe_dump();
                            crate::syscall::futex_dump();
                        }
                    }
                }

                threading::yield_now();

                // Genuinely halt until the next interrupt (timer tick ~10 ms, or a
                // device IRQ). Without this the boot/idle thread is a pure
                // `loop { yield_now() }` busy-spin: yield_now() only fires a self-SGI
                // and returns immediately, so when no other thread is runnable this
                // thread and the network poller ping-pong SGIs at microsecond
                // granularity and pin the host vCPU at 100% even at idle. idle_halt
                // (not raw wfi) also keeps CPU-time accounting honest so the halt
                // isn't billed as busy time. The secondary cores already WFI in their
                // idle loops (smp.rs); this is the BSP equivalent.
                // See docs/KNOWN_ISSUES.md issue #11.
                threading::idle_halt();
            }

            console::print("[AsyncMain] Preemtive main thread terminated\n");
        }
        Err(e) => {
            console::print("[AsyncMain] Preemtive main thread failed: ");
            console::print(e);
            console::print("\n");
        }
    }

    console::print("System halted\n");
    halt();
}

/// One pass of the network drain: post buffers, drain every ready frame, hand
/// wakes to whoever was waiting.
///
/// Split out of the async-main loop so a dedicated thread can run it at packet
/// rate while [`netpoll_maint_step`]'s housekeeping runs at its own, much slower
/// cadence. The two used to share one loop body, which meant the BKL-held
/// housekeeping was re-entered once per network wake: measured 15,412 laps/s
/// under HTTP load against the ~100/s the Phase 7 audit assumed when it decided
/// not to carve `netpoll_maint` (`BKL_PHASE7_AUDIT.md` §2.6 — written before the
/// NIC had an interrupt at all, so its premise was 150x stale).
///
/// This function itself takes **no BKL**. Everything `poll()` touches has its own
/// lock, which is what `no-bkl-network` already established; the dropped window
/// below is that carve-out.
/// Rump-only builds have no smoltcp interface to poll.
///
/// The maintenance thread's structure is shared between the two stacks, so this
/// keeps its call site unconditional rather than sprinkling `#[cfg]` through the
/// loop body. Rump's own RX path is driven by the tap device
/// (`akuma_net::rump_tap`), not from here.
///
/// Its absence is one of four lost `smoltcp` gates that made
/// `scripts/build_devbox.sh` fail to compile — see `akuma-net`'s `lib.rs`
/// comment on `pub mod smoltcp_net` for the class of mistake.
#[cfg(not(feature = "smoltcp"))]
#[inline]
#[cfg(target_os = "none")]
fn netpoll_drain_step() {}

#[cfg(feature = "smoltcp")]
#[inline]
#[cfg(target_os = "none")]
fn netpoll_drain_step() {
    // Poll network stack in a loop until no more progress.
    // Each poll() may only process one RX packet (single VirtIO buffer),
    // so we need to loop to drain bursts of incoming packets. This is
    // critical for bulk transfer throughput (e.g. git clone over SSH):
    // without draining, TCP ACKs/window updates are delayed until the
    // next scheduler slot, causing the remote sender's TCP window to
    // shrink and throughput to collapse.
    // Rump-only builds have no smoltcp interface to poll; the rump stack is
    // driven by rump_server + the per-box sysproxy path instead.
    #[cfg(feature = "smoltcp")]
    {
        // Profiler only: isolate the burst-drain itself from the maintenance work
        // above it (BKL_VFS_CARVE_OUT.md §19 sub-tag split).
        #[cfg(kernel_smp_shared)]
        akuma_exec::sync::set_holder_tag(
            akuma_exec::bkl::current_core_id(),
            akuma_exec::sync::HOLD_TAG_NETPOLL_DRAIN,
        );
        // BKL carve-out (§19.3/§20): every piece of state `poll()` touches (`NETWORK`,
        // transitively `SOCKET_TABLE` via the post-drop wake pass) is already behind
        // its own `PreemptGuard`-protected lock, so the drain doesn't need the BKL for
        // exclusivity — same precedent as `NetBklGuard` (src/syscall/net.rs), whose
        // mechanism this reuses directly. Gated on `kernel_no_bkl_network`
        // specifically, not just `kernel_smp_shared`: that is what makes
        // `PreemptGuard::new()` mask IRQs for the inner `NETWORK` hold, which is what
        // keeps a nested IRQ from ever observing this core "holding NETWORK, wanting
        // the BKL" — the AB-BA shape the `PreemptGuard` doc warns about.
        #[cfg(all(kernel_smp_shared, kernel_no_bkl_network))]
        akuma_exec::bkl::dropped_window_open();
        // Re-arm the netpoll doorbell BEFORE draining, not after.
        //
        // Re-arming afterwards left a window: a packet arriving after the
        // last `poll()` returned false but before the store is missed by
        // this drain, finds `NETPOLL_WAKE_PENDING` still set so its ringer
        // raises NO broadcast, and is then erased by the store. Every core
        // reaches the `wfi` below and sleeps to the 3 ms tick — one
        // swallowed wake, one 3 ms request. That is the shape the tail
        // actually has: Akuma's MINIMUM round trip beats the Linux control
        // (378 us vs 519 us) while p99 is 4.6x worse, i.e. most requests are
        // fine and a minority fall off a tick-shaped cliff.
        //
        // Clearing first inverts the race into a harmless one: a packet
        // landing during the drain now finds the doorbell clear, broadcasts,
        // and leaves an SGI pending that makes the trailing `wfi` return
        // immediately — so the next lap drains it instead of the tick doing
        // it 3 ms later. The cost is one extra SGI per drain that overlaps
        // an arrival, still bounded by the coalescer (a burst mid-drain
        // still rings once, not once per frame).
        //
        // See docs/archive/AKUMA_NET_ISSUES.md §9. Originally NIC-interrupt-only
        // (`NIC_WAKE_PENDING`); renamed when a loopback frame push
        // (`akuma_net::runtime::NetRuntime::wake_netpoll`) became a second ringer
        // — see `ring_netpoll_doorbell` and `docs/archive/LOOPBACK_RING_CONVERSION.md`.
        #[cfg(kernel_smp_shared)]
        NETPOLL_WAKE_PENDING.store(false, core::sync::atomic::Ordering::Relaxed);
        let mut polls = 0u32;
        while akuma_net::smoltcp_net::poll() {
            polls += 1;
            if polls >= 64 {
                break; // Safety cap to avoid starving other threads
            }
        }
        #[cfg(all(kernel_smp_shared, kernel_no_bkl_network))]
        akuma_exec::bkl::dropped_window_close();
    }
}

/// Run the async main loop
///
/// This is the main entry point for async networking.
/// Runs on thread 0 (boot thread) which has a 1MB stack (config::KERNEL_STACK_SIZE).
/// This is sufficient for deep async call chains (SSH, HTTP, etc.).
///
/// Note: Thread 0 uses the boot stack at 0x40700000-0x40800000 which is
/// protected by stack canaries checked periodically in this loop.
#[cfg(target_os = "none")]
fn run_async_main() -> ! {
    use core::future::Future;
    use core::pin::pin;
    use core::task::{Context, Waker};

    // Register this thread as the network poller so the scheduler boost targets it.
    //
    // Under `rump-default` (devbox), this thread drives the (compiled-but-unused)
    // smoltcp stack and does no real work — so it must NOT claim the boost slot.
    // `rump_proxy::start_default_stack` claims it instead, registering
    // `rump_server`'s own main OS thread (the fiber scheduler that services every
    // proxied socket syscall) — NOT the `attach_server` handshake kthread, which
    // parks after `Client::connect` and does no per-call work. See
    // `overlays/devbox/README.md` "Rump net thread starve under CPU-bound load"
    // and `docs/runbooks/debug-devbox.md`.
    #[cfg(not(feature = "rump-default"))]
    threading::set_network_thread_id(threading::current_thread_id());

    // =========================================================================
    // Skip async network if disabled (for debugging)
    // =========================================================================
    if config::SKIP_ASYNC_NETWORK {
        console::print("[AsyncMain] Network SKIPPED via config::SKIP_ASYNC_NETWORK\n");
        console::print("[Idle] Entering minimal idle loop...\n");
        
        // Enable IRQs so timer can fire
        akuma_primitives::irq::unmask_irqs();
        
        loop {
            threading::yield_now();
        }
    }

    // =========================================================================
    // Network initialization and main loop
    // =========================================================================
    console::print("\n--- Network Initialization ---\n");

    // Initialize the akuma-net networking stack. The MMIO slot table lives in
    // `akuma_virtio::slot_addr` now — this used to be a fifth copy of the table.
    if let Err(e) = akuma_net::init(
        akuma_net::NetRuntime {
            uptime_us: timer::uptime_us,
            utc_seconds: timer::utc_seconds,
            yield_now: threading::yield_now,
            // NOT `threading::blocking_relax` — the socket variant, which skips the
            // `yield_now` before halting. Worth +27 % req/s and half the p90 here,
            // and unsafe kernel-wide (it wedges the spawn/reap path at SMP=4).
            // Rationale and the A/B numbers: `threading::blocking_relax_net`.
            blocking_relax: threading::blocking_relax_net,
            // The blocking-socket waiter's park. Unlike `blocking_relax` this
            // marks the thread WAITING, which is what lets `wake_all()` on the
            // socket target it directly instead of leaving it to notice on a
            // timer — see `NetRuntime::park_until`.
            park_until: threading::schedule_blocking,
            current_waker: || threading::get_waker_for_thread(threading::current_thread_id()),
            current_core_id: akuma_exec::bkl::current_core_id,
            current_box_id: || process::current_process_shared().map_or(0, |p| p.box_id),
            // Combined Ctrl-C + pthread_kill check, so a socket read blocked in
            // `wait_until` honours `tkill` the same way pipe/wait loops do.
            is_current_interrupted: process::should_interrupt_blocking_syscall,
            rng_fill: |buf| rng::fill_bytes(buf).expect("RNG required for networking"),
            current_thread_id: || threading::current_thread_id() as u32,
            // Loopback frames never touch virtio, so they have no interrupt of
            // their own to end a parked core's `wfi`/`blocking_relax` — this is
            // the same doorbell `nic_irq_handler` rings for real packets, called
            // instead from `LoopbackRing::push`. See `ring_netpoll_doorbell` and
            // `docs/archive/LOOPBACK_RING_CONVERSION.md`.
            wake_netpoll: ring_netpoll_doorbell,
        },
        config::ENABLE_DHCP,
    ) {
        console::print("[Net] Network init failed: ");
        console::print(e);
        console::print("\n");
        console::print("[Idle] Entering idle loop (no network)\n");
        loop {
            threading::yield_now();
        }
    }

    // ── virtio-net RX interrupt ────────────────────────────────────────────
    //
    // Until 2026-08-19 the timer (PPI 27) was the ONLY device interrupt this
    // kernel registered, so the entire network stack was tick-driven: the
    // async-main netpoll loop ends each lap in `wfi`, and `wait_until` parks a
    // blocked socket reader in `blocking_relax` (`yield_now` + `wfi`). With no
    // NIC interrupt the only thing that can end either wait is the scheduler
    // tick — 3 ms on this host, because QEMU HVF declines to honour a WFI
    // deadline below ~2.5 ms (`akuma_timer::policy::pick_tick`). Measured
    // consequence: blocked readers parked for 4.9 ms on average, and an HTTP
    // request that needs two such waits cost ~7 ms against Linux's ~0.55 ms.
    // Full measurements: docs/archive/AKUMA_NET_ISSUES.md §3.1.
    //
    // Registering the NIC's SPI makes an arriving packet end the `wfi`
    // immediately. The handler does nothing but acknowledge the device — see
    // `nic_irq_handler` — because *returning from WFI* is the whole benefit;
    // the netpoll loop is already sitting right behind it.
    #[cfg(feature = "smoltcp")]
    if let Some(slot) = akuma_net::smoltcp_net::nic_slot() {
        let intid = VIRTIO_MMIO_SPI_BASE + slot;
        irq::register_handler(intid, nic_irq_handler);
        safe_print!(96, "[Net] virtio-net IRQ: slot {} -> INTID {}\n", slot, intid);
    } else {
        console::print("[Net] no virtio-net slot recorded; RX stays tick-driven\n");
    }

    console::print("--- Network Initialization Done ---\n\n");

    // Platform switch for docs/archive/MISSING_NTP_SYSCALLS.md: QEMU virt's
    // PL031 already set the clock back in kernel_main (init_utc_from_rtc), so
    // this is a no-op there. A platform with no RTC (Firecracker) leaves
    // utc_time_us() None at this point — that IS the "no RTC" signal, no
    // separate board check needed — so fall back to a boot-time SNTP round
    // trip instead of leaving the clock stuck at 1970. Runs before IRQs are
    // unmasked below, so it busy-polls the network stack directly; see
    // ntp_boot::try_bootstrap_clock. Never fatal either way — just report
    // which of the two happened.
    //
    // One safe_print! call per outcome, not several console::print calls in
    // a row: this runs with IRQs still masked, but try_bootstrap_clock's own
    // wait loop cooperatively yields while polling, so other ready threads
    // (boot self-test suite spawns, at this point in boot) can genuinely run
    // between separate console::print calls and tear the line — safe_print!
    // formats into one stack buffer and flushes it as a single emit(), so
    // it can't be interleaved mid-message.
    if timer::utc_time_us().is_none() && config::ENABLE_NTP_BOOTSTRAP {
        match ntp_boot::try_bootstrap_clock() {
            Ok(()) => safe_print!(96, "[NTP] boot-time clock sync succeeded: {}\n", timer::utc_iso8601()),
            Err(e) => safe_print!(128, "[NTP] boot-time clock sync failed: {}\n", e),
        }
    }

    // rump feature: bind the BSP's rump tap (/dev/net/tap0) to NIC1 on virtio-mmio-bus.4
    // (RUMP_NIC=1), leaving NIC0 on smoltcp above. Bound to that SPECIFIC slot — not "the 2nd
    // virtio-net" — so it never claims bus.5. If bus.4 has no device (RUMP_NIC=0), init_at
    // fails gracefully and /dev/net/tap0 stays ENODEV.
    #[cfg(feature = "rump")]
    match akuma_net::rump_tap::init_at(mmu::DEV_VIRTIO_VA + 4 * 0x200) {
        Ok(mac) => {
            crate::safe_print!(
                128,
                "[rump] /dev/net/tap0 bound to NIC1 (bus.4), MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}\n",
                mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
            );
        }
        Err(e) => {
            crate::safe_print!(128, "[rump] BSP tap not available: {} (run QEMU with RUMP_NIC=1)\n", e);
        }
    }

    // Run network self-tests if enabled (smoltcp-only suites)
    #[cfg(all(kernel_tests, feature = "smoltcp"))]
    if config::RUN_NETWORK_TESTS {
        (boot_test_hooks().network_tests)();
    }

    // Recompute here (different function from kernel_main's boot_tests_enabled):
    // these spawn-heavy suites are skipped on tiny machines, see kernel_main.
    // Both suites are smoltcp/SSH-coupled, so they compile out with the native stack.
    #[cfg(all(kernel_tests, feature = "smoltcp"))]
    {
        let ram = akuma_exec::mmu::ram_end().saturating_sub(akuma_exec::mmu::ram_base());
        let low_mem_skip_tests = config::LOW_MEM_TEST_SKIP_MB != 0
            && ram <= config::LOW_MEM_TEST_SKIP_MB * 1024 * 1024;
        if !config::DISABLE_ALL_TESTS && !low_mem_skip_tests {
            (boot_test_hooks().process_network_tests)();
        }
    }

    // Rump sysproxy / scheduling regression guards. Compile under `rump` (so
    // they also run on default-smoltcp builds that opt a herd box into rump),
    // not gated on `rump-default`. See `src/rump_tests.rs`.
    #[cfg(all(
        not(kernel_profile_extreme),
        feature = "rump",
        any(not(feature = "no-tests"), feature = "rump-tests"),
    ))]
    // `require`, not `get`: this static and `rust_start`'s registration carry the
    // identical cfg, so an absent hook here means the two drifted apart — and the
    // old `if let Some` spelling turned that into a regression suite silently not
    // running, which is the failure this whole class of hook is meant to make loud.
    if !config::DISABLE_ALL_TESTS {
        (RUMP_TESTS_HOOK.require())();
    }

    // devbox: make the rump stack the DEFAULT for box 0 (the root box) and bring
    // up its rump_server here, so every unboxed process is rump-networked with no
    // herd box / join_box. Mutually exclusive with the demo below (both spawn a
    // box-0 rump_server; the default-stack one is persistent and real).
    #[cfg(feature = "rump-default")]
    rump_proxy::start_default_stack();

    // Kernel-as-client sysproxy demo (RUMP_SYSPROXY.md Step 4): only with
    // RUMP_NIC=1; spawns /bin/rump_server and drives rump_sys_socket over a
    // kernel pipe. Skips cleanly when NIC1 / rump_server is absent. Suppressed
    // when rump-default owns box 0's stack (avoid a second box-0 rump_server).
    #[cfg(all(feature = "rump", not(feature = "rump-default")))]
    rump_proxy::run_rump();

    console::print("[Main] SSH is the userspace /bin/sshd
");

    safe_print!(1024, "[Main] Network ready! Running background polling loop.\n");

    // Enable IRQs for the main loop
    akuma_primitives::irq::unmask_irqs();

    // Auto-start herd process supervisor
    let (_herd_tid, mut herd_channel) = if config::AUTO_START_HERD && fs::is_initialized() {
        const HERD_PATH: &str = "/bin/herd";
        const HERD_ARGS: &[&str] = &["daemon"];
        if fs::exists(HERD_PATH) {
            crate::safe_print!(64, "[Main] Starting herd supervisor...\n");
            match process::spawn_process_with_channel(HERD_PATH, Some(HERD_ARGS), None) {
                Ok((tid, channel, _pid)) => {
                    crate::safe_print!(64, "[Main] Herd started (tid={})\n", tid);
                    (tid, Some(channel))
                }
                Err(e) => {
                    crate::safe_print!(64, "[Main] ERROR: Failed to start herd: {}\n", e);
                    (0, None)
                }
            }
        } else {
            crate::safe_print!(64, "[Main] WARNING: /bin/herd not found, supervisor disabled\n");
            (0, None)
        }
    } else {
        (0, None)
    };

    // No supervisor: start the userspace sshd ourselves, or the box has no way in.
    // Port 22 (the QEMU hostfwd target) with /bin/sh as the login shell — the same
    // command line herd's sshd.conf would hand it, minus the core pinning.
    if config::AUTO_START_SSHD && _herd_tid == 0 && fs::is_initialized() {
        const SSHD_PATH: &str = "/bin/sshd";
        let sshd_args: [&str; 4] = ["--port", "22", "--shell", config::USERSPACE_SSHD_SHELL];
        if fs::exists(SSHD_PATH) {
            crate::safe_print!(64, "[Main] Starting userspace sshd (no supervisor)...\n");
            match process::spawn_process(SSHD_PATH, Some(&sshd_args), None) {
                Ok(tid) => crate::safe_print!(64, "[Main] sshd started (tid={})\n", tid),
                Err(e) => crate::safe_print!(64, "[Main] ERROR: Failed to start sshd: {}\n", e),
            }
        } else {
            crate::safe_print!(64, "[Main] WARNING: /bin/sshd not found, no SSH available\n");
        }
    }

    // Loop iteration counter for debugging hangs
    use core::sync::atomic::{AtomicU64, Ordering};
    static LOOP_COUNTER: AtomicU64 = AtomicU64::new(0);
    static LAST_HEARTBEAT_US: AtomicU64 = AtomicU64::new(0);
    const HEARTBEAT_INTERVAL_US: u64 = 30_000_000; // 30 seconds
    
    // Pin memory monitor
    let mut mem_monitor_pinned = pin!(memory_monitor());
    
    // The executor below never parks on a waker — it polls, drains and loops —
    // so the no-op waker is all it needs. `Waker::noop()` is the safe, `const`
    // stdlib spelling of the hand-rolled `RawWakerVTable` of four empty closures
    // this used to build behind `Waker::from_raw`.
    let mut cx = Context::from_waker(Waker::noop());

    // Measurement builds only: start attributing cross-core BKL wait to the holder.
    #[cfg(kernel_bkl_profile)]
    crate::bkl_profile::init();

    loop {
        timer::NETPOLL_ITERS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        // Separate from NETPOLL_ITERS, which the tick governor swaps to 0 every
        // window and so cannot be read for anything else. This is the async-main
        // LAP rate — how often the loop wakes, drains and halts again. Distinct
        // from `[NICSTAT] poll=`, which counts `smoltcp_net::poll()` calls from
        // ALL callers (this drain, every `wait_until`, epoll, and the post-op
        // flush in send/recv), and is therefore several times larger.
        #[cfg(feature = "net-profile")]
        crate::nic_profile::NETPOLL_LAPS.fetch_add(1, Ordering::Relaxed);
        // Profiler only: name the top-of-iteration housekeeping (heartbeat/pstats
        // logging, reclaim_terminated_slots, bkl_profile::maybe_dump) separately from
        // the smoltcp drain and herd polling below it, so a `netpoll` measurement
        // decomposes instead of crediting the whole iteration to one bucket
        // (BKL_VFS_CARVE_OUT.md §19).
        #[cfg(all(kernel_smp_shared, feature = "smoltcp"))]
        akuma_exec::sync::set_holder_tag(
            akuma_exec::bkl::current_core_id(),
            akuma_exec::sync::HOLD_TAG_NETPOLL_MAINT,
        );

        // Periodic heartbeat
        let count = LOOP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let now_us = timer::uptime_us();
        let last_heartbeat = LAST_HEARTBEAT_US.load(Ordering::Relaxed);
        if now_us.saturating_sub(last_heartbeat) >= HEARTBEAT_INTERVAL_US {
            LAST_HEARTBEAT_US.store(now_us, Ordering::Relaxed);
            let tid = threading::current_thread_id();
            // RX counters ride along on the heartbeat: a stack that polls happily
            // while receiving nothing is otherwise indistinguishable from a stack
            // that is wedged. `posted` climbing with `recvd` stuck at 0 means the
            // device has buffers and is not filling them — which is a very
            // different bug from "we never offered one".
            #[cfg(feature = "smoltcp")]
            let (posted, begin_fail, recvd) = akuma_net::smoltcp_net::rx_counters();
            #[cfg(not(feature = "smoltcp"))]
            let (posted, begin_fail, recvd) = (0usize, 0usize, 0usize);
            crate::safe_print!(160,
                "[Heartbeat] Loop {} | T{} | SmolNet Active | rx posted={} fail={} recvd={}\n",
                count, tid, posted, begin_fail, recvd
            );
        }

        static LAST_PSTATS_US: AtomicU64 = AtomicU64::new(0);
        const PSTATS_INTERVAL_US: u64 = 30_000_000; // 30 seconds
        let last_ps = LAST_PSTATS_US.load(Ordering::Relaxed);
        if now_us.saturating_sub(last_ps) >= PSTATS_INTERVAL_US {
            LAST_PSTATS_US.store(now_us, Ordering::Relaxed);
            akuma_exec::process::dump_running_process_stats();
            // ext2 block-cache instrumentation, on the same 30s cadence. The
            // cache cap (src/fs.rs) can only be sized against a measured hit
            // rate for the real workload, and `cache_stats()` had no in-kernel
            // reader. PMM free + heap total ride along because raising the cap
            // is exactly the change that regressed both (see src/fs.rs).
            {
                let (hits, misses) = akuma_ext2::cache_stats();
                let (used, cap) = akuma_ext2::cache_occupancy();
                let total = hits + misses;
                let pct = (hits * 100).checked_div(total).unwrap_or(0);
                crate::safe_print!(192,
                    "[FSCACHE] hits={} misses={} hit_pct={} slots={}/{} pmm_free={} pmm_total={} heap_mb={}\n",
                    hits, misses, pct, used, cap,
                    crate::pmm::free_count(), crate::pmm::total_count(),
                    crate::allocator::stats().heap_size / 1024 / 1024);
            }
            // Shared read-only file pages, same cadence. `hits` is the number of
            // private frame allocations + `read_at` sweeps this cache avoided, which
            // is the direct measure of the `-j4` amplification it exists to remove.
            if crate::config::SHARED_FILE_PAGES_ENABLED {
                let mut w = console::StackWriter::<192>::new();
                crate::file_page_cache::stats_line(&mut w);
                w.flush();
            }
            // `MADV_DONTNEED` divergence audit (docs/archive/CARGO_HEAP_NULL_RC.md).
            // Both counters are expected to stay 0; either going non-zero means
            // this handler is zeroing frames Linux would have left alone.
            {
                let mut w = console::StackWriter::<128>::new();
                crate::syscall::mem::dontneed_audit_line(&mut w);
                w.flush();
            }
            // Write faults on pages the page table already allows — i.e. absorbed
            // stale TLB entries. `repeats` must stay 0; a non-zero value means the
            // flush is not what is resolving them (docs/archive/CARGO_HEAP_NULL_RC.md).
            {
                use core::sync::atomic::Ordering;
                crate::safe_print!(128, "[TLB] stale_write_faults={} repeats={}\n",
                    exceptions::STALE_TLB_WRITE_FAULTS.load(Ordering::Relaxed),
                    exceptions::STALE_TLB_REPEATS.load(Ordering::Relaxed));
            }
            // Per-thread identity cache health. `fallbacks` is expected to be
            // non-zero only transiently (a thread resolving before its map entry
            // lands); a climbing steady-state value means a writer bypassed the
            // `thread_pid_map_insert`/`_remove` wrappers. `epi_stale`/`epi_moved`
            // are the epilogue audit and only move when `config::IDENTITY_AUDIT`
            // is on — both must stay 0. See
            // docs/archive/IDENTITY_CACHE_SMP_REVIEW.md.
            {
                use core::sync::atomic::Ordering;
                use akuma_exec::process::table;
                crate::safe_print!(160,
                    "[IDENT] hits={} miss={} unstamped={} cleared={} inactive={} null={}\n",
                    table::IDENTITY_HITS.load(Ordering::Relaxed),
                    table::IDENTITY_FALLBACKS.load(Ordering::Relaxed),
                    table::IDENTITY_FB_UNSTAMPED.load(Ordering::Relaxed),
                    table::IDENTITY_FB_CLEARED.load(Ordering::Relaxed),
                    table::IDENTITY_FB_INACTIVE.load(Ordering::Relaxed),
                    table::IDENTITY_FB_NULL.load(Ordering::Relaxed));
                crate::safe_print!(128, "[IDENT] epi_stale={} epi_moved={} audit={}\n",
                    table::EPILOGUE_STALE_IDENTITY.load(Ordering::Relaxed),
                    table::EPILOGUE_IDENTITY_MOVED.load(Ordering::Relaxed),
                    u8::from(crate::config::IDENTITY_AUDIT));
                // Lazy re-stamp (docs/archive/IDENTITY_CACHE_LAZY_RESTAMP.md).
                // `repairs` counts threads rescued from a permanent slow path;
                // `repair_failed` is the bounded waste and should stay near 0.
                crate::safe_print!(128, "[IDENT] repairs={} repair_failed={} stale_gen={}\n",
                    table::IDENTITY_REPAIRS.load(Ordering::Relaxed),
                    table::IDENTITY_REPAIR_FAILED.load(Ordering::Relaxed),
                    table::IDENTITY_FB_STALE_GEN.load(Ordering::Relaxed));
            }
            // Per-core exception-vector entry counts (docs/archive/PAGE_TABLE_UAF_BKL_STORM.md):
            // a core stuck in the unreachable-vector storm never gets past the vector's own
            // first instruction fetch, so its count here freezes solid while healthy cores'
            // counts keep climbing — a live, external-readable confirmation of "zero forward
            // progress on this core" that doesn't depend on catching byte-identical registers
            // across a gdb sample gap. Always prints all MAX_CORES=8 slots — idle/nonexistent
            // cores just read 0, which is self-explanatory.
            {
                use core::sync::atomic::Ordering;
                let e = &exceptions::EXCEPTION_ENTRIES;
                crate::tprint!(160,
                    "[EXC] core0={} core1={} core2={} core3={} core4={} core5={} core6={} core7={}\n",
                    e[0].load(Ordering::Relaxed), e[1].load(Ordering::Relaxed),
                    e[2].load(Ordering::Relaxed), e[3].load(Ordering::Relaxed),
                    e[4].load(Ordering::Relaxed), e[5].load(Ordering::Relaxed),
                    e[6].load(Ordering::Relaxed), e[7].load(Ordering::Relaxed));
                // Decompose the IRQ share by INTID (companion counter, see
                // exceptions::IRQ_BY_INTID). Only nonzero slots print, so this
                // stays one short line on a healthy system.
                {
                    let mut w = console::StackWriter::<192>::new();
                    let _ = core::fmt::Write::write_str(&mut w, "[IRQS]");
                    for (id, c) in exceptions::IRQ_BY_INTID.iter().enumerate() {
                        let v = c.load(Ordering::Relaxed);
                        if v != 0 {
                            let _ = core::fmt::Write::write_fmt(
                                &mut w, format_args!(" {id}={v}"));
                        }
                    }
                    let sp = exceptions::SPURIOUS_IRQS.load(Ordering::Relaxed);
                    let _ = core::fmt::Write::write_fmt(
                        &mut w, format_args!(" spurious={sp}\n"));
                    w.flush();
                }
                // Vector-class decomposition + per-EC histograms of the sync
                // classes (nonzero buckets only) — the discriminator for the
                // ">1M vector entries/s with <10K/s of attributable work"
                // storm (CROSS_CORE_THREAD_COLLAPSE.md §3).
                {
                    let c = &exceptions::EXC_BY_CLASS;
                    let mut w = console::StackWriter::<224>::new();
                    let _ = core::fmt::Write::write_fmt(&mut w, format_args!(
                        "[EXCC] el0={} el1={} irq={} other={} |",
                        c[0].load(Ordering::Relaxed), c[1].load(Ordering::Relaxed),
                        c[2].load(Ordering::Relaxed), c[3].load(Ordering::Relaxed)));
                    for (ec, v) in exceptions::SYNC_EC_EL0.iter().enumerate() {
                        let v = v.load(Ordering::Relaxed);
                        if v != 0 {
                            let _ = core::fmt::Write::write_fmt(
                                &mut w, format_args!(" e0.{ec:#x}={v}"));
                        }
                    }
                    for (ec, v) in exceptions::SYNC_EC_EL1.iter().enumerate() {
                        let v = v.load(Ordering::Relaxed);
                        if v != 0 {
                            let _ = core::fmt::Write::write_fmt(
                                &mut w, format_args!(" e1.{ec:#x}={v}"));
                        }
                    }
                    for (k, c) in &exceptions::MRS_TRAP_ENCODINGS {
                        let key = k.load(Ordering::Relaxed);
                        if key != 0 {
                            let _ = core::fmt::Write::write_fmt(&mut w, format_args!(
                                " mrs.{:#x}={}", key - 1, c.load(Ordering::Relaxed)));
                        }
                    }
                    let _ = core::fmt::Write::write_str(&mut w, "\n");
                    w.flush();
                }
            }
            // Deadlock-hunt aid: the Thread-0 heartbeat's dump trigger fires
            // every 50M idle loops (~never with idle_halt); piggyback on the
            // 30s PSTATS cadence instead so a wedged thread's parked resume
            // point is actually observable.
            if crate::config::DEADLOCK_THREAD_DUMP_ENABLED {
                threading::dump_thread_resume_points();
                crate::syscall::pipe::pipe_dump();
                crate::syscall::futex_dump();
            }
        }

        // Collect cooled-down terminated thread slots.
        //
        // Thread 0's idle loop also does this, but only when nothing else is
        // runnable — i.e. never while the system is busy, which is exactly when
        // slots churn fastest. This loop runs on a system thread (not thread 0)
        // and keeps running under load, so it is the collector that matters for
        // steady-state reclamation; without it, slots sat TERMINATED for tens of
        // seconds and `fork` failed with a full pool while RAM was free
        // (docs/archive/BKL_VFS_CARVE_OUT.md §11.4). The cooldown is still
        // honored, so this cannot recycle a slot whose thread is still on its
        // kernel stack.
        static LAST_RECLAIM_US: AtomicU64 = AtomicU64::new(0);
        const RECLAIM_INTERVAL_US: u64 = 100_000; // 100ms
        let last_reclaim = LAST_RECLAIM_US.load(Ordering::Relaxed);
        if now_us.saturating_sub(last_reclaim) >= RECLAIM_INTERVAL_US {
            LAST_RECLAIM_US.store(now_us, Ordering::Relaxed);
            threading::reclaim_terminated_slots();
            // Same steady-state-collector reasoning as reclaim_terminated_slots above,
            // for RETIRED process-table slots (Phase 7e "Free" half): without a caller
            // that runs under load, not just idle, zombies awaiting their cooldown pile
            // up and register_process's on-demand retry (table.rs) is the only other
            // path that collects them. See
            // docs/archive/BKL_PHASE7E_PROCESS_TABLE_RECLAIM.md.
            akuma_exec::process::reclaim_retired_processes();
        }

        #[cfg(kernel_bkl_profile)]
        crate::bkl_profile::maybe_dump(now_us);
        #[cfg(feature = "net-profile")]
        crate::nic_profile::maybe_dump(now_us);

        GLOBAL_POLL_STEP.store(1, Ordering::Relaxed);
        netpoll_drain_step();

        GLOBAL_POLL_STEP.store(2, Ordering::Relaxed);
        if config::MEM_MONITOR_ENABLED {
            // Profiler only: attribute the (normally disabled) mem-monitor poll
            // separately from the drain (BKL_VFS_CARVE_OUT.md §19).
            #[cfg(all(kernel_smp_shared, feature = "smoltcp"))]
            akuma_exec::sync::set_holder_tag(
                akuma_exec::bkl::current_core_id(),
                akuma_exec::sync::HOLD_TAG_NETPOLL_MEMMON,
            );
            let _ = mem_monitor_pinned.as_mut().poll(&mut cx);
        }

        GLOBAL_POLL_STEP.store(3, Ordering::Relaxed);
        // Profiler only: isolate herd output/heartbeat polling from the drain above it
        // (BKL_VFS_CARVE_OUT.md §19 sub-tag split).
        //
        // This runs once per lap — 15,412 times/s under load — and `bkl-profile`
        // attributes 5.0-14.3 % of all contended BKL time to it. Rate-limiting it
        // to 100 ms was tried 2026-08-20 and measured NEUTRAL-TO-WORSE (req/s
        // 1,040 -> 1,002, p90 2,453 -> 2,727 us on the same machine state). Do not
        // "fix" it again without re-measuring: see AKUMA_NET_ISSUES.md §10.
        #[cfg(all(kernel_smp_shared, feature = "smoltcp"))]
        akuma_exec::sync::set_holder_tag(
            akuma_exec::bkl::current_core_id(),
            akuma_exec::sync::HOLD_TAG_NETPOLL_HERD,
        );
        // Poll herd output
        if let Some(ref channel) = herd_channel {
            if let Some(output) = channel.try_read() {
                for &byte in &output {
                    console::print_char(byte as char);
                }
            }
            if channel.has_exited() {
                let exit_code = channel.exit_code();
                crate::safe_print!(64, "[Herd] Process exited with code {}\n", exit_code);
                herd_channel = None;
            }
        }
        
        GLOBAL_POLL_STEP.store(6, Ordering::Relaxed);
        // Yield after every iteration so threads waiting on network I/O
        // (e.g. SSH sessions) can run promptly when data arrives. The
        // polling loop above already drains bursts (up to 64 packets),
        // so yielding here doesn't hurt bulk throughput — it just lets
        // consumer threads process the data between bursts.
        //
        // Under shared SMP a plain `yield_now()` here is not enough: with nothing
        // else READY on its core this loop re-runs immediately and keeps holding the
        // Big Kernel Lock, so a userspace thread (e.g. sshd, or a login shell it
        // forked) on a PEER core starves waiting for the BKL — the devbox then boots
        // but SSH sessions can't make progress. The `while poll()` above has already
        // drained every ready packet, so there is nothing more to do this iteration:
        // drop the BKL and halt until the next interrupt (mirroring the secondary
        // idle loop in smp_shared.rs). A pending RX/timer/SGI IRQ makes WFI return at
        // once, so burst draining is unaffected, while the peer core gets a BKL window
        // every iteration.
        #[cfg(all(kernel_smp_shared, feature = "smoltcp"))]
        {
            akuma_exec::bkl::leave_kernel();
            // IRQs are enabled; the timer/RX/SGI IRQ wakes us and its handler
            // re-takes the BKL (our enter_kernel below is then idempotent).
            akuma_cpu::park::wfi();
            akuma_exec::bkl::enter_kernel();
            // Profiler only: names the sliver between re-acquiring the BKL post-WFI and
            // the top-of-loop HOLD_TAG_NETPOLL_MAINT call above — negligible, but must
            // be something other than whatever HOLD_TAG_NETPOLL_HERD left behind
            // (BKL_VFS_CARVE_OUT.md §18, §19).
            akuma_exec::sync::set_holder_tag(
                akuma_exec::bkl::current_core_id(),
                akuma_exec::sync::HOLD_TAG_NETPOLL,
            );
        }
        #[cfg(not(all(kernel_smp_shared, feature = "smoltcp")))]
        threading::yield_now();

        // In a rump-only / devbox build there is no smoltcp interface to poll
        // above (the `poll()` block is #[cfg(feature = "smoltcp")]), so this
        // loop's only real work is the low-frequency herd-output / heartbeat
        // polls. Yielding alone makes it a busy-spin that pins the vCPU at 100%
        // alongside the boot thread. Genuinely halt until the next interrupt
        // (timer ~10 ms, or a rump-NIC IRQ); the periodic work above still runs
        // at tick granularity. smoltcp builds keep the busy-poll below so their
        // burst-draining throughput is untouched. See docs/KNOWN_ISSUES.md #11.
        #[cfg(not(feature = "smoltcp"))]
        threading::idle_halt();
    }
}

/// GIC INTID of virtio-mmio slot 0 on the QEMU `virt` machine.
///
/// QEMU wires virtio-mmio transport `i` to SPI `16 + i`, and an SPI's INTID is
/// `32 + spi` — so slot `i` is INTID `48 + i`. Firecracker instead allocates
/// device IRQs from `GSI_LEGACY_START = 0`, which is SPI 32, so the same slot is
/// INTID `32 + i` there. The base therefore comes from the machine description.
///
/// The slot itself is not assumed: `akuma_virtio::probe` reports which one the
/// NIC actually landed on, because neither machine's assignment order is
/// something to hard-code.
#[cfg(feature = "smoltcp")]
#[cfg(target_os = "none")]
const VIRTIO_MMIO_SPI_BASE: u32 = platform::machine::VIRTIO_INTID_BASE;

/// Set by [`ring_netpoll_doorbell`] when it has broadcast a wake, cleared
/// once the network stack has actually been polled.
///
/// This is a **doorbell coalescer**, and it is what keeps the cross-core wake
/// from becoming an IPI storm. Without it a broadcast per packet would cost
/// `(cores - 1)` SGIs per frame, each entering `sgi_scheduler_handler_with_sp`
/// and contending `POOL` — the shape behind `[SGI] POOL contended`. With it,
/// during a burst the first packet rings the doorbell and every packet after it
/// finds it already ringing, so the SGI rate is bounded by how fast the stack
/// polls rather than by how fast packets arrive.
///
/// Two ringers share this flag: [`nic_irq_handler`] for external traffic, and
/// `akuma_net::runtime::NetRuntime::wake_netpoll` (wired to
/// [`ring_netpoll_doorbell`] at registration), called after a loopback frame
/// is queued — loopback never touches virtio, so it has no interrupt of its
/// own to ring this with. Named `NIC_WAKE_PENDING` until the loopback ringer
/// was added; renamed because both ringers need exactly the same thing:
/// "something changed, wake everyone polling" — see
/// `docs/archive/LOOPBACK_RING_CONVERSION.md`.
///
/// Only exists where it is used: the doorbell is about waking *peer* cores, so a
/// single-core build (`extreme-size` drops `smp-shared`) has nobody to poke.
#[cfg(all(feature = "smoltcp", kernel_smp_shared))]
#[cfg(target_os = "none")]
static NETPOLL_WAKE_PENDING: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

/// Ring the cross-core netpoll doorbell: broadcast the scheduler SGI to every
/// core, coalesced through [`NETPOLL_WAKE_PENDING`] so a burst costs one SGI
/// per drain cycle rather than one per frame. No-op off `kernel_smp_shared`
/// (see that static's doc comment).
///
/// Two callers want exactly the same effect — end every core's
/// `wfi`/`blocking_relax` halt immediately instead of leaving it to the next
/// 3 ms timer tick — for two different reasons: [`nic_irq_handler`] (a real
/// packet arrived over virtio, so it can ride the interrupt) and
/// `akuma_net::runtime::NetRuntime::wake_netpoll` (a loopback frame was
/// queued in pure software, so nothing else would ever end that halt for it).
/// See `docs/archive/AKUMA_NET_ISSUES.md` §6.2/§9 for why broadcasting beats a
/// targeted wake, and `docs/archive/LOOPBACK_RING_CONVERSION.md` for the
/// loopback ringer.
/// Rump-only builds have no smoltcp netpoll to wake and no loopback ring to
/// ring it from, so the doorbell is a no-op. `NetRuntime::wake_netpoll` is not
/// an optional field, so it still needs something to point at.
#[cfg(not(feature = "smoltcp"))]
#[cfg(target_os = "none")]
fn ring_netpoll_doorbell() {}

#[cfg(feature = "smoltcp")]
#[cfg(target_os = "none")]
fn ring_netpoll_doorbell() {
    #[cfg(kernel_smp_shared)]
    if !NETPOLL_WAKE_PENDING.swap(true, core::sync::atomic::Ordering::AcqRel) {
        // Wake EVERY core, not just the ones with a parked socket waiter.
        //
        // Targeting was tried 2026-08-20 — a per-core count of waiters
        // (`net_waiter_park`/`_unpark`), poked with `trigger_sgi_core` — on the
        // reasoning that broadcasting ends the `wfi` on cores with nothing to do
        // (2.5 async-main laps per NIC interrupt). It measured WORSE both times:
        // laps per packet went 3.18 -> 6.5-7.6 and throughput ~1,100 -> 454-867
        // req/s, with the swallowed-wake signature (fewer `relax` parks, each
        // 2-3x longer).
        //
        // Why targeting cannot work as written: `blocking_relax` begins with
        // `yield_now`, so a waiter can resume on a core it never marked, and the
        // netpoll loop's own halt is a `wfi` rather than a park — so the set of
        // "cores that need this packet" is not knowable at interrupt time from
        // anything the waiters record. A broadcast is imprecise and cheap; a
        // precise wake here is wrong more often than it is expensive.
        // See docs/archive/AKUMA_NET_ISSUES.md §11.
        akuma_gic::broadcast_sgi(akuma_gic::SGI_SCHEDULER);
    }
}

/// virtio-net interrupt handler: acknowledge, ring the cross-core doorbell,
/// and return.
///
/// Deliberately does no network work. The interrupt exists to end a `wfi`, not
/// to deliver packets — the async-main netpoll loop and every blocked
/// `wait_until` are already polling, and they run the moment this returns. A
/// handler that touched `NETWORK` would be taking a lock the core it just
/// interrupted may be holding, which is the AB-BA wedge `PreemptGuard` exists
/// to prevent. The acknowledge is a raw MMIO write for the same reason (see
/// `akuma_net::smoltcp_net::nic_irq_ack`), which is what makes this handler
/// legal on the `no-bkl-irq` dispatch path in `rust_irq_handler_with_sp`:
/// exactly like the timer handler, it touches only device registers and
/// atomics, never BKL-protected state.
#[cfg(feature = "smoltcp")]
#[cfg(target_os = "none")]
fn nic_irq_handler(_irq: u32) {
    akuma_net::smoltcp_net::nic_irq_ack();

    // The SPI is routed to one core (GICD_IROUTER, affinity 0.0.0.0 — see
    // `gic_v3::enable_irq`), so only that core's `wfi` ends. Every other core
    // may be halted inside `blocking_relax` on behalf of a thread waiting for
    // exactly this packet — `httpd` blocked in `accept`, an ssh session blocked
    // in `recv` — and would sleep until its own timer tick. That is how the
    // interrupt fixed the *minimum* latency (3.0 ms -> 0.38 ms measured) while
    // leaving the median at milliseconds: the fast cases were the ones that
    // happened to be waiting on the routed core.
    //
    // Poking every core with the scheduler SGI ends those halts — see
    // `ring_netpoll_doorbell` for the mechanism.
    ring_netpoll_doorbell();
}

/// Async task that periodically reports memory usage
#[cfg(target_os = "none")]
async fn memory_monitor() -> ! {
    if !config::MEM_MONITOR_ENABLED {
        loop {
            threading::yield_now();
        }
    }
    use core::fmt::Write;
    use akuma_exec::alarms::{Duration, Timer};

    // `console::StackWriter`, not a local `struct Buf([u8; N], usize)` + `impl
    // Write`: that hand-rolled shape is the one `docs/reference/subsystems/console.md`
    // § "Printing rules" names as a re-implementation of the macro body, and whose
    // eight other copies the console audit removed. This one survived because it
    // predates the sweep.
    //
    // The multi-`write!` composition below is exemption 1, not a violation: the
    // `[Mem]` line is ~10 conditionally-included segments, and building it into one
    // fixed stack buffer flushed once is also what stops a peer core interleaving
    // its output into the middle of the line at SMP=4.

    // Wait a bit before starting to let system stabilize
    Timer::after(Duration::from_secs(5)).await;

    console::print("[MemMonitor] Memory monitoring started\n");

    let mut buf = console::StackWriter::<384>::new();

    loop {
        // Proactively return fully-free kernel-heap spans to the PMM so the free
        // pool recovers between workloads (idle watermark trimming). The
        // reclaim-under-pressure path in pmm::alloc_* handles the acute case;
        // this keeps the steady-state pool clean. See src/allocator.rs.
        allocator::reclaim_to_pmm();

        let stats = allocator::stats();
        let allocated_kb = stats.allocated / 1024;
        let free_kb = stats.free / 1024;
        let peak_kb = stats.peak_allocated / 1024;
        let heap_mb = stats.heap_size / 1024 / 1024;
        
        let (total_pages, allocated_pages, _) = pmm::stats();
        let total_ram_mb = (total_pages * mmu::PAGE_SIZE) / 1024 / 1024;
        let free_pages = total_pages.saturating_sub(allocated_pages);
        let free_ram_mb = (free_pages * mmu::PAGE_SIZE) / 1024 / 1024;
        // Page-precise free RAM too: at the low-memory floor the MB figure can't
        // show whether a dead process's pages actually came back (sub-MB), which
        // is exactly the "post-OOM never recovered" symptom we chase here.
        let free_ram_kb = (free_pages * mmu::PAGE_SIZE) / 1024;

        let (threads_ready, threads_running, _) = akuma_exec::threading::thread_stats();
        let threads_used = threads_ready + threads_running;
        let threads_max = akuma_exec::threading::max_threads();

        let uptime_us = timer::uptime_us();
        // Only shown when non-zero: a detected double-free means some caller's
        // free obligations are out of sync with allocations (track_user_frame/
        // cow_ref desync) — see pmm::DOUBLE_FREE_COUNT.
        let dfree = pmm::double_free_count();
        let reclaimed_pages = allocator::reclaimed_pages_total();
        let _ = write!(
            buf,
            "[Mem] Uptime {} | RAM: {}/{}MB free ({}KB) | Heap: {}/{}MB free ({} KB used, {} KB peak) | Allocs: {} | Threads: {}/{} ({}r {}rd)",
            uptime_us, free_ram_mb, total_ram_mb, free_ram_kb, free_kb / 1024, heap_mb, allocated_kb, peak_kb, stats.allocation_count,
            threads_used, threads_max, threads_running, threads_ready
        );
        if dfree > 0 {
            let _ = write!(buf, " | DOUBLE-FREE={dfree}");
        }
        // Pages handed back from the heap to the PMM since boot — non-zero means
        // the heap watermark is being trimmed (see allocator::reclaim_to_pmm).
        // Written straight into the stack buffer; no heap alloc in the mem monitor.
        if reclaimed_pages > 0 {
            let _ = write!(buf, " | reclaimed={}KB", reclaimed_pages * 4);
        }
        // UAF quarantine (docs/archive/CARGO_HEAP_NULL_RC.md): `quar` is how many
        // frames are parked awaiting their poison check, `UAF` how many were
        // written after being freed. `UAF` is the number that matters and must be
        // 0; it is shown only when the instrument is armed.
        if config::PMM_UAF_QUARANTINE {
            let (quar_len, uaf) = pmm::quarantine_stats();
            let _ = write!(buf, " | quar={quar_len} UAF={uaf}");
        }
        // `premature` counts frames released while a live address space still
        // tracked them — the premature free itself, rather than `UAF`'s downstream
        // evidence of one. It catches read-only survivors too, which `UAF` cannot
        // (docs/archive/CARGO_NULL_RC_MEMORY_REFERENCE_AUDIT.md §13.8.2), so it is
        // the stricter of the two and must be 0.
        if config::PMM_PREMATURE_FREE_CHECK {
            let _ = write!(buf, " premature={}", pmm::premature_free_count());
        }
        // Heap high-water diagnostic: how much PMM the heap is sitting on and how
        // much of it is stuck (spans pinned by a live allocation, so reclaim
        // can't return them). At the low-memory floor, `pinned` not falling back
        // to 0 after a workload exits IS the "free PMM never recovered" bug —
        // and `pinUsed` shows how few live bytes are holding it hostage
        // (fragmentation). Only printed when something is actually committed.
        let span = allocator::claimed_span_report();
        if !span.busy && span.live_spans > 0 {
            let _ = write!(
                buf,
                " | spans: {} live {}KB ({} pinned {}KB, pinUsed {}KB; {} free)",
                span.live_spans, span.committed_pages * 4,
                span.pinned_spans, span.pinned_pages * 4,
                span.pinned_used_bytes / 1024, span.free_spans
            );
        }
        let _ = writeln!(buf);
        buf.flush();

        // Stack high-water (no-op unless the probe const is on): right-sizing data
        // for the extreme kernel stacks. Printed on its own line to keep [Mem] short.
        akuma_exec::threading::report_stack_high_water();


        // Report every 10 seconds (or period from config)
        Timer::after(Duration::from_secs(config::MEM_MONITOR_PERIOD_SECONDS)).await;
    }
}
