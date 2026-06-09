#![no_std]
#![no_main]
#![feature(never_type)]
#![feature(alloc_error_handler)]

extern crate alloc;

mod akuma;
mod allocator;
mod async_fs;
mod audio;
// mod async_net;
#[cfg(not(any(feature = "no-tests", kernel_profile_size)))]
mod async_tests;
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
#[cfg(feature = "sc-framebuffer")]
mod fw_cfg;
mod kernel_timer;
mod fs;
#[cfg(not(any(feature = "no-tests", kernel_profile_size)))]
mod fs_tests;
mod gic;
mod irq;
#[cfg(not(any(feature = "no-tests", kernel_profile_size)))]
mod network_tests;
mod pmm;
#[cfg(not(any(feature = "no-tests", kernel_profile_size)))]
mod process_tests;
#[cfg(feature = "sc-framebuffer")]
mod ramfb;
mod rng;
mod shell;
#[cfg(not(any(feature = "no-tests", kernel_profile_size)))]
mod shell_tests;
#[cfg(not(any(feature = "no-tests", kernel_profile_size)))]
mod sync_tests;
mod ssh;
#[cfg(not(any(feature = "no-tests", kernel_profile_size)))]
mod ssh_tests;
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
    let block: [u64; 2] = [0x20026, code as u64];

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
    // #region agent log
    console::print("[FORK-DBG] PANIC ENTERED\n");
    // #endregion
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
    {
        use core::fmt::Write;
        let mut buf = console::StackWriter::<256>::new();
        let _ = write!(buf, "{}", info.message());
        console::print(buf.as_str());
    }
    console::print("\n");
    halt()
}

// Import boot_x0_at_entry from assembly
unsafe extern "C" {
    static boot_x0_at_entry: u64;
}

/// Minimal unsafe entry point - immediately delegates to safe kernel_main
#[unsafe(no_mangle)]
pub extern "C" fn rust_start(dtb_ptr: usize) -> ! {
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
        if total_size >= 64 && total_size <= 16 * 1024 * 1024 {
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
    let fdt = match unsafe { fdt::Fdt::from_ptr(actual_dtb_ptr as *const u8) } {
        Ok(fdt) => fdt,
        Err(_) => {
            console::print("[Memory] Invalid DTB, using defaults\n");
            return (DEFAULT_RAM_BASE, DEFAULT_RAM_SIZE);
        }
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
/// Kernel physical-RAM layout: three contiguous regions starting at `ram_base`
/// — `[.. heap_start)` code+boot-stack, `[heap_start ..)` heap, then user pages.
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
        core::cmp::min(core::cmp::max(ram_size / 8, 64 * MB), 256 * MB)
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
    let kernel_end = unsafe { &_kernel_phys_end as *const u8 as usize };
    let stack_bottom = unsafe { &STACK_BOTTOM as *const u8 as usize };
    let boot_stack_top = unsafe { &STACK_TOP as *const u8 as usize };
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

    // No-op shim for gated-out Tier 2 FD-teardown callbacks (see ExecRuntime below).
    #[cfg(not(all(feature = "sc-eventfd", feature = "sc-epoll", feature = "sc-pidfd")))]
    fn noop_u32(_id: u32) {}

    akuma_exec::init(
        akuma_exec::ExecRuntime {
            uptime_us: timer::uptime_us,
            disable_irqs: irq::disable_irqs,
            enable_irqs: irq::enable_irqs,
            end_of_interrupt: gic::end_of_interrupt,
            trigger_sgi: gic::trigger_sgi,
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
            read_file: |path| crate::fs::read_file(path).map_err(|_| -1),
            read_at: |path, off, buf| crate::vfs::read_at(path, off, buf).map_err(|_| -1),
            resolve_inode: |path| crate::vfs::resolve_inode(path).map_err(|_| -1),
            read_at_by_inode: |_inode, _off, _buf| Err(-1),
            on_process_exit: |_pid| {},
            remove_socket: akuma_net::socket::remove_socket,
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
        },
        akuma_exec::ExecConfig {
            max_threads: config::MAX_THREADS,
            reserved_threads: config::RESERVED_THREADS,
            // Derive the boot-stack size from the linker symbols (the single
            // source of truth — BOOT_STACK_SIZE via --defsym, profile-dependent)
            // rather than config::KERNEL_STACK_SIZE, so slot-0's StackInfo bounds
            // and canary placement always match the actual reservation even when
            // the extreme profile shrinks it. See linker.ld / build.rs.
            kernel_stack_size: boot_stack_top - stack_bottom,
            // Real boot-stack bounds, read from the linker-derived STACK_BOTTOM /
            // STACK_TOP symbols above. The threading crate must NOT hardcode these
            // — when the boot stack was relocated, a stale constant stamped the
            // stack canary into the kernel heap at low RAM (release boot floor
            // jumped to 128 MB). See docs/LOW_MEMORY_ENVIRONMENT.md "Known bug".
            boot_stack_base: stack_bottom,
            boot_stack_top,
            default_thread_stack_size: config::DEFAULT_THREAD_STACK_SIZE,
            system_thread_stack_size: config::SYSTEM_THREAD_STACK_SIZE,
            user_thread_stack_size: config::USER_THREAD_STACK_SIZE,
            user_stack_size,
            enable_stack_canaries: config::ENABLE_STACK_CANARIES,
            stack_canary: config::STACK_CANARY,
            canary_words: config::CANARY_WORDS,
            network_thread_ratio: config::NETWORK_THREAD_RATIO,
            deferred_thread_cleanup: config::DEFERRED_THREAD_CLEANUP,
            thread_cleanup_cooldown_us: config::THREAD_CLEANUP_COOLDOWN_US,
            syscall_debug_info_enabled: config::SYSCALL_DEBUG_INFO_ENABLED,
            fork_brk_serial_progress: config::FORK_BRK_SERIAL_PROGRESS,
            enable_sgi_debug_prints: config::ENABLE_SGI_DEBUG_PRINTS,
            proc_stdin_max_size: config::PROC_STDIN_MAX_SIZE,
            proc_stdout_max_size: config::PROC_STDOUT_MAX_SIZE,
            cow_fork_enabled: config::COW_FORK_ENABLED,
            vfork_fastpath_enabled: config::VFORK_FASTPATH_ENABLED,
        },
    );
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
    process::init_box_registry(); // Init Box 0
    console::print("Threading system initialized\n");

    // =========================================================================
    // Now enable preemptive scheduling (timer interrupts)
    // =========================================================================
    console::print("Configuring scheduler SGI...\n");
    gic::enable_irq(gic::SGI_SCHEDULER);

    console::print("Registering timer IRQ...\n");
    irq::register_handler(30, |irq| timer::timer_irq_handler(irq));
    
    // Register virtual timer IRQ (27) for kernel timer async wakeups
    // CNTV (virtual timer) avoids conflict with scheduler's CNTP
    irq::register_handler(27, |_irq| {
        kernel_timer::on_timer_interrupt();
    });
    gic::enable_irq(27); // Enable virtual timer interrupt

    console::print("Enabling timer...\n");
    timer::enable_timer_interrupts(config::TIMER_INTERVAL_US); // 10ms intervals
    console::print("Preemptive scheduling enabled (10ms timer -> SGI)\n");

    // Enable IRQ-safe allocations now that preemption is active
    allocator::enable_preemption_safe_alloc();

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
                if loop_counter % 10 == 0 {
                    let cleaned = threading::cleanup_terminated();
                    if cleaned > 0 {
                        // Safe print without heap allocation to prevent panics
                        console::print("[Thread0] Cleaned ");
                        console::print_dec(cleaned);
                        console::print(" terminated threads\n");
                    }
                }
                
                // Heartbeat every 1000 iterations to show thread 0 is alive
                if loop_counter % crate::config::THREADING_HEARTBEAT_INTERVAL == 0 {
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
                }
                
                threading::yield_now();
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
            current_box_id: || process::current_process().map(|p| p.box_id).unwrap_or(0),
            is_current_interrupted: process::is_current_interrupted,
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
    };

    console::print("--- Network Initialization Done ---\n\n");

    // Run network self-tests if enabled
    #[cfg(not(any(feature = "no-tests", kernel_profile_size)))]
    if config::RUN_NETWORK_TESTS {
        network_tests::run_tests();
    }

    // Recompute here (different function from kernel_main's boot_tests_enabled):
    // these spawn-heavy suites are skipped on tiny machines, see kernel_main.
    #[cfg(not(any(feature = "no-tests", kernel_profile_size)))]
    {
        let ram = akuma_exec::mmu::ram_end().saturating_sub(akuma_exec::mmu::ram_base());
        let low_mem_skip_tests = config::LOW_MEM_TEST_SKIP_MB != 0
            && ram <= config::LOW_MEM_TEST_SKIP_MB * 1024 * 1024;
        if !config::DISABLE_ALL_TESTS && !low_mem_skip_tests {
            process_tests::run_network_tests();
            ssh_tests::run_all_tests();
        }
    }

    // Initialize SSH host key
    ssh::init_host_key();

    if !config::ENABLE_USERSPACE_SSHD {
        console::print("[Main] Spawning built-in SSH server thread...\n");
        if let Err(e) = threading::spawn_system_thread_fn(|| ssh::server::run()) {
            console::print("[Main] Failed to spawn SSH server: ");
            console::print(e);
            console::print("\n");
        }
    } else {
        console::print("[Main] Built-in SSH server disabled (ENABLE_USERSPACE_SSHD=true)\n");
    }

    safe_print!(1024, "[Main] Network ready! Running background polling loop.\n");
    if !config::ENABLE_USERSPACE_SSHD {
        safe_print!(1024, "[Main] SSH Server: Connect with ssh -o StrictHostKeyChecking=no user@localhost -p {}\n", 
            if crate::config::SSH_PORT == 22 { 2222 } else { crate::config::SSH_PORT });
    }

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

    loop {
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
        }

        GLOBAL_POLL_STEP.store(1, Ordering::Relaxed);
        // Poll network stack in a loop until no more progress.
        // Each poll() may only process one RX packet (single VirtIO buffer),
        // so we need to loop to drain bursts of incoming packets. This is
        // critical for bulk transfer throughput (e.g. git clone over SSH):
        // without draining, TCP ACKs/window updates are delayed until the
        // next scheduler slot, causing the remote sender's TCP window to
        // shrink and throughput to collapse.
        {
            let mut polls = 0u32;
            while akuma_net::smoltcp_net::poll() {
                polls += 1;
                if polls >= 64 {
                    break; // Safety cap to avoid starving other threads
                }
            }
        }
        
        GLOBAL_POLL_STEP.store(2, Ordering::Relaxed);
        if config::MEM_MONITOR_ENABLED {
            let _ = mem_monitor_pinned.as_mut().poll(&mut cx);
        }
        
        GLOBAL_POLL_STEP.store(3, Ordering::Relaxed);
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
        threading::yield_now();
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
        let dfree_marker = if dfree > 0 {
            alloc::format!(" | DOUBLE-FREE={}", dfree)
        } else {
            alloc::string::String::new()
        };
        let reclaimed_pages = allocator::reclaimed_pages_total();
        buf.clear();
        let _ = write!(
            buf,
            "[Mem] Uptime {} | RAM: {}/{}MB free ({}KB) | Heap: {}/{}MB free ({} KB used, {} KB peak) | Allocs: {} | Threads: {}/{} ({}r {}rd){}",
            uptime_us, free_ram_mb, total_ram_mb, free_ram_kb, free_kb / 1024, heap_mb, allocated_kb, peak_kb, stats.allocation_count,
            threads_used, threads_max, threads_running, threads_ready, dfree_marker
        );
        // Pages handed back from the heap to the PMM since boot — non-zero means
        // the heap watermark is being trimmed (see allocator::reclaim_to_pmm).
        // Written straight into the stack buffer; no heap alloc in the mem monitor.
        if reclaimed_pages > 0 {
            let _ = write!(buf, " | reclaimed={}KB", reclaimed_pages * 4);
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
        let _ = write!(buf, "\n");
        console::print(buf.as_str());

        // Stack high-water (no-op unless the probe const is on): right-sizing data
        // for the extreme kernel stacks. Printed on its own line to keep [Mem] short.
        akuma_exec::threading::report_stack_high_water();

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
            let _ = write!(
                buf,
                "[SSH]{} listening | active={} open={} close={} hs_fail={} auth_fail={} panic={} stall_us={}\n",
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
                let _ = write!(
                    buf,
                    "[SSH] STALL DETAIL | step={}({}) listener_valid={} net_holder={} net_site={} net_held_us={} poll_in={} poll_out={} poll_gap={}\n",
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
            let _ = write!(
                buf,
                "[SSH] no listener | active={} open={} close={} hs_fail={} auth_fail={} panic={}\n",
                ssh.active, ssh.opened, ssh.closed, ssh.handshake_fail, ssh.auth_fail, ssh.panicked
            );
        }
        console::print(buf.as_str());

        // Report every 10 seconds (or period from config)
        Timer::after(Duration::from_secs(config::MEM_MONITOR_PERIOD_SECONDS)).await;
    }
}
