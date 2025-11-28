#![no_std]
#![no_main]

extern crate alloc;

mod allocator;
mod boot;
mod console;
mod executor;
mod network;
mod timer;

use alloc::string::ToString;
use alloc::vec::Vec;

use core::panic::PanicInfo;
use fdt::Fdt;
use fdt::FdtError;

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {}
}

static PROMPT: &str = "Akuma >: ";

#[unsafe(no_mangle)]
pub extern "C" fn _start(dtb_ptr: usize) -> ! {
    const RAM_BASE: usize = 0x40000000;

    let ram_size = match detect_memory_size(dtb_ptr) {
        Ok(size) => {
            console::print("Detected RAM: ");
            console::print(&(size / 1024 / 1024).to_string());
            console::print(" MB\n");
            size
        }
        Err(e) => {
            console::print("Error detecting memory: ");
            console::print(e);
            console::print("\nUsing default 32 MB\n");
            32 * 1024 * 1024
        }
    };

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

    // // Initialize async executor
    // executor::init();
    // console::print("Async executor initialized\n");

    // // Initialize timer
    // timer::init();
    // console::print("Timer initialized\n");

    // // Set UTC time to a known value (example: 2025-11-28 12:00:00 UTC)
    // // In a real system, you'd get this from NTP or RTC
    // let example_utc_us = 1732795200_000000u64; // 2025-11-28 12:00:00 UTC
    // timer::set_utc_time_us(example_utc_us);

    // console::print("Current UTC time: ");
    // console::print(&timer::utc_iso8601());
    // console::print("\n");

    // console::print("Uptime: ");
    // console::print(&(timer::uptime_us() / 1_000_000).to_string());
    // console::print(" seconds\n");

    // // Initialize network stack
    // network::init();
    // console::print("Network stack initialized\n");

    // // Spawn example async tasks
    // executor::spawn(async_example_task());
    // executor::spawn(async_network_task());

    let mut should_exit = false;
    while should_exit == false {
        // Run async tasks
        // executor::run_once();
        // network::poll();

        console::print(PROMPT);

        let mut buffer = Vec::new();
        let len = console::read_line(&mut buffer, true);
        if len == 0 {
            continue;
        }
        if let Ok(text) = core::str::from_utf8(&buffer[..len]) {
            console::print("\n");
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
                _ => {
                    console::print_as_akuma("pffft");
                }
            }
        }
    }

    // _start must never return (!) - hang forever
    loop {}
}

fn detect_memory_size(dtb_addr: usize) -> Result<usize, &'static str> {
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
