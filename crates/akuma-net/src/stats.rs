//! Network Statistics
//!
//! Provides network statistics tracking for the async network stack.
//! The actual networking is handled by `async_net` module.

use spinning_top::Spinlock;

// ============================================================================
// Statistics (protected by spinlock)
// ============================================================================

struct NetStats {
    bytes_rx: u64,
    bytes_tx: u64,
    connections: u64,
}

impl NetStats {
    const fn new() -> Self {
        Self {
            bytes_rx: 0,
            bytes_tx: 0,
            connections: 0,
        }
    }
}

static NET_STATS: Spinlock<NetStats> = Spinlock::new(NetStats::new());

// ============================================================================
// Statistics API
// ============================================================================

/// Increment the connection counter
pub fn increment_connections() {
    NET_STATS.lock().connections += 1;
}

/// Add to bytes received counter
pub fn add_bytes_rx(bytes: u64) {
    NET_STATS.lock().bytes_rx += bytes;
}

/// Add to bytes transmitted counter
pub fn add_bytes_tx(bytes: u64) {
    NET_STATS.lock().bytes_tx += bytes;
}

/// Get network statistics: (connections, `bytes_rx`, `bytes_tx`)
pub fn get_stats() -> (u64, u64, u64) {
    let s = NET_STATS.lock();
    (s.connections, s.bytes_rx, s.bytes_tx)
}
