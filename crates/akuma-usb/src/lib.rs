// This crate is unsafe-free by design and stays that way: `forbid` (not
// `deny`) so no module can opt back in with a local `allow`.
#![forbid(unsafe_code)]
#![no_std]
//! Pure USB parsing, for the amd64 bare-metal keyboard.
//!
//! # Why this exists
//!
//! The reference machine (HP 500-502nj, `docs/archive/AKUMA_AMD64_ON_HP_500_502NJ.md`)
//! has a USB keyboard and no PS/2 controller — `amd64/src/kbd.rs`'s i8042 driver
//! finds nothing there. Typing at that box on bare metal needs a USB stack, and
//! a USB stack is mostly two things a host can test without a controller:
//!
//! 1. **parsing what the device says about itself** — the standard descriptor
//!    hierarchy ([`descriptor`]) and, for a keyboard, the HID report descriptor
//!    ([`hid`]) — and
//! 2. **turning its reports into keystrokes** — the 8-byte HID boot-keyboard
//!    report to ASCII ([`hid::BootKeyboardDecoder`], [`keymap`]), the direct
//!    analogue of `kbd.rs`'s set-1 scancode tables.
//!
//! The controller-touching half (MMIO, DMA, the schedule) is not here, but the
//! *shape* of what the driver programs into the EHCI controller is: [`ehci`] has
//! the capability/operational register layout, the BIOS-handoff semaphore, the
//! `PORTSC` decode and — the crux for this machine — the split-transaction
//! queue-head and qTD dword layout for a full-speed interrupt endpoint behind a
//! high-speed hub.
//!
//! # The one hardware fact that scopes the driver
//!
//! Measured on the box, 2026-09-05: the ROCCAT Vulcan enumerates as a
//! **full-speed** (12 Mbit/s) device on **EHCI** controller `00:1a.0`, behind
//! the Intel Integrated Rate Matching Hub (a single-TT USB-2.0 hub). The xHCI
//! controller's `XUSB2PRM` (USB-2.0 Port Routing Mask, PCI config `0xD4`) reads
//! `0x00000000` — **no** USB-2.0 port on this board can be routed to xHCI. So
//! the keyboard cannot be moved off EHCI, and the driver is an EHCI driver with
//! transaction-translator split transactions. That is why [`ehci`] is the
//! controller module in this crate and there is no `xhci` one.
//!
//! All the numbers in the `tests/` fixtures came off the running Linux on that
//! machine (`lsusb -v`, `usbhid-dump`, a read-only mmap of the EHCI BAR, and
//! `setpci`), the same way `akuma-net-rtl8169`'s golden register block did.

pub mod descriptor;
pub mod ehci;
pub mod hid;
pub mod keymap;

/// Little-endian field reads that return `None` rather than panic on a short
/// buffer — every parser in this crate is handed bytes that came off a wire or
/// a device and must treat a truncated one as "no answer", not a crash.
pub(crate) mod raw {
    #[inline]
    pub fn u8(b: &[u8], off: usize) -> Option<u8> {
        b.get(off).copied()
    }

    #[inline]
    pub fn u16(b: &[u8], off: usize) -> Option<u16> {
        Some(u16::from_le_bytes([*b.get(off)?, *b.get(off + 1)?]))
    }

    #[inline]
    pub fn u32(b: &[u8], off: usize) -> Option<u32> {
        Some(u32::from_le_bytes([
            *b.get(off)?,
            *b.get(off + 1)?,
            *b.get(off + 2)?,
            *b.get(off + 3)?,
        ]))
    }
}
