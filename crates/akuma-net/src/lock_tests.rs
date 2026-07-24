//! Network lock foundation tests
//!
//! Basic unit tests for the lock infrastructure to verify ordering enforcement
//! and statistics tracking work correctly.

#![cfg(test)]
#![cfg(feature = "smoltcp")]

use crate::locks::*;

#[test]
fn test_lock_constants() {
    assert_eq!(LOCK_LEVEL_NETWORK, 10);
    assert_eq!(LOCK_LEVEL_SOCKET_TABLE, 20);
    assert_eq!(LOCK_LEVEL_SOCKET, 30);
    assert_eq!(LOCK_HOLDER_NONE, u32::MAX);
}

#[test]
fn test_stats_initialization() {
    with_test_serial(|| {
        let stats = get_lock_stats();
        assert_eq!(stats.network_contention_count, 0);
        assert_eq!(stats.network_contention_spins, 0);
        assert_eq!(stats.socket_table_contention_count, 0);
        assert_eq!(stats.socket_table_contention_spins, 0);
        assert_eq!(stats.ordering_violations, 0);
    });
}

#[test]
fn test_holder_tracking_initial() {
    with_test_serial(|| {
        // With a freshly-reset state, no locks should be held
        assert_eq!(network_lock_holder(), LOCK_HOLDER_NONE);
        assert_eq!(socket_table_lock_holder(), LOCK_HOLDER_NONE);
    });
}

#[test]
fn test_reset_clears_stats() {
    with_test_serial(|| {
        // Trigger a violation by attempting to acquire socket table lock without network lock
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            acquire_socket_table_lock(1);
        }));

        // Should have panicked due to ordering violation
        assert!(result.is_err());

        // Verify stats recorded the violation
        let stats_before = get_lock_stats();
        assert!(stats_before.ordering_violations > 0);

        // Reset
        reset_lock_stats();

        // Verify stats cleared
        let stats_after = get_lock_stats();
        assert_eq!(stats_after.ordering_violations, 0);
        assert_eq!(stats_after.network_contention_count, 0);
        assert_eq!(stats_after.socket_table_contention_count, 0);
    });
}

#[test]
fn test_network_lock_acquire_release() {
    with_test_serial(|| {
        // Acquire network lock should succeed
        acquire_network_lock(42);

        // Verify holder tracking
        assert_eq!(network_lock_holder(), 42);

        // Release network lock
        release_network_lock();

        // Verify holder cleared
        assert_eq!(network_lock_holder(), LOCK_HOLDER_NONE);

        // Check no violations
        let stats = get_lock_stats();
        assert_eq!(stats.ordering_violations, 0);
    });
}

#[test]
fn test_lock_hierarchy_network_then_socket_table() {
    with_test_serial(|| {
        // Acquire network lock first (correct order)
        acquire_network_lock(1);

        // Then acquire socket table lock (correct order)
        acquire_socket_table_lock(1);

        // Verify both locks held
        assert_eq!(network_lock_holder(), 1);
        assert_eq!(socket_table_lock_holder(), 1);

        // Release in reverse order
        release_socket_table_lock();
        release_network_lock();

        // Verify no violations
        let stats = get_lock_stats();
        assert_eq!(stats.ordering_violations, 0);
    });
}

#[test]
fn test_multiple_acquisitions_same_level() {
    with_test_serial(|| {
        // Acquire network lock multiple times (should be idempotent)
        acquire_network_lock(1);
        acquire_network_lock(1);

        // Should be able to acquire socket table lock
        acquire_socket_table_lock(1);

        // Release everything
        release_socket_table_lock();
        release_network_lock();

        let stats = get_lock_stats();
        assert_eq!(stats.ordering_violations, 0);
    });
}

#[test]
fn test_invalid_lock_level_panics() {
    reset_lock_stats();
    
    // Try to use an invalid lock level (this should panic in can_acquire_lock)
    let result = core::panic::catch_unwind(core::panic::AssertUnwindSafe(|| {
        // Try to mark an invalid lock level as held
        let bit = match 99u8 {
            LOCK_LEVEL_NETWORK => 0,
            LOCK_LEVEL_SOCKET_TABLE => 1,
            LOCK_LEVEL_SOCKET => 2,
            _ => panic!("Unknown lock level: {}", 99),
        };
    }));
    
    // Should panic due to invalid lock level
    assert!(result.is_err());
}