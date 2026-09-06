//! The address-space operations an ELF loader needs — as a trait, so the loader
//! is not tied to one architecture's page tables.
//!
//! # Why this exists
//!
//! `akuma-elf` named `akuma_mmu::UserAddressSpace` concretely, and that type is
//! `#[cfg(target_arch = "aarch64")]`: the AArch64 L0–L3 walker, with ASIDs and
//! `TTBR`. So the ELF loader did not compile for `x86_64-unknown-none`, and
//! neither did `akuma-exec` above it, nor `akuma-vfs-glue` and
//! `akuma-syscalls-glue` above that — four crates and most of the kernel's
//! process machinery, unreachable from the amd64 port because of **three
//! method calls**.
//!
//! Those three are all a loader ever does to an address space: make one, get a
//! zeroed page mapped at a virtual address, and put bytes into a page it just
//! mapped. Nothing about that is architecture-specific. The page *tables* are;
//! the loader is not.
//!
//! # What is deliberately not here
//!
//! Everything else `UserAddressSpace` does — CoW share passes, ASID allocation,
//! `TTBR0` installs, unmapping, reference counting, the lazy-region demand
//! paging. A loader touches none of it, and pulling any of it in would make the
//! trait an MMU interface rather than a loader's dependency. If an
//! implementation of this trait starts needing a fourth method, check first
//! whether the loader has grown a job that belongs to the caller.

use crate::types::ElfError;

/// The two page protections an ELF image ever needs.
///
/// **W^X, by construction.** A `PT_LOAD` segment asking for both write and
/// execute gets [`Self::Code`] — the loader has never honoured `PF_W | PF_X`,
/// and making that a property of a two-variant type rather than of a bitmask
/// means it cannot be reintroduced by accident.
///
/// A neutral vocabulary rather than the `u64` of AArch64 PTE bits the loader
/// used to pass: those bits are meaningless on x86, and an implementation that
/// received them would have to pattern-match on a foreign architecture's
/// encoding to find out what was being asked for.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum SegProt {
    /// Readable and executable, never writable. A text segment.
    Code,
    /// Readable and writable, never executable. Data, `.bss`, the stack.
    Data,
}

/// What [`crate::load_elf`] and its helpers require of an address space.
///
/// Implementors: `akuma_mmu::UserAddressSpace` (AArch64, in this crate) and
/// `amd64/src/paging.rs`'s `AddressSpace` (x86_64, in that target).
pub trait UserPages: Sized {
    /// A fresh, empty user address space, or `None` if a frame for its top-level
    /// table could not be had.
    ///
    /// Named `new_space` rather than `new` so it cannot collide with an
    /// implementor's own inherent constructor — both existing implementors have
    /// one, and an inherent method silently wins over a trait method of the same
    /// name.
    fn new_space() -> Option<Self>;

    /// Map one **zeroed** page at `va` with protection `prot`, returning the
    /// physical address of the frame backing it.
    ///
    /// Zeroing is part of the contract, not an implementation detail: a
    /// `PT_LOAD` segment whose `memsz` exceeds its `filesz` relies on the tail
    /// being zero (that is what `.bss` *is*), and the loader deliberately does
    /// not write those bytes. An implementation that hands back a recycled dirty
    /// frame leaks the previous owner's memory into the new process and
    /// corrupts its `.bss` — silently, and only for programs that read a
    /// variable before writing it.
    fn alloc_and_map(&mut self, va: usize, prot: SegProt) -> Result<usize, &'static str>;

    /// Write `bytes` at `offset` within the page mapped at `page_va`.
    ///
    /// Returns `false` if `page_va` is not mapped in this address space. The
    /// write must not cross the page: callers pass a `(page_va, offset, len)`
    /// triple computed by `akuma_mmap::span`, which is host-tested for exactly
    /// that, and an implementation is entitled to assert it.
    fn write_page_bytes(&mut self, page_va: usize, offset: usize, bytes: &[u8]) -> bool;
}

/// `alloc_and_map`, with the loader's error type. A convenience so the three
/// call sites do not each repeat the `map_err`.
pub(crate) fn alloc_page<A: UserPages>(
    space: &mut A,
    va: usize,
    prot: SegProt,
) -> Result<usize, ElfError> {
    space.alloc_and_map(va, prot).map_err(ElfError::MappingFailed)
}

// ============================================================================
// The AArch64 implementation
// ============================================================================
//
// In this crate rather than in `akuma-mmu` because the trait is local here and
// the type is foreign there — either placement is allowed by the orphan rule,
// and this one keeps `akuma-mmu` unaware that an ELF loader exists.

#[cfg(target_arch = "aarch64")]
impl UserPages for akuma_mmu::UserAddressSpace {
    fn new_space() -> Option<Self> {
        Self::new()
    }

    fn alloc_and_map(&mut self, va: usize, prot: SegProt) -> Result<usize, &'static str> {
        Self::alloc_and_map(self, va, prot.to_user_flags()).map(|frame| frame.addr)
    }

    fn write_page_bytes(&mut self, page_va: usize, offset: usize, bytes: &[u8]) -> bool {
        Self::write_page_bytes(self, page_va, offset, bytes)
    }
}

impl SegProt {
    /// The AArch64 PTE flag word for this protection.
    ///
    /// Still needed off the mapping path: `DeferredLazySegment::page_flags`
    /// carries a `u64` into `akuma-exec`'s lazy-region table, which stores and
    /// later re-applies raw PTE bits. That table is AArch64-only machinery, so
    /// the conversion lives here rather than the field becoming a `SegProt`.
    #[must_use]
    pub const fn to_user_flags(self) -> u64 {
        use akuma_mmu::user_flags;
        match self {
            Self::Code => user_flags::RX,
            Self::Data => user_flags::RW_NO_EXEC,
        }
    }
}
