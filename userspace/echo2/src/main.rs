//! echo2 - A simple echo program for Akuma
//!
//! Reads lines from stdin and echoes them back to stdout.

#![no_std]
#![no_main]

use libakuma::{exit, fd, print, read, write};

/// Entry point
#[no_mangle]
pub extern "C" fn _start() -> ! {
    // Print a greeting
    print("echo2: Ready to echo!\n");

    let mut buf = [0u8; 256];

    loop {
        // Read from stdin
        let n = read(fd::STDIN, &mut buf);

        if n <= 0 {
            // EOF or error
            break;
        }

        // Echo back to stdout
        write(fd::STDOUT, &buf[..n as usize]);
    }

    print("echo2: Goodbye!\n");
    exit(0);
}

