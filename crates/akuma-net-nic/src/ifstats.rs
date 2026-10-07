//! Per-interface byte and packet tallies, for `/proc/net/dev` (and so for
//! `busybox ifconfig`'s `RX packets:` / `RX bytes:` lines).
//!
//! Counted in [`crate::loopback::LoopbackAwareDevice`] — the one place every
//! frame the stack sees or sends passes, whichever [`crate::ExternalDevice`]
//! sits behind it (virtio, Realtek, the wifi `Queued` link). Counting here
//! rather than in each driver means a new NIC gets correct numbers for free.
//! `/proc/net/dev` used to print literal zeros, so `ifconfig` could not tell a
//! NIC moving nothing from one moving thousands of frames a second
//! (`amd64/src/net.rs`, `netpoll_daemon`'s doc).
//!
//! Eight relaxed adds' worth of state, no allocation. `tx` counts frames
//! *handed to the device*; a refusal further down shows up as `tx_drop`
//! ([`crate::counters::tx_drop_count`]) where the driver reports one.

use core::sync::atomic::{AtomicU64, Ordering};

static WIRE_RX_BYTES: AtomicU64 = AtomicU64::new(0);
static WIRE_RX_PKTS: AtomicU64 = AtomicU64::new(0);
static WIRE_TX_BYTES: AtomicU64 = AtomicU64::new(0);
static WIRE_TX_PKTS: AtomicU64 = AtomicU64::new(0);
/// Loopback is a ring: every frame sent is later received, so one pair serves
/// both directions of `lo`.
static LO_BYTES: AtomicU64 = AtomicU64::new(0);
static LO_PKTS: AtomicU64 = AtomicU64::new(0);

pub(crate) fn record_wire_rx(len: usize) {
    WIRE_RX_BYTES.fetch_add(len as u64, Ordering::Relaxed);
    WIRE_RX_PKTS.fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn record_wire_tx(len: usize) {
    WIRE_TX_BYTES.fetch_add(len as u64, Ordering::Relaxed);
    WIRE_TX_PKTS.fetch_add(1, Ordering::Relaxed);
}

/// A frame pushed onto the loopback ring (it is received later, so counting at
/// the push keeps `lo`'s RX and TX equal, as on Linux).
pub(crate) fn record_loopback(len: usize) {
    LO_BYTES.fetch_add(len as u64, Ordering::Relaxed);
    LO_PKTS.fetch_add(1, Ordering::Relaxed);
}

/// One interface's counters, in `/proc/net/dev` terms.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    pub rx_bytes: u64,
    pub rx_packets: u64,
    pub rx_drop: u64,
    pub tx_bytes: u64,
    pub tx_packets: u64,
    pub tx_drop: u64,
}

/// `(lo, wire)` — the wire is `eth0`. `rx_drop` is frames refused on receive
/// (127/8 martians); `tx_drop` is the device's own refusals.
#[must_use]
pub fn snapshot() -> (Counts, Counts) {
    let lo = Counts {
        rx_bytes: LO_BYTES.load(Ordering::Relaxed),
        rx_packets: LO_PKTS.load(Ordering::Relaxed),
        tx_bytes: LO_BYTES.load(Ordering::Relaxed),
        tx_packets: LO_PKTS.load(Ordering::Relaxed),
        rx_drop: crate::loopback::loopback_drop_count() as u64,
        tx_drop: 0,
    };
    let wire = Counts {
        rx_bytes: WIRE_RX_BYTES.load(Ordering::Relaxed),
        rx_packets: WIRE_RX_PKTS.load(Ordering::Relaxed),
        rx_drop: crate::loopback::martian_drop_count() as u64,
        tx_bytes: WIRE_TX_BYTES.load(Ordering::Relaxed),
        tx_packets: WIRE_TX_PKTS.load(Ordering::Relaxed),
        tx_drop: crate::counters::tx_drop_count() as u64,
    };
    (lo, wire)
}
