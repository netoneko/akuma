#![no_std]
#![no_main]
#![feature(never_type)]

extern crate alloc;

mod akuma;
mod allocator;
mod async_fs;
// mod async_net;
mod smoltcp_net;
mod async_tests;
mod block;
mod boot;
mod config;
#[macro_use]
mod console;
mod dns;
mod editor;
mod elf_loader;
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
mod mmu;
mod network;
mod network_tests;
mod pmm;
mod process;
mod process_tests;
mod ramfb;
mod rng;
mod shell;
mod shell_tests;
mod socket;
mod ssh;
mod std_compat;
mod syscall;
mod terminal;
mod tests;
mod threading;
mod timer;
mod tls;
mod tls_rng;
mod tls_verifier;
mod vfs;
mod virtio_hal;

use alloc::format;
use alloc::string::ToString;
use core::sync::atomic::AtomicU64;

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

/// DTB magic number (big-endian: 0xd00dfeed)
const DTB_MAGIC: u32 = 0xd00dfeed;

/// Fixed address where we tell QEMU to load the DTB via loader device
/// Use: -device loader,file=virt.dtb,addr=0x4ff00000,force-raw=on
const DTB_FIXED_ADDR: usize = 0x4ff00000;

/// Check for DTB at fixed address, or scan if not found
fn find_dtb(_ram_base: usize, _ram_size: usize, _kernel_end: usize) -> usize {
    // First check the fixed address where we ask QEMU to load DTB
    let magic = unsafe { core::ptr::read_volatile(DTB_FIXED_ADDR as *const u32) };
    if magic == 0xedfe0dd0 {
        console::print("[DTB] Found DTB at fixed address 0x");
        console::print_hex(DTB_FIXED_ADDR as u64);
        console::print("\n");
        return DTB_FIXED_ADDR;
    }
    
    console::print("[DTB] No DTB at fixed address 0x");
    console::print_hex(DTB_FIXED_ADDR as u64);
    console::print("\n");
    console::print("[DTB] Add to QEMU: -device loader,file=virt.dtb,addr=0x4ff00000,force-raw=on\n");
    0
}

/// Detect memory from Device Tree Blob
fn detect_memory(dtb_ptr: usize) -> (usize, usize) {
    const DEFAULT_RAM_BASE: usize = 0x40000000;
    const DEFAULT_RAM_SIZE: usize = 256 * 1024 * 1024; // Must match QEMU -m setting
    // Reserve space at end of RAM for QEMU's DTB and other internal data
    // QEMU places the DTB somewhere in RAM but doesn't tell bare-metal ELFs where
    const DTB_RESERVE: usize = 2 * 1024 * 1024; // 2MB should be plenty

    // If no DTB pointer provided, try to find it by scanning memory
    // Use kernel_phys_end as scan start (DTB won't be in kernel area)
    unsafe extern "C" {
        static _kernel_phys_end: u8;
    }
    let kernel_end = unsafe { &_kernel_phys_end as *const u8 as usize };
    
    let actual_dtb_ptr = if dtb_ptr == 0 {
        find_dtb(DEFAULT_RAM_BASE, DEFAULT_RAM_SIZE, kernel_end)
    } else {
        dtb_ptr
    };

    if actual_dtb_ptr == 0 {
        console::print("[Memory] No DTB found, reserving last 2MB for QEMU data\n");
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
        let dtb_size = region.size.unwrap_or(DEFAULT_RAM_SIZE);
        
        // Reserve memory from DTB location to end of RAM
        // DTB is at actual_dtb_ptr, so usable RAM ends there
        let usable_size = if actual_dtb_ptr > base {
            actual_dtb_ptr - base
        } else {
            dtb_size
        };
        
        console::print("[Memory] Detected from DTB: base=0x");
        console::print_hex(base as u64);
        console::print(", total=");
        console::print_dec(dtb_size / 1024 / 1024);
        console::print(" MB, usable=");
        console::print_dec(usable_size / 1024 / 1024);
        console::print(" MB (DTB at 0x");
        console::print_hex(actual_dtb_ptr as u64);
        console::print(")\n");
        (base, usable_size)
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
    // Stack layout (from boot.rs):
    //   STACK_TOP    = 0x42000000 (32MB from kernel base)
    //   STACK_SIZE   = 0x100000   (1MB)
    //   Stack bottom = 0x41F00000 (31MB from kernel base)
    //
    // Kernel must fit below 0x41F00000 to not corrupt stack!
    const KERNEL_BASE: usize = 0x4000_0000;
    const STACK_BOTTOM: usize = 0x41F0_0000; // STACK_TOP - STACK_SIZE

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
    const MIN_CODE_AND_STACK: usize = 32 * 1024 * 1024; // Minimum 32MB for kernel binary + stack

    // Memory layout:
    // - Code + Stack: max(1/8 of RAM, 32MB) - kernel binary and stack
    // - Heap: 1/2 of RAM - dynamic allocations
    // - User pages: remaining - for user processes
    // Note: See docs/MEMORY_LAYOUT.md for details on sizing constraints

    // Calculate code + stack region (at least 32MB to support kernels up to ~24MB)
    let code_and_stack = core::cmp::max(ram_size / 8, MIN_CODE_AND_STACK);
    let heap_start = ram_base + code_and_stack;
    let heap_size = MIN_CODE_AND_STACK; // 32 MB — keep kernel heap small, maximize user pages
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
    console::print(") [min 32MB]\n");

    console::print("Heap:       ");
    console::print_dec(heap_size / 1024 / 1024);
    console::print(" MB (0x");
    console::print_hex(heap_start as u64);
    console::print(" - 0x");
    console::print_hex(user_pages_start as u64);
    console::print(") [1/2 of RAM]\n");

    console::print("User pages: ");
    console::print_dec(user_pages_size / 1024 / 1024);
    console::print(" MB (0x");
    console::print_hex(user_pages_start as u64);
    console::print(" - 0x");
    console::print_hex((ram_base + ram_size) as u64);
    console::print(") [remaining]\n");

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
    console::print("MMU enabled with identity mapping\n");

    // Log kernel section boundaries (for future read-only protection)
    mmu::protect_kernel_code();

    // Print PMM stats (now that allocator is ready for format!)
    let (total, allocated, free) = pmm::stats();
    crate::safe_print!(128, 
        "PMM stats: {} total pages, {} allocated, {} free\n",
        total,
        allocated,
        free
    );

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
        Err(e) => {
            console::print("[RNG] Hardware RNG not available: ");
            crate::safe_print!(32, "{}\n", e);
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
    console::print(&freq.to_string());
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
    console::print(&(timer::uptime_us() / 1_000_000).to_string());
    console::print(" seconds\n");

    // Initialize threading (but don't enable timer yet!)
    console::print("Initializing threading...\n");
    threading::init();
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

                            // Run process execution tests
                            process_tests::run_all_tests();

                            // Run shell tests (pipelines with /bin binaries)
                            shell_tests::run_all_tests();
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

    if crate::config::COOPERATIVE_MAIN_THREAD {
        run_async_main();
    } else {
        run_async_main_preemptive();
    }
}

fn run_async_main_preemptive() -> ! {
    // Use spawn_system_thread_fn - it uses SYSTEM_THREAD_STACK_SIZE (256KB)
    // which equals ASYNC_THREAD_STACK_SIZE, so no custom size needed
    let thread_result = crate::threading::spawn_system_thread_fn(|| {
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
/// Note: Thread 0 uses the boot stack at 0x41F00000-0x42000000 which is
/// protected by stack canaries checked periodically in this loop.
fn run_async_main() -> ! {
    use core::future::Future;
    use core::pin::pin;
    use core::task::{Context, RawWaker, RawWakerVTable, Waker};

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

    // Initialize the smoltcp network stack
    if let Err(e) = smoltcp_net::init() {
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

        GLOBAL_POLL_STEP.store(1, Ordering::Relaxed);
        // Poll network stack in a loop until no more progress.
        // Each poll() may only process one RX packet (single VirtIO buffer),
        // so we need to loop to drain bursts of incoming packets. This is
        // critical for bulk transfer throughput (e.g. git clone over SSH):
        // without draining, TCP ACKs/window updates are delayed until the
        // next scheduler slot, causing the remote sender's TCP window to
        // shrink and throughput to collapse.
        let mut net_progress = false;
        {
            let mut polls = 0u32;
            while smoltcp_net::poll() {
                net_progress = true;
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

/// Check if IRQs are currently enabled (I bit in DAIF is clear)
#[inline]
fn is_irq_enabled() -> bool {
    let daif: u64;
    unsafe {
        core::arch::asm!("mrs {}, daif", out(reg) daif, options(nomem, nostack));
    }
    // Bit 7 (0x80) is the I flag - if clear, IRQs are enabled
    (daif & 0x80) == 0
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
        buf: [u8; 128],
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
                buf: [0; 128],
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
        let uptime_us = timer::uptime_us();
        buf.clear();
        let _ = write!(
            buf,
            "[Mem] Uptime {} | Used: {} KB | Free: {} KB | Peak: {} KB | Heap: {} MB | Allocs: {}\n",
            uptime_us, allocated_kb, free_kb, peak_kb, heap_mb, stats.allocation_count
        );
        console::print(buf.as_str());

        // Report every 10 seconds
        Timer::after(Duration::from_secs(config::MEM_MONITOR_PERIOD_SECONDS)).await;
    }
}
