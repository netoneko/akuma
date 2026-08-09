#![no_std]
#![no_main]
#![feature(never_type)]
#![feature(alloc_error_handler)]
// Kernel-specific: MMIO and error-code paths require these casts intentionally.
#![allow(clippy::cast_ptr_alignment)]
#![allow(clippy::cast_possible_wrap)]
#![allow(clippy::wrong_self_convention)] // kernel types don't follow std naming
#![allow(clippy::inline_always)] // used for hot syscall paths
#![allow(clippy::needless_pass_by_value)] // trait bounds often require owned types
// Rump-only build (devbox: smoltcp compiled out). The whole in-kernel interactive
// surface — the built-in shell (builtin/fs/exec commands), the `neko` editor, and
// the `async_fs` helpers behind it — is reached ONLY through the built-in SSH
// server, which is smoltcp-based and gone here. In this reduced build those
// subsystems are compiled but unused (SSH is the userspace /bin/sshd, and the
// shell is userspace busybox), so silence dead-code for this config only. The
// default/size/extreme builds keep dead-code denied.
#![cfg_attr(not(feature = "smoltcp"), allow(dead_code))]

extern crate alloc;

mod akuma;
mod allocator;
mod async_fs;
mod audio;
// mod async_net;
#[cfg(not(any(feature = "no-tests", kernel_profile_size)))]
mod async_tests;
#[cfg(kernel_bkl_profile)]
mod bkl_profile;
mod block;
mod boot;
mod config;
#[macro_use]
mod console;
#[cfg(not(any(feature = "no-tests", kernel_profile_size)))]
mod daif_tests;
#[cfg(feature = "neko")]
mod editor;
// mod embassy_net_driver;
// mod embassy_time_driver; // replaced by kernel_timer
// mod embassy_virtio_driver;
mod exceptions;
mod file_page_cache;
// fw_cfg exists only to configure ramfb, so it follows the framebuffer gate.
#[cfg(feature = "sc-framebuffer")]
mod fw_cfg;
mod kernel_timer;
mod fs;
#[cfg(not(any(feature = "no-tests", kernel_profile_size)))]
mod fs_tests;
mod gic;
#[cfg(not(feature = "gic-v2"))]
mod gic_v3;
mod irq;
#[cfg(all(not(any(feature = "no-tests", kernel_profile_size)), feature = "smoltcp"))]
mod network_tests;
mod pmm;
#[cfg(not(any(feature = "no-tests", kernel_profile_size)))]
mod process_tests;
#[cfg(not(any(feature = "no-tests", kernel_profile_size)))]
mod pthread_tests;
#[cfg(feature = "sc-framebuffer")]
mod ramfb;
mod rng;
#[cfg(feature = "rump")]
mod rump_proxy;
#[cfg(all(
    not(kernel_profile_size),
    feature = "rump",
    any(not(feature = "no-tests"), feature = "rump-tests"),
))]
mod rump_tests;
mod shell;
#[cfg(not(any(feature = "no-tests", kernel_profile_size)))]
mod shell_tests;
#[cfg(kernel_smp)]
mod smp;
// Real (shared-kernel) SMP — the inverse of the multikernel `smp` module. One
// shared kernel across all cores; see docs/reference/subsystems/smp-shared.md.
#[cfg(kernel_smp_shared)]
mod smp_shared;
#[cfg(not(any(feature = "no-tests", kernel_profile_size)))]
mod sync_tests;
// The built-in (in-kernel) SSH server. Compiled in only under
// `cfg(kernel_builtin_ssh)` (build.rs): it is built on smoltcp sockets, so it
// needs the native stack, AND it is pointless once `userspace-sshd` is on —
// there the userspace /bin/sshd serves SSH and this copy is never started.
// Gating the module (rather than only `config::ENABLE_USERSPACE_SSHD` at
// runtime) is what keeps the whole SSH-2 implementation, and the `akuma-ssh`
// crate behind it, out of the image.
#[cfg(kernel_builtin_ssh)]
mod ssh;
#[cfg(all(not(any(feature = "no-tests", kernel_profile_size)), kernel_builtin_ssh))]
mod ssh_tests;
// Every image needs some way in: either the built-in server or a userspace one.
#[cfg(all(not(kernel_builtin_ssh), not(feature = "userspace-sshd")))]
compile_error!("no in-kernel SSH without `smoltcp`; enable the `userspace-sshd` feature (userspace /bin/sshd)");
mod syscall;
#[cfg(not(any(feature = "no-tests", kernel_profile_size)))]
mod tests;
mod timer;
mod vfs;
mod virtio_hal;

use core::sync::atomic::AtomicU64;

use akuma_exec::{mmu, process, threading};
use core::panic::PanicInfo;

/// Global poll step counter for debugging hangs.
/// Used by the timer watchdog to report which step is blocking.
pub static GLOBAL_POLL_STEP: AtomicU64 = AtomicU64::new(0);

/// Halt the CPU in a low-power wait loop. Safe wrapper around wfi.
#[inline]
fn halt() -> ! {
    halt_with_code(1)
}

/// Exit QEMU with a specific exit code using ARM semihosting.
/// Requires QEMU to be started with `-semihosting` flag.
/// Falls back to wfi loop if semihosting is not available.
#[inline]
fn halt_with_code(code: u32) -> ! {
    // Use ARM semihosting SYS_EXIT_EXTENDED (0x20) to exit QEMU with a code
    // The parameter block contains [reason, exit_code]
    // ADP_Stopped_ApplicationExit = 0x20026
    let block: [u64; 2] = [0x20026, u64::from(code)];

    unsafe {
        core::arch::asm!(
            "hlt #0xf000",
            in("x0") 0x20u64,        // SYS_EXIT_EXTENDED
            in("x1") block.as_ptr(),
            options(nomem, nostack)
        );
    }

    // If semihosting is not available, fall back to wfi loop
    loop {
        // SAFETY: wfi just puts CPU in low-power state until next interrupt.
        // It has no memory safety implications.
        unsafe { core::arch::asm!("wfi") }
    }
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    console::print("\n\n!!! PANIC !!!\n");
    if let Some(location) = info.location() {
        console::print("Location: ");
        console::print(location.file());
        console::print(":");
        console::print_dec(location.line() as usize);
        console::print("\n");
    }
    // Use stack-based formatting to avoid heap allocation during panic
    // This prevents double-panic if the heap is corrupted
    console::print("Message: ");
    crate::safe_print!(256, "{}\n", info.message());
    halt()
}

// Import boot_x0_at_entry from assembly
unsafe extern "C" {
    static boot_x0_at_entry: u64;
}

/// Minimal unsafe entry point - immediately delegates to safe kernel_main
#[unsafe(no_mangle)]
pub extern "C" fn rust_start(dtb_ptr: usize) -> ! {
    // Multikernel: snapshot pristine `.data` BEFORE anything mutates a static, so
    // each secondary's replicated `.data` starts from correct initial values
    // (docs/MULTIKERNEL.md §4.2). Must be the very first action. smp-only.
    #[cfg(kernel_smp)]
    smp::snapshot_pristine_data();

    // Early debug: print raw DTB pointer before anything else
    console::print("DTB ptr from boot (x0 arg): 0x");
    console::print_hex(dtb_ptr as u64);
    console::print("\n");
    
    // Also print what was stored at very first instruction
    let x0_at_entry = unsafe { boot_x0_at_entry };
    console::print("x0 at _boot entry: 0x");
    console::print_hex(x0_at_entry);
    console::print("\n");

    kernel_main(dtb_ptr)
}

/// Scan RAM for a QEMU-generated DTB when x0 is zero.
///
/// Kernel is at 0x40100000 (text_offset = 1 MB); DTB is placed at
/// ALIGN_UP(kernel_load + image_size, 2MB) = 0x40200000.
fn scan_for_dtb() -> usize {
    const FDT_MAGIC_LE: u32 = 0xedfe0dd0; // big-endian 0xd00dfeed read as little-endian

    // DTB is at the 2MB-aligned address just above the kernel image
    // (ALIGN_UP(0x40100000 + image_size, 2MB) = 0x40200000).
    const DTB_LOCATION: usize = 0x4020_0000;

    let magic = unsafe { core::ptr::read_volatile(DTB_LOCATION as *const u32) };
    if magic == FDT_MAGIC_LE {
        let total_size = u32::from_be(unsafe { core::ptr::read_volatile((DTB_LOCATION + 4) as *const u32) });
        if (64..=16 * 1024 * 1024).contains(&total_size) {
            console::print("[DTB] Found at 0x");
            console::print_hex(DTB_LOCATION as u64);
            console::print("\n");
            return DTB_LOCATION;
        }
    }

    console::print("[DTB] Not found at expected location 0x");
    console::print_hex(DTB_LOCATION as u64);
    console::print("\n");
    0
}

/// Detect memory from Device Tree Blob.
///
/// QEMU does NOT set x0 for ELF kernels, so we scan RAM for the
/// QEMU-generated DTB when x0 is zero.
fn detect_memory(dtb_ptr: usize) -> (usize, usize) {
    const DEFAULT_RAM_BASE: usize = 0x4000_0000; // QEMU virt: 1 GB

    const DEFAULT_RAM_SIZE: usize = 256 * 1024 * 1024;
    const DTB_RESERVE: usize = 2 * 1024 * 1024; // 2 MB

    let actual_dtb_ptr = if dtb_ptr != 0 { dtb_ptr } else { scan_for_dtb() };

    if actual_dtb_ptr == 0 {
        console::print("[Memory] No DTB found, using default 256MB\n");
        return (DEFAULT_RAM_BASE, DEFAULT_RAM_SIZE - DTB_RESERVE);
    }

    // SAFETY: We found a valid DTB magic at this address
    let fdt = if let Ok(fdt) = unsafe { fdt::Fdt::from_ptr(actual_dtb_ptr as *const u8) } { fdt } else {
        console::print("[Memory] Invalid DTB, using defaults\n");
        return (DEFAULT_RAM_BASE, DEFAULT_RAM_SIZE);
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
pub(crate) struct MemoryLayout {
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
pub(crate) fn compute_memory_layout(
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
pub(crate) fn reserve_calc_ram(ram_size: usize, clamp_mb: usize) -> usize {
    if clamp_mb != 0 {
        core::cmp::min(ram_size, clamp_mb * 1024 * 1024)
    } else {
        ram_size
    }
}

pub(crate) fn compute_heap_size(ram_size: usize, code_and_stack: usize) -> usize {
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
        #[cfg(kernel_profile_size)]
        const SMALL_FLOOR: usize = 512 * 1024;
        #[cfg(not(kernel_profile_size))]
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

/// Decide how many thread slots get a stack allocated (`thread_limit`, capped at
/// `MAX_THREADS`). Thread stacks come from PMM (the user-pages pool), so on a
/// small machine the full 64-thread pool (~9 MB) is the real boot floor. Give the
/// pool at most ~half of user pages (leaving the rest for processes), keeping the
/// `reserved` system threads plus at least a couple of user threads. See
/// docs/LOW_MEMORY_ENVIRONMENT.md.
pub(crate) fn compute_thread_limit(user_pages_size: usize) -> usize {
    if config::THREAD_LIMIT_OVERRIDE != 0 {
        return config::THREAD_LIMIT_OVERRIDE.min(config::MAX_THREADS);
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
/// `config::` constants. Factored out of `kernel_main` so a multikernel SECONDARY can
/// register the SAME runtime in its OWN (replicated) `RUNTIME`/`CONFIG` cells — the BSP
/// sets those after the `.data` snapshot, so a secondary's copy is pristine and must be
/// registered locally (docs/MULTIKERNEL.md §15, R3). The function pointers are shared
/// kernel code that resolves every `static` (PMM/heap) to whichever core runs them.
///
/// Stack bounds + canary toggle are parameters because they differ per core: the
/// secondary's "boot" stack is its isolated per-core stack, not the BSP boot stack.
pub(crate) fn build_exec_runtime(
    boot_stack_base: usize,
    boot_stack_top: usize,
    user_stack_size: usize,
    enable_stack_canaries: bool,
) -> (akuma_exec::ExecRuntime, akuma_exec::ExecConfig) {
    // No-op shim for gated-out Tier 2 FD-teardown callbacks (see ExecRuntime below).
    #[cfg(not(all(feature = "sc-eventfd", feature = "sc-epoll", feature = "sc-pidfd")))]
    fn noop_u32(_id: u32) {}

    let rt = akuma_exec::ExecRuntime {
        uptime_us: timer::uptime_us,
        disable_irqs: irq::disable_irqs,
        enable_irqs: irq::enable_irqs,
        end_of_interrupt: gic::end_of_interrupt,
        // Real shared-kernel SMP: voluntary reschedules (yield_now / schedule_blocking)
        // must ring THIS core's scheduler SGI, not the hardcoded PE0 that `trigger_sgi`
        // targets — otherwise a secondary's yield/block pokes the BSP and never
        // reschedules itself. On the BSP (aff0 = 0) `trigger_sgi_self` is equivalent.
        #[cfg(kernel_smp_shared)]
        trigger_sgi: gic::trigger_sgi_self,
        #[cfg(not(kernel_smp_shared))]
        trigger_sgi: gic::trigger_sgi,
        // Cross-core wakeup (M4): nudge an idle peer to run a just-woken thread. No-op
        // off shared-SMP.
        #[cfg(kernel_smp_shared)]
        wake_remote_idle: smp_shared::wake_remote_idle,
        #[cfg(not(kernel_smp_shared))]
        wake_remote_idle: || {},
        // Direct a scheduler SGI at the woken thread's last-known core so its
        // scheduler picks up the READY thread promptly. No-op off shared-SMP.
        #[cfg(kernel_smp_shared)]
        wake_core: smp_shared::wake_core,
        #[cfg(not(kernel_smp_shared))]
        wake_core: |_| {},
        alloc_page_zeroed: || pmm::alloc_page_zeroed(),
        alloc_page: || pmm::alloc_page(),
        free_page: pmm::free_page,
        pmm_stats: pmm::stats,
        track_frame: pmm::track_frame,
        free_count: pmm::free_count,
        total_count: pmm::total_count,
        alloc_pages_contiguous_zeroed: pmm::alloc_pages_contiguous_zeroed,
        free_pages_contiguous: pmm::free_pages_contiguous,
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
        resolve_inode: |path| crate::vfs::resolve_inode(path).map_err(|_| -1),
        read_at_by_inode: |_inode, _off, _buf| Err(-1),
        on_process_exit: |_pid| {},
        remove_socket: akuma_net::socket::remove_socket,
        socket_clone_ref: akuma_net::socket::socket_clone_ref,
        futex_wake: crate::syscall::futex_wake,
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
        #[cfg(feature = "sc-epoll")]
        epoll_destroy: crate::syscall::poll::epoll_destroy,
        #[cfg(not(feature = "sc-epoll"))]
        epoll_destroy: noop_u32,
        #[cfg(feature = "sc-pidfd")]
        pidfd_close: crate::syscall::pidfd::pidfd_close,
        #[cfg(not(feature = "sc-pidfd"))]
        pidfd_close: noop_u32,
        resolve_symlinks: |path| crate::vfs::resolve_symlinks(path),
        file_size: |path| crate::fs::file_size(path).map_err(|_| "fs error"),
        get_box_namespace: |box_id| crate::vfs::get_box_namespace(box_id),
        set_spawn_namespace: crate::vfs::set_spawn_namespace,
        clear_spawn_namespace: crate::vfs::clear_spawn_namespace,
        print_str: console::print,
        cow_ref_inc: pmm::cow_ref_inc,
        cow_ref_dec: pmm::cow_ref_dec,
        cow_ref_get: pmm::cow_ref_get,
        cow_fault_lock: pmm::cow_fault_lock,
        cow_fault_unlock: pmm::cow_fault_unlock,
        // No user-AS overlay on the BSP / single-kernel build. A multikernel secondary
        // sets this when it initializes (src/smp.rs) so the normal spawn path builds a
        // correct user table on it too (docs/MULTIKERNEL.md §4.2/R4b.3a).
        prepare_user_address_space: None,
        // No cross-core fd close on the BSP / single-kernel build; a secondary sets it
        // when it initializes (src/smp.rs) so a RemoteFd left open at exit is freed on the
        // owner (docs/MULTIKERNEL.md §8.1).
        remote_fd_close: None,
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
        // BSP/single-kernel: use the normal size-based loader. A multikernel secondary flips
        // this to true (it forwards file reads to the owner; whole-file is simplest there).
        prefer_whole_file_load: false,
    };
    (rt, cfg)
}

/// Main kernel initialization - all safe code
fn kernel_main(dtb_ptr: usize) -> ! {
    // Detect memory from DTB (must be done before heap init, so print first)
    console::print("Akuma Kernel starting...\n");

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

    unsafe extern "C" {
        static _kernel_phys_end: u8;
        static STACK_BOTTOM: u8;
        static STACK_TOP: u8;
    }
    let kernel_end = &raw const _kernel_phys_end as usize;
    let stack_bottom = &raw const STACK_BOTTOM as usize;
    let boot_stack_top = &raw const STACK_TOP as usize;
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

    let (ram_base, ram_size) = detect_memory(dtb_ptr);

    // Multikernel (docs/MULTIKERNEL.md): snapshot CPU/PSCI info from the DTB NOW,
    // before the heap allocator (which can be placed exactly at the DTB's address
    // on large-RAM configs) overwrites it. `bringup_secondaries` later reads the
    // stash, never the DTB. No-op without the `smp` feature.
    #[cfg(kernel_smp)]
    smp::probe_dtb(dtb_ptr);

    // Real (shared-kernel) SMP: same rationale — snapshot CPU/PSCI info before the
    // heap can overwrite the DTB. No-op without the `smp-shared` feature.
    #[cfg(kernel_smp_shared)]
    smp_shared::probe_dtb(dtb_ptr);

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

    // Initialize Physical Memory Manager
    // After this, the allocator can switch to page-based allocation
    let kernel_end = heap_start + heap_size;
    console::print("Initializing PMM...\n");
    pmm::init(ram_base, ram_size, kernel_end);

    // Signal that PMM is ready - allocator will switch to page mode
    allocator::mark_pmm_ready();
    console::print("PMM initialized, allocator switched to page mode\n");

    // Multikernel: remove the secondary cores' RAM partitions from the BSP PMM NOW,
    // before any BSP allocation (e.g. mmu::init below) can claim a page inside one.
    // Each secondary owns + manages its partition via its own per-core PMM (R2), so
    // the two pools must be strictly disjoint. (No-op single-core.)
    #[cfg(kernel_smp)]
    smp::reserve_secondary_partitions(ram_base, ram_size);

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
    gic::init();
    console::print("GIC initialized\n");

    // Multikernel (docs/MULTIKERNEL.md) — M0: wake secondary cores. They reuse the
    // BSP's boot page tables (isolation-by-convention) and park after reporting
    // Online. No-op when QEMU exposes a single CPU (default `-smp 1`). Gated to the
    // `smp` feature so the default single-core build never compiles it.
    #[cfg(kernel_smp)]
    smp::bringup_secondaries(ram_base, ram_size);

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
    match audio::init() {
        Ok(()) => console::print("[SND] virtio-sound ready (/dev/dsp)\n"),
        Err(_e) => console::print("[SND] virtio-sound not available\n"),
    }

    // =========================================================================
    // Framebuffer initialization (ramfb via fw_cfg)
    // =========================================================================
    #[cfg(feature = "sc-framebuffer")]
    match ramfb::init(320, 200) {
        Ok(()) => {
            console::print("[ramfb] Framebuffer ready\n");
        }
        Err(e) => {
            console::print("[ramfb] Not available: ");
            console::print(e);
            console::print("\n");
        }
    }

    // Initialize kernel timer (CNTV alarm queue for async timeouts)
    kernel_timer::init();

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
    gic::enable_irq(gic::SGI_SCHEDULER);

    console::print("Registering timer IRQ...\n");
    // Single hardware timer: the virtual timer (CNTV) fires PPI 27. Its handler
    // drives preemption AND services the async alarm queue (kernel_timer). The
    // physical timer (CNTP/PPI 30) is not used — it is inaccessible to the guest
    // under QEMU HVF (programming it faults with EC=0x0).
    irq::register_handler(27, timer::timer_irq_handler);
    gic::enable_irq(27); // Enable virtual timer interrupt

    console::print("Enabling timer...\n");
    timer::enable_timer_interrupts(config::TIMER_INTERVAL_US); // 10ms intervals
    console::print("Preemptive scheduling enabled (10ms timer -> SGI)\n");

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
    let low_mem_skip_tests = config::LOW_MEM_TEST_SKIP_MB != 0
        && ram_size <= config::LOW_MEM_TEST_SKIP_MB * 1024 * 1024;
    #[cfg(not(any(feature = "no-tests", kernel_profile_size)))]
    let boot_tests_enabled = !config::DISABLE_ALL_TESTS && !low_mem_skip_tests;
    if low_mem_skip_tests {
        crate::safe_print!(128,
            "[TESTS] low-mem ({} MB <= {} MB): skipping boot self-test suite\n",
            ram_size / 1024 / 1024, config::LOW_MEM_TEST_SKIP_MB);
    }

    // Run DAIF / IRQ-mask tests first — these verify the foundational
    // invariants that every later subsystem relies on. See
    // docs/STABILITY_URGENT_ISSUES.md issue #1.
    #[cfg(not(any(feature = "no-tests", kernel_profile_size)))]
    if boot_tests_enabled {
        daif_tests::run_all_tests();
    }

    // Run memory tests (no filesystem dependency)
    #[cfg(not(any(feature = "no-tests", kernel_profile_size)))]
    {
        if boot_tests_enabled {
            if !tests::run_memory_tests() {
                console::print("\n!!! MEMORY TESTS FAILED - HALTING !!!\n");
                halt();
            }

            // =========================================================================
            // Run async tests (before network takes over the main loop)
            // =========================================================================
            if !async_tests::run_all() {
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

                        #[cfg(not(any(feature = "no-tests", kernel_profile_size)))]
                        if boot_tests_enabled {
                            // Run filesystem tests
                            fs_tests::run_all_tests();

                            // Run threading tests (requires fs for parallel process tests)
                            if !tests::run_threading_tests() {
                                console::print("\n!!! THREADING TESTS FAILED - HALTING !!!\n");
                                if !config::IGNORE_THREADING_TESTS {
                                    halt();
                                } else {
                                    console::print("WARNING: Threading tests failed but continuing...\n");
                                }
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
                                sync_tests::run_all_tests();

                                // Run pthread / threading-API conformance tests
                                // (per-thread signal mask, sigaltstack, tkill,
                                // gettid — the §7k.3 regression class).
                                pthread_tests::run_all_tests();

                                // Run process execution tests
                                process_tests::run_all_tests();

                                // Run shell tests (pipelines with /bin binaries)
                                shell_tests::run_all_tests();
                            }

                            // Run memory benchmarks (always prints, never fails)
                            tests::run_benchmarks();
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
                    if cleaned > 0 {
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

/// Run the async main loop
///
/// This is the main entry point for async networking.
/// Runs on thread 0 (boot thread) which has a 1MB stack (config::KERNEL_STACK_SIZE).
/// This is sufficient for deep async call chains (SSH, HTTP, etc.).
///
/// Note: Thread 0 uses the boot stack at 0x40700000-0x40800000 which is
/// protected by stack canaries checked periodically in this loop.
fn run_async_main() -> ! {
    use core::future::Future;
    use core::pin::pin;
    use core::task::{Context, RawWaker, RawWakerVTable, Waker};

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
        unsafe {
            core::arch::asm!("msr daifclr, #2", options(nomem, nostack));
        }
        
        loop {
            threading::yield_now();
        }
    }

    // =========================================================================
    // Network initialization and main loop
    // =========================================================================
    console::print("\n--- Network Initialization ---\n");

    // Initialize the akuma-net networking stack
    let mmio_addrs: [usize; 8] = [
        mmu::DEV_VIRTIO_VA,
        mmu::DEV_VIRTIO_VA + 0x200,
        mmu::DEV_VIRTIO_VA + 0x400,
        mmu::DEV_VIRTIO_VA + 0x600,
        mmu::DEV_VIRTIO_VA + 0x800,
        mmu::DEV_VIRTIO_VA + 0xa00,
        mmu::DEV_VIRTIO_VA + 0xc00,
        mmu::DEV_VIRTIO_VA + 0xe00,
    ];
    if let Err(e) = akuma_net::init(
        akuma_net::NetRuntime {
            virt_to_phys: mmu::virt_to_phys,
            phys_to_virt: |pa| mmu::phys_to_virt(pa),
            uptime_us: timer::uptime_us,
            utc_seconds: timer::utc_seconds,
            yield_now: threading::yield_now,
            blocking_relax: threading::blocking_relax,
            current_box_id: || process::current_process_shared().map_or(0, |p| p.box_id),
            // Combined Ctrl-C + pthread_kill check, so a socket read blocked in
            // `wait_until` honours `tkill` the same way pipe/wait loops do.
            is_current_interrupted: process::should_interrupt_blocking_syscall,
            rng_fill: |buf| rng::fill_bytes(buf).expect("RNG required for networking"),
            current_thread_id: || threading::current_thread_id() as u32,
        },
        &mmio_addrs,
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

    console::print("--- Network Initialization Done ---\n\n");

    // rump feature: bind the BSP's rump tap (/dev/net/tap0) to NIC1 on virtio-mmio-bus.4
    // (RUMP_NIC=1), leaving NIC0 on smoltcp above. Bound to that SPECIFIC slot — not "the 2nd
    // virtio-net" — so it never claims bus.5, which is reserved for a secondary core's LOCAL
    // rump stack (CORE2_NIC=1; see smp::RUMP_NIC_CORE). This lets CORE2_NIC=1 be used ALONE. If
    // bus.4 has no device (RUMP_NIC=0), init_at fails gracefully and /dev/net/tap0 stays ENODEV.
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
    #[cfg(all(not(any(feature = "no-tests", kernel_profile_size)), feature = "smoltcp"))]
    if config::RUN_NETWORK_TESTS {
        network_tests::run_tests();
    }

    // Recompute here (different function from kernel_main's boot_tests_enabled):
    // these spawn-heavy suites are skipped on tiny machines, see kernel_main.
    // Both suites are smoltcp/SSH-coupled, so they compile out with the native stack.
    #[cfg(all(not(any(feature = "no-tests", kernel_profile_size)), feature = "smoltcp"))]
    {
        let ram = akuma_exec::mmu::ram_end().saturating_sub(akuma_exec::mmu::ram_base());
        let low_mem_skip_tests = config::LOW_MEM_TEST_SKIP_MB != 0
            && ram <= config::LOW_MEM_TEST_SKIP_MB * 1024 * 1024;
        if !config::DISABLE_ALL_TESTS && !low_mem_skip_tests {
            process_tests::run_network_tests();
            // The SSH suite exercises the built-in server, so it goes when it does.
            #[cfg(kernel_builtin_ssh)]
            ssh_tests::run_all_tests();
        }
    }

    // Rump sysproxy / scheduling regression guards. Compile under `rump` (so
    // they also run on default-smoltcp builds that opt a herd box into rump),
    // not gated on `rump-default`. See `src/rump_tests.rs`.
    #[cfg(all(
        not(kernel_profile_size),
        feature = "rump",
        any(not(feature = "no-tests"), feature = "rump-tests"),
    ))]
    if !config::DISABLE_ALL_TESTS {
        rump_tests::run_all_tests();
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

    // Multikernel: start the console drainer now that preemption is live and the BSP
    // can spawn system threads. It forwards secondary cores' per-core console rings
    // (§8.2) to the UART; secondaries have been buffering output since bringup.
    // (No-op single-core.)
    #[cfg(kernel_smp)]
    smp::start_console_drainer();

    // Multikernel: start the persistent forward-server (R4b.2) — drains the BSP inbox
    // and services cross-core forward requests for the system's lifetime, the steady-
    // state replacement for the transient bringup wait loop. (No-op single-core.)
    #[cfg(kernel_smp)]
    smp::start_fwd_server();

    // Multikernel: activate a secondary for the forward-latency micro-benchmark (no-op unless
    // the bench is enabled), so a plain `SMP=2` boot measures the transport without disk/herd.
    #[cfg(kernel_smp)]
    smp::autostart_bench_core();

    // Built-in (smoltcp) SSH server. Compiled out entirely when the native stack
    // is absent (devbox / rump-only) or `userspace-sshd` is on — there SSH is the
    // userspace /bin/sshd, and nothing of this implementation is in the image.
    #[cfg(kernel_builtin_ssh)]
    {
        ssh::init_host_key();
        console::print("[Main] Spawning built-in SSH server thread...\n");
        if let Err(e) = threading::spawn_system_thread_fn(|| ssh::server::run()) {
            console::print("[Main] Failed to spawn SSH server: ");
            console::print(e);
            console::print("\n");
        }
    }
    #[cfg(not(kernel_builtin_ssh))]
    console::print("[Main] Built-in SSH server not compiled; userspace /bin/sshd only\n");

    safe_print!(1024, "[Main] Network ready! Running background polling loop.\n");
    #[cfg(kernel_builtin_ssh)]
    safe_print!(1024, "[Main] SSH Server: Connect with ssh -o StrictHostKeyChecking=no user@localhost -p {}\n",
        if crate::config::SSH_PORT == 22 { 2222 } else { crate::config::SSH_PORT });

    // Enable IRQs for the main loop
    unsafe {
        core::arch::asm!("msr daifclr, #2", options(nomem, nostack));
    }

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

    // Loop iteration counter for debugging hangs
    use core::sync::atomic::{AtomicU64, Ordering};
    static LOOP_COUNTER: AtomicU64 = AtomicU64::new(0);
    static LAST_HEARTBEAT_US: AtomicU64 = AtomicU64::new(0);
    const HEARTBEAT_INTERVAL_US: u64 = 30_000_000; // 30 seconds
    
    // Pin memory monitor
    let mut mem_monitor_pinned = pin!(memory_monitor());
    
    // Simple waker for executor
    static VTABLE: RawWakerVTable = RawWakerVTable::new(
        |_| RawWaker::new(core::ptr::null(), &VTABLE),
        |_| {}, |_| {}, |_| {},
    );
    let raw_waker = RawWaker::new(core::ptr::null(), &VTABLE);
    let waker = unsafe { Waker::from_raw(raw_waker) };
    let mut cx = Context::from_waker(&waker);

    // Measurement builds only: start attributing cross-core BKL wait to the holder.
    #[cfg(kernel_bkl_profile)]
    crate::bkl_profile::init();

    loop {
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
            crate::safe_print!(160, 
                "[Heartbeat] Loop {} | T{} | SmolNet Active\n",
                count, tid
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
                let pct = if total == 0 { 0 } else { hits * 100 / total };
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
            // `MADV_DONTNEED` divergence audit (proposals/CARGO_HEAP_NULL_RC.md).
            // Both counters are expected to stay 0; either going non-zero means
            // this handler is zeroing frames Linux would have left alone.
            {
                let mut w = console::StackWriter::<128>::new();
                crate::syscall::mem::dontneed_audit_line(&mut w);
                w.flush();
            }
            // Write faults on pages the page table already allows — i.e. absorbed
            // stale TLB entries. `repeats` must stay 0; a non-zero value means the
            // flush is not what is resolving them (proposals/CARGO_HEAP_NULL_RC.md).
            {
                use core::sync::atomic::Ordering;
                crate::safe_print!(128, "[TLB] stale_write_faults={} repeats={}\n",
                    exceptions::STALE_TLB_WRITE_FAULTS.load(Ordering::Relaxed),
                    exceptions::STALE_TLB_REPEATS.load(Ordering::Relaxed));
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

        GLOBAL_POLL_STEP.store(1, Ordering::Relaxed);
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
            // SAFETY: IRQs are enabled; the timer/RX/SGI IRQ wakes us and its handler
            // re-takes the BKL (our enter_kernel below is then idempotent).
            unsafe {
                core::arch::asm!("wfi", options(nomem, nostack));
            }
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

/// Async task that periodically reports memory usage
async fn memory_monitor() -> ! {
    if !config::MEM_MONITOR_ENABLED {
        loop {
            threading::yield_now();
        }
    }
    use core::fmt::Write;
    use crate::kernel_timer::{Duration, Timer};

    // Stack-allocated buffer to avoid heap allocation when printing stats
    struct StackBuffer {
        buf: [u8; 384],
        pos: usize,
    }

    impl Write for StackBuffer {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
            let bytes = s.as_bytes();
            let remaining = self.buf.len() - self.pos;
            let to_copy = bytes.len().min(remaining);
            self.buf[self.pos..self.pos + to_copy].copy_from_slice(&bytes[..to_copy]);
            self.pos += to_copy;
            Ok(())
        }
    }

    impl StackBuffer {
        fn new() -> Self {
            Self {
                buf: [0; 384],
                pos: 0,
            }
        }

        fn as_str(&self) -> &str {
            core::str::from_utf8(&self.buf[..self.pos]).unwrap_or("")
        }

        fn clear(&mut self) {
            self.pos = 0;
        }
    }

    // Wait a bit before starting to let system stabilize
    Timer::after(Duration::from_secs(5)).await;

    console::print("[MemMonitor] Memory monitoring started\n");

    let mut buf = StackBuffer::new();

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
        buf.clear();
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
        // UAF quarantine (proposals/CARGO_HEAP_NULL_RC.md): `quar` is how many
        // frames are parked awaiting their poison check, `UAF` how many were
        // written after being freed. `UAF` is the number that matters and must be
        // 0; it is shown only when the instrument is armed.
        if config::PMM_UAF_QUARANTINE {
            let (quar_len, uaf) = pmm::quarantine_stats();
            let _ = write!(buf, " | quar={quar_len} UAF={uaf}");
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
        console::print(buf.as_str());

        // Stack high-water (no-op unless the probe const is on): right-sizing data
        // for the extreme kernel stacks. Printed on its own line to keep [Mem] short.
        akuma_exec::threading::report_stack_high_water();

        // SSH server stats + stall watchdog. Reports on the built-in server, so
        // it compiles out with it (rump-only builds and `userspace-sshd` images).
        #[cfg(kernel_builtin_ssh)]
        {
        let ssh = ssh::server::stats();
        buf.clear();
        if ssh.alive {
            // Stall watchdog: if the accept loop hasn't ticked SERVER_TICK_US
            // for >5s while reporting alive, that's a soft hang in the SSH
            // server. We don't auto-respawn (the dead thread still owns the
            // listener socket; a parallel respawn would collide on port
            // SSH_PORT) but a loud log makes the failure mode visible to the
            // operator and to the Python harness in scripts/ssh_harness.py.
            const SSH_STALL_THRESHOLD_US: u64 = 5_000_000;
            let stall_us = uptime_us.saturating_sub(ssh.last_tick_us);
            let stall_marker = if stall_us > SSH_STALL_THRESHOLD_US {
                " STALLED"
            } else {
                ""
            };
            let _ = writeln!(
                buf,
                "[SSH]{} listening | active={} open={} close={} hs_fail={} auth_fail={} panic={} stall_us={}",
                stall_marker, ssh.active, ssh.opened, ssh.closed, ssh.handshake_fail, ssh.auth_fail, ssh.panicked, stall_us
            );
            // Phase-1 instrumentation: when STALLED, dump the accept-loop
            // step + NETWORK lock holder snapshot so the log alone tells us
            // which of (a) NETWORK contention, (b) poll() stuck, (c) listener
            // handle freed is responsible. See docs/STABILITY_URGENT_ISSUES.md.
            if stall_us > SSH_STALL_THRESHOLD_US {
                let (holder, locked_at, site, polls_in, polls_out) =
                    akuma_net::smoltcp_net::network_holder_snapshot();
                let net_held_us = if holder == akuma_net::smoltcp_net::NETWORK_HOLDER_NONE {
                    0
                } else {
                    uptime_us.saturating_sub(locked_at)
                };
                let holder_str = if holder == akuma_net::smoltcp_net::NETWORK_HOLDER_NONE {
                    -1_i64
                } else {
                    i64::from(holder)
                };
                let _ = writeln!(
                    buf,
                    "[SSH] STALL DETAIL | step={}({}) listener_valid={} net_holder={} net_site={} net_held_us={} poll_in={} poll_out={} poll_gap={}",
                    ssh.last_step,
                    ssh::server::step::name(ssh.last_step),
                    ssh.listener_valid,
                    holder_str,
                    site.as_str(),
                    net_held_us,
                    polls_in,
                    polls_out,
                    polls_in.saturating_sub(polls_out),
                );
            }
        } else {
            let _ = writeln!(
                buf,
                "[SSH] no listener | active={} open={} close={} hs_fail={} auth_fail={} panic={}",
                ssh.active, ssh.opened, ssh.closed, ssh.handshake_fail, ssh.auth_fail, ssh.panicked
            );
        }
        console::print(buf.as_str());
        }

        // Report every 10 seconds (or period from config)
        Timer::after(Duration::from_secs(config::MEM_MONITOR_PERIOD_SECONDS)).await;
    }
}
