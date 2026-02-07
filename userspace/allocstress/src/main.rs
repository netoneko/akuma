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
        
        // The budget is ~196,000 allocations. 
        // If we hit 250,000 and haven't crashed, the fix might be working.
        if count >= 250000 {
            println("allocstress: reached 250,000 allocations without failure!");
            break;
        }
    }

    println("allocstress: done");
    exit(0);
}
