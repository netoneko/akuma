//! stackstress - Stress test for exception stack overflow detection
//!
//! Makes many rapid syscalls to trigger potential stack corruption early.
//! Run this FIRST before other tests to check for stack issues.
//!
//! Usage: stackstress [iterations] [mode]
//!   iterations - Number of test iterations (default: 100)
//!   mode       - 1=sleep, 2=write, 3=mixed (default: 3)

#![no_std]
#![no_main]

use libakuma::{exit, getpid, print, arg, argc};

const DEFAULT_ITERATIONS: u32 = 100;
const DEFAULT_MODE: u32 = 3;

#[no_mangle]
pub extern "C" fn _start() -> ! {
    let pid = getpid();
    
    // Parse arguments
    let iterations = if argc() > 1 {
        arg(1).and_then(|s| parse_u32(s)).unwrap_or(DEFAULT_ITERATIONS)
    } else {
        DEFAULT_ITERATIONS
    };
    
    let mode = if argc() > 2 {
        arg(2).and_then(|s| parse_u32(s)).unwrap_or(DEFAULT_MODE)
    } else {
        DEFAULT_MODE
    };

    print("stackstress: PID=");
    print_num(pid);
    print(" iterations=");
    print_num(iterations);
    print(" mode=");
    print_num(mode);
    print("\n");

    // Capture initial state for comparison
    let start_uptime = libakuma::uptime();
    
    match mode {
        1 => stress_sleep(iterations),
        2 => stress_write(iterations),
        _ => stress_mixed(iterations),
    }
    
    let end_uptime = libakuma::uptime();
    let duration_ms = (end_uptime - start_uptime) / 1000;
    
    print("stackstress: PASSED after ");
    print_num64(duration_ms);
    print("ms (");
    print_num(iterations);
    print(" iterations)\n");
    
    // Exit with PID as exit code - useful for verifying multiple processes
    exit(0);
}

/// Mode 1: Rapid short sleeps to stress schedule_blocking
fn stress_sleep(iterations: u32) {
    print("  mode: sleep (stressing schedule_blocking)\n");
    for i in 0..iterations {
        // Very short sleep - 1ms to maximize schedule_blocking calls
        libakuma::sleep_ms(1);
        
        // Periodically report progress
        if (i + 1) % 25 == 0 {
            print("    sleep iteration ");
            print_num(i + 1);
            print("/");
            print_num(iterations);
            print("\n");
        }
    }
}

/// Mode 2: Rapid writes to stress syscall handler
fn stress_write(iterations: u32) {
    print("  mode: write (stressing syscall path)\n");
    for i in 0..iterations {
        // Multiple writes per iteration to stress the syscall path
        print(".");
        
        // Every 50 iterations, newline
        if (i + 1) % 50 == 0 {
            print(" ");
            print_num(i + 1);
            print("\n");
        }
    }
    print("\n");
}

/// Mode 3: Mixed workload - sleeps + writes interleaved
fn stress_mixed(iterations: u32) {
    print("  mode: mixed (sleep + write + uptime)\n");
    for i in 0..iterations {
        // Short sleep
        libakuma::sleep_ms(1);
        
        // Write syscall
        print(".");
        
        // Uptime syscall (adds another syscall type)
        let _ = libakuma::uptime();
        
        // Every 25 iterations, report
        if (i + 1) % 25 == 0 {
            print(" ");
            print_num(i + 1);
            print("/");
            print_num(iterations);
            let uptime_ms = libakuma::uptime() / 1000;
            print(" t=");
            print_num64(uptime_ms);
            print("ms\n");
        }
    }
}

// Helper functions
fn parse_u32(s: &str) -> Option<u32> {
    let mut result: u32 = 0;
    for c in s.bytes() {
        if c >= b'0' && c <= b'9' {
            result = result.checked_mul(10)?.checked_add((c - b'0') as u32)?;
        } else {
            return None;
        }
    }
    Some(result)
}

fn print_num(n: u32) {
    print_num64(n as u64);
}

fn print_num64(n: u64) {
    if n == 0 {
        print("0");
        return;
    }

    let mut buf = [0u8; 20];
    let mut i = 0;
    let mut num = n;

    while num > 0 {
        buf[i] = b'0' + (num % 10) as u8;
        num /= 10;
        i += 1;
    }

    while i > 0 {
        i -= 1;
        let s = [buf[i]];
        libakuma::write(libakuma::fd::STDOUT, &s);
    }
}
