#![no_std]
#![no_main]
#![feature(never_type)]
#![feature(alloc_error_handler)]

extern crate alloc;

mod akuma;
mod allocator;
mod async_fs;
// mod async_net;
mod async_tests;
mod block;
mod boot;
mod config;
#[macro_use]
mod console;
mod editor;
// mod embassy_net_driver;
// mod embassy_time_driver; // replaced by kernel_timer
// mod embassy_virtio_driver;
mod exceptions;
mod fw_cfg;
mod kernel_timer;
mod fs;
mod fs_tests;
mod gic;
mod irq;
mod network_tests;
mod pmm;
mod process_tests;
mod ramfb;
mod rng;
mod shell;
mod shell_tests;
mod sync_tests;
mod ssh;
mod syscall;
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
/// When using ARM64 Image header boot, the kernel is at 0x40200000 and
/// QEMU places DTB in the first 2MB at 0x40000000.
#[cfg(not(feature = "firecracker"))]
fn scan_for_dtb() -> usize {
    const FDT_MAGIC_LE: u32 = 0xedfe0dd0; // big-endian 0xd00dfeed read as little-endian

    // With ARM64 Image header, DTB is placed at RAM_BASE (0x40000000)
    // before the kernel which is at RAM_BASE + 2MB (0x40200000)
    const DTB_LOCATION: usize = 0x4000_0000;

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
/// Firecracker passes the FDT address in x0 per the ARM64 boot protocol.
/// QEMU does NOT set x0 for ELF kernels, so we scan RAM for the
/// QEMU-generated DTB when x0 is zero.
fn detect_memory(dtb_ptr: usize) -> (usize, usize) {
    #[cfg(not(feature = "firecracker"))]
    const DEFAULT_RAM_BASE: usize = 0x4000_0000; // QEMU virt: 1 GB
    #[cfg(feature = "firecracker")]
    const DEFAULT_RAM_BASE: usize = 0x8000_0000; // Firecracker: 2 GB

    const DEFAULT_RAM_SIZE: usize = 256 * 1024 * 1024;
    const DTB_RESERVE: usize = 2 * 1024 * 1024; // 2 MB

    #[cfg(not(feature = "firecracker"))]
    let actual_dtb_ptr = if dtb_ptr != 0 { dtb_ptr } else { scan_for_dtb() };
    #[cfg(feature = "firecracker")]
    let actual_dtb_ptr = dtb_ptr;

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

/// Main kernel initialization - all safe code
fn kernel_main(dtb_ptr: usize) -> ! {
    // Detect memory from DTB (must be done before heap init, so print first)
    console::print("Akuma Kernel starting...\n");

    // =========================================================================
    // CRITICAL: Verify kernel binary doesn't overlap with boot stack
    // =========================================================================
    // Stack layout: STACK_TOP = KERNEL_BASE + 8 MB, STACK_SIZE = 1 MB
    //   QEMU:        KERNEL_BASE=0x40200000, STACK_BOTTOM=0x40900000
    //   Firecracker: KERNEL_BASE=0x80000000, STACK_BOTTOM=0x80700000
    //
    // QEMU virt loads flat binary with ARM64 Image header at RAM_BASE + 2MB
    // (0x40200000). The first 2MB (0x40000000-0x401FFFFF) contains DTB.
    #[cfg(not(feature = "firecracker"))]
    const KERNEL_BASE: usize = 0x4020_0000;
    #[cfg(feature = "firecracker")]
    const KERNEL_BASE: usize = 0x8000_0000;

    #[cfg(not(feature = "firecracker"))]
    const STACK_BOTTOM: usize = 0x4090_0000;
    #[cfg(feature = "firecracker")]
    const STACK_BOTTOM: usize = 0x8070_0000;

    unsafe extern "C" {
        static _kernel_phys_end: u8;
    }
    let kernel_end = unsafe { &_kernel_phys_end as *const u8 as usize };
    let kernel_size = kernel_end - KERNEL_BASE;

    console::print("Kernel binary: ");
    console::print_dec(kernel_size / 1024);
    console::print(" KB (0x");
    console::print_hex(KERNEL_BASE as u64);
    console::print(" - 0x");
    console::print_hex(kernel_end as u64);
    console::print(")\n");

    if kernel_end >= STACK_BOTTOM {
        console::print("\n!!! FATAL: Kernel binary overlaps with boot stack !!!\n");
        console::print("Kernel end:   0x");
        console::print_hex(kernel_end as u64);
        console::print("\nStack bottom: 0x");
        console::print_hex(STACK_BOTTOM as u64);
        console::print("\n\nThe kernel has grown too large. Options:\n");
        console::print("  1. Increase STACK_TOP in boot.rs (move stack higher)\n");
        console::print("  2. Reduce kernel size (remove unused features)\n");
        console::print("  3. Move to dynamic stack allocation\n");
        console::print("\nHALTING.\n");
        halt();
    }

    // Safety margin check - warn if kernel is getting close to stack
    let margin = STACK_BOTTOM - kernel_end;
    if margin < 4 * 1024 * 1024 {
        // Less than 4MB margin
        console::print("WARNING: Kernel is within 4MB of stack! (");
        console::print_dec(margin / 1024);
        console::print(" KB margin)\n");
    }

    let (ram_base, ram_size) = detect_memory(dtb_ptr);

    // Memory layout constants
    const MIN_CODE_AND_STACK: usize = 8 * 1024 * 1024; // 8MB for kernel binary (~2MB) + 1MB boot stack

    // Memory layout:
    // - Code + Stack: max(1/16 of RAM, 8MB) - kernel binary and boot stack
    // - Heap: 1/8 of RAM (min 64MB, max 256MB) - kernel data structures
    //   Sized dynamically so that memory-hungry workloads (go build, bun, etc.)
    //   don't exhaust kernel metadata allocations, but capped to save user RAM.
    // - User pages: remaining - for user processes
    let code_and_stack = core::cmp::max(ram_size / 16, MIN_CODE_AND_STACK);
    let heap_start = ram_base + code_and_stack;
    let heap_size = core::cmp::min(
        core::cmp::max(ram_size / 8, 64 * 1024 * 1024),
        256 * 1024 * 1024
    );
    let user_pages_start = heap_start + heap_size;
    let user_pages_size = ram_size.saturating_sub(code_and_stack + heap_size);

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
    console::print(") [min 8MB]\n");

    console::print("Heap:       ");
    console::print_dec(heap_size / 1024 / 1024);
    console::print(" MB (0x");
    console::print_hex(heap_start as u64);
    console::print(" - 0x");
    console::print_hex(user_pages_start as u64);
    console::print(") [fixed 8MB]\n");

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

    // Initialize MMU with identity mapping for kernel
    console::print("Initializing MMU...\n");
    mmu::init(ram_base, ram_size);

    // Register exec runtime before init_shared_device_tables, which needs
    // the PMM callbacks via runtime(). The function pointers are just stored
    // here — subsystems like GIC/timer don't need to be initialized yet.
    console::print("Initializing exec subsystem...\n");
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
            eventfd_close: crate::syscall::eventfd::eventfd_close,
            eventfd_clone_ref: crate::syscall::eventfd::eventfd_clone_ref,
            epoll_destroy: crate::syscall::poll::epoll_destroy,
            pidfd_close: crate::syscall::pidfd::pidfd_close,
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
            kernel_stack_size: config::KERNEL_STACK_SIZE,
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
    // Framebuffer initialization (ramfb via fw_cfg)
    // =========================================================================
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
    // Run memory tests (no filesystem dependency)
    if !config::DISABLE_ALL_TESTS {
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

                        if !config::DISABLE_ALL_TESTS {
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

                            // Run futex sync tests
                            sync_tests::run_all_tests();

                            // Run process execution tests
                            process_tests::run_all_tests();

                            // Run shell tests (pipelines with /bin binaries)
                            shell_tests::run_all_tests();

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
    if config::RUN_NETWORK_TESTS {
        network_tests::run_tests();
    }

    if !config::DISABLE_ALL_TESTS {
        process_tests::run_network_tests();
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
        buf: [u8; 256],
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
                buf: [0; 256],
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
        let stats = allocator::stats();
        let allocated_kb = stats.allocated / 1024;
        let free_kb = stats.free / 1024;
        let peak_kb = stats.peak_allocated / 1024;
        let heap_mb = stats.heap_size / 1024 / 1024;
        
        let (total_pages, allocated_pages, _) = pmm::stats();
        let total_ram_mb = (total_pages * mmu::PAGE_SIZE) / 1024 / 1024;
        let free_ram_mb = (total_pages.saturating_sub(allocated_pages) * mmu::PAGE_SIZE) / 1024 / 1024;

        let (threads_ready, threads_running, _) = akuma_exec::threading::thread_stats();
        let threads_used = threads_ready + threads_running;
        let threads_max = akuma_exec::threading::max_threads();

        let uptime_us = timer::uptime_us();
        buf.clear();
        let _ = write!(
            buf,
            "[Mem] Uptime {} | RAM: {}/{}MB free | Heap: {}/{}MB free ({} KB used, {} KB peak) | Allocs: {} | Threads: {}/{} ({}r {}rd)\n",
            uptime_us, free_ram_mb, total_ram_mb, free_kb / 1024, heap_mb, allocated_kb, peak_kb, stats.allocation_count,
            threads_used, threads_max, threads_running, threads_ready
        );
        console::print(buf.as_str());

        // Report every 10 seconds (or period from config)
        Timer::after(Duration::from_secs(config::MEM_MONITOR_PERIOD_SECONDS)).await;
    }
}
