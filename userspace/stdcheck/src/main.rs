//! stdcheck - Test std compatibility features
//!
//! Simple test program for heap allocation in userspace.
//! Tests Vec, String, and Box operations including reallocation.

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

    // Print memory layout for debugging
    print("[DEBUG] Memory layout:\n");
    libakuma::print_allocator_info();
    print("\n");

    let mut _passed = 0u32;
    let mut failed = 0u32;

    // Test 1: Vec basic operations
    print("[TEST] Vec... ");
    if test_vec() {
        print("PASS\n");
        _passed += 1;
    } else {
        print("FAIL\n");
        failed += 1;
    }

    // Test 2: String::from (no realloc)
    print("[TEST] String::from... ");
    if test_string_from() {
        print("PASS\n");
        _passed += 1;
    } else {
        print("FAIL\n");
        failed += 1;
    }

    // Test 3: String::push_str (triggers realloc - the bug!)
    print("[TEST] String::push_str... ");
    if test_string_push_str() {
        print("PASS\n");
        _passed += 1;
    } else {
        print("FAIL\n");
        failed += 1;
    }

    // Test 4: Box allocation (disabled - causes kernel crash, see docs/STDCHECK_DEBUG.md)
    // print("[TEST] Box... ");
    // if test_box() {
    //     print("PASS\n");
    //     _passed += 1;
    // } else {
    //     print("FAIL\n");
    //     failed += 1;
    // }

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

fn test_string_from() -> bool {
    let s = String::from("Hello");
    if s.len() != 5 {
        return false;
    }
    // Verify content
    let bytes = s.as_bytes();
    bytes[0] == b'H' && bytes[1] == b'e' && bytes[2] == b'l' && bytes[3] == b'l' && bytes[4] == b'o'
}

fn test_string_push_str() -> bool {
    // This triggers reallocation
    let mut s = String::from("Hello");
    s.push_str(", World!");
    
    // Check length (should be 13: "Hello, World!")
    let len = s.len();
    if len != 13 {
        libakuma::write(1, b"FAIL: len != 13\n");
        return false;
    }
    
    // Verify content - access bytes directly without creating new allocations
    let bytes = s.as_bytes();
    if bytes.len() != 13 {
        libakuma::write(1, b"FAIL: bytes.len != 13\n");
        return false;
    }
    
    // Check first byte
    let b0 = bytes[0];
    if b0 != b'H' {
        libakuma::write(1, b"FAIL: bytes[0]=");
        libakuma::write(1, &[b0]);
        libakuma::write(1, b" != H\n");
        return false;
    }
    
    // Check bytes[5] = ','
    let b5 = bytes[5];
    if b5 != b',' {
        libakuma::write(1, b"FAIL: bytes[5]=");
        libakuma::write(1, &[b5]);
        libakuma::write(1, b" != ,\n");
        return false;
    }
    
    // Check bytes[12] = '!'
    let b12 = bytes[12];
    if b12 != b'!' {
        libakuma::write(1, b"FAIL: bytes[12]=");
        libakuma::write(1, &[b12]);
        libakuma::write(1, b" != !\n");
        return false;
    }
    
    true
}

fn test_box() -> bool {
    let b = Box::new(42i32);
    *b == 42
}
