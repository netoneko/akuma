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
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn page_geometry() {
        assert_eq!(PAGE_SIZE, 4096);
        assert_eq!(PAGE_SHIFT, 12);
        assert_eq!(1usize << PAGE_SHIFT, PAGE_SIZE);
    }
}
