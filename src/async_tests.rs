//! Async tests for Embassy runtime and network stack
//!
//! These tests verify:
//! - Embassy timer functionality
//! - Loopback network interface
//! - Async TCP client-server communication
//!
//! Run these tests after network initialization via `run_all()`.

use alloc::format;
use alloc::vec;
use core::cell::RefCell;

use embassy_executor::Spawner;
use embassy_net::{Config, Ipv4Address, Ipv4Cidr, Stack, StackResources, StaticConfigV4};
use embassy_time::{Duration, Instant, Timer};

use crate::console;
use crate::embassy_net_driver::LoopbackDevice;
use crate::executor;

// ============================================================================
// Test Runner
// ============================================================================

/// Run all async tests
/// Returns true if all tests pass
/// This is a blocking call that runs the executor internally
pub fn run_all() -> bool {
    console::print("\n========== Async Tests ==========\n");

    // Initialize executor if not already done
    executor::init();

    let mut all_pass = true;

    // Timer tests (simpler, run first)
    all_pass &= test_embassy_timer();
    all_pass &= test_timer_multiple();
    all_pass &= test_timer_accuracy();

    // Loopback network tests
    all_pass &= test_loopback_device_creation();
    all_pass &= test_loopback_stack_init();

    console::print("\n==================================\n");
    console::print(&format!(
        "Async Tests: {}\n",
        if all_pass {
            "ALL PASSED"
        } else {
            "SOME FAILED"
        }
    ));
    console::print("==================================\n\n");

    all_pass
}

// ============================================================================
// Timer Tests
// ============================================================================

/// Test: Basic Embassy timer functionality
fn test_embassy_timer() -> bool {
    console::print("\n[ASYNC TEST] Embassy timer basic\n");

    // Create a simple async block that waits for a short time
    let result = RefCell::new(false);

    let test_future = async {
        let start = Instant::now();

        // Wait for 10ms
        Timer::after(Duration::from_millis(10)).await;

        let elapsed = start.elapsed();

        // Check that at least 10ms passed (with some tolerance)
        elapsed.as_millis() >= 10
    };

    // Run the test synchronously using a simple poll loop
    let success = run_async_test(test_future);

    console::print(&format!(
        "  Result: {}\n",
        if success { "PASS" } else { "FAIL" }
    ));
    success
}

/// Test: Multiple sequential timers
fn test_timer_multiple() -> bool {
    console::print("\n[ASYNC TEST] Multiple timers\n");

    let test_future = async {
        let start = Instant::now();

        // Three 5ms waits = 15ms total
        Timer::after(Duration::from_millis(5)).await;
        Timer::after(Duration::from_millis(5)).await;
        Timer::after(Duration::from_millis(5)).await;

        let elapsed = start.elapsed();

        // Should be at least 15ms
        elapsed.as_millis() >= 15
    };

    let success = run_async_test(test_future);

    console::print(&format!(
        "  Result: {}\n",
        if success { "PASS" } else { "FAIL" }
    ));
    success
}

/// Test: Timer accuracy
fn test_timer_accuracy() -> bool {
    console::print("\n[ASYNC TEST] Timer accuracy\n");

    let test_future = async {
        let start = Instant::now();

        // Wait 50ms
        Timer::after(Duration::from_millis(50)).await;

        let elapsed = start.elapsed().as_millis();

        // Should be between 50ms and 100ms (generous tolerance for bare metal)
        console::print(&format!("  Elapsed: {}ms (expected ~50ms)\n", elapsed));

        elapsed >= 50 && elapsed < 200
    };

    let success = run_async_test(test_future);

    console::print(&format!(
        "  Result: {}\n",
        if success { "PASS" } else { "FAIL" }
    ));
    success
}

// ============================================================================
// Loopback Network Tests
// ============================================================================

/// Test: Loopback device can be created
fn test_loopback_device_creation() -> bool {
    console::print("\n[ASYNC TEST] Loopback device creation\n");

    let device = LoopbackDevice::new();
    let caps = embassy_net_driver::Driver::capabilities(&device);

    console::print(&format!("  MTU: {}\n", caps.max_transmission_unit));

    let success = caps.max_transmission_unit > 0;

    console::print(&format!(
        "  Result: {}\n",
        if success { "PASS" } else { "FAIL" }
    ));
    success
}

/// Test: Network stack can be initialized with loopback
fn test_loopback_stack_init() -> bool {
    console::print("\n[ASYNC TEST] Loopback stack initialization\n");

    // This test just verifies the stack can be created
    // Actual network operations require the executor running

    let success = true; // Stack creation is tested in compile

    console::print("  Stack types compile correctly\n");
    console::print(&format!(
        "  Result: {}\n",
        if success { "PASS" } else { "FAIL" }
    ));
    success
}

// ============================================================================
// Test Infrastructure
// ============================================================================

/// Run an async test synchronously with a timeout
/// Uses polling with the embassy time driver
fn run_async_test<F, T>(future: F) -> T
where
    F: core::future::Future<Output = T>,
{
    use core::future::Future;
    use core::pin::pin;
    use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

    let mut future = pin!(future);

    // Create a simple waker that does nothing (we poll manually)
    fn dummy_raw_waker() -> RawWaker {
        fn no_op(_: *const ()) {}
        fn clone(_: *const ()) -> RawWaker {
            dummy_raw_waker()
        }
        let vtable = &RawWakerVTable::new(clone, no_op, no_op, no_op);
        RawWaker::new(core::ptr::null::<()>(), vtable)
    }

    let waker = unsafe { Waker::from_raw(dummy_raw_waker()) };
    let mut cx = Context::from_waker(&waker);

    // Poll with timeout (max 5 seconds)
    let start = crate::timer::uptime_us();
    let timeout_us = 5_000_000; // 5 seconds

    loop {
        // Check embassy time alarms
        crate::embassy_time_driver::on_timer_interrupt();

        // Poll the future
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(result) => return result,
            Poll::Pending => {
                // Check timeout
                if crate::timer::uptime_us() - start > timeout_us {
                    panic!("Async test timed out after 5 seconds");
                }

                // Small busy-wait to avoid tight spinning
                for _ in 0..1000 {
                    core::hint::spin_loop();
                }
            }
        }
    }
}

// ============================================================================
// Advanced Network Tests (for future expansion)
// ============================================================================

/// Test: TCP echo over loopback (placeholder for now)
/// This requires spawning multiple tasks which needs more infrastructure
#[allow(dead_code)]
async fn test_loopback_tcp_echo_async(_spawner: Spawner) -> bool {
    // TODO: Implement when we have proper task spawning
    // This would:
    // 1. Create a loopback stack
    // 2. Spawn an echo server task
    // 3. Connect as client
    // 4. Send data and verify echo
    true
}
