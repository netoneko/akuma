//! Realtek RTL8169/8168/8111 gigabit Ethernet, as logic rather than as a poke
//! sequence.
//!
//! The chip is a register block plus two descriptor rings in DMA memory. This
//! crate owns **what to write, in what order, and what the answers mean**; it
//! owns no memory-mapped pointer, allocates nothing, and cannot touch a device.
//! Both of those live behind two traits the consumer implements: [`Regs`] for
//! the register window and [`Rings`] for descriptor and buffer memory.
//!
//! That split is what makes the driver testable. `cargo test` runs the real
//! bring-up sequence, the real transmit path and the real receive path against
//! [`model::FakeChip`] — a simulated RTL8168g that enforces the ownership
//! protocol and *panics on a violation* — so the hardware-specific decisions
//! are decided by tests on a laptop rather than by a boot on the one machine
//! that has the part.
//!
//! `#![forbid(unsafe_code)]`: every hardware access is a trait call, so there is
//! nothing here to make unsafe. The `unsafe` lives in whatever implements
//! [`Regs`] — a `read_volatile` on a device-mapped BAR — and in whatever
//! implements [`Rings`], which is the one thing in the system that knows a
//! virtual address and its physical counterpart.
//!
//! # Provenance
//!
//! Written against a live RTL8168g (XID `0x4c0`, PCI `10ec:8168` rev 0c). Every
//! register offset, width and reset value in [`regs`] was read back off that
//! chip before it was written down, and `tests/golden_registers.rs` holds the
//! full 256-byte dump of it in a working state — link up, 1 Gbps, full duplex,
//! receiving traffic — as a fixture the map is asserted against. Chip models
//! this crate has not run on are [`chip::Model::Unknown`] on purpose.
//!
//! # What is deliberately not here
//!
//! * **No interrupt handler.** [`Nic::take_interrupts`] reads and acknowledges
//!   `ISR`; routing the line and deciding when to call it are the consumer's.
//! * **No allocation and no buffer ownership.** The rings live in memory the
//!   consumer allocated, mapped and can name physically.
//! * **No PHY firmware.** Realtek ships per-part patch blobs; the chip
//!   negotiates and passes traffic without them, and loading one is a
//!   separate concern from driving the MAC.
//! * **No multicast filter.** The hash table is programmed all-ones or all-zero;
//!   a real filter belongs above this layer.

#![no_std]
#![forbid(unsafe_code)]

pub mod chip;
pub mod desc;
pub mod driver;
pub mod link;
pub mod mdio;
pub mod regs;
pub mod ring;

#[cfg(any(test, feature = "model"))]
pub mod model;

pub use chip::Model;
pub use desc::Desc;
pub use driver::Nic;
pub use link::{LinkState, Speed};

/// An Ethernet station address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MacAddr(pub [u8; 6]);

impl MacAddr {
    /// Whether this address is all zeroes — what an uninitialised or
    /// unpowered chip reads back, and never a valid station address.
    #[must_use]
    pub const fn is_zero(&self) -> bool {
        let mut i = 0;
        while i < 6 {
            if self.0[i] != 0 {
                return false;
            }
            i += 1;
        }
        true
    }

    /// Whether the multicast bit is set. A station address never has it, so
    /// finding it set means the read went wrong.
    #[must_use]
    pub const fn is_multicast(&self) -> bool {
        self.0[0] & 0x01 != 0
    }

    /// Whether every byte is `0xFF` — what a read off the end of a mapping
    /// returns, and the broadcast address, which is never a station address.
    #[must_use]
    pub const fn is_broadcast(&self) -> bool {
        let mut i = 0;
        while i < 6 {
            if self.0[i] != 0xFF {
                return false;
            }
            i += 1;
        }
        true
    }

    /// Whether this looks like a real station address the chip loaded from its
    /// EEPROM, as opposed to a failed read.
    #[must_use]
    pub const fn is_plausible(&self) -> bool {
        !self.is_zero() && !self.is_multicast() && !self.is_broadcast()
    }
}

/// The register window.
///
/// Reads take `&mut self` because several of these registers change when read —
/// `ISR` is write-1-to-clear but some counters are read-to-clear, and `PHYAR`
/// is a request/response port. Taking a shared reference would let two callers
/// interleave halves of one transaction.
///
/// Offsets are byte offsets from the start of the register block, and the width
/// of the access is part of the register's definition: see [`regs`].
pub trait Regs {
    /// Read one byte.
    fn r8(&mut self, off: u16) -> u8;
    /// Read one halfword.
    fn r16(&mut self, off: u16) -> u16;
    /// Read one word.
    fn r32(&mut self, off: u16) -> u32;
    /// Write one byte.
    fn w8(&mut self, off: u16, val: u8);
    /// Write one halfword.
    fn w16(&mut self, off: u16, val: u16);
    /// Write one word.
    fn w32(&mut self, off: u16, val: u32);
    /// Busy-wait. Used only in bounded reset and MDIO polls.
    fn delay_us(&mut self, us: u32);
}

/// Descriptor rings and frame buffers, in memory the device can reach.
///
/// The implementor owns the mapping and is the only thing that knows both the
/// virtual and physical address of anything. Every address this trait returns
/// is a **device** address: a physical address on bare metal, an IOVA where an
/// IOMMU is translating.
///
/// # The contract
///
/// Both ring bases must be **256-byte aligned** — the chip ignores the low bits
/// rather than faulting, so a misaligned ring silently points somewhere else.
/// Ring lengths must be powers of two. Receive buffers must be at least
/// [`ring::RX_BUF_SIZE`] bytes, since this driver refuses split frames.
pub trait Rings {
    /// Number of receive descriptors. A power of two.
    fn rx_ring_len(&self) -> usize;
    /// Number of transmit descriptors. A power of two.
    fn tx_ring_len(&self) -> usize;
    /// Device address of the receive descriptor array. 256-byte aligned.
    fn rx_ring_phys(&self) -> u64;
    /// Device address of the transmit descriptor array. 256-byte aligned.
    fn tx_ring_phys(&self) -> u64;

    /// Read receive descriptor `i`.
    fn rx_desc(&self, i: usize) -> Desc;
    /// Write receive descriptor `i`.
    fn set_rx_desc(&mut self, i: usize, d: Desc);
    /// Read transmit descriptor `i`.
    fn tx_desc(&self, i: usize) -> Desc;
    /// Write transmit descriptor `i`.
    fn set_tx_desc(&mut self, i: usize, d: Desc);

    /// Device address of receive buffer `i`.
    fn rx_buf_phys(&self, i: usize) -> u64;
    /// Device address of transmit buffer `i`.
    fn tx_buf_phys(&self, i: usize) -> u64;

    /// Copy `len` bytes out of receive buffer `i` into `dst`.
    ///
    /// Returns how many bytes were copied, which is `min(len, dst.len())`.
    fn rx_buf_read(&self, i: usize, len: usize, dst: &mut [u8]) -> usize;
    /// Copy a frame into transmit buffer `i`. Returns bytes copied.
    fn tx_buf_write(&mut self, i: usize, src: &[u8]) -> usize;
    /// Zero transmit buffer `i` from `from` up to but not including `to`.
    ///
    /// This exists so short frames can be padded to the Ethernet minimum
    /// **without leaking**. Padding by simply telling the chip a larger length
    /// puts whatever the previous frame left in those bytes onto the wire — a
    /// real information leak, and an invisible one, since the frame is valid
    /// and the receiver ignores the tail.
    fn tx_buf_zero(&mut self, i: usize, from: usize, to: usize);
}

/// What can go wrong bringing the chip up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// The soft reset never completed. The chip is wedged or absent.
    ResetTimeout,
    /// A ring base address is not 256-byte aligned.
    RingMisaligned {
        /// The offending address.
        phys: u64,
    },
    /// A ring length is not a power of two, or is zero.
    RingLength {
        /// The offending length.
        len: usize,
    },
    /// The station address read back as something that cannot be one.
    ///
    /// Carries what was read, because the value distinguishes the causes: all
    /// zeroes is an unpowered or unmapped chip, all ones is a read that fell
    /// off the end of the mapping.
    ImplausibleMac(MacAddr),
    /// The chip is not one this driver has been run against.
    ///
    /// Advisory: [`Nic::probe_unverified`] proceeds anyway.
    UnknownChip(chip::Xid),
}

/// Why a transmit could not be queued.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxError {
    /// The ring is full. Reclaim completions and retry.
    Full,
    /// The frame is longer than [`ring::MAX_FRAME`] or shorter than a
    /// zero-length one can be padded to.
    BadLength {
        /// The length that was refused.
        len: usize,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three implausible reads, and what each one means in the field.
    #[test]
    fn a_failed_read_is_not_mistaken_for_an_address() {
        // An unpowered chip, or a BAR that is mapped but not backed.
        assert!(!MacAddr([0; 6]).is_plausible());
        // A read that fell off the end of the mapping.
        assert!(!MacAddr([0xFF; 6]).is_plausible());
        // A station address never has the multicast bit; finding it set means
        // the bytes came from somewhere else.
        assert!(!MacAddr([0x01, 0x02, 0x03, 0x04, 0x05, 0x06]).is_plausible());
    }

    #[test]
    fn the_reference_chips_address_is_plausible() {
        let mac = MacAddr([0x60, 0x02, 0x92, 0x61, 0x4e, 0x73]);
        assert!(mac.is_plausible());
        assert!(!mac.is_zero());
        assert!(!mac.is_multicast());
        assert!(!mac.is_broadcast());
    }

    /// The broadcast check has to be exact rather than a prefix test: an
    /// address that differs from broadcast only in its last bytes is a legal
    /// unicast address and must survive.
    ///
    /// The first byte is `0xFE`, not `0xFF`, and that is not a detail — every
    /// address starting `0xFF` has the multicast bit set and is refused for
    /// that reason instead, which would make this test pass while testing
    /// nothing.
    #[test]
    fn nearly_broadcast_is_still_an_address() {
        let almost = MacAddr([0xFE, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]);
        assert!(!almost.is_multicast(), "0xFE is a unicast prefix");
        assert!(!almost.is_broadcast());
        assert!(almost.is_plausible());
    }
}
