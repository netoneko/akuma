//! Dropbear SSH Server Wrapper for Akuma OS
//!
//! This is a Rust entry point that ensures the environment is set up
//! correctly before executing the Dropbear C logic.

#![no_std]
#![no_main]

extern crate alloc;

use libakuma::{print, exit, mkdir_p};

#[no_mangle]
pub extern "C" fn _start() -> ! {
    main();
    exit(0);
}

fn main() {
    print("[dropbear] Starting userspace SSH server...
");

    // Ensure required directories exist
    mkdir_p("/etc/dropbear");
    mkdir_p("/var/log");

    // TODO: In Phase 4, we will call dropbear_main() from the compiled C code.
    // For now, this is a placeholder.
    print("[dropbear] Error: C entry point not yet linked.
");
}
