#![no_std]
#![no_main]
#![feature(never_type)]

extern crate alloc;

mod akuma;
mod allocator;
mod async_fs;
mod async_net;
mod async_tests;
mod block;
mod boot;
mod config;
mod console;
mod dns;
mod editor;
mod elf_loader;
mod embassy_net_driver;
mod embassy_time_driver;
mod embassy_virtio_driver;
mod exceptions;
mod executor;
mod fs;
mod fs_tests;
mod gic;
mod irq;
mod mmu;
mod netcat_server;
mod network;
mod pmm;
mod process;
mod process_tests;
mod rhai;
mod rng;
mod shell;
mod shell_tests;
mod ssh;
mod std_compat;
mod syscall;
mod tests;
mod threading;
mod timer;
mod tls;
mod tls_rng;
mod tls_verifier;
mod vfs;
mod virtio_hal;
mod web_server;

use alloc::string::ToString;

use core::panic::PanicInfo;

use crate::threading::spawn_fn;

/// Halt the CPU in a low-power wait loop. Safe wrapper around wfi.
#[inline]
fn halt() -> ! {
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
        console::print(&location.line().to_string());
        console::print("\n");
    }
    console::print("Message: ");
    console::print(&alloc::format!("{}\n", info.message()));
    halt()
}

/// Minimal unsafe entry point - immediately delegates to safe kernel_main
#[unsafe(no_mangle)]
pub extern "C" fn rust_start(dtb_ptr: usize) -> ! {
    kernel_main(dtb_ptr)
}

/// Detect memory from Device Tree Blob
fn detect_memory(dtb_ptr: usize) -> (usize, usize) {
    const DEFAULT_RAM_BASE: usize = 0x40000000;
    const DEFAULT_RAM_SIZE: usize = 128 * 1024 * 1024; // Must match QEMU -m setting

    if dtb_ptr == 0 {
        console::print("[Memory] No DTB pointer, using defaults\n");
        return (DEFAULT_RAM_BASE, DEFAULT_RAM_SIZE);
    }

    // SAFETY: QEMU passes a valid DTB pointer in x0
    let fdt = match unsafe { fdt::Fdt::from_ptr(dtb_ptr as *const u8) } {
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
        let size = region.size.unwrap_or(DEFAULT_RAM_SIZE);
        console::print("[Memory] Detected from DTB: base=0x");
        console::print(&alloc::format!("{:x}", base));
        console::print(", size=");
        console::print(&(size / 1024 / 1024).to_string());
        console::print(" MB\n");
        (base, size)
    } else {
        console::print("[Memory] No memory region in DTB, using defaults\n");
        (DEFAULT_RAM_BASE, DEFAULT_RAM_SIZE)
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
    let heap_size = ram_size / 4; // 32 MB for 128 MB RAM
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
    console::print(&alloc::format!(
        "PMM stats: {} total pages, {} allocated, {} free\n",
        total,
        allocated,
        free
    ));

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
            console::print(&alloc::format!("{}\n", e));
        }
    }

    // Initialize Embassy time driver (bridges ARM timer to Embassy async)
    embassy_time_driver::init();
    console::print("Embassy time driver initialized\n");

    // Initialize Embassy executor
    executor::init();

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
    console::print("Threading system initialized\n");

    // =========================================================================
    // Now enable preemptive scheduling (timer interrupts)
    // =========================================================================
    console::print("Configuring scheduler SGI...\n");
    gic::enable_irq(gic::SGI_SCHEDULER);

    console::print("Registering timer IRQ...\n");
    irq::register_handler(30, |irq| timer::timer_irq_handler(irq));

    console::print("Enabling timer...\n");
    timer::enable_timer_interrupts(10_000); // 10ms intervals
    console::print("Preemptive scheduling enabled (10ms timer -> SGI)\n");

    // Enable IRQ-safe allocations now that preemption is active
    allocator::enable_preemption_safe_alloc();

    // Run memory tests (no filesystem dependency)
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

    // =========================================================================
    // Filesystem initialization
    // =========================================================================
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
                                console::print(&alloc::format!("  [DIR]  {}\n", entry.name));
                            } else {
                                console::print(&alloc::format!(
                                    "  [FILE] {} ({} bytes)\n",
                                    entry.name,
                                    entry.size
                                ));
                            }
                        }
                    }

                    // Run filesystem tests
                    fs_tests::run_all_tests();

                    // Run threading tests (requires fs for parallel process tests)
                    if !tests::run_threading_tests() {
                        console::print("\n!!! THREADING TESTS FAILED - HALTING !!!\n");
                        halt();
                    }

                    // Run process execution tests
                    process_tests::run_all_tests();

                    // Run shell tests (pipelines with /bin binaries)
                    shell_tests::run_all_tests();
                }
                Err(e) => {
                    console::print("[FS] Filesystem init failed: ");
                    console::print(&alloc::format!("{}\n", e));
                    console::print("[FS] Continuing without filesystem...\n");
                }
            }
        }
        Err(e) => {
            console::print("[Block] Block device not found: ");
            console::print(&alloc::format!("{}\n", e));
            console::print("[Block] Continuing without filesystem...\n");
        }
    }

    console::print("--- Filesystem Initialization Done ---\n\n");

    if crate::config::COOPERATIVE_MAIN_THREAD {
        run_async_main();
    } else {
        run_async_main_preemptive();
    }
}

fn run_async_main_preemptive() -> ! {
    let thread_result = crate::threading::spawn_fn_with_options(|| {
            run_async_main();
            //  {
            //     Ok(()) => {
            //         console::print("[AsyncMain] Preemtive main thread finished\n");
            //     }
            //     Err(e) => {
            //         console::print("[AsyncMain] Preemtive main thread failed: ");
            //         console::print(e);
            //         console::print("\n");
            //     }
            // }
        },
        false,
    );

    match thread_result {
        Ok(thread_id) => {
            loop {
                if threading::is_thread_terminated(thread_id) {
                    break;
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
    use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

    // =========================================================================
    // Async Network initialization and main loop
    // =========================================================================
    console::print("\n--- Async Network Initialization ---\n");

    // Initialize the async network stack
    let net_init = match async_net::init() {
        Ok(init) => {
            console::print("[AsyncNet] Network initialized successfully\n");
            init
        }
        Err(e) => {
            console::print("[AsyncNet] Network init failed: ");
            console::print(e);
            console::print("\n");
            console::print("[Idle] Entering idle loop (no network)\n");
            loop {
                threading::yield_now();
            }
        }
    };

    console::print("--- Async Network Initialization Done ---\n\n");

    // Initialize SSH host key
    ssh::init_host_key();

    // Run the async main loop in the main thread
    // This drives both the network runner and the SSH server

    console::print("[AsyncMain] Starting async network loop...\n");

    // Simple waker that does nothing (we poll in a loop)
    static VTABLE: RawWakerVTable = RawWakerVTable::new(
        |_| RawWaker::new(core::ptr::null(), &VTABLE),
        |_| {},
        |_| {},
        |_| {},
    );
    let raw_waker = RawWaker::new(core::ptr::null(), &VTABLE);
    let waker = unsafe { Waker::from_raw(raw_waker) };
    let mut cx = Context::from_waker(&waker);

    let mut runner = net_init.runner;
    let stack = net_init.stack;

    // Extract loopback stack and runner
    let mut loopback_runner = net_init.loopback.runner;
    let loopback_stack = net_init.loopback.stack;

    // First, wait for IP address (DHCP or fallback to static)
    {
        let mut wait_ip_pinned = pin!(async_net::wait_for_ip(&stack));
        let runner_fut = runner.run();
        let mut runner_pinned = pin!(runner_fut);
        let loopback_runner_fut = loopback_runner.run();
        let mut loopback_runner_pinned = pin!(loopback_runner_fut);

        loop {
            // Poll the network runner (needed for DHCP to work)
            let _ = runner_pinned.as_mut().poll(&mut cx);
            // Poll the loopback runner
            let _ = loopback_runner_pinned.as_mut().poll(&mut cx);

            // Poll the wait_for_ip future
            if let Poll::Ready(()) = wait_ip_pinned.as_mut().poll(&mut cx) {
                break;
            }

            // Process pending IRQ work
            executor::process_irq_work();
            executor::run_once();
            threading::yield_now();
        }
    }

    console::print("[AsyncMain] Network ready!\n");
    console::print(
        "[AsyncMain] SSH Server: Connect with ssh -o StrictHostKeyChecking=no user@localhost -p 2222\n",
    );
    console::print("[AsyncMain] HTTP Server: http://localhost:8080/\n");

    // Store stack references for curl/nslookup commands
    async_net::set_global_stack(stack);
    async_net::set_loopback_stack(loopback_stack);

    // Pin the futures directly using the pin! macro (no unsafe needed)
    let mut runner_pinned = pin!(runner.run());
    let mut loopback_runner_pinned = pin!(loopback_runner.run());
    let mut ssh_pinned = pin!(ssh::run(stack));
    let mut web_pinned = pin!(web_server::run(stack));
    let mut web_loopback_pinned = pin!(web_server::run(loopback_stack));
    let mut mem_monitor_pinned = pin!(memory_monitor());

    // Enable IRQs for the main async loop
    // The boot thread (thread 0) starts with all exceptions masked.
    // We need to enable IRQs so that:
    // 1. SGIs can be delivered for thread scheduling (yield_now)
    // 2. Timer interrupts can fire for preemptive scheduling
    // 3. The embassy time driver can wake up on timer interrupts
    unsafe {
        // Clear the IRQ mask bit (bit 1 of DAIF, which is bit 7 of the value)
        core::arch::asm!("msr daifclr, #2", options(nomem, nostack));
    }

    loop {
        // Poll the main network runner
        let _ = runner_pinned.as_mut().poll(&mut cx);

        // Poll loopback runner - process any pending packets
        let _ = loopback_runner_pinned.as_mut().poll(&mut cx);

        // Poll the SSH server (runs curl commands that send to loopback)
        let _ = ssh_pinned.as_mut().poll(&mut cx);

        // Poll loopback runner again - process packets sent by curl
        let _ = loopback_runner_pinned.as_mut().poll(&mut cx);

        // Poll the HTTP web servers
        let _ = web_pinned.as_mut().poll(&mut cx);
        let _ = web_loopback_pinned.as_mut().poll(&mut cx);

        // Poll loopback runner again - process response packets from web server
        let _ = loopback_runner_pinned.as_mut().poll(&mut cx);

        // Poll the memory monitor
        let _ = mem_monitor_pinned.as_mut().poll(&mut cx);

        // Process pending IRQ work
        executor::process_irq_work();

        // Poll the executor for any other tasks
        executor::run_once();

        // Periodic stack canary check (every ~1000 iterations to reduce overhead)
        static mut CANARY_CHECK_COUNTER: u32 = 0;
        unsafe {
            CANARY_CHECK_COUNTER = CANARY_CHECK_COUNTER.wrapping_add(1);
            if CANARY_CHECK_COUNTER % 1000 == 0 && config::ENABLE_STACK_CANARIES {
                let bad = threading::check_all_stack_canaries();
                if !bad.is_empty() {
                    console::print("[WARN] Stack overflow detected in threads: ");
                    for tid in &bad {
                        console::print(&alloc::format!("{} ", tid));
                    }
                    console::print("\n");
                }
            }
        }

        // Yield to other threads (cooperative multitasking)
        threading::yield_now();
    }
}

/// Async task that periodically reports memory usage
async fn memory_monitor() -> ! {
    use core::fmt::Write;
    use embassy_time::{Duration, Timer};

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

        buf.clear();
        let _ = write!(
            buf,
            "[Mem] Used: {} KB | Free: {} KB | Peak: {} KB | Heap: {} MB | Allocs: {}\n",
            allocated_kb, free_kb, peak_kb, heap_mb, stats.allocation_count
        );
        console::print(buf.as_str());

        // Report every 10 seconds
        Timer::after(Duration::from_secs(10)).await;
    }
}
