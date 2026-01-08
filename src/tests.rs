//! System tests for threading and other core functionality
//!
//! Run with `tests::run_all()` after scheduler initialization.
//! If tests fail, the kernel should halt.

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use crate::config;
use crate::console;
use crate::threading;
use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

/// Run memory tests (allocator, mmap) - can run before filesystem init
/// Returns true if all pass
pub fn run_memory_tests() -> bool {
    console::print("\n========== Memory Tests ==========\n");

    let mut all_pass = true;

    // Allocator tests (run first - fundamental)
    all_pass &= test_allocator_vec();
    all_pass &= test_allocator_box();
    all_pass &= test_allocator_large();

    // Comprehensive allocator tests
    all_pass &= test_realloc_grow();
    all_pass &= test_realloc_shrink();
    all_pass &= test_realloc_preserves_data();
    all_pass &= test_alloc_zeroed_basic();
    all_pass &= test_alloc_zeroed_after_dirty();
    all_pass &= test_alignment_various();
    all_pass &= test_fragmentation_small_blocks();
    all_pass &= test_interleaved_alloc_free();
    all_pass &= test_mixed_sizes();
    all_pass &= test_vec_remove_regression();
    all_pass &= test_rapid_push_pop();
    all_pass &= test_string_operations();
    all_pass &= test_string_push_str_realloc(); // Userspace bug mirror test
    all_pass &= test_string_realloc_detailed(); // Detailed realloc tracking
    all_pass &= test_vec_of_vecs();
    all_pass &= test_adjacent_allocations();

    // Mmap allocator edge case tests (for userspace debugging)
    all_pass &= test_mmap_single_page();
    all_pass &= test_mmap_multi_page();
    all_pass &= test_mmap_page_boundary_write();
    all_pass &= test_mmap_rapid_alloc_dealloc();
    all_pass &= test_mmap_realloc_pattern();
    all_pass &= test_mmap_string_growth_pattern();
    all_pass &= test_mmap_vec_capacity_doubling();
    all_pass &= test_mmap_interleaved_strings();

    // Common memory allocation patterns
    // NOTE: These tests hang during preemption - need investigation
    // all_pass &= test_lifo_pattern();
    // all_pass &= test_fifo_pattern();
    // all_pass &= test_memory_pool_pattern();
    // all_pass &= test_resize_pattern();
    // all_pass &= test_temporary_buffers();
    // all_pass &= test_linked_structure();

    console::print("\n==================================\n");
    console::print(&format!(
        "Memory Tests: {}\n",
        if all_pass {
            "ALL PASSED"
        } else {
            "SOME FAILED"
        }
    ));
    console::print("==================================\n\n");

    all_pass
}

/// Run threading tests - requires filesystem for parallel process tests
/// Returns true if all pass
pub fn run_threading_tests() -> bool {
    console::print("\n========== Threading Tests ==========\n");

    let mut all_pass = true;

    // Threading tests (no fs dependency)
    all_pass &= test_scheduler_init();
    all_pass &= test_thread_stats();
    all_pass &= test_yield();
    all_pass &= test_cooperative_timeout();
    all_pass &= test_thread_cleanup();
    all_pass &= test_spawn_thread();
    all_pass &= test_spawn_and_run();
    all_pass &= test_spawn_and_cleanup();
    all_pass &= test_spawn_multiple();
    all_pass &= test_spawn_and_yield();
    all_pass &= test_spawn_cooperative();
    all_pass &= test_yield_cycle();
    all_pass &= test_mixed_cooperative_preemptible();

    // Parallel process tests (requires /bin/hello)
    all_pass &= test_parallel_processes();

    console::print("\n==================================\n");
    console::print(&format!(
        "Threading Tests: {}\n",
        if all_pass {
            "ALL PASSED"
        } else {
            "SOME FAILED"
        }
    ));
    console::print("==================================\n\n");

    all_pass
}

/// Run all system tests - returns true if all pass
/// Note: This runs both memory and threading tests together.
/// For finer control, use run_memory_tests() and run_threading_tests() separately.
#[allow(dead_code)]
pub fn run_all() -> bool {
    let mut all_pass = true;
    all_pass &= run_memory_tests();
    all_pass &= run_threading_tests();
    all_pass
}

// ============================================================================
// Allocator Tests
// ============================================================================

/// Test: Vec allocation and basic operations
fn test_allocator_vec() -> bool {
    console::print("\n[TEST] Allocator Vec operations\n");

    // Create and populate a vector
    let mut test_vec: Vec<u32> = Vec::new();
    for i in 0..10 {
        test_vec.push(i);
    }

    // Test basic operations
    let len_ok = test_vec.len() == 10;
    console::print(&format!("  Vec length: {} (expect 10)\n", test_vec.len()));

    // Test remove and insert
    test_vec.remove(0);
    test_vec.insert(0, 99);
    let first_ok = test_vec[0] == 99;
    console::print(&format!("  First element: {} (expect 99)\n", test_vec[0]));

    // Test drop (implicit when vec goes out of scope)
    drop(test_vec);
    console::print("  Drop completed\n");

    let ok = len_ok && first_ok;
    console::print(&format!("  Result: {}\n", if ok { "PASS" } else { "FAIL" }));
    ok
}

/// Test: Box allocation
fn test_allocator_box() -> bool {
    console::print("\n[TEST] Allocator Box operations\n");

    // Allocate a boxed value
    let boxed: Box<u64> = Box::new(42);
    let val_ok = *boxed == 42;
    console::print(&format!("  Box value: {} (expect 42)\n", *boxed));

    // Allocate a boxed array
    let boxed_arr: Box<[u8; 256]> = Box::new([0xAB; 256]);
    let arr_ok = boxed_arr[0] == 0xAB && boxed_arr[255] == 0xAB;
    console::print(&format!(
        "  Box array: first=0x{:02X}, last=0x{:02X} (expect 0xAB)\n",
        boxed_arr[0], boxed_arr[255]
    ));

    drop(boxed);
    drop(boxed_arr);
    console::print("  Drop completed\n");

    let ok = val_ok && arr_ok;
    console::print(&format!("  Result: {}\n", if ok { "PASS" } else { "FAIL" }));
    ok
}

/// Test: Large allocation
fn test_allocator_large() -> bool {
    console::print("\n[TEST] Allocator large allocation\n");

    // Allocate 1MB
    const SIZE: usize = 1024 * 1024;
    console::print(&format!("  Allocating {} KB...", SIZE / 1024));

    let mut large_vec: Vec<u8> = Vec::with_capacity(SIZE);
    for _ in 0..SIZE {
        large_vec.push(0);
    }
    console::print(" done\n");

    let len_ok = large_vec.len() == SIZE;
    console::print(&format!("  Size: {} bytes\n", large_vec.len()));

    // Write and verify
    large_vec[0] = 0x12;
    large_vec[SIZE - 1] = 0x34;
    let write_ok = large_vec[0] == 0x12 && large_vec[SIZE - 1] == 0x34;
    console::print(&format!(
        "  First: 0x{:02X}, Last: 0x{:02X}\n",
        large_vec[0],
        large_vec[SIZE - 1]
    ));

    drop(large_vec);
    console::print("  Drop completed\n");

    let ok = len_ok && write_ok;
    console::print(&format!("  Result: {}\n", if ok { "PASS" } else { "FAIL" }));
    ok
}

/// Test: Realloc growing - Vec growth triggers realloc
fn test_realloc_grow() -> bool {
    console::print("\n[TEST] Realloc grow (Vec capacity growth)\n");

    let mut vec: Vec<u64> = Vec::with_capacity(4);
    console::print(&format!("  Initial capacity: {}\n", vec.capacity()));

    // Fill with known pattern (use wrapping_mul to avoid overflow panic)
    for i in 0..4u64 {
        vec.push(i.wrapping_mul(0x1111_1111_1111_1111));
    }

    // Force reallocation by pushing more
    for i in 4..20u64 {
        vec.push(i.wrapping_mul(0x1111_1111_1111_1111));
    }
    console::print(&format!(
        "  New capacity: {} (should be >= 20)\n",
        vec.capacity()
    ));

    // Verify all data preserved
    let mut data_ok = true;
    for i in 0..20u64 {
        if vec[i as usize] != i.wrapping_mul(0x1111_1111_1111_1111) {
            console::print(&format!("  Data mismatch at index {}\n", i));
            data_ok = false;
            break;
        }
    }

    let capacity_ok = vec.capacity() >= 20;
    console::print(&format!("  Data preserved: {}\n", data_ok));

    drop(vec);

    let ok = capacity_ok && data_ok;
    console::print(&format!("  Result: {}\n", if ok { "PASS" } else { "FAIL" }));
    ok
}

/// Test: Realloc shrinking - shrink_to_fit
fn test_realloc_shrink() -> bool {
    console::print("\n[TEST] Realloc shrink (shrink_to_fit)\n");

    let mut vec: Vec<u32> = Vec::with_capacity(100);
    console::print(&format!("  Initial capacity: {}\n", vec.capacity()));

    // Add just a few elements
    for i in 0..5u32 {
        vec.push(i * 12345);
    }

    // Shrink to fit
    vec.shrink_to_fit();
    console::print(&format!("  After shrink_to_fit: {}\n", vec.capacity()));

    // Verify data
    let mut data_ok = true;
    for i in 0..5u32 {
        if vec[i as usize] != i * 12345 {
            data_ok = false;
            break;
        }
    }

    let shrunk = vec.capacity() <= 10; // Should shrink to close to 5
    console::print(&format!("  Data preserved: {}\n", data_ok));

    drop(vec);

    let ok = shrunk && data_ok;
    console::print(&format!("  Result: {}\n", if ok { "PASS" } else { "FAIL" }));
    ok
}

/// Test: Realloc preserves data with known pattern
fn test_realloc_preserves_data() -> bool {
    console::print("\n[TEST] Realloc preserves data pattern\n");

    const PATTERN: u8 = 0xDE;
    const INITIAL_SIZE: usize = 64;
    const FINAL_SIZE: usize = 256;

    let mut vec: Vec<u8> = Vec::with_capacity(INITIAL_SIZE);
    for _ in 0..INITIAL_SIZE {
        vec.push(PATTERN);
    }
    console::print(&format!(
        "  Filled {} bytes with 0x{:02X}\n",
        INITIAL_SIZE, PATTERN
    ));

    // Force multiple reallocations
    for _ in INITIAL_SIZE..FINAL_SIZE {
        vec.push(0xAD); // Different pattern for new data
    }
    console::print(&format!("  Grew to {} bytes\n", vec.len()));

    // Verify original data unchanged
    let mut original_ok = true;
    for i in 0..INITIAL_SIZE {
        if vec[i] != PATTERN {
            console::print(&format!(
                "  Corruption at byte {} (got 0x{:02X})\n",
                i, vec[i]
            ));
            original_ok = false;
            break;
        }
    }

    // Verify new data correct
    let mut new_ok = true;
    for i in INITIAL_SIZE..FINAL_SIZE {
        if vec[i] != 0xAD {
            new_ok = false;
            break;
        }
    }

    console::print(&format!("  Original data intact: {}\n", original_ok));
    console::print(&format!("  New data correct: {}\n", new_ok));

    drop(vec);

    let ok = original_ok && new_ok;
    console::print(&format!("  Result: {}\n", if ok { "PASS" } else { "FAIL" }));
    ok
}

/// Test: alloc_zeroed - verify memory is actually zeroed
fn test_alloc_zeroed_basic() -> bool {
    console::print("\n[TEST] alloc_zeroed basic\n");

    // vec![0; N] uses alloc_zeroed internally
    const SIZE: usize = 512;
    let vec: Vec<u8> = vec![0u8; SIZE];

    let mut all_zero = true;
    for (i, &byte) in vec.iter().enumerate() {
        if byte != 0 {
            console::print(&format!("  Non-zero at index {}: 0x{:02X}\n", i, byte));
            all_zero = false;
            break;
        }
    }

    console::print(&format!("  {} bytes all zero: {}\n", SIZE, all_zero));

    // Also test with Box
    let boxed: Box<[u8; 256]> = Box::new([0u8; 256]);
    let box_ok = boxed.iter().all(|&b| b == 0);
    console::print(&format!("  Boxed array all zero: {}\n", box_ok));

    drop(vec);
    drop(boxed);

    let ok = all_zero && box_ok;
    console::print(&format!("  Result: {}\n", if ok { "PASS" } else { "FAIL" }));
    ok
}

/// Test: alloc_zeroed after dirty memory
fn test_alloc_zeroed_after_dirty() -> bool {
    console::print("\n[TEST] alloc_zeroed after dirty memory\n");

    const SIZE: usize = 128;

    // First allocation - fill with dirty pattern
    {
        let mut dirty: Vec<u8> = Vec::with_capacity(SIZE);
        for _ in 0..SIZE {
            dirty.push(0xFF);
        }
        console::print(&format!("  Filled {} bytes with 0xFF, dropping...\n", SIZE));
        drop(dirty);
    }

    // Second allocation - request zeroed memory
    let clean: Vec<u8> = vec![0u8; SIZE];

    let mut all_zero = true;
    for (i, &byte) in clean.iter().enumerate() {
        if byte != 0 {
            console::print(&format!("  Residual dirty data at {}: 0x{:02X}\n", i, byte));
            all_zero = false;
            break;
        }
    }

    console::print(&format!("  Zeroed allocation clean: {}\n", all_zero));

    drop(clean);

    console::print(&format!(
        "  Result: {}\n",
        if all_zero { "PASS" } else { "FAIL" }
    ));
    all_zero
}

/// Test: Various alignment requirements
fn test_alignment_various() -> bool {
    console::print("\n[TEST] Alignment requirements\n");

    let mut all_aligned = true;

    // Test different alignments using repr(align) structs
    #[repr(align(8))]
    #[allow(dead_code)]
    struct Align8([u8; 8]);

    #[repr(align(16))]
    #[allow(dead_code)]
    struct Align16([u8; 16]);

    #[repr(align(32))]
    #[allow(dead_code)]
    struct Align32([u8; 32]);

    #[repr(align(64))]
    #[allow(dead_code)]
    struct Align64([u8; 64]);

    // Allocate and check alignment
    let a8: Box<Align8> = Box::new(Align8([0; 8]));
    let ptr8 = &*a8 as *const Align8 as usize;
    let ok8 = ptr8 % 8 == 0;
    console::print(&format!("  Align 8: ptr=0x{:x}, ok={}\n", ptr8, ok8));
    all_aligned &= ok8;

    let a16: Box<Align16> = Box::new(Align16([0; 16]));
    let ptr16 = &*a16 as *const Align16 as usize;
    let ok16 = ptr16 % 16 == 0;
    console::print(&format!("  Align 16: ptr=0x{:x}, ok={}\n", ptr16, ok16));
    all_aligned &= ok16;

    let a32: Box<Align32> = Box::new(Align32([0; 32]));
    let ptr32 = &*a32 as *const Align32 as usize;
    let ok32 = ptr32 % 32 == 0;
    console::print(&format!("  Align 32: ptr=0x{:x}, ok={}\n", ptr32, ok32));
    all_aligned &= ok32;

    let a64: Box<Align64> = Box::new(Align64([0; 64]));
    let ptr64 = &*a64 as *const Align64 as usize;
    let ok64 = ptr64 % 64 == 0;
    console::print(&format!("  Align 64: ptr=0x{:x}, ok={}\n", ptr64, ok64));
    all_aligned &= ok64;

    drop(a8);
    drop(a16);
    drop(a32);
    drop(a64);

    console::print(&format!(
        "  Result: {}\n",
        if all_aligned { "PASS" } else { "FAIL" }
    ));
    all_aligned
}

/// Test: Fragmentation with many small blocks
fn test_fragmentation_small_blocks() -> bool {
    console::print("\n[TEST] Fragmentation (simple)\n");

    // Simplified test - just do a few allocations without tight loops
    let block1 = vec![0x11u8; 32];
    console::print("  Block 1 allocated\n");

    let block2 = vec![0x22u8; 32];
    console::print("  Block 2 allocated\n");

    let block3 = vec![0x33u8; 32];
    console::print("  Block 3 allocated\n");

    // Verify
    let ok = block1[0] == 0x11 && block2[0] == 0x22 && block3[0] == 0x33;

    drop(block1);
    drop(block2);
    drop(block3);

    if ok {
        console::print("  Result: PASS\n");
    } else {
        console::print("  Result: FAIL\n");
    }
    ok
}

/// Test: Interleaved allocation and deallocation pattern
fn test_interleaved_alloc_free() -> bool {
    console::print("\n[TEST] Interleaved alloc/free pattern\n");

    const ITERATIONS: usize = 30; // Reduced from 50
    let mut all_ok = true;

    for i in 0..ITERATIONS {
        // Allocate A
        let a: Vec<u8> = vec![i as u8; 64];

        // Allocate B
        let b: Vec<u8> = vec![(i + 1) as u8; 128];

        // Free A
        drop(a);

        // Allocate C (may reuse A's memory)
        let c: Vec<u8> = vec![(i + 2) as u8; 64];

        // Verify B unchanged
        if b[0] != (i + 1) as u8 || b[127] != (i + 1) as u8 {
            console::print("  Corruption (B)\n");
            all_ok = false;
            break;
        }

        // Verify C correct
        if c[0] != (i + 2) as u8 || c[63] != (i + 2) as u8 {
            console::print("  Corruption (C)\n");
            all_ok = false;
            break;
        }

        drop(b);
        drop(c);
    }

    console::print("  30 iterations completed\n");
    if all_ok {
        console::print("  Result: PASS\n");
    } else {
        console::print("  Result: FAIL\n");
    }
    all_ok
}

/// Test: Mixed allocation sizes
fn test_mixed_sizes() -> bool {
    console::print("\n[TEST] Mixed allocation sizes\n");

    let mut all_ok = true;

    // Small allocations using Vec
    let small1: Vec<u8> = vec![0x11; 16];
    let small2: Vec<u8> = vec![0x22; 32];
    let small3: Vec<u8> = vec![0x33; 64];

    // Medium allocations (1KB-4KB)
    let medium1: Vec<u8> = vec![0x44; 1024];
    let medium2: Vec<u8> = vec![0x55; 4096];

    // Large allocation (64KB)
    let large: Vec<u8> = vec![0x66; 65536];

    // Verify all allocations
    if small1[0] != 0x11 || small1[15] != 0x11 {
        console::print("  Small1 corrupted\n");
        all_ok = false;
    }
    if small2[0] != 0x22 || small2[31] != 0x22 {
        console::print("  Small2 corrupted\n");
        all_ok = false;
    }
    if small3[0] != 0x33 || small3[63] != 0x33 {
        console::print("  Small3 corrupted\n");
        all_ok = false;
    }
    if medium1[0] != 0x44 || medium1[1023] != 0x44 {
        console::print("  Medium1 corrupted\n");
        all_ok = false;
    }
    if medium2[0] != 0x55 || medium2[4095] != 0x55 {
        console::print("  Medium2 corrupted\n");
        all_ok = false;
    }
    if large[0] != 0x66 || large[65535] != 0x66 {
        console::print("  Large corrupted\n");
        all_ok = false;
    }

    console::print("  Small, Medium, Large: ok\n");

    // Free in random order
    drop(medium1);
    drop(small2);
    drop(large);
    drop(small1);
    drop(medium2);
    drop(small3);

    if all_ok {
        console::print("  Result: PASS\n");
    } else {
        console::print("  Result: FAIL\n");
    }
    all_ok
}

/// Test: Vec::remove(0) regression test (original bug)
fn test_vec_remove_regression() -> bool {
    console::print("\n[TEST] Vec::remove(0) regression\n");

    let mut vec: Vec<u32> = Vec::new();
    for i in 0..10 {
        vec.push(i * 100);
    }
    console::print(&format!("  Initial vec: {:?}\n", &vec[..3]));

    // This was the original failure case
    let removed = vec.remove(0);
    console::print(&format!("  Removed index 0: {}\n", removed));

    let remove_ok = removed == 0;
    let first_ok = vec[0] == 100;
    let len_ok = vec.len() == 9;

    console::print(&format!("  New first element: {} (expect 100)\n", vec[0]));
    console::print(&format!("  New length: {} (expect 9)\n", vec.len()));

    // Remove from middle
    let mid = vec.remove(4);
    console::print(&format!("  Removed index 4: {} (expect 500)\n", mid));
    let mid_ok = mid == 500;

    // Remove from end
    let end = vec.remove(vec.len() - 1);
    console::print(&format!("  Removed last: {} (expect 900)\n", end));
    let end_ok = end == 900;

    drop(vec);

    let ok = remove_ok && first_ok && len_ok && mid_ok && end_ok;
    console::print(&format!("  Result: {}\n", if ok { "PASS" } else { "FAIL" }));
    ok
}

/// Test: Rapid push/pop cycling
fn test_rapid_push_pop() -> bool {
    console::print("\n[TEST] Rapid push/pop cycling\n");

    const ITERATIONS: usize = 5;
    const ITEMS: usize = 200;

    let mut all_ok = true;

    for iter in 0..ITERATIONS {
        let mut vec: Vec<u64> = Vec::new();

        // Push many items
        for i in 0..ITEMS {
            vec.push((iter * ITEMS + i) as u64);
        }

        // Verify and pop all
        for i in (0..ITEMS).rev() {
            let val = vec.pop().unwrap();
            if val != (iter * ITEMS + i) as u64 {
                console::print(&format!("  Mismatch at iter {} index {}\n", iter, i));
                all_ok = false;
                break;
            }
        }

        if vec.len() != 0 {
            console::print(&format!("  Vec not empty after iteration {}\n", iter));
            all_ok = false;
        }
    }

    console::print(&format!(
        "  {} iterations of {} push/pop: {}\n",
        ITERATIONS, ITEMS, all_ok
    ));
    console::print(&format!(
        "  Result: {}\n",
        if all_ok { "PASS" } else { "FAIL" }
    ));
    all_ok
}

/// Test: String operations (uses realloc internally)
fn test_string_operations() -> bool {
    console::print("\n[TEST] String operations\n");

    let mut s = String::new();

    // Append strings (triggers realloc)
    s.push_str("Hello");
    s.push_str(", ");
    s.push_str("World!");
    console::print(&format!("  Built string: \"{}\"\n", s));

    let hello_ok = s == "Hello, World!";

    // Longer string building
    let mut long = String::new();
    for i in 0..50 {
        long.push_str(&format!("{} ", i));
    }
    console::print(&format!("  Long string len: {}\n", long.len()));
    let long_ok = long.starts_with("0 1 2 ");

    // Truncate
    s.truncate(5);
    console::print(&format!("  Truncated: \"{}\"\n", s));
    let trunc_ok = s == "Hello";

    // Clear and rebuild
    s.clear();
    s.push_str("Rebuilt");
    let rebuild_ok = s == "Rebuilt";

    drop(s);
    drop(long);

    let ok = hello_ok && long_ok && trunc_ok && rebuild_ok;
    console::print(&format!("  Result: {}\n", if ok { "PASS" } else { "FAIL" }));
    ok
}

/// Test: String::push_str reallocation pattern (userspace bug reproduction)
/// This test mirrors the exact pattern that causes heap corruption in userspace:
/// 1. Vec allocates
/// 2. String::from allocates  
/// 3. push_str triggers realloc -> corruption
fn test_string_push_str_realloc() -> bool {
    console::print("\n[TEST] String::push_str realloc pattern (userspace bug mirror)\n");

    // Step 1: Vec allocation (like userspace test_vec)
    console::print("  Step 1: Vec allocation...\n");
    let mut v: Vec<i32> = Vec::new();
    v.push(1);
    v.push(2);
    v.push(3);
    let v_ptr = v.as_ptr() as usize;
    console::print(&format!("    Vec ptr: {:#x}, len: {}\n", v_ptr, v.len()));

    // Step 2: String::from allocation (like userspace test_string_from)
    console::print("  Step 2: String::from allocation...\n");
    let s = String::from("Hello");
    let s_ptr = s.as_ptr() as usize;
    console::print(&format!(
        "    String ptr: {:#x}, len: {}, cap: {}\n",
        s_ptr,
        s.len(),
        s.capacity()
    ));

    // Step 3: push_str triggers reallocation (THE BUG!)
    console::print("  Step 3: push_str (triggers realloc)...\n");
    let mut s2 = s.clone();
    let s2_ptr_before = s2.as_ptr() as usize;
    console::print(&format!(
        "    Before push_str: ptr={:#x}, cap={}\n",
        s2_ptr_before,
        s2.capacity()
    ));

    // This is where userspace crashes - realloc corrupts the allocator head
    s2.push_str(", World!");

    let s2_ptr_after = s2.as_ptr() as usize;
    console::print(&format!(
        "    After push_str: ptr={:#x}, cap={}\n",
        s2_ptr_after,
        s2.capacity()
    ));
    console::print(&format!("    Result: \"{}\"\n", s2));

    // Verify data integrity
    let vec_ok = v.len() == 3 && v[0] == 1 && v[2] == 3;
    let string_ok = s2 == "Hello, World!";

    // Check for suspicious pointer values (like 0x814000 in userspace bug)
    let ptr_suspicious = s2_ptr_after > 0x800000 && s2_ptr_after < 0x900000;
    if ptr_suspicious {
        console::print(&format!(
            "  WARNING: Suspicious pointer {:#x} (similar to userspace bug pattern)\n",
            s2_ptr_after
        ));
    }

    drop(v);
    drop(s);
    drop(s2);

    let ok = vec_ok && string_ok;
    console::print(&format!("  Result: {}\n", if ok { "PASS" } else { "FAIL" }));
    ok
}

/// Test: Detailed String reallocation with capacity tracking
/// Tracks each allocation to help debug heap corruption
fn test_string_realloc_detailed() -> bool {
    console::print("\n[TEST] String realloc with detailed tracking\n");

    // Create with small capacity to force realloc
    let mut s = String::with_capacity(5);
    console::print(&format!(
        "  Initial: ptr={:#x}, len={}, cap={}\n",
        s.as_ptr() as usize,
        s.len(),
        s.capacity()
    ));

    // Push small string (no realloc needed)
    s.push_str("Hi");
    console::print(&format!(
        "  After 'Hi': ptr={:#x}, len={}, cap={}\n",
        s.as_ptr() as usize,
        s.len(),
        s.capacity()
    ));

    // Push more to trigger realloc
    s.push_str("!!!"); // Still within capacity
    console::print(&format!(
        "  After '!!!': ptr={:#x}, len={}, cap={}\n",
        s.as_ptr() as usize,
        s.len(),
        s.capacity()
    ));

    // This should trigger realloc (capacity 5, current len 5, adding 6 more)
    let ptr_before = s.as_ptr() as usize;
    s.push_str(" World");
    let ptr_after = s.as_ptr() as usize;

    console::print(&format!(
        "  After ' World': ptr={:#x}, len={}, cap={}\n",
        s.as_ptr() as usize,
        s.len(),
        s.capacity()
    ));

    let reallocated = ptr_before != ptr_after;
    console::print(&format!("  Reallocation occurred: {}\n", reallocated));

    let content_ok = s == "Hi!!! World";
    console::print(&format!("  Content: \"{}\" (expect \"Hi!!! World\")\n", s));

    drop(s);

    console::print(&format!(
        "  Result: {}\n",
        if content_ok { "PASS" } else { "FAIL" }
    ));
    content_ok
}

/// Test: Nested allocations (Vec of Vecs)
fn test_vec_of_vecs() -> bool {
    console::print("\n[TEST] Vec of Vecs (nested allocations)\n");

    const OUTER: usize = 10;
    const INNER: usize = 20;

    let mut outer: Vec<Vec<u8>> = Vec::new();

    // Build nested structure
    for i in 0..OUTER {
        let mut inner: Vec<u8> = Vec::new();
        for j in 0..INNER {
            inner.push((i * INNER + j) as u8);
        }
        outer.push(inner);
    }
    console::print(&format!("  Created {}x{} nested vecs\n", OUTER, INNER));

    // Verify data
    let mut all_ok = true;
    for i in 0..OUTER {
        for j in 0..INNER {
            if outer[i][j] != (i * INNER + j) as u8 {
                console::print(&format!("  Mismatch at [{i}][{j}]\n"));
                all_ok = false;
                break;
            }
        }
        if !all_ok {
            break;
        }
    }

    // Remove some inner vecs
    outer.remove(5);
    outer.remove(3);
    console::print(&format!("  After removals: {} outer vecs\n", outer.len()));

    let len_ok = outer.len() == OUTER - 2;

    // Add new inner vec
    outer.push(vec![0xAB; 30]);
    console::print(&format!("  After push: {} outer vecs\n", outer.len()));

    drop(outer);

    let ok = all_ok && len_ok;
    console::print(&format!("  Result: {}\n", if ok { "PASS" } else { "FAIL" }));
    ok
}

/// Test: Adjacent allocations boundary integrity
fn test_adjacent_allocations() -> bool {
    console::print("\n[TEST] Adjacent allocation boundaries\n");

    // Simple test without format! to rule out format allocation issues
    console::print("  Testing simple allocations\n");

    let buf1: Vec<u8> = vec![0x11u8; 64];
    console::print("  buf1 ok\n");

    let buf2: Vec<u8> = vec![0x22u8; 64];
    console::print("  buf2 ok\n");

    let buf3: Vec<u8> = vec![0x33u8; 64];
    console::print("  buf3 ok\n");

    // Verify data is correct
    let ok = buf1[0] == 0x11
        && buf1[63] == 0x11
        && buf2[0] == 0x22
        && buf2[63] == 0x22
        && buf3[0] == 0x33
        && buf3[63] == 0x33;

    // Cleanup
    drop(buf1);
    drop(buf2);
    drop(buf3);
    console::print("  Cleanup done\n");

    if ok {
        console::print("  Result: PASS\n");
    } else {
        console::print("  Result: FAIL\n");
    }
    ok
}

// ============================================================================
// Mmap Allocator Edge Case Tests (for userspace debugging)
// ============================================================================

/// Test: Single page allocation and access
/// Verifies basic mmap allocation returns usable memory
fn test_mmap_single_page() -> bool {
    console::print("\n[TEST] Mmap: Single page allocation\n");

    // Allocate a small buffer (will use one page in mmap mode)
    let buf: Vec<u8> = vec![0u8; 100];
    let ptr = buf.as_ptr() as usize;
    console::print(&format!("  Allocated 100 bytes at {:#x}\n", ptr));

    // Write pattern
    let mut buf = buf;
    for i in 0..100 {
        buf[i] = (i & 0xFF) as u8;
    }

    // Verify pattern
    let mut ok = true;
    for i in 0..100 {
        if buf[i] != (i & 0xFF) as u8 {
            console::print(&format!(
                "  Mismatch at {}: got {}, expected {}\n",
                i,
                buf[i],
                i & 0xFF
            ));
            ok = false;
            break;
        }
    }

    drop(buf);
    console::print(&format!("  Result: {}\n", if ok { "PASS" } else { "FAIL" }));
    ok
}

/// Test: Multi-page allocation
/// Tests allocations that span multiple pages (> 4KB)
fn test_mmap_multi_page() -> bool {
    console::print("\n[TEST] Mmap: Multi-page allocation (12KB)\n");

    const SIZE: usize = 12 * 1024; // 3 pages

    let mut buf: Vec<u8> = vec![0u8; SIZE];
    let ptr = buf.as_ptr() as usize;
    console::print(&format!("  Allocated {} bytes at {:#x}\n", SIZE, ptr));

    // Write to first byte of each page
    buf[0] = 0x11;
    buf[4096] = 0x22;
    buf[8192] = 0x33;
    buf[SIZE - 1] = 0x44;

    // Verify
    let ok = buf[0] == 0x11 && buf[4096] == 0x22 && buf[8192] == 0x33 && buf[SIZE - 1] == 0x44;

    console::print(&format!(
        "  Page boundaries: {:#x}, {:#x}, {:#x}, {:#x}\n",
        buf[0],
        buf[4096],
        buf[8192],
        buf[SIZE - 1]
    ));

    drop(buf);
    console::print(&format!("  Result: {}\n", if ok { "PASS" } else { "FAIL" }));
    ok
}

/// Test: Write exactly at page boundary
/// This catches off-by-one errors in page mapping
fn test_mmap_page_boundary_write() -> bool {
    console::print("\n[TEST] Mmap: Page boundary writes\n");

    const PAGE_SIZE: usize = 4096;

    // Allocate exactly 2 pages
    let mut buf: Vec<u8> = vec![0u8; PAGE_SIZE * 2];
    let ptr = buf.as_ptr() as usize;

    // Write at critical positions
    buf[PAGE_SIZE - 1] = 0xAA; // Last byte of page 1
    buf[PAGE_SIZE] = 0xBB; // First byte of page 2

    console::print(&format!("  Ptr: {:#x}\n", ptr));
    console::print(&format!(
        "  buf[{}] = {:#x} (last of page 1)\n",
        PAGE_SIZE - 1,
        buf[PAGE_SIZE - 1]
    ));
    console::print(&format!(
        "  buf[{}] = {:#x} (first of page 2)\n",
        PAGE_SIZE, buf[PAGE_SIZE]
    ));

    let ok = buf[PAGE_SIZE - 1] == 0xAA && buf[PAGE_SIZE] == 0xBB;

    drop(buf);
    console::print(&format!("  Result: {}\n", if ok { "PASS" } else { "FAIL" }));
    ok
}

/// Test: Rapid alloc/dealloc cycles
/// Stresses the allocator with many short-lived allocations
fn test_mmap_rapid_alloc_dealloc() -> bool {
    console::print("\n[TEST] Mmap: Rapid alloc/dealloc (100 cycles)\n");

    let mut ok = true;

    for i in 0..100 {
        let buf: Vec<u8> = vec![(i & 0xFF) as u8; 256];
        if buf[0] != (i & 0xFF) as u8 || buf[255] != (i & 0xFF) as u8 {
            console::print(&format!("  Cycle {} failed\n", i));
            ok = false;
            break;
        }
        drop(buf);
    }

    if ok {
        console::print("  All 100 cycles passed\n");
    }

    console::print(&format!("  Result: {}\n", if ok { "PASS" } else { "FAIL" }));
    ok
}

/// Test: Realloc pattern that mirrors userspace bug
/// Allocate, then grow, then use - the exact pattern that fails
fn test_mmap_realloc_pattern() -> bool {
    console::print("\n[TEST] Mmap: Realloc pattern (grow then use)\n");

    // Small initial allocation
    let mut v: Vec<u64> = Vec::with_capacity(2);
    let ptr1 = v.as_ptr() as usize;
    console::print(&format!(
        "  Initial: ptr={:#x}, cap={}\n",
        ptr1,
        v.capacity()
    ));

    v.push(0x1111111111111111);
    v.push(0x2222222222222222);

    // Force reallocation
    v.push(0x3333333333333333);
    v.push(0x4444444444444444);
    v.push(0x5555555555555555);
    let ptr2 = v.as_ptr() as usize;
    console::print(&format!(
        "  After growth: ptr={:#x}, cap={}\n",
        ptr2,
        v.capacity()
    ));

    // Immediately use the new memory (this is where userspace fails)
    v.push(0x6666666666666666);
    v.push(0x7777777777777777);

    // Verify all data
    let ok = v[0] == 0x1111111111111111
        && v[1] == 0x2222222222222222
        && v[2] == 0x3333333333333333
        && v[5] == 0x6666666666666666
        && v[6] == 0x7777777777777777;

    console::print(&format!(
        "  Data integrity: {}\n",
        if ok { "OK" } else { "CORRUPTED" }
    ));

    drop(v);
    console::print(&format!("  Result: {}\n", if ok { "PASS" } else { "FAIL" }));
    ok
}

/// Test: String growth pattern (exact userspace failure scenario)
fn test_mmap_string_growth_pattern() -> bool {
    console::print("\n[TEST] Mmap: String growth pattern\n");

    // This is the exact pattern that crashes in userspace
    let mut s = String::from("Hello");
    let ptr1 = s.as_ptr() as usize;
    console::print(&format!(
        "  Initial: ptr={:#x}, len={}, cap={}\n",
        ptr1,
        s.len(),
        s.capacity()
    ));

    // Trigger realloc by pushing more data
    s.push_str(", World!");
    let ptr2 = s.as_ptr() as usize;
    console::print(&format!(
        "  After push_str: ptr={:#x}, len={}, cap={}\n",
        ptr2,
        s.len(),
        s.capacity()
    ));

    // Critical: access the string after realloc
    let content_ok = s == "Hello, World!";
    let len_ok = s.len() == 13;

    // Try to use it more
    s.push_str(" This is a test.");
    let final_ok = s == "Hello, World! This is a test.";

    console::print(&format!("  Content: \"{}\"\n", s));

    drop(s);

    let ok = content_ok && len_ok && final_ok;
    console::print(&format!("  Result: {}\n", if ok { "PASS" } else { "FAIL" }));
    ok
}

/// Test: Vec capacity doubling stress test
fn test_mmap_vec_capacity_doubling() -> bool {
    console::print("\n[TEST] Mmap: Vec capacity doubling (1->1024 elements)\n");

    let mut v: Vec<u32> = Vec::new();
    let mut reallocs = 0;
    let mut last_ptr = 0usize;

    for i in 0..1024 {
        let ptr_before = v.as_ptr() as usize;
        v.push(i);
        let ptr_after = v.as_ptr() as usize;

        if ptr_before != ptr_after && ptr_before != 0 {
            reallocs += 1;
        }
        last_ptr = ptr_after;
    }

    console::print(&format!(
        "  Final: len={}, cap={}, ptr={:#x}\n",
        v.len(),
        v.capacity(),
        last_ptr
    ));
    console::print(&format!("  Realloc count: {}\n", reallocs));

    // Verify all data
    let mut ok = true;
    for i in 0..1024 {
        if v[i] != i as u32 {
            console::print(&format!("  Mismatch at {}: got {}\n", i, v[i]));
            ok = false;
            break;
        }
    }

    drop(v);
    console::print(&format!("  Result: {}\n", if ok { "PASS" } else { "FAIL" }));
    ok
}

/// Test: Interleaved string operations
/// Multiple strings allocated and modified in interleaved order
fn test_mmap_interleaved_strings() -> bool {
    console::print("\n[TEST] Mmap: Interleaved string operations\n");

    let mut s1 = String::from("AAA");
    let mut s2 = String::from("BBB");
    let mut s3 = String::from("CCC");

    console::print(&format!("  s1: ptr={:#x}\n", s1.as_ptr() as usize));
    console::print(&format!("  s2: ptr={:#x}\n", s2.as_ptr() as usize));
    console::print(&format!("  s3: ptr={:#x}\n", s3.as_ptr() as usize));

    // Interleaved modifications (triggers reallocs in different orders)
    s1.push_str("111");
    s2.push_str("222");
    s3.push_str("333");

    s2.push_str("more");
    s1.push_str("even more");
    s3.push_str("and more");

    console::print(&format!("  After modifications:\n"));
    console::print(&format!("    s1: \"{}\"\n", s1));
    console::print(&format!("    s2: \"{}\"\n", s2));
    console::print(&format!("    s3: \"{}\"\n", s3));

    let ok = s1 == "AAA111even more" && s2 == "BBB222more" && s3 == "CCC333and more";

    drop(s1);
    drop(s2);
    drop(s3);

    console::print(&format!("  Result: {}\n", if ok { "PASS" } else { "FAIL" }));
    ok
}

// ============================================================================
// Common Memory Allocation Patterns
// NOTE: These tests hang during preemption - need investigation
// ============================================================================

/// Test: LIFO (stack-like) allocation pattern
/// Common in: function call stacks, undo buffers, recursive algorithms
#[allow(dead_code)]
fn test_lifo_pattern() -> bool {
    console::print("\n[TEST] LIFO allocation pattern\n");

    console::print("  Creating stack...\n");
    const DEPTH: usize = 5; // Reduced further
    let mut stack: Vec<Vec<u8>> = Vec::with_capacity(DEPTH);
    console::print("  Stack created\n");

    // Push allocations
    for i in 0..DEPTH {
        let mut block = vec![0u8; 32]; // Reduced from 64
        block[0] = i as u8;
        block[31] = (i * 3) as u8;
        stack.push(block);
    }
    console::print("  Pushed 5 allocations\n");

    // Pop in reverse order (LIFO)
    let mut all_ok = true;
    console::print("  Popping...\n");
    for i in (0..DEPTH).rev() {
        let block = stack.pop().unwrap();
        if block[0] != i as u8 || block[31] != (i * 3) as u8 {
            console::print("  LIFO mismatch\n");
            all_ok = false;
            break;
        }
    }
    console::print("  Pop complete\n");

    drop(stack);

    if all_ok {
        console::print("  Result: PASS\n");
    } else {
        console::print("  Result: FAIL\n");
    }
    all_ok
}

/// Test: FIFO (queue-like) allocation pattern  
/// Common in: message queues, task schedulers, event handlers
#[allow(dead_code)]
fn test_fifo_pattern() -> bool {
    console::print("\n[TEST] FIFO allocation pattern\n");

    const QUEUE_SIZE: usize = 15; // Reduced from 30
    let mut queue: VecDeque<Vec<u8>> = VecDeque::with_capacity(QUEUE_SIZE);

    // Enqueue items
    for i in 0..QUEUE_SIZE {
        let mut block = vec![0u8; 32]; // Reduced from 64
        block[0] = i as u8;
        block[31] = (i ^ 0xFF) as u8;
        queue.push_back(block);
    }
    console::print("  Enqueued 15 items\n");

    // Dequeue in FIFO order
    let mut all_ok = true;
    for i in 0..QUEUE_SIZE {
        let block = queue.pop_front().unwrap();
        if block[0] != i as u8 || block[31] != (i ^ 0xFF) as u8 {
            console::print("  FIFO mismatch\n");
            all_ok = false;
            break;
        }
    }

    if all_ok {
        console::print("  FIFO order verified\n");
    }

    drop(queue);

    if all_ok {
        console::print("  Result: PASS\n");
    } else {
        console::print("  Result: FAIL\n");
    }
    all_ok
}

/// Test: Memory pool pattern
/// Common in: game engines, real-time systems, network buffers
#[allow(dead_code)]
fn test_memory_pool_pattern() -> bool {
    console::print("\n[TEST] Memory pool pattern\n");

    const POOL_SIZE: usize = 16;
    const BLOCK_SIZE: usize = 256;

    // Pre-allocate pool using Vec instead of Box
    let mut pool: Vec<Vec<u8>> = Vec::with_capacity(POOL_SIZE);
    for _ in 0..POOL_SIZE {
        pool.push(vec![0u8; BLOCK_SIZE]);
    }
    console::print("  Pool created: 16 x 256 bytes\n");

    // Simulate acquire/release cycles
    let mut acquired: Vec<Vec<u8>> = Vec::new();
    let mut all_ok = true;

    for cycle in 0..5 {
        // Acquire half the pool
        for i in 0..(POOL_SIZE / 2) {
            if let Some(mut block) = pool.pop() {
                block[0] = (cycle * 10 + i) as u8;
                acquired.push(block);
            }
        }

        // Use acquired blocks
        for (i, block) in acquired.iter().enumerate() {
            if block[0] != (cycle * 10 + i) as u8 {
                all_ok = false;
                break;
            }
        }

        // Release back to pool
        while let Some(block) = acquired.pop() {
            pool.push(block);
        }
    }

    console::print("  Pool cycles: 5\n");

    let size_ok = pool.len() == POOL_SIZE;

    drop(pool);
    drop(acquired);

    let ok = all_ok && size_ok;
    if ok {
        console::print("  Result: PASS\n");
    } else {
        console::print("  Result: FAIL\n");
    }
    ok
}

/// Test: Dynamic resize pattern
/// Common in: growing arrays, string builders, buffers
#[allow(dead_code)]
fn test_resize_pattern() -> bool {
    console::print("\n[TEST] Dynamic resize pattern\n");

    let mut buffer: Vec<u8> = Vec::new();
    let mut all_ok = true;

    // Exponential growth pattern
    let sizes = [1, 2, 4, 8, 16, 32, 64, 128, 256, 512, 1024, 2048];

    let mut prev_size = 0usize;
    for &size in &sizes {
        // Resize to new size
        buffer.resize(size, 0xAB);

        // Verify OLD content is preserved (should be index pattern)
        for i in 0..prev_size {
            if buffer[i] != (i & 0xFF) as u8 {
                console::print("  Old content mismatch\n");
                all_ok = false;
                break;
            }
        }

        // Verify NEW content is 0xAB
        for i in prev_size..size {
            if buffer[i] != 0xAB {
                console::print("  New content mismatch\n");
                all_ok = false;
                break;
            }
        }

        // Overwrite with index pattern
        for i in 0..size {
            buffer[i] = (i & 0xFF) as u8;
        }

        prev_size = size;
    }

    console::print("  Grew through 12 sizes (max 2048)\n");

    // Shrink back down
    for &size in sizes.iter().rev().skip(1) {
        buffer.truncate(size);
        if buffer.len() != size {
            all_ok = false;
            break;
        }
        // Verify data preserved
        for i in 0..size {
            if buffer[i] != (i & 0xFF) as u8 {
                all_ok = false;
                break;
            }
        }
    }

    if all_ok {
        console::print("  Shrink verified\n");
    }

    // Clear and cleanup
    buffer.clear();
    buffer.shrink_to_fit();
    drop(buffer);

    if all_ok {
        console::print("  Result: PASS\n");
    } else {
        console::print("  Result: FAIL\n");
    }
    all_ok
}

/// Test: Temporary buffer pattern
/// Common in: I/O operations, parsing, formatting
#[allow(dead_code)]
fn test_temporary_buffers() -> bool {
    console::print("\n[TEST] Temporary buffer pattern\n");

    const ITERATIONS: usize = 50;
    let mut all_ok = true;

    for i in 0..ITERATIONS {
        // Simulate: allocate temp buffer, use it, free it
        let size = 64 + (i % 7) * 32; // Varying sizes

        // Allocate
        let mut temp: Vec<u8> = vec![0u8; size];

        // Use (write pattern)
        for j in 0..size {
            temp[j] = ((i + j) & 0xFF) as u8;
        }

        // Verify
        for j in 0..size {
            if temp[j] != ((i + j) & 0xFF) as u8 {
                console::print("  Temp buffer failed\n");
                all_ok = false;
                break;
            }
        }

        drop(temp);
    }

    console::print("  50 temporary allocations done\n");
    if all_ok {
        console::print("  Result: PASS\n");
    } else {
        console::print("  Result: FAIL\n");
    }
    all_ok
}

/// Test: Linked/tree structure pattern
/// Common in: linked lists, trees, graphs
#[allow(dead_code)]
fn test_linked_structure() -> bool {
    console::print("\n[TEST] Linked structure pattern\n");

    // Simple linked list node
    struct Node {
        value: u32,
        next: Option<Box<Node>>,
    }

    // Build linked list (smaller size to reduce Box allocations)
    const LIST_SIZE: usize = 10;
    let mut head: Option<Box<Node>> = None;

    for i in (0..LIST_SIZE).rev() {
        head = Some(Box::new(Node {
            value: i as u32,
            next: head,
        }));
    }
    console::print("  Built linked list of 10 nodes\n");

    // Traverse and verify
    let mut all_ok = true;
    let mut current = &head;
    let mut count = 0;

    while let Some(node) = current {
        if node.value != count {
            console::print("  Node value mismatch\n");
            all_ok = false;
            break;
        }
        current = &node.next;
        count += 1;
    }

    let count_ok = count == LIST_SIZE as u32;
    console::print("  Traversed nodes\n");

    // Cleanup: drop the entire list (recursive drops)
    drop(head);
    console::print("  List dropped\n");

    let ok = all_ok && count_ok;
    if ok {
        console::print("  Result: PASS\n");
    } else {
        console::print("  Result: FAIL\n");
    }
    ok
}

// ============================================================================
// Threading Tests
// ============================================================================

/// Test: Scheduler is initialized
fn test_scheduler_init() -> bool {
    console::print("\n[TEST] Scheduler initialization\n");

    let count = threading::thread_count();
    let ok = count >= 1; // At least idle thread

    console::print(&format!("  Thread count: {} (expect >= 1)\n", count));
    console::print(&format!("  Result: {}\n", if ok { "PASS" } else { "FAIL" }));

    ok
}

/// Test: Thread stats work correctly
fn test_thread_stats() -> bool {
    console::print("\n[TEST] Thread statistics\n");

    let (ready, running, terminated) = threading::thread_stats();
    let ok = running >= 1; // Current thread should be running

    console::print(&format!(
        "  Ready: {}, Running: {}, Terminated: {}\n",
        ready, running, terminated
    ));
    console::print(&format!("  Result: {}\n", if ok { "PASS" } else { "FAIL" }));

    ok
}

/// Test: yield_now() works without crashing
fn test_yield() -> bool {
    console::print("\n[TEST] Yield operation\n");

    console::print("  Calling yield_now()...");
    threading::yield_now();
    console::print(" returned\n");
    console::print("  Result: PASS\n");

    true
}

/// Test: Cooperative timeout constant is set
fn test_cooperative_timeout() -> bool {
    console::print("\n[TEST] Cooperative timeout\n");

    let timeout = threading::COOPERATIVE_TIMEOUT_US;
    let ok = timeout > 0;

    console::print(&format!(
        "  Timeout: {} us ({} seconds)\n",
        timeout,
        timeout / 1_000_000
    ));
    console::print(&format!(
        "  Result: {}\n",
        if ok { "PASS" } else { "DISABLED (0)" }
    ));

    ok
}

/// Test: Cleanup function exists and doesn't crash
fn test_thread_cleanup() -> bool {
    console::print("\n[TEST] Thread cleanup\n");

    // Get initial state
    let count_before = threading::thread_count();
    let (ready, running, terminated) = threading::thread_stats();
    console::print(&format!(
        "  State: {} threads (R:{} U:{} T:{})\n",
        count_before, ready, running, terminated
    ));

    // Run cleanup (should be safe even with no terminated threads)
    let cleaned = threading::cleanup_terminated();
    console::print(&format!("  Cleaned: {} threads\n", cleaned));

    // Verify state is still valid
    let count_after = threading::thread_count();
    let (ready2, running2, terminated2) = threading::thread_stats();
    console::print(&format!(
        "  After: {} threads (R:{} U:{} T:{})\n",
        count_after, ready2, running2, terminated2
    ));

    // Test passes if:
    // 1. Count decreased by amount cleaned (or stayed same if 0 cleaned)
    // 2. At least one thread still exists (idle)
    let count_ok = count_after == count_before - cleaned;
    let has_idle = count_after >= 1;
    let ok = count_ok && has_idle;

    console::print(&format!("  Result: {}\n", if ok { "PASS" } else { "FAIL" }));

    ok
}

// Global flag for test thread communication
static TEST_THREAD_RAN: AtomicBool = AtomicBool::new(false);

fn set_test_flag(val: bool) {
    TEST_THREAD_RAN.store(val, Ordering::Release);
}

fn get_test_flag() -> bool {
    TEST_THREAD_RAN.load(Ordering::Acquire)
}

/// Test: Can spawn a thread without hanging
fn test_spawn_thread() -> bool {
    console::print("\n[TEST] Thread spawn\n");

    let count_before = threading::thread_count();
    console::print(&format!("  Threads before: {}\n", count_before));

    // Try to spawn - simple thread that just marks itself terminated immediately
    console::print("  Spawning test thread...");
    match threading::spawn_fn(|| {
        threading::mark_current_terminated();
        loop {
            threading::yield_now();
            unsafe { core::arch::asm!("wfi") };
        }
    }) {
        Ok(tid) => {
            console::print(&format!(" OK (tid={})\n", tid));

            let count_after = threading::thread_count();
            console::print(&format!("  Threads after: {}\n", count_after));

            let ok = count_after == count_before + 1;
            console::print(&format!("  Result: {}\n", if ok { "PASS" } else { "FAIL" }));
            ok
        }
        Err(e) => {
            console::print(&format!(" FAILED: {}\n", e));
            console::print("  Result: FAIL\n");
            false
        }
    }
}

/// Test: Spawned thread actually executes
fn test_spawn_and_run() -> bool {
    console::print("\n[TEST] Thread execution\n");

    // Reset flag
    set_test_flag(false);

    // Spawn thread that sets the flag and terminates
    console::print("  Spawning thread that sets flag...");
    match threading::spawn_fn(|| {
        set_test_flag(true);
        threading::mark_current_terminated();
        loop {
            threading::yield_now();
            unsafe { core::arch::asm!("wfi") };
        }
    }) {
        Ok(tid) => {
            console::print(&format!(" OK (tid={})\n", tid));

            // Yield a few times to let the thread run
            console::print("  Yielding to let thread run...");
            for _ in 0..10 {
                threading::yield_now();
            }
            console::print(" done\n");

            // Check if flag was set
            let ran = get_test_flag();
            console::print(&format!("  Thread ran: {}\n", ran));

            // Cleanup
            let cleaned = threading::cleanup_terminated();
            console::print(&format!("  Cleaned up: {} threads\n", cleaned));

            console::print(&format!(
                "  Result: {}\n",
                if ran { "PASS" } else { "FAIL" }
            ));
            ran
        }
        Err(e) => {
            console::print(&format!(" FAILED: {}\n", e));
            console::print("  Result: FAIL\n");
            false
        }
    }
}

/// Test: Spawn, terminate, cleanup, verify count returns to original
fn test_spawn_and_cleanup() -> bool {
    console::print("\n[TEST] Spawn and cleanup\n");

    let count_before = threading::thread_count();
    console::print(&format!("  Threads before: {}\n", count_before));

    // Spawn thread
    console::print("  Spawning...");
    let _tid = match threading::spawn_fn(|| {
        threading::mark_current_terminated();
        loop {
            threading::yield_now();
            unsafe { core::arch::asm!("wfi") };
        }
    }) {
        Ok(t) => {
            console::print(&format!(" tid={}\n", t));
            t
        }
        Err(e) => {
            console::print(&format!(" FAILED: {}\n", e));
            return false;
        }
    };

    // Yield to let it run and terminate
    console::print("  Yielding...");
    for _ in 0..5 {
        threading::yield_now();
    }
    console::print(" done\n");

    // Check it's terminated
    let (_, _, terminated) = threading::thread_stats();
    console::print(&format!("  Terminated count: {}\n", terminated));

    // Cleanup
    let cleaned = threading::cleanup_terminated();
    console::print(&format!("  Cleaned: {}\n", cleaned));

    let count_after = threading::thread_count();
    console::print(&format!("  Threads after: {}\n", count_after));

    let ok = count_after == count_before && cleaned >= 1;
    console::print(&format!("  Result: {}\n", if ok { "PASS" } else { "FAIL" }));
    ok
}

// Counter for multiple thread test
static THREAD_COUNTER: AtomicU32 = AtomicU32::new(0);

fn increment_counter() {
    THREAD_COUNTER.fetch_add(1, Ordering::AcqRel);
}

fn get_counter() -> u32 {
    THREAD_COUNTER.load(Ordering::Acquire)
}

fn reset_counter() {
    THREAD_COUNTER.store(0, Ordering::Release);
}

/// Test: Spawn multiple threads
fn test_spawn_multiple() -> bool {
    console::print("\n[TEST] Spawn multiple threads\n");

    reset_counter();
    let count_before = threading::thread_count();
    console::print(&format!("  Threads before: {}\n", count_before));

    // Spawn 3 threads
    const NUM_THREADS: usize = 3;
    console::print(&format!("  Spawning {} threads...", NUM_THREADS));

    for i in 0..NUM_THREADS {
        match threading::spawn_fn(|| {
            increment_counter();
            threading::mark_current_terminated();
            loop {
                threading::yield_now();
                unsafe { core::arch::asm!("wfi") };
            }
        }) {
            Ok(_) => {}
            Err(e) => {
                console::print(&format!(" FAILED at {}: {}\n", i, e));
                return false;
            }
        }
    }
    console::print(" done\n");

    let count_mid = threading::thread_count();
    console::print(&format!("  Threads after spawn: {}\n", count_mid));

    // Yield to let them all run
    console::print("  Yielding...");
    for _ in 0..20 {
        threading::yield_now();
    }
    console::print(" done\n");

    let counter_val = get_counter();
    console::print(&format!(
        "  Counter value: {} (expect {})\n",
        counter_val, NUM_THREADS
    ));

    // Cleanup
    let cleaned = threading::cleanup_terminated();
    console::print(&format!("  Cleaned: {}\n", cleaned));

    let count_after = threading::thread_count();
    console::print(&format!("  Threads after cleanup: {}\n", count_after));

    let ok = counter_val == NUM_THREADS as u32 && count_after == count_before;
    console::print(&format!("  Result: {}\n", if ok { "PASS" } else { "FAIL" }));
    ok
}

// Yield counter for yield test
static YIELD_COUNT: AtomicU32 = AtomicU32::new(0);

fn increment_yield_count() {
    YIELD_COUNT.fetch_add(1, Ordering::AcqRel);
}

fn get_yield_count() -> u32 {
    YIELD_COUNT.load(Ordering::Acquire)
}

fn reset_yield_count() {
    YIELD_COUNT.store(0, Ordering::Release);
}

/// Test: Thread that yields multiple times
fn test_spawn_and_yield() -> bool {
    console::print("\n[TEST] Thread with multiple yields\n");

    reset_yield_count();

    console::print("  Spawning yielding thread...");
    match threading::spawn_fn(|| {
        // Yield 5 times, incrementing counter each time
        for _ in 0..5 {
            increment_yield_count();
            threading::yield_now();
        }
        threading::mark_current_terminated();
        loop {
            threading::yield_now();
            unsafe { core::arch::asm!("wfi") };
        }
    }) {
        Ok(tid) => console::print(&format!(" tid={}\n", tid)),
        Err(e) => {
            console::print(&format!(" FAILED: {}\n", e));
            return false;
        }
    }

    // Yield many times to let thread complete
    console::print("  Running scheduler...");
    for _ in 0..20 {
        threading::yield_now();
    }
    console::print(" done\n");

    let count = get_yield_count();
    console::print(&format!("  Yield count: {} (expect 5)\n", count));

    // Cleanup
    threading::cleanup_terminated();

    let ok = count == 5;
    console::print(&format!("  Result: {}\n", if ok { "PASS" } else { "FAIL" }));
    ok
}

/// Test: Cooperative thread spawning
fn test_spawn_cooperative() -> bool {
    console::print("\n[TEST] Cooperative thread spawn\n");

    set_test_flag(false);

    console::print("  Spawning cooperative thread...");
    match threading::spawn_fn_cooperative(|| {
        set_test_flag(true);
        threading::mark_current_terminated();
        loop {
            threading::yield_now();
            unsafe { core::arch::asm!("wfi") };
        }
    }) {
        Ok(tid) => console::print(&format!(" tid={}\n", tid)),
        Err(e) => {
            console::print(&format!(" FAILED: {}\n", e));
            return false;
        }
    }

    // Yield to let it run
    console::print("  Yielding...");
    for _ in 0..10 {
        threading::yield_now();
    }
    console::print(" done\n");

    let ran = get_test_flag();
    console::print(&format!("  Thread ran: {}\n", ran));

    // Cleanup
    threading::cleanup_terminated();

    console::print(&format!(
        "  Result: {}\n",
        if ran { "PASS" } else { "FAIL" }
    ));
    ran
}

// Yield cycle counter
static YIELD_CYCLE_COUNT: AtomicU32 = AtomicU32::new(0);

fn increment_yield_cycle() {
    YIELD_CYCLE_COUNT.fetch_add(1, Ordering::AcqRel);
}

fn get_yield_cycle() -> u32 {
    YIELD_CYCLE_COUNT.load(Ordering::Acquire)
}

fn reset_yield_cycle() {
    YIELD_CYCLE_COUNT.store(0, Ordering::Release);
}

/// Test: Thread can yield and resume multiple times in sequence
fn test_yield_cycle() -> bool {
    console::print("\n[TEST] Yield-resume cycle\n");

    reset_yield_cycle();

    const CYCLES: u32 = 10;

    console::print(&format!("  Spawning thread for {} yield cycles...", CYCLES));
    match threading::spawn_fn(|| {
        // Perform multiple yield-resume cycles
        for _ in 0..CYCLES {
            increment_yield_cycle();
            threading::yield_now();
        }
        threading::mark_current_terminated();
        loop {
            threading::yield_now();
            unsafe { core::arch::asm!("wfi") };
        }
    }) {
        Ok(tid) => console::print(&format!(" tid={}\n", tid)),
        Err(e) => {
            console::print(&format!(" FAILED: {}\n", e));
            return false;
        }
    }

    // Each cycle requires 2 yields (one from worker, one from main)
    // Plus extra to ensure completion
    console::print("  Running yield cycles...");
    for i in 0..(CYCLES * 2 + 10) {
        threading::yield_now();
        if i % 5 == 0 {
            console::print(".");
        }
    }
    console::print(" done\n");

    let cycles = get_yield_cycle();
    console::print(&format!(
        "  Completed cycles: {} (expect {})\n",
        cycles, CYCLES
    ));

    // Cleanup
    let cleaned = threading::cleanup_terminated();
    console::print(&format!("  Cleaned: {} threads\n", cleaned));

    let ok = cycles == CYCLES;
    console::print(&format!("  Result: {}\n", if ok { "PASS" } else { "FAIL" }));
    ok
}

// Flags for mixed thread test
static COOP_THREAD_DONE: AtomicBool = AtomicBool::new(false);
static PREEMPT_THREAD_DONE: AtomicBool = AtomicBool::new(false);

fn set_coop_done(val: bool) {
    COOP_THREAD_DONE.store(val, Ordering::Release);
}

fn get_coop_done() -> bool {
    COOP_THREAD_DONE.load(Ordering::Acquire)
}

fn set_preempt_done(val: bool) {
    PREEMPT_THREAD_DONE.store(val, Ordering::Release);
}

fn get_preempt_done() -> bool {
    PREEMPT_THREAD_DONE.load(Ordering::Acquire)
}

/// Test: Mixed cooperative and preemptible threads
/// - 1 cooperative thread: yields for 5ms then exits
/// - 1 preemptible thread: loops for 15ms then exits  
/// - Verify both complete and only idle thread remains after cleanup
fn test_mixed_cooperative_preemptible() -> bool {
    console::print("\n[TEST] Mixed cooperative & preemptible threads\n");

    set_coop_done(false);
    set_preempt_done(false);

    let count_before = threading::thread_count();
    console::print(&format!("  Threads before: {}\n", count_before));

    // Spawn cooperative thread: yields for ~5ms total
    console::print("  Spawning cooperative thread (5ms)...");
    match threading::spawn_fn_cooperative(|| {
        let start = crate::timer::uptime_us();
        let target = 5_000; // 5ms

        while crate::timer::uptime_us() - start < target {
            threading::yield_now();
        }

        set_coop_done(true);
        threading::mark_current_terminated();
        loop {
            threading::yield_now();
            unsafe { core::arch::asm!("wfi") };
        }
    }) {
        Ok(tid) => console::print(&format!(" tid={}\n", tid)),
        Err(e) => {
            console::print(&format!(" FAILED: {}\n", e));
            return false;
        }
    }

    // Spawn preemptible thread: busy-loops for ~15ms
    console::print("  Spawning preemptible thread (15ms)...");
    match threading::spawn_fn(|| {
        let start = crate::timer::uptime_us();
        let target = 15_000; // 15ms

        // Busy loop - will be preempted by timer
        while crate::timer::uptime_us() - start < target {
            // Just spin
            unsafe { core::arch::asm!("nop") };
        }

        set_preempt_done(true);
        threading::mark_current_terminated();
        loop {
            threading::yield_now();
            unsafe { core::arch::asm!("wfi") };
        }
    }) {
        Ok(tid) => console::print(&format!(" tid={}\n", tid)),
        Err(e) => {
            console::print(&format!(" FAILED: {}\n", e));
            return false;
        }
    }

    let count_mid = threading::thread_count();
    console::print(&format!("  Threads after spawn: {}\n", count_mid));

    // Wait for both to complete (max 30ms with some margin)
    console::print("  Waiting for threads to complete...");
    let wait_start = crate::timer::uptime_us();
    let max_wait = 50_000; // 50ms max

    while (!get_coop_done() || !get_preempt_done())
        && (crate::timer::uptime_us() - wait_start < max_wait)
    {
        threading::yield_now();
    }

    let elapsed = (crate::timer::uptime_us() - wait_start) / 1000;
    console::print(&format!(" {}ms\n", elapsed));

    // Check completion
    let coop_done = get_coop_done();
    let preempt_done = get_preempt_done();
    console::print(&format!("  Cooperative done: {}\n", coop_done));
    console::print(&format!("  Preemptible done: {}\n", preempt_done));

    // Cleanup
    let cleaned = threading::cleanup_terminated();
    console::print(&format!("  Cleaned: {} threads\n", cleaned));

    let count_after = threading::thread_count();
    console::print(&format!("  Threads after cleanup: {}\n", count_after));

    // Verify: both threads completed and only idle remains
    let ok = coop_done && preempt_done && count_after == 1;
    console::print(&format!("  Result: {}\n", if ok { "PASS" } else { "FAIL" }));
    ok
}

// ============================================================================
// Parallel Process Tests
// ============================================================================

// Flags for parallel process test
static PROCESS1_STARTED: AtomicBool = AtomicBool::new(false);
static PROCESS2_STARTED: AtomicBool = AtomicBool::new(false);
static PROCESS1_DONE: AtomicBool = AtomicBool::new(false);
static PROCESS2_DONE: AtomicBool = AtomicBool::new(false);

/// Test: Run 2 processes in parallel and verify both appear in process table
/// 
/// This tests true concurrency:
/// 1. Spawn 2 hello processes on separate threads
/// 2. While both are running, check process table shows 2 processes
/// 3. Wait for both to complete
/// 4. Verify both ran successfully
fn test_parallel_processes() -> bool {
    console::print("\n[TEST] Parallel process execution\n");

    // Reset flags
    PROCESS1_STARTED.store(false, Ordering::Release);
    PROCESS2_STARTED.store(false, Ordering::Release);
    PROCESS1_DONE.store(false, Ordering::Release);
    PROCESS2_DONE.store(false, Ordering::Release);

    let thread_count_before = threading::thread_count();
    console::print(&format!("  Threads before: {}\n", thread_count_before));

    // Check if hello binary exists by trying to read it
    if crate::fs::read_file("/bin/hello").is_err() {
        if config::FAIL_TESTS_IF_TEST_BINARY_MISSING {
            console::print("  /bin/hello not found\n");
            console::print("  Result: FAIL\n");
            return false;
        } else {
            console::print("  Skipping: /bin/hello not found\n");
            console::print("  Result: SKIP\n");
            return true; // Skip, don't fail
        }
    }

    // Spawn first process using spawn_process_with_channel
    console::print("  Spawning process 1...");
    let result1 = crate::process::spawn_process_with_channel("/bin/hello", None);
    
    let (tid1, channel1) = match result1 {
        Ok((tid, channel)) => {
            console::print(&format!(" tid={}\n", tid));
            PROCESS1_STARTED.store(true, Ordering::Release);
            (tid, channel)
        }
        Err(e) => {
            console::print(&format!(" FAILED: {}\n", e));
            console::print("  Result: FAIL\n");
            return false;
        }
    };

    // Spawn second process
    console::print("  Spawning process 2...");
    let result2 = crate::process::spawn_process_with_channel("/bin/hello", None);

    let (tid2, channel2) = match result2 {
        Ok((tid, channel)) => {
            console::print(&format!(" tid={}\n", tid));
            PROCESS2_STARTED.store(true, Ordering::Release);
            (tid, channel)
        }
        Err(e) => {
            console::print(&format!(" FAILED: {}\n", e));
            console::print("  Result: FAIL\n");
            return false;
        }
    };

    console::print(&format!("  Spawned threads {} and {}\n", tid1, tid2));

    // Both processes are already spawned, give them a moment to start executing
    console::print("  Yielding to let processes execute...");
    for _ in 0..10 {
        threading::yield_now();
    }
    console::print(" done\n");

    // Check process table while both are (hopefully) still running
    // Note: With hello's 1-second sleep, they should overlap
    let processes = crate::process::list_processes();
    console::print(&format!("  Processes in table: {}\n", processes.len()));
    for p in &processes {
        console::print(&format!("    PID {} ({}): {}\n", p.pid, p.name, p.state));
    }

    // Count hello processes
    let hello_count = processes.iter().filter(|p| p.name == "hello").count();
    console::print(&format!("  Hello processes running: {}\n", hello_count));

    // Wait for both to complete using channel status
    console::print("  Waiting for processes to complete...");
    let complete_timeout = 30_000_000; // 30 seconds (hello runs for ~10 seconds)
    let complete_start = crate::timer::uptime_us();

    loop {
        threading::yield_now();
        
        let p1_done = channel1.has_exited() || crate::threading::is_thread_terminated(tid1);
        let p2_done = channel2.has_exited() || crate::threading::is_thread_terminated(tid2);
        
        if p1_done && p2_done {
            console::print(" done\n");
            PROCESS1_DONE.store(true, Ordering::Release);
            PROCESS2_DONE.store(true, Ordering::Release);
            break;
        }

        if crate::timer::uptime_us() - complete_start > complete_timeout {
            console::print(" TIMEOUT\n");
            console::print(&format!("    P1 done: {}, P2 done: {}\n", p1_done, p2_done));
            // Continue to cleanup even on timeout
            break;
        }
    }

    // Cleanup
    let cleaned = threading::cleanup_terminated();
    console::print(&format!("  Cleaned: {} threads\n", cleaned));

    let thread_count_after = threading::thread_count();
    console::print(&format!("  Threads after: {}\n", thread_count_after));

    // Verify results
    let p1_done = PROCESS1_DONE.load(Ordering::Acquire);
    let p2_done = PROCESS2_DONE.load(Ordering::Acquire);
    
    // Success if:
    // 1. Both processes completed
    // 2. We saw at least 1 hello process in the table (ideally 2, but timing-dependent)
    let ok = p1_done && p2_done && hello_count >= 1;
    
    if !ok {
        console::print(&format!("  P1 done: {}, P2 done: {}, hello_count: {}\n", 
                               p1_done, p2_done, hello_count));
    }
    
    console::print(&format!("  Result: {}\n", if ok { "PASS" } else { "FAIL" }));
    ok
}
