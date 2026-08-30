//! The `SHARED_FILE_PAGES_ENABLED` gate over `akuma-mmap`'s sharing predicate.
//!
//! # What is left here, and why only this
//!
//! This module used to be "memory arithmetic and the decisions built directly on
//! it". Two rounds of extraction have taken almost all of it away, each time to
//! the crate that already owned the concept:
//!
//! - **The PMM's arithmetic** — the user-page reserve, the reclaim escalation's
//!   decision (`next_reclaim_step`), the quarantine poison codec — went to
//!   `akuma-pmm` in `docs/archive/PMM_EXTRACT.md` §7 Step 6, with its 8 host
//!   tests. Genuinely PMM concepts, parked here only because no PMM crate
//!   existed yet and `src/` was host-unreachable.
//! - **The pure mapping predicate** — `mapping_is_read_only_to_user` — went to
//!   `akuma_mmap::user_flags::is_read_only_to_user` on 2026-08-30
//!   (`docs/archive/AKUMA_EXEC_SPLIT_AGAIN.md` §3.3), joining `is_write`,
//!   `is_exec` and `from_prot`, which were already there. It reads the `AP`
//!   field of a PTE and nothing else, so it belongs with the vocabulary that
//!   defines `AP`. It also stopped carrying a fourth private copy of `AP_MASK`
//!   in the move.
//!
//! What could not follow is the **gate**: [`is_shareable_mapping`] is that
//! predicate ANDed with `config().shared_file_pages_enabled`, and `akuma-mmap`
//! has an empty `[dependencies]` table by design — it cannot read an
//! `ExecConfig`. That line is the seam, and it is the honest one: the arithmetic
//! is `akuma-mmap`'s, the kill switch is this crate's. The two tests below pin
//! it, so logic that leaks back into the wrapper is visible.
//!
//! Fork's own copy-range math (`process::fork_code_start`,
//! `process::fork_page_count_for_len`) stays next to `fork_process`, its only
//! consumer.

use crate::mmu;
use crate::runtime::config;

// ============================================================================
// The gated mapping predicate — all that is left here since 2026-08-30.
// ============================================================================

/// Pure: does a mapping with `map_flags` give EL0 **no write access**?
///
/// **Moved to `akuma_mmap::user_flags::is_read_only_to_user`** on 2026-08-30
/// (`docs/archive/AKUMA_EXEC_SPLIT_AGAIN.md` §3.3) — it is a pure function of
/// that crate's own `AP`-field vocabulary and reads no kernel state. Kept as a
/// re-export here so this module's own doc history stays followable; new call
/// sites should name it through `mmu::user_flags`.
pub use akuma_mmap::user_flags::is_read_only_to_user as mapping_is_read_only_to_user;

/// Is a page mapped with `map_flags` eligible for the shared file-page cache?
///
/// A writable private file mapping would need copy-on-write before sharing, so
/// ELF data segments carrying relocations stay private. Gated by
/// `config().shared_file_pages_enabled` (the `SHARED_FILE_PAGES_ENABLED` kill
/// switch), which makes every page ineligible when off.
///
/// **This is the half of the old `memmath` that could not move down.** Its
/// partner predicate went to `akuma-mmap`, whose `[dependencies]` table is empty
/// by design — reading an `ExecConfig` is exactly what a crate with no
/// dependencies cannot do. Splitting the pair along that line is the seam: the
/// arithmetic is `akuma-mmap`'s, the kill switch is this crate's.
#[must_use]
#[inline]
pub fn is_shareable_mapping(map_flags: u64) -> bool {
    config().shared_file_pages_enabled && mmu::user_flags::is_read_only_to_user(map_flags)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() {
        crate::runtime::register_config_for_test();
    }

    /// **A non-executable mapping is never shareable.**
    ///
    /// This is what makes the merged DA/IA demand-paging body's `is_exec` gate inert
    /// for `file_page_cache`: both cache calls in that body (`lookup_and_ref`'s
    /// `want_exec` and `insert`'s `icache_done`) are reached only under
    /// `is_shareable_mapping`, so if every shareable mapping is also executable, they
    /// can only ever be called with `is_exec == true` — which is exactly what the
    /// instruction-abort arm used to hardcode. Sharing requires `AP_RO_ALL`, and the
    /// only `AP_RO_ALL` values a lazy region can record (`user_flags::from_prot` and
    /// `elf::load::segment_page_flags` are the two producers) leave `UXN` clear.
    ///
    /// If this test ever fails, the `is_exec` gate has become observable in the shared
    /// cache: `insert(.., false)` would start publishing `icache_done: false` for
    /// pages that had it `true` before. That is still the *safe* direction ("maintain
    /// it yourself"), but it is a behaviour change, so it should be a decision rather
    /// than a surprise. See `docs/archive/COW_PILE_AUDIT.md` §12.1.
    #[test]
    fn non_exec_mappings_are_never_shareable() {
        setup();
        for flags in [
            mmu::user_flags::NONE,
            mmu::user_flags::RO,
            mmu::user_flags::RX,
            mmu::user_flags::RW,
            mmu::user_flags::RW_NO_EXEC,
        ] {
            if !mmu::user_flags::is_exec(flags) {
                assert!(
                    !is_shareable_mapping(flags),
                    "non-exec mapping {flags:#x} must not be shareable"
                );
            }
        }
        // The two producers of a lazy region's flags, exhaustively.
        for prot in 0..8u32 {
            let flags = mmu::user_flags::from_prot(prot);
            assert!(
                mmu::user_flags::is_exec(flags) || !is_shareable_mapping(flags),
                "from_prot({prot}) = {flags:#x} is shareable but not exec"
            );
        }
    }

    /// With the gate injected **on**, the gated form must agree with the pure
    /// predicate for every flag combination — i.e. the gate adds nothing but the
    /// kill switch. This is the test that pins the seam: if it ever fails, logic
    /// has leaked into the wrapper that belongs in `akuma-mmap`.
    #[test]
    fn gated_shareable_agrees_with_the_predicate_when_enabled() {
        setup();
        assert!(config().shared_file_pages_enabled, "for_test must enable the gate");
        for flags in [
            mmu::user_flags::RO,
            mmu::user_flags::RX,
            mmu::user_flags::RW,
            mmu::user_flags::RW_NO_EXEC,
        ] {
            assert_eq!(
                is_shareable_mapping(flags),
                mmu::user_flags::is_read_only_to_user(flags)
            );
        }
    }
}
