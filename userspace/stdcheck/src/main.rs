//! stdcheck - Test std compatibility features
//!
//! This program tests that std-like features work correctly in userspace,
//! including heap allocation (Vec, String, Box) and HashMap.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use core::hash::{BuildHasherDefault, Hasher};
use hashbrown::HashMap as HashbrownMap;
use libakuma::{exit, print};

/// Simple FNV-1a hasher for no_std HashMap
/// This is a basic hasher suitable for testing purposes
#[derive(Default)]
struct FnvHasher(u64);

impl Hasher for FnvHasher {
    fn write(&mut self, bytes: &[u8]) {
        const FNV_PRIME: u64 = 0x100000001b3;
        for byte in bytes {
            self.0 ^= *byte as u64;
            self.0 = self.0.wrapping_mul(FNV_PRIME);
        }
    }

    fn finish(&self) -> u64 {
        self.0
    }
}

type FnvBuildHasher = BuildHasherDefault<FnvHasher>;
type FnvHashMap<K, V> = HashbrownMap<K, V, FnvBuildHasher>;

// Re-export as HashMap for more natural usage
#[allow(dead_code)]
type HashMap<K, V> = FnvHashMap<K, V>;

/// Simple test result tracking
struct TestRunner {
    passed: u32,
    failed: u32,
}

impl TestRunner {
    fn new() -> Self {
        Self { passed: 0, failed: 0 }
    }

    fn run(&mut self, name: &str, result: bool) {
        print("[TEST] ");
        print(name);
        print("... ");
        if result {
            print("PASS\n");
            self.passed += 1;
        } else {
            print("FAIL\n");
            self.failed += 1;
        }
    }

    fn summary(&self) -> bool {
        print("\n=== Summary ===\n");
        print("Passed: ");
        print_num(self.passed);
        print("\nFailed: ");
        print_num(self.failed);
        print("\n");
        self.failed == 0
    }
}

/// Print a number (simple implementation since we don't have format!)
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
        let c = buf[i] as char;
        let s: [u8; 1] = [c as u8];
        if let Ok(s) = core::str::from_utf8(&s) {
            print(s);
        }
    }
}

#[no_mangle]
pub extern "C" fn _start() -> ! {
    print("=== stdcheck: Testing std compatibility ===\n\n");

    let mut runner = TestRunner::new();

    // Test 1: Vec basic operations
    runner.run("Vec::new and push", test_vec_basic());

    // Test 2: Vec with capacity
    runner.run("Vec::with_capacity", test_vec_capacity());

    // Test 3: String operations
    runner.run("String::from and push_str", test_string_basic());

    // Test 4: String concatenation
    runner.run("String concatenation", test_string_concat());

    // Test 5: Box allocation
    runner.run("Box::new", test_box_basic());

    // Test 6: Box with large data
    runner.run("Box with array", test_box_array());

    // Test 7: HashMap basic
    runner.run("HashMap insert and get", test_hashmap_basic());

    // Test 8: HashMap iteration
    runner.run("HashMap len and contains", test_hashmap_len());

    // Test 9: Nested allocations
    runner.run("Vec<String>", test_vec_of_strings());

    // Test 10: HashMap with String keys
    runner.run("HashMap<String, i32>", test_hashmap_string_keys());

    print("\n");
    let all_passed = runner.summary();

    if all_passed {
        print("\n=== stdcheck: All tests PASSED ===\n");
        exit(0);
    } else {
        print("\n=== stdcheck: Some tests FAILED ===\n");
        exit(1);
    }
}

// ============================================================================
// Test Functions
// ============================================================================

fn test_vec_basic() -> bool {
    let mut v: Vec<i32> = Vec::new();
    v.push(1);
    v.push(2);
    v.push(3);
    v.push(4);
    
    v.len() == 4 && v[0] == 1 && v[3] == 4
}

fn test_vec_capacity() -> bool {
    let mut v: Vec<i32> = Vec::with_capacity(100);
    v.push(42);
    
    v.capacity() >= 100 && v.len() == 1 && v[0] == 42
}

fn test_string_basic() -> bool {
    let mut s = String::from("Hello");
    s.push_str(", World!");
    
    s == "Hello, World!" && s.len() == 13
}

fn test_string_concat() -> bool {
    let s1 = String::from("foo");
    let s2 = String::from("bar");
    let s3 = s1 + &s2;
    
    s3 == "foobar"
}

fn test_box_basic() -> bool {
    let b = Box::new(42i32);
    *b == 42
}

fn test_box_array() -> bool {
    let b = Box::new([1u8; 256]);
    b[0] == 1 && b[255] == 1 && b.len() == 256
}

fn test_hashmap_basic() -> bool {
    let mut map: FnvHashMap<&str, i32> = FnvHashMap::default();
    map.insert("one", 1);
    map.insert("two", 2);
    map.insert("three", 3);
    
    map.get("one") == Some(&1) 
        && map.get("two") == Some(&2)
        && map.get("three") == Some(&3)
        && map.get("four").is_none()
}

fn test_hashmap_len() -> bool {
    let mut map: FnvHashMap<i32, i32> = FnvHashMap::default();
    map.insert(1, 100);
    map.insert(2, 200);
    map.insert(3, 300);
    
    map.len() == 3 
        && map.contains_key(&1)
        && map.contains_key(&2)
        && !map.contains_key(&99)
}

fn test_vec_of_strings() -> bool {
    let mut v: Vec<String> = Vec::new();
    v.push(String::from("first"));
    v.push(String::from("second"));
    v.push(String::from("third"));
    
    v.len() == 3 && v[0] == "first" && v[2] == "third"
}

fn test_hashmap_string_keys() -> bool {
    let mut map: FnvHashMap<String, i32> = FnvHashMap::default();
    map.insert(String::from("alpha"), 1);
    map.insert(String::from("beta"), 2);
    
    map.get("alpha") == Some(&1) && map.get("beta") == Some(&2)
}

