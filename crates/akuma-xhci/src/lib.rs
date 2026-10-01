// Unsafe-free by design: `forbid` (not `deny`) so no module can opt back in.
#![forbid(unsafe_code)]
#![no_std]
//! Pure xHCI layout and bit math, for the amd64 bare-metal USB disk.
//!
//! # Why this exists
//!
//! The HP 500-502nj ("the trashcan", `docs/archive/AKUMA_SELF_HEALING_PORT.md`)
//! boots from a 512 MiB ext2 image GRUB loads whole into RAM — it does not
//! persist. The only spare disk is trapped in a USB-to-SATA enclosure (the drive
//! is screwed into a caddy that will not open), so persistence means speaking
//! USB. On that box `XUSB2PRM = 0`, so xHCI only ever sees SuperSpeed devices —
//! which is exactly what the enclosure is, and it is the *only* thing on the
//! xHCI bus. One SuperSpeed device, no hub, so the driver is a **minimal** xHCI:
//! one slot, a command ring, an event ring, a transfer ring per endpoint,
//! control + two bulk endpoints, and **no split transactions** (the thing that
//! made [`akuma_usb::ehci`](../akuma_usb/ehci/index.html) hard).
//!
//! # What is here and what is not
//!
//! Here: the register offsets ([`regs`]), the TRB encode/decode and the ring
//! cycle-bit bookkeeping ([`trb`]), the device-context builders ([`context`]),
//! and the extended-capability + `USBLEGSUP` walk ([`xcap`]). All of it is pure
//! `[u32; N]` / bit math with `parse(&[u8])` decoders, testable on a host with
//! no controller — the numbers in `tests/` came off the running Linux on the box
//! (a read-only mmap of the BAR, `setpci`, `lsusb -v`), the same way
//! `akuma-usb`'s and `akuma-net-rtl8169`'s fixtures did.
//!
//! Not here: the MMIO on the mapped BAR, the `.bss` DMA rings/contexts/scratchpad
//! with known physical addresses, the reset sequencing and the event-ring drain
//! loop. That is `amd64/src/xhci.rs`, behind one stated DMA contract, the way
//! `akuma-net-nic`'s `rtl8169.rs` is the glue over `akuma-net-rtl8169`.
//!
//! Descriptor parsing (device/config/interface/endpoint) is **not duplicated** —
//! `akuma_usb::descriptor` is transport-independent and already host-tested; the
//! glue consumes it directly.

pub mod context;
pub mod device;
pub mod recovery;
pub mod regs;
pub mod trb;
pub mod xcap;

/// Little-endian field reads that return `None` on a short buffer rather than
/// panic — every decoder here is handed bytes off a device and must treat a
/// truncated read as "no answer", not a crash. Mirrors `akuma_usb::raw`.
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

/// USB device speed, as the value xHCI records in a Slot Context's Speed field
/// (xHCI §6.2.2, Table 6-8 for this controller's PORTSC PLS/Speed encoding).
///
/// The port-status `Port Speed` field (`PORTSC` bits 13:10) uses the same
/// numbering on the Intel parts this targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Speed {
    Full = 1,
    Low = 2,
    High = 3,
    Super = 4,
}

impl Speed {
    /// Decode a `PORTSC` / Slot-Context speed field. `None` for the reserved
    /// values (0, and 5..15 on a controller that does not implement them).
    #[must_use]
    pub fn from_field(v: u32) -> Option<Self> {
        match v & 0xf {
            1 => Some(Self::Full),
            2 => Some(Self::Low),
            3 => Some(Self::High),
            4 => Some(Self::Super),
            _ => None,
        }
    }

    /// The default `Max Packet Size` for endpoint 0 at this speed (USB 2.0
    /// §5.5.3 / USB 3.2 §8.12.6.2). SuperSpeed is fixed at 512; the others are
    /// the spec maximum, refined from the device descriptor's `bMaxPacketSize0`
    /// once it has been read.
    #[must_use]
    pub fn default_ep0_max_packet(self) -> u16 {
        match self {
            Self::Low => 8,
            Self::Full => 64,
            Self::High => 64,
            Self::Super => 512,
        }
    }
}

/// The xHCI **Interval** field for an interrupt endpoint (xHCI §6.2.3.6): the
/// endpoint is polled every `2^value` microframes of 125 µs.
///
/// It is not the descriptor's `bInterval`, and the conversion is where an
/// interrupt endpoint most often goes wrong silently — a device that answers
/// 125 µs polls works and burns bandwidth, one given a 1-second interval works
/// and feels broken.
///
/// * **Low/Full speed**: `bInterval` is milliseconds (frames), so the period is
///   `bInterval * 8` microframes, rounded **down** to a power of two and
///   clamped to 3..=10 (1 ms .. 128 ms), as Linux does.
/// * **High/Super speed**: `bInterval` is already an exponent, `2^(bInterval-1)`
///   microframes, so the field is `bInterval - 1`, clamped to 0..=15.
///
/// A `bInterval` of 0 (illegal for interrupt) is treated as 1.
#[must_use]
pub fn interrupt_interval(speed: Speed, b_interval: u8) -> u8 {
    let b = b_interval.max(1);
    match speed {
        Speed::Low | Speed::Full => {
            let micro = u32::from(b) * 8;
            let exp = 31 - micro.leading_zeros();
            exp.clamp(3, 10) as u8
        }
        Speed::High | Speed::Super => (b - 1).min(15),
    }
}

#[cfg(test)]
mod interval_tests {
    use super::*;

    #[test]
    fn full_speed_one_ms_is_eight_microframes() {
        assert_eq!(interrupt_interval(Speed::Full, 1), 3);
    }

    #[test]
    fn full_speed_rounds_down_to_a_power_of_two() {
        // 10 ms = 80 microframes -> 64 (2^6), not 128.
        assert_eq!(interrupt_interval(Speed::Full, 10), 6);
        assert_eq!(interrupt_interval(Speed::Full, 8), 6);
        assert_eq!(interrupt_interval(Speed::Full, 16), 7);
    }

    #[test]
    fn full_speed_is_clamped_to_the_spec_range() {
        assert_eq!(interrupt_interval(Speed::Full, 255), 10);
        assert_eq!(interrupt_interval(Speed::Low, 255), 10);
        assert_eq!(interrupt_interval(Speed::Full, 0), 3, "0 is illegal; treated as 1");
    }

    #[test]
    fn high_speed_is_already_an_exponent() {
        assert_eq!(interrupt_interval(Speed::High, 1), 0);
        assert_eq!(interrupt_interval(Speed::High, 4), 3);
        assert_eq!(interrupt_interval(Speed::High, 16), 15);
        assert_eq!(interrupt_interval(Speed::Super, 255), 15);
    }

    #[test]
    fn a_full_speed_keyboard_polls_at_least_every_128_ms() {
        for b in 1..=255u8 {
            let e = interrupt_interval(Speed::Full, b);
            assert!((3..=10).contains(&e), "bInterval {b} -> {e}");
        }
    }
}
