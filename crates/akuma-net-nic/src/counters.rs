//! Device-level RX/TX tallies.
//!
//! Separate from [`crate::nicstat`]: those are latency histograms behind the
//! `net-profile` feature and cost two CNTVCT reads per packet, whereas these are
//! four relaxed adds that ship in every build. They came down from the stack
//! with the device on 2026-08-30 — `posted` climbing while `received` stays 0
//! means the device has buffers and is not filling them, which is a question
//! about the NIC and not about smoltcp.

use core::sync::atomic::{AtomicUsize, Ordering};

pub(crate) static RX_BUFFERS_POSTED: AtomicUsize = AtomicUsize::new(0);
pub(crate) static RX_BEGIN_FAILURES: AtomicUsize = AtomicUsize::new(0);
pub(crate) static RX_FRAMES_RECEIVED: AtomicUsize = AtomicUsize::new(0);
pub(crate) static TX_DROP_COUNT: AtomicUsize = AtomicUsize::new(0);

/// `(buffers_posted, begin_failures, frames_received)` since boot.
#[must_use]
pub fn rx_counters() -> (usize, usize, usize) {
    (
        RX_BUFFERS_POSTED.load(Ordering::Relaxed),
        RX_BEGIN_FAILURES.load(Ordering::Relaxed),
        RX_FRAMES_RECEIVED.load(Ordering::Relaxed),
    )
}

/// Frames the device refused, or that could not get a ring slot.
#[must_use]
pub fn tx_drop_count() -> usize {
    TX_DROP_COUNT.load(Ordering::Relaxed)
}
