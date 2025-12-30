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
mod console;
mod dns;
mod embassy_net_driver;
mod embassy_time_driver;
mod embassy_virtio_driver;
mod exceptions;
mod executor;
mod fs;
mod fs_tests;
mod gic;
mod irq;
mod netcat_server;
mod network;
mod rhai;
mod rng;
mod shell;
mod shell_tests;
mod ssh;
mod ssh_crypto;
mod ssh_server;
mod tests;
mod threading;
mod timer;
mod tls;
mod tls_rng;
mod vfs;
mod virtio_hal;
mod web_server;

use alloc::string::ToString;

use core::panic::PanicInfo;

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
pub extern "C" fn rust_start(_dtb_ptr: usize) -> ! {
    kernel_main()
}

/// Main kernel initialization - all safe code
fn kernel_main() -> ! {
    const RAM_BASE: usize = 0x40000000;

    let ram_size = 128 * 1024 * 1024; // 128 MB

    let code_and_stack = ram_size / 16; // 1/16 of total RAM
    let heap_start = RAM_BASE + code_and_stack;

    let heap_size = if ram_size > code_and_stack {
        ram_size - code_and_stack
    } else {
        console::print("Not enough RAM for heap\n");
        halt();
    };

    if let Err(e) = allocator::init(heap_start, heap_size) {
        console::print("Allocator init failed: ");
        console::print(e);
        console::print("\n");
        halt();
    }

    console::print("Heap initialized: ");
    console::print(&(heap_size / 1024 / 1024).to_string());
    console::print(" MB\n");

    // Initialize GIC (Generic Interrupt Controller)
    gic::init();
    console::print("GIC initialized\n");

    // Set up exception vectors and enable IRQs
    exceptions::init();
    console::print("IRQ handling enabled\n");

    // Initialize timer
    timer::init();
    console::print("Timer initialized\n");

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

    // Run system tests (includes allocator tests)
    if !tests::run_all() {
        console::print("\n!!! SYSTEM TESTS FAILED - HALTING !!!\n");
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
    // Run shell tests (pipelines and grep)
    // =========================================================================
    shell_tests::run_all_tests();

    // =========================================================================
    // Hardware RNG initialization
    // =========================================================================
    match rng::init() {
        Ok(()) => {
            console::print("[RNG] Hardware RNG initialized successfully\n");
        }
        Err(e) => {
            console::print("[RNG] Hardware RNG not available: ");
            console::print(&alloc::format!("{}\n", e));
            console::print("[RNG] Falling back to timer-based entropy\n");
        }
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
    run_async_main(net_init);
}

/// Run the async main loop
/// This is the main entry point for async networking
fn run_async_main(net_init: async_net::NetworkInit) -> ! {
    use core::future::Future;
    use core::pin::pin;
    use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

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
    let mut ssh_pinned = pin!(ssh_server::run(stack));
    let mut web_pinned = pin!(web_server::run(stack));
    let mut web_loopback_pinned = pin!(web_server::run(loopback_stack));
    let mut mem_monitor_pinned = pin!(memory_monitor());

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

        // Yield to other threads (cooperative multitasking)
        threading::yield_now();
    }
}

/// Async task that periodically reports memory usage
async fn memory_monitor() -> ! {
    use embassy_time::{Duration, Timer};

    // Wait a bit before starting to let system stabilize
    Timer::after(Duration::from_secs(5)).await;

    console::print("[MemMonitor] Memory monitoring started\n");

    loop {
        let stats = allocator::stats();
        let allocated_kb = stats.allocated / 1024;
        let free_kb = stats.free / 1024;
        let peak_kb = stats.peak_allocated / 1024;
        let heap_mb = stats.heap_size / 1024 / 1024;

        console::print(&alloc::format!(
            "[Mem] Used: {} KB | Free: {} KB | Peak: {} KB | Heap: {} MB | Allocs: {}\n",
            allocated_kb,
            free_kb,
            peak_kb,
            heap_mb,
            stats.allocation_count
        ));

        // Report every 10 seconds
        Timer::after(Duration::from_secs(10)).await;
    }
}
