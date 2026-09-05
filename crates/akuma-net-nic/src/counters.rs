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
/// Frames handed to the device and accepted by it.
///
/// The counterpart `TX_DROP_COUNT` never had: a drop count alone cannot tell
/// "nothing was sent" from "nothing was asked to be sent", and on a bring-up
/// where the question is whether a NIC moves anything at all, that is the
/// whole question.
pub(crate) static TX_FRAMES_SENT: AtomicUsize = AtomicUsize::new(0);

/// The link, as the driver last read it from the PHY, packed so a reader needs
/// no lock: `0` = never sampled, otherwise bit 0 = up, bits 1..3 = speed
/// (1/2/3 = 10/100/1000, 0 = unknown), bit 3 = full duplex, bit 7 always set so
/// "sampled and down" is distinguishable from "never sampled".
pub(crate) static LINK_STATE: core::sync::atomic::AtomicU8 =
    core::sync::atomic::AtomicU8::new(0);

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

/// Frames the device accepted for transmission since boot.
#[must_use]
pub fn tx_frames_sent() -> usize {
    TX_FRAMES_SENT.load(Ordering::Relaxed)
}

/// The last sampled link state: `None` until a driver has read the PHY at all,
/// then `(up, speed_mbit, full_duplex)` with `speed_mbit == 0` for "up but the
/// rate is not one this driver decodes".
#[must_use]
pub fn link_state() -> Option<(bool, u16, bool)> {
    let raw = LINK_STATE.load(Ordering::Relaxed);
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
///
/// Only the Realtek has a PHY this crate reads, so this is gated on that
/// feature — virtio has no link state to report and `link_state()` correctly
/// answers `None` there rather than inventing an "up".
#[cfg(feature = "rtl8169")]
pub(crate) fn set_link_state(up: bool, speed_mbit: u16, full_duplex: bool) {
    let speed_bits: u8 = match speed_mbit {
        10 => 1,
        100 => 2,
        1000 => 3,
        _ => 0,
    };
    let raw = 0x80
        | u8::from(up)
        | (speed_bits << 1)
        | (u8::from(full_duplex) << 3);
    LINK_STATE.store(raw, Ordering::Relaxed);
}
