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
mod threading;
mod timer;
mod virtio_hal;

use alloc::string::ToString;
use alloc::vec::Vec;

use core::panic::PanicInfo;
use fdt::Fdt;
use fdt::FdtError;

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
    loop {}
}

static PROMPT: &str = "Akuma >: ";

#[unsafe(no_mangle)]
pub extern "C" fn rust_start(mut dtb_ptr: usize) -> ! {
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
        loop {}
    };

    if let Err(e) = allocator::init(heap_start, heap_size) {
        console::print("Allocator init failed: ");
        console::print(e);
        console::print("\n");
        loop {}
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

    // Initialize threading (IRQs still enabled, timer not yet configured)
    console::print("Initializing threading...\n");
    threading::init();
    console::print("Threading system initialized\n");

    // Enable timer-driven preemptive multitasking via SGI
    // 1. Enable SGI 0 for scheduling (SGIs are always enabled, but register a dummy handler)
    console::print("Configuring scheduler SGI...\n");
    gic::enable_irq(gic::SGI_SCHEDULER);
    
    // 2. Timer IRQ (PPI 14, maps to IRQ 30) will trigger the SGI
    console::print("Registering timer IRQ...\n");
    irq::register_handler(30, |irq| timer::timer_irq_handler(irq));
    
    console::print("Enabling timer...\n");
    timer::enable_timer_interrupts(10_000); // 10ms intervals
    console::print("Preemptive scheduling enabled (10ms timer -> SGI)\n");

    // Test allocator
    let mut test_vec: Vec<u32> = Vec::new();
    for i in 0..10 {
        test_vec.push(i);
    }
    test_vec.remove(0);
    test_vec.insert(0, 99);
    drop(test_vec);
    console::print("Allocator OK\n");

    // Heartbeat thread
    extern "C" fn heartbeat_thread() -> ! {
        unsafe {
            const UART_BASE: *mut u8 = 0x0900_0000 as *mut u8;
            UART_BASE.write_volatile(b'H');
            UART_BASE.write_volatile(b'!');
            
            loop {
                for _ in 0..10_000_000 {
                    core::arch::asm!("nop");
                }
                UART_BASE.write_volatile(b'H');
            }
        }
    }

    // Network initialization thread  
    extern "C" fn network_thread() -> ! {
        console::print("\n[Net] Starting network initialization...\n");
        
        // Try network init (busy-waits will be preempted)
        match network::init(0) {
            Ok(()) => console::print("[Net] SUCCESS!\n"),
            Err(e) => {
                console::print("[Net] Failed: ");
                console::print(e);
                console::print("\n");
            }
        }
        
        console::print("[Net] Thread done, entering idle loop\n");
        
        loop {
            unsafe { core::arch::asm!("wfi") };
        }
    }

    // Spawn threads
    threading::spawn(heartbeat_thread).expect("Failed to spawn heartbeat thread");
    console::print("Heartbeat thread spawned\n");
    
    // Network thread - now works with preemptive threading!
    threading::spawn(network_thread).expect("Failed to spawn network thread");
    console::print("Network thread spawned\n");

    let mut should_exit = false;
    let mut buffer = Vec::new();
    let mut prompt_shown = false;

    while should_exit == false {
        // No executor - threads run preemptively via timer IRQ

        // Show prompt if we're ready for input
        if !prompt_shown && buffer.is_empty() {
            console::print(PROMPT);
            prompt_shown = true;
        }

        // Check for input (non-blocking)
        if console::has_char() {
            let c = console::getchar();
            buffer.push(c);
            console::print(&(c as char).to_string());

            // Process line when Enter is pressed
            if c == b'\n' || c == b'\r' {
                console::print("\n");
                prompt_shown = false;

                if let Ok(text) = core::str::from_utf8(&buffer[..buffer.len() - 1]) {
                    match text.trim().to_lowercase().as_str() {
                        "exit" => {
                            console::print_as_akuma("MEOWWWW!");
                            should_exit = true;
                        }
                        "meow" => {
                            console::print_as_akuma("Meow");
                        }
                        "time" => {
                            console::print("UTC: ");
                            console::print(&timer::utc_iso8601());
                            console::print("\nUptime: ");
                            console::print(&(timer::uptime_us() / 1_000_000).to_string());
                            console::print(" seconds\n");
                        }
                        "uptime" => {
                            let uptime_sec = timer::uptime_us() / 1_000_000;
                            let days = uptime_sec / 86400;
                            let hours = (uptime_sec % 86400) / 3600;
                            let minutes = (uptime_sec % 3600) / 60;
                            let seconds = uptime_sec % 60;
                            console::print("Uptime: ");
                            console::print(&days.to_string());
                            console::print(" days, ");
                            console::print(&hours.to_string());
                            console::print(":");
                            console::print(&minutes.to_string());
                            console::print(":");
                            console::print(&seconds.to_string());
                            console::print("\n");
                        }
                        "" => {
                            // Empty line, just show prompt again
                        }
                        _ => {
                            console::print_as_akuma("pffft");
                        }
                    }
                }
                buffer.clear();
            }
        }
    }

    // _start must never return (!) - hang forever
    loop {}
}

fn detect_memory_size(dtb_addr: usize) -> Result<usize, &'static str> {
    if dtb_addr == 0 {
        return Err("DTB pointer is null");
    }

    unsafe {
        match Fdt::from_ptr(dtb_addr as *const u8) {
            Ok(fdt) => {
                let total: usize = fdt
                    .memory()
                    .regions()
                    .filter_map(|region| region.size)
                    .sum();

                if total == 0 {
                    Err("No memory regions found in DTB")
                } else {
                    Ok(total)
                }
            }
            Err(err) => Err(match err {
                FdtError::BadMagic => "Bad FDT magic value",
                FdtError::BadPtr => "Bad FDT pointer",
                FdtError::BufferTooSmall => "Buffer too small",
            }),
        }
    }
}

// Example async task - prints every 5 seconds
async fn async_example_task() {
    let mut counter = 0;
    loop {
        executor::sleep_sec(5).await;
        counter += 1;
        console::print("[Async] Heartbeat #");
        console::print(&counter.to_string());
        console::print(" at ");
        console::print(&timer::utc_iso8601_simple());
        console::print("\n");
    }
}

// Example network async task - polls every 100ms
async fn async_network_task() {
    loop {
        executor::sleep_ms(100).await;
        // Poll network stack
        network::poll();
    }
}
