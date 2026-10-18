#![cfg_attr(not(test), no_std)]
//! The amd64 interrupt descriptor table.
//!
//! # What this replaces
//!
//! `amd64/src/idt.rs` held the table as `static mut IDT: [Entry; 256]`, reached
//! only through `&raw mut` and written by four `unsafe` blocks, each vouching for
//! the same fact: "single core, interrupts masked, written before `lidt`". That
//! was a promise about *when*, made at every call site, for a table whose only
//! hazard is a torn write the CPU might read.
//!
//! A gate is two 64-bit words. [`Gate`] holds them as [`AtomicU64`]s, so a store
//! is a store the CPU's own descriptor fetch can race with exactly as it always
//! could, but Rust can see it: **filling the table is safe**. What it cannot make
//! safe is the *timing* — installing a gate for a vector that can fire this
//! instant sends the CPU through a half-written descriptor — and that stays the
//! caller's rule, stated on [`Idt::set`].
//!
//! # The one contract
//!
//! > [`Idt::load`] points `IDTR` at a table that lives for the rest of the
//! > program. Every gate in it that can be raised names a handler that returns
//! > with `iretq` (or never returns).
//!
//! The first sentence is the type system's (`&'static self`); the second is the
//! caller's, and nothing here can check it — a gate is an address. `lidt` is the
//! single `unsafe` in the crate. It is not in `akuma-cpu`, which holds
//! instructions that are *safe to execute*: this one re-points where every
//! exception goes.
//!
//! # Why not `forbid(unsafe_code)`
//!
//! The same reason `akuma-gic` cannot: an instruction that configures hardware.
//! The encoding is pure and host-tested ([`encode`]); only `lidt` is not.

use core::sync::atomic::{AtomicU64, Ordering};

/// Vectors in the table.
pub const LEN: usize = 256;

/// The 64-bit code selector `boot.s` far-jumped through. One GDT in this kernel
/// and `boot.s` owns it, so the selector is a constant rather than a read of `cs`.
pub const KERNEL_CS: u16 = 0x08;

/// Present, DPL 0, 64-bit **interrupt** gate. An interrupt gate rather than a trap
/// gate, so `IF` is cleared on entry and a handler cannot be re-entered by a
/// device interrupt.
pub const KERNEL_GATE: u8 = 0x8E;

/// As [`KERNEL_GATE`] with DPL 3.
///
/// A ring-3 `int n` naming this vector reaches the handler instead of raising
/// `#GP`. Only for vectors a program is *meant* to raise itself (`#BP`, from
/// `int3`); DPL governs software `int n` only.
pub const USER_GATE: u8 = 0xEE;

/// The two words of a 16-byte gate descriptor for `handler`.
///
/// ```text
/// low:  offset[15:0] | selector << 16 | ist << 32 | type_attr << 40 | offset[31:16] << 48
/// high: offset[63:32] | reserved(0) << 32
/// ```
///
/// `ist` is always 0 ("use the current stack"): there is no IST in this kernel
/// yet, and a double fault therefore runs on the faulting stack.
#[must_use]
pub const fn encode(handler: u64, type_attr: u8) -> (u64, u64) {
    let low = (handler & 0xFFFF)
        | ((KERNEL_CS as u64) << 16)
        | ((type_attr as u64) << 40)
        | (((handler >> 16) & 0xFFFF) << 48);
    let high = handler >> 32;
    (low, high)
}

/// One 16-byte gate, as the CPU reads it.
#[repr(C)]
pub struct Gate {
    low: AtomicU64,
    high: AtomicU64,
}

impl Gate {
    const fn empty() -> Self {
        Self { low: AtomicU64::new(0), high: AtomicU64::new(0) }
    }

    /// The `(low, high)` words, for tests and diagnostics.
    #[must_use]
    pub fn words(&self) -> (u64, u64) {
        (self.low.load(Ordering::Relaxed), self.high.load(Ordering::Relaxed))
    }

    fn store(&self, (low, high): (u64, u64)) {
        // The word holding the present bit goes last, so a gate that was
        // not-present never reads as present with half an address.
        self.high.store(high, Ordering::Relaxed);
        self.low.store(low, Ordering::Release);
    }
}

/// The table: 256 gates, 16-byte aligned (the CPU wants 8 at minimum and is
/// faster on 16).
#[repr(C, align(16))]
pub struct Idt {
    gates: [Gate; LEN],
}

const _: () = {
    assert!(core::mem::size_of::<Gate>() == 16);
    assert!(core::mem::size_of::<Idt>() == 16 * LEN);
};

impl Idt {
    /// An empty table (every gate not-present). `const`, for `static`.
    #[must_use]
    pub const fn new() -> Self {
        Self { gates: [const { Gate::empty() }; LEN] }
    }

    /// Point `vector` at `handler` with a kernel-only gate.
    ///
    /// **Safe to call, not safe to call at any time**: the CPU reads this table on
    /// every exception, so a gate for a vector that can be raised *right now* is
    /// read mid-update. Install before the vector can fire — the boot path does,
    /// with interrupts masked, and `lapic::init` programs the timer only after
    /// its handler is in place.
    pub fn set(&self, vector: u8, handler: usize) {
        self.gates[usize::from(vector)].store(encode(handler as u64, KERNEL_GATE));
    }

    /// As [`Self::set`] with DPL 3: a ring-3 `int n` may raise it.
    pub fn set_user(&self, vector: u8, handler: usize) {
        self.gates[usize::from(vector)].store(encode(handler as u64, USER_GATE));
    }

    /// Point every vector at `handler`.
    pub fn fill(&self, handler: usize) {
        for g in &self.gates {
            g.store(encode(handler as u64, KERNEL_GATE));
        }
    }

    /// The gate for `vector`.
    #[must_use]
    pub fn gate(&self, vector: u8) -> &Gate {
        &self.gates[usize::from(vector)]
    }

    /// Load this table on the calling core.
    ///
    /// `IDTR` is per-core, so every AP must call this; the table is the same one
    /// and by the time a secondary runs it is complete. Off x86_64 bare metal
    /// this is a no-op, like the rest of the tree's host stubs.
    pub fn load(&'static self) {
        #[cfg(all(target_os = "none", target_arch = "x86_64"))]
        {
            #[repr(C, packed)]
            struct Idtr {
                limit: u16,
                base: u64,
            }
            let idtr = Idtr {
                limit: (core::mem::size_of::<Self>() - 1) as u16,
                base: core::ptr::from_ref(self) as u64,
            };
            // SAFETY: the crate contract. `self` is `'static`, so the base stays
            // valid; `lidt` reads the 10-byte operand and records it.
            unsafe {
                core::arch::asm!(
                    "lidt [{}]",
                    in(reg) &raw const idtr,
                    options(readonly, nostack, preserves_flags)
                );
            }
        }
    }
}

impl Default for Idt {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_splits_the_address_across_the_three_offset_fields() {
        let (low, high) = encode(0xFFFF_8000_1234_5678, KERNEL_GATE);
        assert_eq!(low & 0xFFFF, 0x5678, "offset[15:0]");
        assert_eq!((low >> 16) & 0xFFFF, u64::from(KERNEL_CS), "selector");
        assert_eq!((low >> 32) & 0xFF, 0, "ist");
        assert_eq!((low >> 40) & 0xFF, u64::from(KERNEL_GATE), "type_attr");
        assert_eq!((low >> 48) & 0xFFFF, 0x1234, "offset[31:16]");
        assert_eq!(high, 0xFFFF_8000, "offset[63:32], reserved zero");
    }

    #[test]
    fn the_user_gate_differs_only_in_dpl() {
        let (k, kh) = encode(0x40_1000, KERNEL_GATE);
        let (u, uh) = encode(0x40_1000, USER_GATE);
        assert_eq!(kh, uh);
        assert_eq!(k ^ u, u64::from(KERNEL_GATE ^ USER_GATE) << 40);
        assert_eq!(KERNEL_GATE & 0x80, 0x80, "present");
        assert_eq!((USER_GATE >> 5) & 3, 3, "DPL 3");
        assert_eq!((KERNEL_GATE >> 5) & 3, 0, "DPL 0");
    }

    #[test]
    fn set_fill_and_gate_round_trip() {
        let idt = Idt::new();
        assert_eq!(idt.gate(7).words(), (0, 0), "a new table is all not-present");
        idt.fill(0xFFFF_FFFF_8000_1000);
        assert_eq!(idt.gate(255).words(), encode(0xFFFF_FFFF_8000_1000, KERNEL_GATE));
        idt.set(14, 0xFFFF_FFFF_8000_2000);
        idt.set_user(3, 0xFFFF_FFFF_8000_3000);
        assert_eq!(idt.gate(14).words(), encode(0xFFFF_FFFF_8000_2000, KERNEL_GATE));
        assert_eq!(idt.gate(3).words(), encode(0xFFFF_FFFF_8000_3000, USER_GATE));
        assert_eq!(idt.gate(13).words(), encode(0xFFFF_FFFF_8000_1000, KERNEL_GATE), "neighbours untouched");
    }

    #[test]
    fn the_table_is_four_kib_and_sixteen_aligned() {
        assert_eq!(core::mem::size_of::<Idt>(), 4096);
        assert_eq!(core::mem::align_of::<Idt>(), 16);
    }
}
