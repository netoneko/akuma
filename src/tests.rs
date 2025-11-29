//! System tests for threading and other core functionality
//!
//! Run with `tests::run_all()` after scheduler initialization.
//! If tests fail, the kernel should halt.

use crate::console;
use crate::threading;
use alloc::format;

/// Run all system tests - returns true if all pass
pub fn run_all() -> bool {
    console::print("\n========== System Tests ==========\n");
    
    let mut all_pass = true;
    
    // Threading tests
    all_pass &= test_scheduler_init();
    all_pass &= test_thread_stats();
    all_pass &= test_yield();
    all_pass &= test_cooperative_timeout();
    all_pass &= test_thread_cleanup();
    
    console::print("\n==================================\n");
    console::print(&format!("Overall: {}\n", if all_pass { "ALL TESTS PASSED" } else { "SOME TESTS FAILED" }));
    console::print("==================================\n\n");
    
    all_pass
}

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
    
    console::print(&format!("  Ready: {}, Running: {}, Terminated: {}\n", ready, running, terminated));
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
    
    console::print(&format!("  Timeout: {} us ({} seconds)\n", timeout, timeout / 1_000_000));
    console::print(&format!("  Result: {}\n", if ok { "PASS" } else { "DISABLED (0)" }));
    
    ok
}

/// Test: Cleanup function exists and doesn't crash
fn test_thread_cleanup() -> bool {
    console::print("\n[TEST] Thread cleanup\n");
    
    // Get initial state
    let count_before = threading::thread_count();
    let (ready, running, terminated) = threading::thread_stats();
    console::print(&format!("  State: {} threads (R:{} U:{} T:{})\n", count_before, ready, running, terminated));
    
    // Run cleanup (should be safe even with no terminated threads)
    let cleaned = threading::cleanup_terminated();
    console::print(&format!("  Cleaned: {} threads\n", cleaned));
    
    // Verify state is still valid
    let count_after = threading::thread_count();
    let (ready2, running2, terminated2) = threading::thread_stats();
    console::print(&format!("  After: {} threads (R:{} U:{} T:{})\n", count_after, ready2, running2, terminated2));
    
    // Test passes if:
    // 1. Count decreased by amount cleaned (or stayed same if 0 cleaned)
    // 2. At least one thread still exists (idle)
    let count_ok = count_after == count_before - cleaned;
    let has_idle = count_after >= 1;
    let ok = count_ok && has_idle;
    
    console::print(&format!("  Result: {}\n", if ok { "PASS" } else { "FAIL" }));
    
    ok
}

