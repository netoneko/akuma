#![no_std]
#![no_main]

extern crate alloc;
use libakuma::{exit, print, println, print_dec, allocation_count};
use alloc::string::String;

#[no_mangle]
pub extern "C" fn _start() -> ! {
    println("allocstress: starting virtual address exhaustion test");
    
    let mut count = 0;
    loop {
        {
            // Trigger an allocation and immediate deallocation
            let _s = String::from("stress");
            count += 1;
        }

        if count % 10000 == 0 {
            print("Allocations: ");
            print_dec(count);
            print(" (actual count: ");
            print_dec(allocation_count());
            println(")");
        }
        
        // If we hit 2,000,000 and haven't crashed, the fix is definitely working.
        if count >= 2000000 {
            println("allocstress: reached 2,000,000 allocations without failure!");
            break;
        }
    }

    println("allocstress: done");
    exit(0);
}
