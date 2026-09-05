//! Device-level RX/TX tallies.
//!
//! Separate from [`crate::nicstat`]: those are latency histograms behind the
//! `net-profile` feature and cost two CNTVCT reads per packet, whereas these are
//! four relaxed adds that ship in every build. They came down from the stack
//! with the device on 2026-08-30 — `posted` climbing while `received` stays 0
//! means the device has buffers and is not filling them, which is a question
//! about the NIC and not about smoltcp.

use core::sync::atomic::{AtomicUsize, Ordering};

/// Every device tally, in **one `#[repr(C)] struct`** with a known value at
/// each end.
///
/// The layout is the point. These were eight separate `static`s, and Rust makes
/// no promise about where the linker puts those relative to one another — so a
/// canary declared "before" and "after" them in source order might sit anywhere,
/// and a canary that is not adjacent proves nothing. Measured on the HP box:
/// `rx`, `drop`, `rxfail` and the ISR accumulator were each overwritten with
/// what look like physical addresses while both canaries read intact, which is
/// either "the write was surgical" or "the canaries were somewhere else
/// entirely", and there was no way to tell which.
///
/// `#[repr(C)]` fixes the order and the padding, so `lo` and `hi` genuinely
/// bracket the block. Now an intact pair means the write landed *inside* the
/// counters and nowhere else — which for a DMA scribble would be a remarkable
/// coincidence, and for a wild pointer is a real clue.
#[repr(C)]
pub(crate) struct CounterBlock {
    pub(crate) canary_lo: AtomicUsize,
    pub(crate) rx_buffers_posted: AtomicUsize,
    pub(crate) rx_begin_failures: AtomicUsize,
    pub(crate) rx_frames_received: AtomicUsize,
    pub(crate) tx_drop_count: AtomicUsize,
    pub(crate) tx_frames_sent: AtomicUsize,
    pub(crate) rx_ring_dry: AtomicUsize,
    /// How many times the receiver has been restarted after stalling silently.
    /// A number that keeps climbing is the machine staying reachable *despite*
    /// a bug, not evidence there isn't one.
    pub(crate) rx_kicks: AtomicUsize,
    pub(crate) rx_isr_seen: core::sync::atomic::AtomicU32,
    /// The link, as the driver last read it from the PHY, packed so a reader
    /// needs no lock: bit 0 up, bits 1..3 speed (1/2/3 = 10/100/1000), bit 3
    /// full duplex, bit 7 always set so "sampled and down" is distinguishable
    /// from "never sampled".
    pub(crate) link_state: core::sync::atomic::AtomicU8,
    pub(crate) canary_hi: AtomicUsize,
}

/// The value both canaries must always hold. Not zero and not a small integer:
/// a pattern that cannot be mistaken for a plausible count or a null write.
pub(crate) const CANARY_VALUE: usize = 0x5EED_1234_5EED_1234;

pub(crate) static C: CounterBlock = CounterBlock {
    canary_lo: AtomicUsize::new(CANARY_VALUE),
    rx_buffers_posted: AtomicUsize::new(0),
    rx_begin_failures: AtomicUsize::new(0),
    rx_frames_received: AtomicUsize::new(0),
    tx_drop_count: AtomicUsize::new(0),
    tx_frames_sent: AtomicUsize::new(0),
    rx_ring_dry: AtomicUsize::new(0),
    rx_kicks: AtomicUsize::new(0),
    rx_isr_seen: core::sync::atomic::AtomicU32::new(0),
    link_state: core::sync::atomic::AtomicU8::new(0),
    canary_hi: AtomicUsize::new(CANARY_VALUE),
};

/// `(low canary intact, high canary intact)` — see [`CounterBlock`].
#[must_use]
pub fn canaries_intact() -> (bool, bool) {
    (
        C.canary_lo.load(Ordering::Relaxed) == CANARY_VALUE,
        C.canary_hi.load(Ordering::Relaxed) == CANARY_VALUE,
    )
}

/// Where the counter block lives, so a boot can print it next to the DMA
/// addresses and the two can be compared by eye.
#[must_use]
pub fn counter_block_addr() -> usize {
    (&raw const C) as usize
}

/// `(every ISR bit seen so far, times the receive ring ran dry, receiver
/// restarts)`.
#[must_use]
pub fn isr_history() -> (u32, usize, usize) {
    (
        C.rx_isr_seen.load(Ordering::Relaxed),
        C.rx_ring_dry.load(Ordering::Relaxed),
        C.rx_kicks.load(Ordering::Relaxed),
    )
}

/// The last sampled link state: `None` until a driver has read the PHY at all,
/// then `(up, speed_mbit, full_duplex)` with `speed_mbit == 0` for "up but the
/// rate is not one this driver decodes".
#[must_use]
pub fn link_state() -> Option<(bool, u16, bool)> {
    let raw = C.link_state.load(Ordering::Relaxed);
    if raw & 0x80 == 0 {
        return None;
    }
    let speed = match (raw >> 1) & 0x3 {
        1 => 10,
        2 => 100,
        3 => 1000,
        _ => 0,
    };
    Some((raw & 1 != 0, speed, raw & 0x8 != 0))
}

/// Record a PHY reading. Called by the driver glue that owns the chip.
#[cfg(feature = "rtl8169")]
pub(crate) fn set_link_state(up: bool, speed_mbit: u16, full_duplex: bool) {
    let speed_bits: u8 = match speed_mbit {
        10 => 1,
        100 => 2,
        1000 => 3,
        _ => 0,
    };
    let raw = 0x80 | u8::from(up) | (speed_bits << 1) | (u8::from(full_duplex) << 3);
    C.link_state.store(raw, Ordering::Relaxed);
}

/// Frames the device accepted for transmission since boot.
#[must_use]
pub fn tx_frames_sent() -> usize {
    C.tx_frames_sent.load(Ordering::Relaxed)
}

/// Frames the device refused, or that could not get a ring slot.
#[must_use]
pub fn tx_drop_count() -> usize {
    C.tx_drop_count.load(Ordering::Relaxed)
}

/// `(buffers_posted, begin_failures, frames_received)` since boot.
#[must_use]
pub fn rx_counters() -> (usize, usize, usize) {
    (
        C.rx_buffers_posted.load(Ordering::Relaxed),
        C.rx_begin_failures.load(Ordering::Relaxed),
        C.rx_frames_received.load(Ordering::Relaxed),
    )
}
