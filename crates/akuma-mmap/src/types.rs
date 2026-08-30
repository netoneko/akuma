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

    /// Is a page mapped with these flags eligible for the shared file-page cache?
    ///
    /// [`is_read_only_to_user`] ANDed with the `SHARED_FILE_PAGES_ENABLED` kill
    /// switch, which the caller passes in. A writable private file mapping would
    /// need copy-on-write before sharing, so ELF data segments carrying
    /// relocations stay private.
    ///
    /// **The gate is a parameter, not a config read**, which is what let this
    /// function live in a crate with an empty `[dependencies]` table. The first
    /// version of the split left it behind in `akuma-exec` for exactly that
    /// reason — it was written as `config().shared_file_pages_enabled && …`, and
    /// a crate that cannot depend on anything cannot read an `ExecConfig`. But
    /// the config read was never part of the *decision*; it was one boolean the
    /// decision consumed. Taking it as an argument moves the whole predicate down
    /// here and leaves the config read at the one call site that owns the switch
    /// (`src/file_page_cache.rs`).
    ///
    /// It also made the tests pure: they pass `true`/`false` instead of
    /// registering an injectable config, and the `gate == false` case — the kill
    /// switch actually working — became testable for the first time.
    #[must_use]
    pub const fn is_shareable_mapping(flags: u64, shared_file_pages_enabled: bool) -> bool {
        shared_file_pages_enabled && is_read_only_to_user(flags)
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

    /// **A non-executable mapping is never shareable.**
    ///
    /// This is what makes the merged DA/IA demand-paging body's `is_exec` gate
    /// inert for `file_page_cache`: both cache calls in that body
    /// (`lookup_and_ref`'s `want_exec` and `insert`'s `icache_done`) are reached
    /// only under `is_shareable_mapping`, so if every shareable mapping is also
    /// executable, they can only ever be called with `is_exec == true` — which is
    /// exactly what the instruction-abort arm used to hardcode. Sharing requires
    /// `AP_RO_ALL`, and the only `AP_RO_ALL` values a lazy region can record
    /// (`from_prot` and `akuma_elf::load::segment_page_flags` are the two
    /// producers) leave `UXN` clear.
    ///
    /// If this ever fails, the `is_exec` gate has become observable in the shared
    /// cache: `insert(.., false)` would start publishing `icache_done: false` for
    /// pages that had it `true`. Still the *safe* direction, but a behaviour
    /// change, so it should be a decision rather than a surprise. See
    /// `docs/archive/COW_PILE_AUDIT.md` §12.1.
    #[test]
    fn non_exec_mappings_are_never_shareable() {
        for f in [
            user_flags::NONE,
            user_flags::RO,
            user_flags::RX,
            user_flags::RW,
            user_flags::RW_NO_EXEC,
        ] {
            if !user_flags::is_exec(f) {
                assert!(
                    !user_flags::is_shareable_mapping(f, true),
                    "non-exec mapping {f:#x} must not be shareable"
                );
            }
        }
        // The two producers of a lazy region's flags, exhaustively.
        for prot in 0..8u32 {
            let f = user_flags::from_prot(prot);
            assert!(
                user_flags::is_exec(f) || !user_flags::is_shareable_mapping(f, true),
                "from_prot({prot}) = {f:#x} is shareable but not exec"
            );
        }
    }

    /// With the gate on, the gated form must agree with the pure predicate for
    /// every flag combination — i.e. the gate adds nothing but the kill switch.
    #[test]
    fn gated_shareable_agrees_with_the_predicate_when_enabled() {
        for f in [
            user_flags::NONE,
            user_flags::RO,
            user_flags::RX,
            user_flags::RW,
            user_flags::RW_NO_EXEC,
        ] {
            assert_eq!(
                user_flags::is_shareable_mapping(f, true),
                user_flags::is_read_only_to_user(f),
                "{f:#x}"
            );
        }
    }

    /// **The kill switch actually kills.** Untestable until the gate became a
    /// parameter: proving it needed an injected `ExecConfig` with the flag off,
    /// and the host tests only ever registered one with it on
    /// (`register_config_for_test` hardcodes `true`). So the one behaviour
    /// `SHARED_FILE_PAGES_ENABLED` exists to provide had no test at all.
    #[test]
    fn the_kill_switch_makes_every_mapping_unshareable() {
        for prot in 0..8u32 {
            assert!(!user_flags::is_shareable_mapping(user_flags::from_prot(prot), false));
        }
        for f in [
            user_flags::NONE,
            user_flags::RO,
            user_flags::RX,
            user_flags::RW,
            user_flags::RW_NO_EXEC,
            user_flags::EXEC,
        ] {
            assert!(!user_flags::is_shareable_mapping(f, false), "{f:#x}");
        }
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
