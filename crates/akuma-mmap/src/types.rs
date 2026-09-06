//! Page geometry, and the **architecture-neutral** permission vocabulary regions speak.
//!
//! # What changed, and why
//!
//! Until 2026-09-06 this module defined `flags` — literal AArch64 descriptor bits,
//! `AP_RW_ALL = 1 << 6`, `PXN = 1 << 53`, `UXN = 1 << 54` — and `user_flags`, six
//! `u64` constants built from them. `MmapRegion::flags` stored one.
//!
//! That made this crate's *code* portable and its *data* not, which is the worse
//! of the two failures. `akuma-mmap` has an empty `[dependencies]` table, forbids
//! `unsafe`, and builds for `x86_64-unknown-none` today — so the amd64 kernel
//! could compile it, adopt it, and get **silently wrong answers**. x86_64 uses
//! bit 1 (R/W), bit 2 (U/S) and bit 63 (NX); the two permission masks share
//! **exactly zero** bits, and AArch64's `AP_MASK` (bits [7:6]) lands on x86's
//! **Dirty** and **PAT**. `is_write` would have evaluated "Dirty set, PAT clear"
//! — answering *"has this page been written?"* when asked *"may it be written?"*.
//! Right often enough to pass a smoke test, wrong exactly when it matters: a
//! clean writable page reads as read-only, so the write-fault handler treats a
//! legitimate store as an `mprotect(PROT_READ)` violation. That is
//! `GRANT_RECORDS_VS_DENY_RECORDS.md`'s bug, re-created by porting its fix.
//!
//! So [`Prot`] is what a region records now, and the bits live with the walker
//! that writes them: `akuma_mmu::types` for AArch64, `amd64/src/paging.rs` for
//! x86_64. This is [`akuma_cow`](https://docs.rs/akuma-cow)'s shape — that crate
//! takes `pte_writable: bool` and `marked: bool`, decoded booleans rather than a
//! PTE, which is precisely why it already serves both kernels.
//!
//! # Why an opaque token and not `{ read, write, exec }`
//!
//! Because the AArch64 table it replaces is not expressible as a triple.
//! `user_flags::RO` (`AP_RO_ALL`) and `user_flags::RX` (`AP_RO_ALL | PXN`) grant
//! EL0 exactly the same thing — read and execute — and differ only in whether
//! **EL1** may fetch. A `{read, write, exec}` struct collapses them, and
//! `to_pte` would then have to guess which one to re-emit; the guess changes
//! PXN on a live mapping, which is a behaviour change smuggled inside a
//! refactor.
//!
//! A token keeps the round trip exact: each variant names one encoding, the
//! arch layer's `to_pte` is a total match, and `prot_roundtrips_to_todays_bits`
//! in `akuma-mmu` pins every variant to the `u64` it produced before the move.
//! The predicates below are the same three questions the bit arithmetic
//! answered, asked of the token instead.
//!
//! `EXEC` is deliberately an alias of [`Prot::RO`] rather than a seventh
//! variant: the two constants were **byte-identical** (`AP_RO_ALL`) before this
//! change, so making them distinct here would invent a difference the kernel
//! never had.

pub const PAGE_SIZE: usize = 4096;
pub const PAGE_SHIFT: usize = 12;

/// The protection a mapping is *supposed* to have, named rather than encoded.
///
/// Six distinct values, matching one-for-one the six distinct `u64`s the
/// AArch64 `user_flags` table produced. The inner `u8` is an opaque tag with no
/// arithmetic meaning — nothing outside this module may construct one from a
/// number, which is what stops a raw PTE being smuggled in as a `Prot`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Prot(u8);

impl Prot {
    /// `PROT_NONE`: the owner may not read, write or execute. Distinct from
    /// "not recorded" — see `MmapRegion::prot_recorded`.
    pub const NONE: Self = Self(0);
    /// Read and execute by the owner. EL1 fetch permitted (no `PXN` on AArch64)
    /// — the distinction from [`Self::RX`], and the only thing separating them.
    pub const RO: Self = Self(1);
    /// Read, write and execute.
    pub const RW: Self = Self(2);
    /// Read and write, no execute.
    pub const RW_NO_EXEC: Self = Self(3);
    /// Read and execute by the owner, **not** by the kernel (`PXN`).
    pub const RX: Self = Self(4);
    /// Read only — no write, no execute.
    pub const RO_NO_EXEC: Self = Self(5);
    /// Historical alias: `user_flags::EXEC` was byte-identical to `RO`.
    pub const EXEC: Self = Self::RO;

    /// Every variant, for the arch layers' exhaustiveness tests. Adding a
    /// variant without adding it here fails those tests rather than silently
    /// leaving a hole in a `to_pte` match.
    pub const ALL: [Self; 6] =
        [Self::NONE, Self::RO, Self::RW, Self::RW_NO_EXEC, Self::RX, Self::RO_NO_EXEC];

    /// The opaque tag. For the arch layer's `to_pte` match and for diagnostics;
    /// it is not a PTE and means nothing to hardware.
    #[must_use]
    pub const fn tag(self) -> u8 {
        self.0
    }

    /// The protection an `mmap`/`mprotect` `prot` argument asks for.
    ///
    /// The same three-way decision the AArch64 `from_prot` made, unchanged:
    /// `PROT_WRITE` wins over `PROT_EXEC`, and a `prot` of 0 is `PROT_NONE`.
    /// Note it never yields [`Self::RO_NO_EXEC`] — a plain `PROT_READ` mapping
    /// stays executable, which is the AArch64 table's behaviour and is pinned
    /// by test rather than defended here.
    #[must_use]
    pub const fn from_prot(prot: u32) -> Self {
        if prot == 0 {
            return Self::NONE;
        }
        match (prot & 0x2 != 0, prot & 0x4 != 0) {
            (true, _) => Self::RW_NO_EXEC,
            (false, true) => Self::RX,
            (false, false) => Self::RO,
        }
    }

    /// Whether this mapping lets the owner **write** to the page.
    ///
    /// The predicate every permission-repair path in the fault handler needs,
    /// and the one whose absence let `mprotect` be defeated: a CoW-shared page
    /// and an `mprotect(PROT_READ)` page are both read-only in the hardware and
    /// cannot be told apart from it. The region records which; this reads that
    /// record. Was `flags & AP_MASK == AP_RW_ALL`.
    #[must_use]
    pub const fn is_write(self) -> bool {
        matches!(self.0, 2 | 3)
    }

    /// Whether the owner may *fetch instructions* from the page.
    ///
    /// Decides whether a demand-paged frame needs I-cache maintenance
    /// (`dc cvau` + `ic ivau`), which is only load-bearing for a page some PE
    /// will fetch from. Was `flags & UXN == 0`, so it was true for `RO`, `RW`
    /// and `RX` and false for the rest — including `RO`, which carries no
    /// `UXN`. That is the AArch64 encoding, not an oversight.
    #[must_use]
    pub const fn is_exec(self) -> bool {
        matches!(self.0, 1 | 2 | 4)
    }

    /// Whether this is `PROT_NONE`. Was `flags == user_flags::NONE`.
    #[must_use]
    pub const fn is_none(self) -> bool {
        matches!(self.0, 0)
    }

    /// Whether the owner has **no write access** — the complement of
    /// [`Self::is_write`] over the access field, kept as its own name because
    /// the two are asked for opposite purposes: `is_write` asks whether a store
    /// may proceed, this asks whether a page may be *shared*.
    ///
    /// Was `flags & AP_MASK == AP_RO_ALL`, which is **not** simply `!is_write`:
    /// `NONE` was `AP_RO_EL1`, so it answered `false` to both. Preserved
    /// exactly — a `PROT_NONE` page is not shareable.
    #[must_use]
    pub const fn is_read_only_to_user(self) -> bool {
        matches!(self.0, 1 | 4 | 5)
    }

    /// Is a page mapped like this eligible for the shared file-page cache?
    ///
    /// [`Self::is_read_only_to_user`] ANDed with the kill switch, which the
    /// caller passes in. **The gate is a parameter, not a config read**, which
    /// is what lets this live in a crate with an empty `[dependencies]` table —
    /// and what made the `gate == false` case testable at all.
    #[must_use]
    pub const fn is_shareable_mapping(self, shared_file_pages_enabled: bool) -> bool {
        shared_file_pages_enabled && self.is_read_only_to_user()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_prot_matches_the_aarch64_table() {
        assert_eq!(Prot::from_prot(0), Prot::NONE);
        assert_eq!(Prot::from_prot(1), Prot::RO);
        assert_eq!(Prot::from_prot(2), Prot::RW_NO_EXEC);
        assert_eq!(Prot::from_prot(4), Prot::RX);
    }

    /// `from_prot` and `is_write` must agree: anything carrying `PROT_WRITE` is
    /// writable and nothing else is. This is the pair the fault handler relies
    /// on to tell an `mprotect` downgrade from a CoW demotion.
    #[test]
    fn from_prot_and_is_write_agree() {
        for prot in 0u32..8 {
            assert_eq!(Prot::from_prot(prot).is_write(), prot & 0x2 != 0, "prot={prot:#x}");
        }
    }

    #[test]
    fn is_write_only_for_the_writable_pair() {
        assert!(Prot::RW.is_write());
        assert!(Prot::RW_NO_EXEC.is_write());
        for p in [Prot::RO, Prot::RX, Prot::EXEC, Prot::NONE, Prot::RO_NO_EXEC] {
            assert!(!p.is_write(), "{p:?}");
        }
    }

    /// `RO` carries no `UXN`, so it *is* executable — the AArch64 encoding, and
    /// the reason `RO_NO_EXEC` had to be named separately.
    #[test]
    fn is_exec_matches_the_uxn_reading() {
        for p in [Prot::RO, Prot::RW, Prot::RX, Prot::EXEC] {
            assert!(p.is_exec(), "{p:?}");
        }
        for p in [Prot::NONE, Prot::RW_NO_EXEC, Prot::RO_NO_EXEC] {
            assert!(!p.is_exec(), "{p:?}");
        }
    }

    /// `PROT_NONE` answered `false` to `is_read_only_to_user` because it was
    /// `AP_RO_EL1`, not `AP_RO_ALL`. So this is not `!is_write`, and a
    /// `PROT_NONE` page must not become shareable.
    #[test]
    fn read_only_to_user_excludes_prot_none() {
        for p in [Prot::RO, Prot::RX, Prot::RO_NO_EXEC] {
            assert!(p.is_read_only_to_user(), "{p:?}");
        }
        for p in [Prot::NONE, Prot::RW, Prot::RW_NO_EXEC] {
            assert!(!p.is_read_only_to_user(), "{p:?}");
        }
        assert!(!Prot::NONE.is_shareable_mapping(true));
        assert!(!Prot::RO.is_shareable_mapping(false));
        assert!(Prot::RO.is_shareable_mapping(true));
    }

    /// `EXEC` and `RO` were byte-identical before the move; keep them so.
    #[test]
    fn exec_is_an_alias_of_ro() {
        assert_eq!(Prot::EXEC, Prot::RO);
    }

    /// Every variant is in `ALL`, so an arch layer's exhaustiveness test cannot
    /// silently miss one.
    #[test]
    fn all_lists_every_distinct_variant() {
        let mut tags: [u8; 6] = Prot::ALL.map(Prot::tag);
        tags.sort_unstable();
        assert_eq!(tags, [0, 1, 2, 3, 4, 5]);
    }

    #[test]
    fn page_geometry() {
        assert_eq!(PAGE_SIZE, 4096);
        assert_eq!(PAGE_SHIFT, 12);
        assert_eq!(1usize << PAGE_SHIFT, PAGE_SIZE);
    }
}
