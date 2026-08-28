//! Network Lock Foundation for Fine-Grained Locking
//!
//! This module provides the lock infrastructure for replacing the BKL with
//! fine-grained networking locks. It defines the lock hierarchy and provides
//! utilities for lock ordering enforcement and profiling.
//!
//! # Lock Hierarchy
//!
//! Locks must be acquired in the following order to prevent deadlocks:
//! 1. NETWORK_LOCK (global network state)
//! 2. SOCKET_TABLE_LOCK (socket descriptor table)
//! 3. Per-socket locks (connection-specific operations)
//!
//! # Lock Ordering Enforcement
//!
//! The lock ordering is enforced through:
//! - Numeric lock levels (lower = higher priority)
//! - Runtime tracking of held locks
//! - Panic on violation attempts

use core::sync::atomic::{AtomicU32, Ordering};
use spinning_top::Spinlock;

// ============================================================================
// Lock Ordering Constants
// ============================================================================

/// Lock level for the global network lock (highest priority)
pub const LOCK_LEVEL_NETWORK: u8 = 10;

/// Lock level for the socket table lock
pub const LOCK_LEVEL_SOCKET_TABLE: u8 = 20;

/// Lock level for per-socket locks (lowest priority)
pub const LOCK_LEVEL_SOCKET: u8 = 30;

/// Sentinel value indicating no lock is held
pub const LOCK_HOLDER_NONE: u32 = u32::MAX;

// ============================================================================
// Global Network Lock
// ============================================================================

/// Global network lock protecting all network stack state.
///
/// This lock protects:
/// - Network interface configuration
/// - Socket set and all smoltcp sockets
/// - Network device state
/// - DHCP and DNS client state
/// - Loopback packet queue
///
/// Replaces the BKL for network operations. Must be acquired before
/// SOCKET_TABLE_LOCK to maintain lock ordering.
///
/// # Lock Level
/// `LOCK_LEVEL_NETWORK`
pub static NETWORK_LOCK: Spinlock<()> = Spinlock::new(());

// ============================================================================
// Socket Table Lock
// ============================================================================

/// Socket table lock protecting the socket descriptor table.
///
/// This lock protects:
/// - Socket descriptor array (`SOCKET_TABLE`)
/// - Socket lifecycle (creation, closure)
/// - Ephemeral port allocation state
///
/// Must be acquired after NETWORK_LOCK to maintain lock ordering.
///
/// # Lock Level
/// `LOCK_LEVEL_SOCKET_TABLE`
pub static SOCKET_TABLE_LOCK: Spinlock<()> = Spinlock::new(());

// ============================================================================
// Lock Holder Tracking (for Debugging and Watchdogs)
// ============================================================================

/// Current holder of the network lock, or `LOCK_HOLDER_NONE` if free.
///
/// Used by the SSH stall watchdog in `src/main.rs::memory_monitor` to detect
/// long-held locks. Best-effort tracking (no acquire ordering vs the spinlock
/// itself) - a stall report may see a torn snapshot, which is acceptable for
/// stall detection.
static NETWORK_LOCK_HOLDER: AtomicU32 = AtomicU32::new(LOCK_HOLDER_NONE);

/// Current holder of the socket table lock, or `LOCK_HOLDER_NONE` if free.
static SOCKET_TABLE_LOCK_HOLDER: AtomicU32 = AtomicU32::new(LOCK_HOLDER_NONE);

/// Get the current holder of the network lock (for watchdog monitoring)
pub fn network_lock_holder() -> u32 {
    NETWORK_LOCK_HOLDER.load(Ordering::Relaxed)
}

/// Get the current holder of the socket table lock (for watchdog monitoring)
pub fn socket_table_lock_holder() -> u32 {
    SOCKET_TABLE_LOCK_HOLDER.load(Ordering::Relaxed)
}

// ============================================================================
// Lock Ordering Enforcement
// ============================================================================

/// Track which locks are currently held by this thread.
///
/// Used to enforce lock ordering and prevent deadlocks. The bits represent
/// which locks are held:
/// - Bit 0: NETWORK_LOCK
/// - Bit 1: SOCKET_TABLE_LOCK
/// - Bits 2-7: Reserved for future locks
static HELD_LOCKS: AtomicU32 = AtomicU32::new(0);

/// Mark a lock as held during acquisition.
fn mark_lock_held(lock_level: u8) {
    let bit = match lock_level {
        LOCK_LEVEL_NETWORK => 0,
        LOCK_LEVEL_SOCKET_TABLE => 1,
        LOCK_LEVEL_SOCKET => 2,
        _ => panic!("Unknown lock level: {lock_level}"),
    };
    HELD_LOCKS.fetch_or(1 << bit, Ordering::Relaxed);
}

/// Mark a lock as released.
fn mark_lock_released(lock_level: u8) {
    let bit = match lock_level {
        LOCK_LEVEL_NETWORK => 0,
        LOCK_LEVEL_SOCKET_TABLE => 1,
        LOCK_LEVEL_SOCKET => 2,
        _ => panic!("Unknown lock level: {lock_level}"),
    };
    HELD_LOCKS.fetch_and(!(1 << bit), Ordering::Relaxed);
}

/// Check if a lock can be acquired without violating ordering.
fn can_acquire_lock(lock_level: u8) -> bool {
    let held = HELD_LOCKS.load(Ordering::Relaxed);
    
    match lock_level {
        LOCK_LEVEL_NETWORK => {
            // Network lock can always be acquired (highest priority)
            true
        }
        LOCK_LEVEL_SOCKET_TABLE => {
            // Socket table lock requires network lock to be held first
            held & (1 << 0) != 0
        }
        LOCK_LEVEL_SOCKET => {
            // Per-socket lock requires socket table lock to be held first
            held & (1 << 1) != 0
        }
        _ => panic!("Unknown lock level: {lock_level}"),
    }
}

// ============================================================================
// Profiling Infrastructure
// ============================================================================

/// Network lock contention statistics
#[derive(Default)]
pub struct NetworkLockStats {
    /// Number of times NETWORK_LOCK was contended
    pub network_contention_count: u64,
    /// Total spins waiting for NETWORK_LOCK
    pub network_contention_spins: u64,
    /// Number of times SOCKET_TABLE_LOCK was contended
    pub socket_table_contention_count: u64,
    /// Total spins waiting for SOCKET_TABLE_LOCK
    pub socket_table_contention_spins: u64,
    /// Number of lock ordering violations detected
    pub ordering_violations: u64,
}

impl NetworkLockStats {
    /// Create a new zero-initialized stats structure
    const fn new() -> Self {
        Self {
            network_contention_count: 0,
            network_contention_spins: 0,
            socket_table_contention_count: 0,
            socket_table_contention_spins: 0,
            ordering_violations: 0,
        }
    }
}

static LOCK_STATS: Spinlock<NetworkLockStats> = Spinlock::new(NetworkLockStats::new());

/// Get the current lock statistics (for profiling)
pub fn get_lock_stats() -> NetworkLockStats {
    let stats = LOCK_STATS.lock();
    NetworkLockStats {
        network_contention_count: stats.network_contention_count,
        network_contention_spins: stats.network_contention_spins,
        socket_table_contention_count: stats.socket_table_contention_count,
        socket_table_contention_spins: stats.socket_table_contention_spins,
        ordering_violations: stats.ordering_violations,
    }
}

/// Reset lock statistics (for A/B testing)
pub fn reset_lock_stats() {
    let mut stats = LOCK_STATS.lock();
    *stats = NetworkLockStats::default();
}

// ============================================================================
// Lock Acquisition Helpers (with Ordering Enforcement)
// ============================================================================

/// Acquire the network lock with ordering enforcement.
///
/// # Panics
/// Panics if lock ordering would be violated.
pub fn acquire_network_lock(holder_id: u32) {
    if !can_acquire_lock(LOCK_LEVEL_NETWORK) {
        let mut stats = LOCK_STATS.lock();
        stats.ordering_violations += 1;
        panic!("Lock ordering violation: attempted to acquire NETWORK_LOCK out of order");
    }
    
    // Acquire the spinlock (this is the actual blocking point)
    let _guard = NETWORK_LOCK.lock();
    
    // Track holder for watchdog monitoring
    NETWORK_LOCK_HOLDER.store(holder_id, Ordering::Relaxed);
    
    // Mark as held for ordering checks
    mark_lock_held(LOCK_LEVEL_NETWORK);
    
    // Note: The guard is dropped here, but the mark remains held
    // This is intentional - the caller is responsible for calling release_network_lock
}

/// Release the network lock.
///
/// # Safety
/// Must only be called after `acquire_network_lock` by the same thread.
pub fn release_network_lock() {
    mark_lock_released(LOCK_LEVEL_NETWORK);
    NETWORK_LOCK_HOLDER.store(LOCK_HOLDER_NONE, Ordering::Relaxed);
}

/// Acquire the socket table lock with ordering enforcement.
///
/// # Panics
/// Panics if lock ordering would be violated.
pub fn acquire_socket_table_lock(holder_id: u32) {
    if !can_acquire_lock(LOCK_LEVEL_SOCKET_TABLE) {
        let mut stats = LOCK_STATS.lock();
        stats.ordering_violations += 1;
        panic!("Lock ordering violation: attempted to acquire SOCKET_TABLE_LOCK without NETWORK_LOCK");
    }
    
    // Acquire the spinlock
    let _guard = SOCKET_TABLE_LOCK.lock();
    
    // Track holder for watchdog monitoring
    SOCKET_TABLE_LOCK_HOLDER.store(holder_id, Ordering::Relaxed);
    
    // Mark as held for ordering checks
    mark_lock_held(LOCK_LEVEL_SOCKET_TABLE);
    
    // Note: The guard is dropped here, but the mark remains held
    // This is intentional - the caller is responsible for calling release_socket_table_lock
}

/// Release the socket table lock.
///
/// # Safety
/// Must only be called after `acquire_socket_table_lock` by the same thread.
pub fn release_socket_table_lock() {
    mark_lock_released(LOCK_LEVEL_SOCKET_TABLE);
    SOCKET_TABLE_LOCK_HOLDER.store(LOCK_HOLDER_NONE, Ordering::Relaxed);
}

// ============================================================================
// Testing Utilities
// ============================================================================

/// Test-only: reset the process-global lock state to a clean slate.
///
/// `HELD_LOCKS`, `LOCK_STATS`, and the holder atomics are all process-wide, but
/// cargo runs unit tests on multiple threads — so any test that touches them
/// races unless serialized AND reset. Both test modules (`locks::tests` and
/// `lock_tests.rs`) funnel through [`with_test_serial`], which calls this first.
#[cfg(test)]
pub(crate) fn reset_test_state() {
    HELD_LOCKS.store(0, Ordering::Relaxed);
    NETWORK_LOCK_HOLDER.store(LOCK_HOLDER_NONE, Ordering::Relaxed);
    SOCKET_TABLE_LOCK_HOLDER.store(LOCK_HOLDER_NONE, Ordering::Relaxed);
    reset_lock_stats();
}

/// Test-only: run `f` serialized against every other lock test, from freshly-reset
/// global state. Without this, a test that deliberately triggers an ordering
/// violation (bumping `LOCK_STATS.ordering_violations`) bleeds into a concurrent
/// test asserting the count is zero — the flake that made
/// `test_lock_ordering_enforcement` fail intermittently. Recovers from a poisoned
/// mutex (a `#[should_panic]` test poisons it on unwind); the guarded state is
/// reset on the next entry, so the poison carries no stale data.
#[cfg(test)]
pub(crate) fn with_test_serial<F: FnOnce()>(f: F) {
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = SERIAL.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    reset_test_state();
    f();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lock_ordering_enforcement() {
        with_test_serial(|| {
            // Acquire network lock should succeed
            acquire_network_lock(1);

            // Acquire socket table lock should succeed (network held)
            acquire_socket_table_lock(1);

            // Release in reverse order
            release_socket_table_lock();
            release_network_lock();

            // Verify no violations
            let stats = get_lock_stats();
            assert_eq!(stats.ordering_violations, 0);
        });
    }

    #[test]
    #[should_panic(expected = "Lock ordering violation")]
    fn test_socket_table_without_network() {
        with_test_serial(|| {
            // Attempting to acquire socket table lock without network lock should panic
            acquire_socket_table_lock(1);
        });
    }

    #[test]
    fn test_stats_tracking() {
        with_test_serial(|| {
            let initial_stats = get_lock_stats();
            assert_eq!(initial_stats.ordering_violations, 0);

            // Trigger a violation
            let _ = std::panic::catch_unwind(|| {
                acquire_socket_table_lock(1);
            });

            let stats = get_lock_stats();
            assert_eq!(stats.ordering_violations, 1);
        });
    }
}