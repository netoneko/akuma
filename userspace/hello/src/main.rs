//! hello - Long-running process for testing argv and streaming
//!
//! Outputs "hello" periodically with configurable count and delay.
//! Usage: hello [outputs] [delay_ms]
//!   outputs  - Number of outputs (default: 10)
//!   delay_ms - Delay between outputs in milliseconds (default: 1000)

#![no_std]
#![no_main]

use libakuma::{exit, getpid, print, arg, argc};

// ============================================================================
// Configuration Defaults
// ============================================================================

const DEFAULT_OUTPUTS: u32 = 10;
const DEFAULT_DELAY_MS: u64 = 1000;
const MICROSECONDS: u64 = 1000;

// ============================================================================
// Implementation
// ============================================================================

#[no_mangle]
pub extern "C" fn _start() -> ! {
    let start_time = libakuma::uptime();
    let pid = getpid();
    
    // Parse command line arguments
    let total_outputs = if argc() > 1 {
        arg(1).and_then(|s| parse_u32(s)).unwrap_or(DEFAULT_OUTPUTS)
    } else {
        DEFAULT_OUTPUTS
    };
    
    let delay_ms = if argc() > 2 {
        arg(2).and_then(|s| parse_u64(s)).unwrap_or(DEFAULT_DELAY_MS)
    } else {
        DEFAULT_DELAY_MS
    };

    // Print startup message
    print("hello: started (PID ");
    print_num(pid);
    print(", outputs=");
    print_num(total_outputs);
    print(", delay_ms=");
    print_num64(delay_ms);
    print(")\n");

    // Output "hello" periodically
    for i in 0..total_outputs {
        print("hello (");
        print_num(i + 1);
        print("/");
        print_num(total_outputs);
        print(")\n");
        
        // Print uptime for debugging
        let uptime = libakuma::uptime();
        print_num64(uptime);
        print("\n");

        // Sleep between outputs (except after the last one)
        if i + 1 < total_outputs {
            libakuma::sleep_ms(delay_ms);
        }
    }

    print("hello: done\n");
    let end_time = libakuma::uptime();
    let total_runtime_ms = (end_time - start_time)/MICROSECONDS;
    // Only (n-1) sleeps happen (no sleep after last output)
    let expected_runtime_ms = (total_outputs - 1) as u64 * delay_ms;
    print("hello: uptime=");
    print_num64(total_runtime_ms);
    print("ms ");
    print("expected=");
    print_num64(expected_runtime_ms);
    print("ms ");
    // Use saturating_sub to avoid underflow, then show signed difference
    if total_runtime_ms >= expected_runtime_ms {
        print("overhead=+");
        print_num64(total_runtime_ms - expected_runtime_ms);
    } else {
        print("overhead=-");
        print_num64(expected_runtime_ms - total_runtime_ms);
    }
    print("ms\n");
    exit(0);
}

// ============================================================================
// Helper Functions
// ============================================================================

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

fn parse_u64(s: &str) -> Option<u64> {
    let mut result: u64 = 0;
    for c in s.bytes() {
        if c >= b'0' && c <= b'9' {
            result = result.checked_mul(10)?.checked_add((c - b'0') as u64)?;
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

    // Reverse and print
    while i > 0 {
        i -= 1;
        let s = [buf[i]];
        libakuma::write(libakuma::fd::STDOUT, &s);
    }
}
