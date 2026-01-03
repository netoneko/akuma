//! elftest - Minimal ELF loading verification
//!
//! This is the simplest possible userspace binary to verify ELF loading works.
//! If this binary runs and exits with code 42, ELF loading is correct.
//!
//! Tests:
//! - Entry point is reached
//! - Code segment is properly mapped and executable
//! - exit() syscall works

#![no_std]
#![no_main]

use libakuma::{exit, print};

#[no_mangle]
pub extern "C" fn _start() -> ! {
    // If we reach here, ELF loading worked!
    print("ELF OK\n");
    
    // Exit with a specific code to verify
    exit(42);
}

