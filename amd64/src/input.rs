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

use crate::{kbd, serial};

/// Take one byte from any console input device.
#[must_use]
pub fn getb() -> Option<u8> {
    serial::getb().or_else(kbd::getb).or_else(crate::xhci::keyboard_getb)
}

/// Is a byte waiting on any console input device? Non-destructive.
#[must_use]
pub fn has_byte() -> bool {
    serial::has_byte() || kbd::has_byte() || crate::xhci::keyboard_has_byte()
}
