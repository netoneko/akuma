#![no_std]
//! The PL011 console UART — two registers, three byte-level operations.
//!
//! # Why this is a crate
//!
//! It exists to hold **one `unsafe` block**: the statement that
//! [`akuma_primitives::addr::DEV_UART_VA`] is a mapped PL011 register window.
//! That is the whole content. `src/console.rs` had three such blocks (one per
//! register access) before [`akuma_primitives::mmio::MmioReg`] collapsed them to
//! one, and this crate is where that one lives so the console file itself holds
//! none.
//!
//! # Why not somewhere that already exists
//!
//! - **`akuma-cpu`** is "every AArch64 *instruction* that is safe to execute" —
//!   barriers, cache/TLB maintenance, `DAIF`, read-only system registers. MMIO is
//!   not an instruction category; it is a memory access to a mapped window.
//! - **`akuma-mmu`** builds the device window and could vouch for windows it
//!   maps — but **it does not map this one.** The boot assembly installs the
//!   UART's L3 entry before Rust runs, which is exactly why `src/boot.rs`'s
//!   `UART_L3_SLOT` exists and why its doc says the boot assembly and
//!   `akuma_exec::mmu` "must agree on it". The console prints from the first
//!   Rust instruction; a mapping `akuma-mmu` had to establish first would be too
//!   late.
//! - A general `device_reg(va)` helper in either crate would be **worse than what
//!   it replaced**: it can vouch that a VA sits in the mapped window, but not
//!   that the machine has the device behind it. Handing out a working-looking
//!   register for an absent device is how `ramfb::init` took `EC=0x25` with
//!   `FAR=0x8000012008` on the first Firecracker boot.
//!
//! # What deliberately stayed in `src/console.rs`
//!
//! Everything that is *policy* rather than *device*: the cross-core `Spinlock`
//! and its per-core reentrancy guard, the `MULTICORE` runtime gate, IRQ masking,
//! and all formatting. Two concrete reasons, not taste:
//!
//! 1. That code is gated on `cfg(kernel_console_lock)`, and a crate only sees a
//!    cfg its **own** `build.rs` emits. Moving it here would mean a build script
//!    and a forwarded feature, or a silently dead gate — `akuma-exec` shipped a
//!    family of dormant `kernel_profile_extreme` gates exactly that way.
//! 2. It needs `akuma_exec::bkl::current_core_id`, which would drag the 23k-line
//!    execution crate under the console.
//!
//! So this crate has **no cfgs, no `build.rs`, and one dependency**. Writing a
//! byte is the same instruction it was before the move.

use akuma_primitives::addr::DEV_UART_VA;
use akuma_primitives::mmio::MmioReg;

/// Data register offset. Write to transmit, read to receive.
const DR_OFFSET: usize = 0x00;
/// Flag register offset.
const FR_OFFSET: usize = 0x18;

/// Receive FIFO empty.
const RXFE: u32 = 1 << 4;

/// The PL011's two registers.
struct Uart {
    dr: MmioReg<u8>,
    fr: MmioReg<u32>,
}

/// The console UART at its remapped VA (physical `0x0900_0000` via L0[1]).
///
/// `const` rather than `static`, which is not a style choice: `MmioReg` is
/// deliberately `!Sync`, so parking one in a `static` obliges the driver to write
/// `unsafe impl Sync` and say what serialises access. A `const` is materialised
/// at each use instead of having one address, so it never raises the question —
/// and the honest answer would be awkward, because by default **nothing**
/// serialises this. That is intentional: the console must work from a panic
/// handler, from an IRQ, and from a core holding no locks, so cross-core
/// serialisation is opt-in (`CONSOLE_LOCK=1`, in `src/console.rs`) and concurrent
/// writers merely interleave bytes at the data register. Interleaved bytes are a
/// legibility problem, not memory unsafety — the register is device memory, not
/// Rust-visible storage.
///
/// SAFETY: `DEV_UART_VA` is the kernel's fixed device mapping of the PL011. The
/// boot assembly installs its L3 entry before any Rust runs (`src/boot.rs`,
/// `UART_L3_SLOT`) and it is never unmapped, so the window is live for every call
/// below, including the earliest possible `print!`. The two offsets and widths
/// are the ones the PL011 spec defines.
const UART: Uart = unsafe {
    Uart {
        dr: MmioReg::new(DEV_UART_VA + DR_OFFSET),
        fr: MmioReg::new(DEV_UART_VA + FR_OFFSET),
    }
};

/// Write one byte to the data register.
///
/// Does **not** wait for space in the transmit FIFO. That is inherited
/// behaviour, not an oversight: the console's value is that it still emits when
/// the kernel is failing, and a `TXFF` spin is a place to hang while trying to
/// report why. A full FIFO drops the byte.
#[inline]
pub fn write_byte(byte: u8) {
    UART.write(byte);
}

/// Read one byte from the data register.
///
/// Only meaningful when [`has_data`] is true; the PL011 returns stale contents
/// otherwise.
#[inline]
#[must_use]
pub fn read_byte() -> u8 {
    UART.read()
}

/// Is there a byte waiting in the receive FIFO?
#[inline]
#[must_use]
pub fn has_data() -> bool {
    (UART.flags() & RXFE) == 0
}

impl Uart {
    #[inline]
    fn write(&self, byte: u8) {
        self.dr.write(byte);
    }

    #[inline]
    fn read(&self) -> u8 {
        self.dr.read()
    }

    #[inline]
    fn flags(&self) -> u32 {
        self.fr.read()
    }
}
