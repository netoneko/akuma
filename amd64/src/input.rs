//! Console input: whichever of the machine's input devices has a byte.
//!
//! Three sources, all polled, all able to be absent: the 16550 (`serial`), the
//! i8042 keyboard (`kbd`, the firmware's PS/2 emulation) and the native USB
//! keyboard (`xhci::keyboard_getb`, which exists because that emulation ends when
//! the xHCI driver takes the controller). A VMM guest has the first and not the second;
//! the bare-metal reference machine has the second (through firmware's USB
//! legacy emulation) and not the first. `fd.rs`'s console read and `poll(2)`
//! ask here rather than picking one, so a shell reads from whatever the user
//! is typing on. Output is unaffected: `serial::puts` already mirrors to the
//! framebuffer.

use core::sync::atomic::{AtomicU64, Ordering};

use crate::{kbd, serial};

/// `uptime_us` of the last console input byte — a keystroke, a serial byte, a
/// USB HID report — which `/proc/power`'s `idle_ms=` fact and the power daemon
/// (`powersaved`) read. Written only when a byte is actually seen, so an idle
/// box pays nothing and a keystroke pays one store.
static LAST_INPUT_US: AtomicU64 = AtomicU64::new(0);

/// Record console input activity. `has_byte` counts too, not just [`getb`]: a
/// key waiting on a dead or busy reader is still a person at the machine, and
/// the screen must come back for them either way.
fn note_input() {
    LAST_INPUT_US.store(crate::net::uptime_us(), Ordering::Release);
}

/// Milliseconds since the last console input byte. A boot that has seen none
/// reports the whole uptime (saturating subtraction from the zero epoch), so
/// an untouched machine dims — which is exactly the point of the fact.
#[must_use]
pub fn idle_ms() -> u64 {
    crate::net::uptime_us().saturating_sub(LAST_INPUT_US.load(Ordering::Acquire)) / 1000
}

/// Take one byte from any console input device.
#[must_use]
pub fn getb() -> Option<u8> {
    let b = serial::getb().or_else(kbd::getb).or_else(crate::xhci::keyboard_getb);
    if b.is_some() {
        note_input();
    }
    b
}

/// Is a byte waiting on any console input device? Non-destructive.
#[must_use]
pub fn has_byte() -> bool {
    let any = serial::has_byte() || kbd::has_byte() || crate::xhci::keyboard_has_byte();
    if any {
        note_input();
    }
    any
}
