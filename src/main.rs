#![no_std]
#![no_main]

extern crate alloc;

mod allocator;
mod boot;
mod console;

use alloc::vec::Vec;
use alloc::string::ToString;
use core::panic::PanicInfo;
use fdt::Fdt;

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
    
    let code_and_stack = ram_size / 16;  // 1/16 of total RAM
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

    let mut should_exit = false;
    while should_exit == false {
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
            Err(_) => Err("could not detect memory size"),
        }
    }
}
