#![no_std]
#![no_main]

extern crate alloc;

mod allocator;
mod boot;
mod console;

use alloc::string::String;
use alloc::vec::Vec;
use core::panic::PanicInfo;

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {}
}

static PROMPT: &str = "Akuma >: ";

#[unsafe(no_mangle)]
pub extern "C" fn _start() -> ! {
    allocator::init();

    console::print(PROMPT);

    let mut should_exit = false;    
    while should_exit == false {
        let mut buffer = Vec::new();
        let len = console::read_line(&mut buffer, true);
        if len == 0 {
            continue;
        }
        if let Ok(text) = core::str::from_utf8(&buffer[..len]) {
            match text {
                "exit" => {
                    console::print("\nBye!\n");
                    should_exit = true;
                }
                "meow" => {
                    console::print("\nMeow\n");
                }
                _ => {
                    // nothing to do for now
                }
            }
        }
        console::print(PROMPT);
    }
}
