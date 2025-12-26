#![no_std]
#![no_main]

extern crate alloc;

mod allocator;
mod boot;
mod console;
mod exceptions;
mod executor;
mod gic;
mod irq;
mod network;
mod tests;
mod threading;
mod timer;
mod virtio_hal;

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

    // DTB pointer workaround: QEMU with -device loader puts DTB at 0x44000000
    // But we can't safely read it yet before setting up, so use after heap init

    // let ram_size = match detect_memory_size(dtb_ptr) {
    //     Ok(size) => {
    //         console::print("Detected RAM: ");
    //         console::print(&(size / 1024 / 1024).to_string());
    //         console::print(" MB\n");
    //         size
    //     }
    //     Err(e) => {
    //         console::print("Error detecting memory: ");
    //         console::print(e);
    //         console::print("\nUsing default 32 MB\n");
    //         32 * 1024 * 1024
    //     }
    // };

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

    // Skip executor - using threads instead
    // executor::init();

    // Initialize timer
    timer::init();
    console::print("Timer initialized\n");

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
    // Network initialization
    // =========================================================================
    console::print("\n--- Network Initialization ---\n");
    match network::init(0) {
        Ok(()) => {
            console::print("[Net] Network initialized successfully\n");
            console::print("[Net] Starting network server thread...\n");
            
            // Spawn network handler thread
            match threading::spawn(network::netcat_server_entry) {
                Ok(tid) => console::print(&alloc::format!("[Net] Server thread started (tid={})\n", tid)),
                Err(e) => {
                    console::print("[Net] Failed to spawn server thread: ");
                    console::print(e);
                    console::print("\n");
                }
            }
        }
        Err(e) => {
            console::print("[Net] Network init failed: ");
            console::print(e);
            console::print("\n");
        }
    }
    console::print("--- Network Initialization Done ---\n\n");

    // Thread 0 becomes the idle loop
    console::print("[Idle] Entering idle loop (network server running in background)\n");
    loop {
        threading::yield_now();
    }
}
