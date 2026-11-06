//! Typed addresses: [`PhysAddr`], [`VirtAddr`] and [`Pfn`].
//!
//! `docs/archive/NEWTYPE_AUDIT.md` found the kernel's address vocabulary was raw `usize`/`u64` with a naming
//! convention, and that VA/PA/PFN confusion heads its bug history (`AKUMA_AMD64_4B_FLIP.md`,
//! `AKUMA_AMD64_PHYSMAP_ABOVE_4GIB.md`, `MPROTECT_TLB_ASID_BUG.md`). Translation is the identity on AArch64 and a
//! constant offset on amd64, which is exactly what makes the confusion cheap to write and invisible to a host test.
//! These types buy **distinctness**, not conversion: a `VirtAddr` cannot be passed where a `PhysAddr` is wanted, and
//! the arithmetic that makes no sense does not compile.
//!
//! The operator set is deliberately small:
//!
//! * `addr + n` and `addr - n` (n a byte count, `u64`/`usize`) give an address;
//! * `addr - addr` gives a byte count (never an address);
//! * there is no `addr + addr`, no `addr * n`, no cross-type arithmetic;
//! * `Pfn <-> PhysAddr` is an explicit conversion (`Pfn::phys`, `PhysAddr::pfn`).
//!
//! The integer width follows the hardware: a physical address is a `u64` (52 bits architecturally, whatever `usize`
//! is), a virtual address is a `usize` (it becomes a pointer), a frame number is a `u64`.

use core::fmt;
use core::ops::{Add, Sub};

/// Bytes in a page.
pub const PAGE_SIZE: usize = 4096;
/// `log2(PAGE_SIZE)`.
pub const PAGE_SHIFT: u32 = 12;

macro_rules! addr_common {
    ($t:ident, $int:ty) => {
        impl $t {
            /// The address `v`.
            #[must_use]
            pub const fn new(v: $int) -> Self {
                Self(v)
            }

            /// The raw number.
            #[must_use]
            pub const fn get(self) -> $int {
                self.0
            }

            /// Whether this is page-aligned.
            #[must_use]
            pub const fn is_page_aligned(self) -> bool {
                self.0 & (PAGE_SIZE as $int - 1) == 0
            }

            /// Byte offset within its page.
            #[must_use]
            pub const fn page_offset(self) -> usize {
                (self.0 & (PAGE_SIZE as $int - 1)) as usize
            }

            /// Rounded down to a page boundary.
            #[must_use]
            pub const fn align_down(self) -> Self {
                Self(self.0 & !(PAGE_SIZE as $int - 1))
            }

            /// Rounded up to a page boundary; `None` on overflow.
            #[must_use]
            pub const fn align_up(self) -> Option<Self> {
                match self.0.checked_add(PAGE_SIZE as $int - 1) {
                    Some(v) => Some(Self(v & !(PAGE_SIZE as $int - 1))),
                    None => None,
                }
            }

            /// `self + bytes`, or `None` on overflow.
            #[must_use]
            pub const fn checked_add(self, bytes: $int) -> Option<Self> {
                match self.0.checked_add(bytes) {
                    Some(v) => Some(Self(v)),
                    None => None,
                }
            }

            /// `self - bytes`, or `None` on underflow.
            #[must_use]
            pub const fn checked_sub(self, bytes: $int) -> Option<Self> {
                match self.0.checked_sub(bytes) {
                    Some(v) => Some(Self(v)),
                    None => None,
                }
            }
        }

        impl Add<$int> for $t {
            type Output = Self;
            fn add(self, bytes: $int) -> Self {
                Self(self.0 + bytes)
            }
        }

        impl Sub<$int> for $t {
            type Output = Self;
            fn sub(self, bytes: $int) -> Self {
                Self(self.0 - bytes)
            }
        }

        /// The distance in bytes between two addresses of the same kind.
        impl Sub for $t {
            type Output = $int;
            fn sub(self, other: Self) -> $int {
                self.0 - other.0
            }
        }

        impl fmt::Debug for $t {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!(stringify!($t), "({:#x})"), self.0)
            }
        }

        impl fmt::LowerHex for $t {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::LowerHex::fmt(&self.0, f)
            }
        }
    };
}

/// A physical address.
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct PhysAddr(u64);

/// A virtual address (a pointer's number; nothing here dereferences it).
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct VirtAddr(usize);

/// A page frame number: a physical address divided by [`PAGE_SIZE`].
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Pfn(u64);

addr_common!(PhysAddr, u64);
addr_common!(VirtAddr, usize);

impl PhysAddr {
    /// The frame containing this address (rounded down).
    #[must_use]
    pub const fn pfn(self) -> Pfn {
        Pfn(self.0 >> PAGE_SHIFT)
    }

    /// The raw number as a `usize` (a physical address always fits on the 64-bit targets this kernel has).
    #[must_use]
    pub const fn as_usize(self) -> usize {
        self.0 as usize
    }

    /// The address `v` (the PMM's bookkeeping is `usize`).
    #[must_use]
    pub const fn from_usize(v: usize) -> Self {
        Self(v as u64)
    }
}

impl VirtAddr {
    /// The address of `p`.
    #[must_use]
    pub fn from_ptr<T>(p: *const T) -> Self {
        Self(p as usize)
    }

    /// This address as a read pointer. Creating a pointer is safe; using it is the caller's `unsafe`.
    #[must_use]
    pub const fn as_ptr<T>(self) -> *const T {
        self.0 as *const T
    }

    /// This address as a write pointer. Same as [`VirtAddr::as_ptr`].
    #[must_use]
    pub const fn as_mut_ptr<T>(self) -> *mut T {
        self.0 as *mut T
    }
}

impl Pfn {
    /// Frame number `v`.
    #[must_use]
    pub const fn new(v: u64) -> Self {
        Self(v)
    }

    /// The raw number.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// The physical address of the first byte of this frame; `None` if it does not fit.
    #[must_use]
    pub const fn phys(self) -> Option<PhysAddr> {
        match self.0.checked_mul(PAGE_SIZE as u64) {
            Some(v) => Some(PhysAddr(v)),
            None => None,
        }
    }
}

/// `pfn + n` is the frame `n` frames on.
impl Add<u64> for Pfn {
    type Output = Self;
    fn add(self, frames: u64) -> Self {
        Self(self.0 + frames)
    }
}

/// The distance in frames.
impl Sub for Pfn {
    type Output = u64;
    fn sub(self, other: Self) -> u64 {
        self.0 - other.0
    }
}

impl fmt::Debug for Pfn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Pfn({:#x})", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arithmetic_keeps_kinds() {
        let pa = PhysAddr::new(0x1000);
        assert_eq!(pa + 0x20, PhysAddr::new(0x1020));
        assert_eq!(PhysAddr::new(0x1020) - pa, 0x20u64);
        assert_eq!(pa - 0x1000u64, PhysAddr::new(0));
        let va = VirtAddr::new(0xffff_8000_0000_1000);
        assert_eq!(va + 8, VirtAddr::new(0xffff_8000_0000_1008));
        assert_eq!(VirtAddr::new(0x20) - VirtAddr::new(0x8), 0x18usize);
    }

    #[test]
    fn page_helpers() {
        let pa = PhysAddr::new(0x12345);
        assert!(!pa.is_page_aligned());
        assert_eq!(pa.page_offset(), 0x345);
        assert_eq!(pa.align_down(), PhysAddr::new(0x12000));
        assert_eq!(pa.align_up(), Some(PhysAddr::new(0x13000)));
        assert_eq!(PhysAddr::new(0x13000).align_up(), Some(PhysAddr::new(0x13000)));
        assert_eq!(PhysAddr::new(u64::MAX).align_up(), None);
        assert!(VirtAddr::new(0x4000).is_page_aligned());
    }

    #[test]
    fn pfn_round_trip() {
        let pa = PhysAddr::new(0x12345);
        let pfn = pa.pfn();
        assert_eq!(pfn, Pfn::new(0x12));
        assert_eq!(pfn.phys(), Some(PhysAddr::new(0x12000)));
        assert_eq!(pfn + 3, Pfn::new(0x15));
        assert_eq!(Pfn::new(0x15) - pfn, 3);
        assert_eq!(Pfn::new(u64::MAX).phys(), None);
    }

    #[test]
    fn checked_ops_report_overflow() {
        assert_eq!(PhysAddr::new(u64::MAX).checked_add(1), None);
        assert_eq!(PhysAddr::new(0).checked_sub(1), None);
        assert_eq!(VirtAddr::new(1).checked_add(2), Some(VirtAddr::new(3)));
    }

    #[test]
    fn pointers_round_trip() {
        let x = 7u32;
        let va = VirtAddr::from_ptr(&raw const x);
        assert_eq!(va.as_ptr::<u32>(), &raw const x);
    }

    #[test]
    fn same_size_as_the_integers() {
        assert_eq!(core::mem::size_of::<PhysAddr>(), 8);
        assert_eq!(core::mem::size_of::<VirtAddr>(), core::mem::size_of::<usize>());
        assert_eq!(core::mem::size_of::<Pfn>(), 8);
    }
}
