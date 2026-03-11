//! Kernel-level tests for akuma-exec.
//!
//! These tests run inside the kernel at boot time and can exercise
//! functionality that requires the runtime (threading, MMU, etc.).
//! Call `run_all_tests()` from the kernel boot sequence.

use crate::runtime::runtime;
use crate::threading;
use crate::threading::types::{Context, StackInfo, MAX_THREADS};
use crate::mmu::asid::AsidAllocator;

fn print(msg: &str) {
    (runtime().print_str)(msg);
}

macro_rules! test_pass {
    ($name:expr) => {
        print(concat!("  [PASS] ", $name, "\n"));
    };
}

macro_rules! test_fail {
    ($name:expr, $msg:expr) => {
        (runtime().print_str)(concat!("  [FAIL] ", $name, ": "));
        (runtime().print_str)($msg);
        (runtime().print_str)("\n");
    };
}

macro_rules! assert_test {
    ($cond:expr, $name:expr) => {
        if $cond {
            test_pass!($name);
        } else {
            test_fail!($name, "assertion failed");
        }
    };
    ($cond:expr, $name:expr, $msg:expr) => {
        if $cond {
            test_pass!($name);
        } else {
            test_fail!($name, $msg);
        }
    };
}

pub fn run_all_tests() {
    print("\n--- akuma-exec Kernel Tests ---\n");
    test_context_validity();
    test_stack_info_operations();
    test_asid_allocator();
    test_thread_pool_initialized();
    test_current_thread_id();
    test_stack_requirements();
    print("--- akuma-exec Kernel Tests Done ---\n\n");
}

fn test_context_validity() {
    let ctx = Context::zero();
    assert_test!(ctx.is_valid(), "context_zero_is_valid");

    let mut ctx2 = Context::zero();
    ctx2.magic = 0;
    assert_test!(!ctx2.is_valid(), "context_corrupt_magic_invalid");
}

fn test_stack_info_operations() {
    let s1 = StackInfo::new(0x1000, 0x2000);
    let s2 = StackInfo::new(0x4000, 0x1000);
    assert_test!(!s1.overlaps(&s2), "stack_info_disjoint_no_overlap");
    assert_test!(s1.contains(0x2000), "stack_info_contains_mid");
    assert_test!(!s1.contains(0x3000), "stack_info_not_contains_top");

    let empty = StackInfo::empty();
    assert_test!(!empty.is_allocated(), "stack_info_empty_not_allocated");
}

fn test_asid_allocator() {
    let mut alloc = AsidAllocator::new();
    let first = alloc.alloc();
    assert_test!(first == Some(1), "asid_first_alloc_is_1");

    let second = alloc.alloc();
    assert_test!(second == Some(2), "asid_second_alloc_is_2");

    alloc.free(1);
    let mut found_freed = false;
    for _ in 0..255 {
        if let Some(a) = alloc.alloc() {
            if a == 1 {
                found_freed = true;
                break;
            }
        }
    }
    assert_test!(found_freed, "asid_freed_can_be_reallocated");
}

fn test_thread_pool_initialized() {
    let (running, ready, terminated) = threading::thread_stats();
    assert_test!(running > 0, "thread_pool_has_running_threads");
    let _ = (ready, terminated);
}

fn test_current_thread_id() {
    let tid = threading::current_thread_id();
    assert_test!(tid < MAX_THREADS, "current_thread_id_in_range");
}

fn test_stack_requirements() {
    let summary = threading::calculate_stack_requirements();
    assert_test!(summary.total_bytes > 0, "stack_requirements_nonzero");
    assert_test!(summary.system_thread_count > 0, "stack_has_system_threads");
    assert_test!(summary.user_thread_count > 0, "stack_has_user_threads");
}
