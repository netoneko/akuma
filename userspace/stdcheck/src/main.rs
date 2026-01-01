//! stdcheck - Test std compatibility features
//!
//! Simple test program for heap allocation in userspace.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use libakuma::{exit, print};

#[no_mangle]
pub extern "C" fn _start() -> ! {
    print("=== stdcheck: Testing std compatibility ===\n\n");

    let mut passed = 0u32;
    let mut failed = 0u32;

    // Test 1: Vec basic operations
    print("[TEST] Vec... ");
    if test_vec() {
        print("PASS\n");
        passed += 1;
    } else {
        print("FAIL\n");
        failed += 1;
    }

    // Test 2: String operations
    print("[TEST] String... ");
    if test_string() {
        print("PASS\n");
        passed += 1;
    } else {
        print("FAIL\n");
        failed += 1;
    }

    // Test 3: Box allocation
    print("[TEST] Box... ");
    if test_box() {
        print("PASS\n");
        passed += 1;
    } else {
        print("FAIL\n");
        failed += 1;
    }

    print("\n=== stdcheck: All tests complete ===\n");
    
    if failed == 0 {
        print("Result: ALL PASSED\n");
        exit(0);
    } else {
        print("Result: SOME FAILED\n");
        exit(1);
    }
}

fn test_vec() -> bool {
    let mut v: Vec<i32> = Vec::new();
    v.push(1);
    v.push(2);
    v.push(3);
    v.len() == 3 && v[0] == 1 && v[2] == 3
}

fn test_string() -> bool {
    let s = String::from("Hello");
    s.len() == 5
}

fn test_box() -> bool {
    let b = Box::new(42i32);
    *b == 42
}
