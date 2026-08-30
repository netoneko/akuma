//! Page geometry and the PTE permission vocabulary regions speak.
//!
//! Only the bits a *region* names live here. Page-table structure (`PageTable`,
//! `ENTRIES_PER_TABLE`, `BITS_PER_LEVEL`), memory attributes (`MAIR_*`,
//! `attr_index`) and block sizes stay in `akuma_exec::mmu::types` with the walker
//! that uses them; `akuma_exec::mmu::types` re-exports this module so
//! `crate::mmu::flags::*` and `crate::mmu::user_flags::*` still resolve there.

pub const PAGE_SIZE: usize = 4096;
pub const PAGE_SHIFT: usize = 12;

pub mod flags {
    pub const VALID: u64 = 1 << 0;
    pub const TABLE: u64 = 1 << 1;
    pub const BLOCK: u64 = 0 << 1;
    pub const AF: u64 = 1 << 10;
    pub const SH_INNER: u64 = 3 << 8;
    pub const SH_OUTER: u64 = 2 << 8;
    pub const AP_RW_EL1: u64 = 0 << 6;
    pub const AP_RW_ALL: u64 = 1 << 6;
    pub const AP_RO_EL1: u64 = 2 << 6;
    pub const AP_RO_ALL: u64 = 3 << 6;
    /// AP field mask (bits [7:6]) — isolates the access-permission bits from a PTE
    /// or a `user_flags` value so the two can be compared.
    pub const AP_MASK: u64 = 3 << 6;
    pub const USER: u64 = 1 << 6;
    pub const PXN: u64 = 1 << 53;
    pub const UXN: u64 = 1 << 54;
    pub const NG: u64 = 1 << 11;
}

pub mod user_flags {
    use super::flags;
    /// PROT_NONE: EL1-only access, EL0 gets no read/write/exec.
    pub const NONE: u64 = flags::AP_RO_EL1 | flags::UXN | flags::PXN;
    pub const RO: u64 = flags::AP_RO_ALL;
    pub const RW: u64 = flags::AP_RW_ALL;
    pub const EXEC: u64 = flags::AP_RO_ALL;
    pub const RW_NO_EXEC: u64 = flags::AP_RW_ALL | flags::UXN | flags::PXN;
    pub const RX: u64 = flags::AP_RO_ALL | flags::PXN;

    #[must_use]
    pub fn from_prot(prot: u32) -> u64 {
        if prot == 0 { return NONE; }
        match (prot & 0x2 != 0, prot & 0x4 != 0) {
            (true, _)      => RW_NO_EXEC,
            (false, true)  => RX,
            (false, false) => RO,
        }
    }

    #[must_use]
    pub fn is_none(flags: u64) -> bool {
        flags == NONE
    }

    /// Whether a mapping with these flags lets EL0 **write** to the page.
    ///
    /// The predicate every permission-repair path in the EL0 fault handler needs,
    /// and the one whose absence let `mprotect` be defeated: a CoW-shared page and
    /// an `mprotect(PROT_READ)` page are both read-only in the PTE and cannot be
    /// told apart from the hardware state alone. `MmapRegion::flags` records which
    /// one it is; this reads that record.
    ///
    /// Reads the `AP` field and nothing else — `UXN`/`PXN` decide execution, which
    /// is irrelevant to whether a store may proceed.
    #[must_use]
    pub const fn is_write(flags: u64) -> bool {
        flags & flags::AP_MASK == flags::AP_RW_ALL
    }

    /// Whether a mapping with these flags lets EL0 *fetch instructions* from the page.
    ///
    /// This is the predicate that decides whether a demand-paged frame needs the
    /// `dc cvau` + `ic ivau` sequence: I-cache maintenance is only load-bearing for a
    /// page some PE will fetch from. It reads `UXN` and nothing else, deliberately —
    /// `AP` decides read/write, `PXN` decides EL1 fetch, and neither is relevant to
    /// what EL0 can execute.
    #[must_use]
    pub const fn is_exec(flags: u64) -> bool {
        flags & flags::UXN == 0
    }

    /// Whether a mapping with these flags gives EL0 **no write access** — true only
    /// for `AP_RO_ALL`, i.e. [`RO`] and [`RX`].
    ///
    /// The exact complement of [`is_write`] over the `AP` field, kept as its own
    /// name because the two are read for opposite purposes: `is_write` is asked
    /// whether a store may proceed, this is asked whether a page may be *shared*.
    /// Sharing a page that EL0 can write would need copy-on-write first, which is
    /// why ELF data segments carrying relocations stay private.
    ///
    /// Moved here from `akuma_exec::memmath` on 2026-08-30
    /// (`docs/archive/AKUMA_EXEC_SPLIT_AGAIN.md` §3.3). It is a pure function of
    /// this module's own vocabulary and never belonged a crate up; the version it
    /// replaced also carried a fourth private copy of `AP_MASK`, which this one
    /// reads from [`flags`] instead. The *gated* form — the same predicate ANDed
    /// with the `SHARED_FILE_PAGES_ENABLED` kill switch — stays in `akuma-exec`,
    /// because reading a runtime config is exactly what this crate has no
    /// dependencies to do.
    #[must_use]
    pub const fn is_read_only_to_user(flags: u64) -> bool {
        flags & flags::AP_MASK == flags::AP_RO_ALL
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only `AP_RO_ALL` mappings are read-only to EL0. Moved with the predicate
    /// from `akuma_exec::memmath` (`AKUMA_EXEC_SPLIT_AGAIN.md` §3.3).
    #[test]
    fn only_user_read_only_mappings_are_read_only() {
        assert!(user_flags::is_read_only_to_user(user_flags::RO));
        assert!(user_flags::is_read_only_to_user(user_flags::RX));
        assert!(!user_flags::is_read_only_to_user(user_flags::RW));
        assert!(!user_flags::is_read_only_to_user(user_flags::RW_NO_EXEC));
    }

    /// The predicate must read *only* the AP field: a page that is RO to EL0 stays
    /// read-only whatever its execute/attr bits say, and a writable one is never
    /// rescued by them.
    #[test]
    fn read_only_predicate_ignores_bits_outside_the_ap_field() {
        let other = flags::UXN | flags::PXN | flags::AF;
        assert!(user_flags::is_read_only_to_user(user_flags::RO | other));
        assert!(!user_flags::is_read_only_to_user(user_flags::RW | other));
    }

    /// [`user_flags::is_write`] and [`user_flags::is_read_only_to_user`] are
    /// **mutually exclusive but not exhaustive**, and the gap is load-bearing.
    ///
    /// `AP` has three values an EL0 mapping can take, not two: `AP_RW_ALL`
    /// (writable), `AP_RO_ALL` (read-only *to EL0*, shareable), and `AP_RO_EL1` —
    /// which is [`user_flags::NONE`], the `PROT_NONE` encoding, where EL0 has no
    /// access at all. A `PROT_NONE` page is therefore neither writable nor
    /// "read-only to user", and code that reaches for `!is_write(..)` as a stand-in
    /// for the sharing predicate would wrongly treat it as shareable.
    ///
    /// Neither predicate had a test tying them together while they lived in
    /// different crates; this is the one that says why they are two functions.
    #[test]
    fn write_and_read_only_are_exclusive_but_not_exhaustive() {
        for prot in 0..8u32 {
            let f = user_flags::from_prot(prot);
            assert!(
                !(user_flags::is_write(f) && user_flags::is_read_only_to_user(f)),
                "from_prot({prot}) = {f:#x} claims both"
            );
        }
        // The gap, named: PROT_NONE answers `false` to both.
        let none = user_flags::NONE;
        assert!(!user_flags::is_write(none));
        assert!(!user_flags::is_read_only_to_user(none));
        assert!(user_flags::is_none(none));
    }

    #[test]
    fn user_flags_from_prot() {
        // prot 0 = PROT_NONE (no EL0 access)
        assert_eq!(user_flags::from_prot(0), user_flags::NONE);
        assert!(user_flags::is_none(user_flags::from_prot(0)));
        // prot 1 = PROT_READ
        assert_eq!(user_flags::from_prot(1), user_flags::RO);
        assert!(!user_flags::is_none(user_flags::from_prot(1)));
        // prot 2 = PROT_WRITE
        assert_eq!(user_flags::from_prot(2), user_flags::RW_NO_EXEC);
        // prot 4 = PROT_EXEC
        assert_eq!(user_flags::from_prot(4), user_flags::RX);
    }

    #[test]
    fn user_flags_is_exec_reads_only_uxn() {
        assert!(user_flags::is_exec(user_flags::RX));
        assert!(user_flags::is_exec(user_flags::EXEC));
        assert!(!user_flags::is_exec(user_flags::RW_NO_EXEC));
        assert!(!user_flags::is_exec(user_flags::NONE));
        // `RO`/`RW` carry no UXN, so they are exec by this predicate — that is the
        // AArch64 encoding, not an oversight: a PTE without UXN *is* EL0-executable.
        assert!(user_flags::is_exec(user_flags::RO));
        // PXN (EL1 fetch) must not influence the answer.
        assert!(user_flags::is_exec(user_flags::RO | flags::PXN));
        assert!(!user_flags::is_exec(user_flags::RO | flags::UXN));
    }

    /// The write predicate, across the whole `user_flags` table. `RW`/`RW_NO_EXEC`
    /// permit a store; `RO`, `RX`, `EXEC` and `NONE` must not — those four are
    /// exactly the states a repair path must refuse to upgrade.
    #[test]
    fn user_flags_is_write_reads_only_the_ap_field() {
        assert!(user_flags::is_write(user_flags::RW));
        assert!(user_flags::is_write(user_flags::RW_NO_EXEC));
        assert!(!user_flags::is_write(user_flags::RO));
        assert!(!user_flags::is_write(user_flags::RX));
        assert!(!user_flags::is_write(user_flags::EXEC));
        assert!(!user_flags::is_write(user_flags::NONE));
        // UXN/PXN must not influence the answer either way.
        assert!(user_flags::is_write(user_flags::RW | flags::UXN | flags::PXN));
        assert!(!user_flags::is_write(user_flags::RO | flags::UXN | flags::PXN));
    }

    /// `from_prot` and `is_write` must agree: anything carrying `PROT_WRITE` is
    /// writable, and nothing else is. This is the pair the fault handler relies on
    /// to tell an `mprotect` downgrade from a CoW demotion.
    #[test]
    fn from_prot_and_is_write_agree() {
        for prot in 0u32..8 {
            let want = prot & 0x2 != 0;
            assert_eq!(user_flags::is_write(user_flags::from_prot(prot)), want,
                       "prot={prot:#x}");
        }
    }

    #[test]
    fn page_geometry() {
        assert_eq!(PAGE_SIZE, 4096);
        assert_eq!(PAGE_SHIFT, 12);
        assert_eq!(1usize << PAGE_SHIFT, PAGE_SIZE);
    }
}
