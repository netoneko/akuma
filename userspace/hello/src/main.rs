//! hello - Long-running process for testing ps command
//!
//! Outputs "hello" periodically and runs for a configurable duration.
//! Used to verify that `ps` can list running processes.

#![no_std]
#![no_main]

use libakuma::{exit, print, getpid};

// ============================================================================
// Configuration
// ============================================================================

/// Delay between each "hello" output (in busy-wait iterations)
/// Approximate: 10_000_000 iterations ≈ 1 second on QEMU
/// So 100_000_000 ≈ 10 seconds
const DELAY_ITERATIONS: u64 = 100_000_000;

/// Total number of "hello" outputs before exiting
/// 6 outputs × 10 seconds = 60 seconds total runtime
const TOTAL_OUTPUTS: u32 = 6;

// ============================================================================
// Implementation  
// ============================================================================

/// Busy-wait delay (no sleep syscall available)
#[inline(never)]
fn delay(iterations: u64) {
    for _ in 0..iterations {
        // Prevent optimizer from eliminating the loop
        core::hint::black_box(());
    }
}

#[no_mangle]
pub extern "C" fn _start() -> ! {
    let pid = getpid();
    
    // Print startup message with PID
    print("hello: started (PID ");
    print_num(pid);
    print(")\n");
    
    // Output "hello" periodically
    for i in 0..TOTAL_OUTPUTS {
        print("hello (");
        print_num(i + 1);
        print("/");
        print_num(TOTAL_OUTPUTS);
        print(")\n");
        
        // Don't delay after the last output
        if i + 1 < TOTAL_OUTPUTS {
            delay(DELAY_ITERATIONS);
        }
    }
    
    print("hello: done\n");
    exit(0);
}

/// Print a u32 number (simple implementation)
fn print_num(n: u32) {
    if n == 0 {
        print("0");
        return;
    }
    
    let mut buf = [0u8; 10];
    let mut i = 0;
    let mut num = n;
    
    while num > 0 {
        buf[i] = b'0' + (num % 10) as u8;
        num /= 10;
        i += 1;
    }
    
    // Reverse and print
    while i > 0 {
        i -= 1;
        let s = [buf[i]];
        // Use raw write syscall for single char
        libakuma::write(libakuma::fd::STDOUT, &s);
    }
}

