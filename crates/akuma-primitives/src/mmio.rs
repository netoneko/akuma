//! Typed access to memory-mapped device registers.
//!
//! # Why this exists
//!
//! Device drivers here spelled every register access as a raw
//! `read_volatile((base + OFFSET) as *const u32)`, which put an `unsafe` block on
//! *each access* — 30 of them in `akuma-virtio`'s rng driver alone, all vouching
//! for the same fact: that `base` is a mapped device window. That is `unsafe`
//! marking the wrong thing. The property that needs a human's word is **"this
//! address is a device register"**, and it is true once per device, not once per
//! read.
//!
//! [`MmioReg`] moves the obligation to construction. After a caller has vouched
//! for an address, reads and writes of that register are safe operations, because
//! a volatile access to a real, mapped register cannot violate memory safety on
//! its own.
//!
//! See `docs/archive/TRIM_FAT_MMIO_NEWTYPE.md` for the conversion plan and the
//! sites deliberately left raw.
//!
//! # What this is not
//!
//! Not an abstraction over device protocol. It carries no ordering, no
//! endianness, and no named registers: fences stay at the call site where the
//! driver put them, `to_be()` stays at the call site, and a read whose only
//! purpose is the protocol step still has to be written as a read. Converting a
//! driver to `MmioReg` must not change a single instruction it emits.

/// A memory-mapped device register of width `T`.
///
/// `unsafe` lives at construction: the caller vouches that the address is a
/// device register of exactly this width, mapped (Device-nGnRnE) for the
/// kernel's lifetime. Afterwards [`read`](Self::read) and [`write`](Self::write)
/// are safe.
///
/// The raw-pointer field makes this `!Send`/`!Sync` by default, which is
/// deliberate — a driver that wants to park its registers in a `static` has to
/// write the `unsafe impl` itself and say what serialises the accesses.
#[derive(Clone, Copy, Debug)]
pub struct MmioReg<T>(*mut T);

impl<T: Copy> MmioReg<T> {
    /// Name the register at `addr`.
    ///
    /// `const` so drivers with fixed register addresses (`fw_cfg`, the PL011
    /// console) can build theirs in a `const` item and stay free of any
    /// init-order dependency.
    ///
    /// # Safety
    /// `addr` is a device register of width `T`, correctly aligned, and mapped
    /// as device memory for the kernel's lifetime.
    #[inline]
    #[must_use]
    pub const unsafe fn new(addr: usize) -> Self {
        Self(addr as *mut T)
    }

    /// Volatile read of the register.
    #[inline]
    #[must_use]
    pub fn read(&self) -> T {
        // SAFETY: the constructor's caller vouched that this is a mapped device
        // register of width `T`.
        unsafe { self.0.read_volatile() }
    }

    /// Volatile write of the register.
    #[inline]
    pub fn write(&self, value: T) {
        // SAFETY: as `read` — the address was vouched for at construction.
        unsafe { self.0.write_volatile(value) };
    }
}

#[cfg(test)]
mod tests {
    use super::MmioReg;

    /// There is no device on the host, but volatile access to ordinary memory is
    /// well-defined, so the plumbing — width, address arithmetic, round-trip — is
    /// testable even though device semantics are not.
    #[test]
    fn round_trips_through_the_named_address() {
        let mut cell: u32 = 0;
        // SAFETY: not a device register, but a valid, aligned, live `u32` for the
        // duration of the test, which is what the accessors actually require.
        let reg = unsafe { MmioReg::<u32>::new(&raw mut cell as usize) };

        assert_eq!(reg.read(), 0);
        reg.write(0x7472_6976);
        assert_eq!(reg.read(), 0x7472_6976);
        assert_eq!(cell, 0x7472_6976, "the write must land at the named address");
    }

    /// The width is part of the register's identity: `fw_cfg` alone needs u8, u16
    /// and u64 registers, which is why the type is generic rather than u32-only.
    #[test]
    fn each_width_accesses_exactly_its_own_bytes() {
        let mut cell: u64 = 0;
        let base = &raw mut cell as usize;

        // SAFETY: `cell` is a live, 8-byte-aligned `u64`; a `u8` at its base and a
        // `u16` at its base are both in bounds and aligned.
        let (byte, half, word) = unsafe {
            (
                MmioReg::<u8>::new(base),
                MmioReg::<u16>::new(base),
                MmioReg::<u64>::new(base),
            )
        };

        // Which byte of the u64 a narrow write lands in is the host's endianness,
        // so count touched bytes rather than assume a position.
        word.write(0);
        byte.write(0xAB);
        assert_eq!(byte.read(), 0xAB);
        assert_eq!(
            word.read().to_ne_bytes().iter().filter(|&&b| b != 0).count(),
            1,
            "a u8 write must touch exactly one byte of the u64"
        );

        word.write(0);
        half.write(0xBEEF);
        assert_eq!(half.read(), 0xBEEF);
        assert_eq!(
            word.read().to_ne_bytes().iter().filter(|&&b| b != 0).count(),
            2,
            "a u16 write must touch exactly two bytes of the u64"
        );
    }
}
