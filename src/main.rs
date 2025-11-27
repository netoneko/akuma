#![no_std]
#![no_main]

extern crate alloc;

mod allocator;
mod boot;
mod console;

use alloc::string::String;
use core::panic::PanicInfo;
#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {}
}

#[unsafe(no_mangle)]
pub extern "C" fn _start() -> ! {
    allocator::init();

    console::print("Hello from bare metal!\n");
    console::print("Allocator initialized!\n");
    console::print("Akuma >: ");

    loop {
        let mut buffer = [0u8; 100];
        let len = console::read_line(&mut buffer);
        console::print("\nYou typed: ");
        if let Ok(text) = core::str::from_utf8(&buffer[..len]) {
            console::print(text);
            // Test allocator - create a String
            let _allocated = String::from(text);
        }
        console::print("\nAkuma >: ");
    }
}
