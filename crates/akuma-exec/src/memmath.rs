//! Memory arithmetic and the decisions built directly on it.
//!
//! # Why this module exists
//!
//! What's left is the mapping predicates: does a page's AP bits give EL0 write
//! access, and is a mapping eligible for the shared file-page cache. Both used
//! to live in `src/` — the kernel binary, which no host test can reach — so
//! they were checked by booting a VM instead of a unit test.
//! `docs/archive/TRIMMING_FAT_EMBARASSING_DUPLICATIONS.md` §5.11 has the full
//! argument, including why this is a module in `akuma-exec` rather than a new
//! crate: nothing outside `akuma-exec` and `src/` consumes it, so a crate would
//! cut no `cargo tree` edge — the one criterion `akuma-primitives` exists to
//! satisfy.
//!
//! # It used to hold the PMM's arithmetic too
//!
//! The user-page reserve, the reclaim escalation's decision, and the quarantine
//! poison codec all lived here for a stretch (`docs/archive/PMM_EXTRACT.md` §5)
//! — genuinely PMM concepts, parked in `akuma-exec` only because no PMM crate
//! existed yet and `src/` was host-unreachable. They migrated to `akuma-pmm` for
//! real in that plan's §7 Step 6, along with their 8 host tests (the reserve's 3
//! and the poison codec's 4 portable ones; the escalation's decision had already
//! moved in Step 4). Not everything followed: `poison_word_frame`, the thin
//! wrapper that gates the codec on `config().pmm_uaf_quarantine` and supplies
//! the *live* `mmu::ram_base()`/`ram_end()` window, needs this crate's `mmu` —
//! it moved to `src/pmm.rs` instead, next to its one caller
//! (`report_poison_value`), which already lived there for the identical reason.
//!
//! # The config gate
//!
//! `is_shareable_mapping` is gated by a kill switch, and the gate lives here
//! with it rather than as a wrapper in `src/`: `ExecConfig` is *injectable*
//! (`runtime::register_config_for_test`), so a gate is no reason to leave a
//! decision unreachable. `ExecConfig::for_test()` sets it **on**, for the same
//! reason it sets `syscall_debug_info_enabled` — a gate left off makes every
//! test of the gated path skip the branch it exists to cover. The gated
//! function delegates to a **pure predicate**
//! ([`mapping_is_read_only_to_user`]) tested exhaustively on its own, and the
//! gated wrapper is checked for agreeing with it.
//!
//! Fork's own copy-range math (`process::fork_code_start`,
//! `process::fork_page_count_for_len`) stays next to `fork_process`, its only
//! consumer; this module is for arithmetic shared across subsystems.

use crate::mmu;
use crate::runtime::config;

// ============================================================================
// Mapping predicates — this module's only section since Step 6 moved
// everything else out (see the module doc).
// ============================================================================

/// Pure: does a mapping with `map_flags` give EL0 **no write access**? True only
/// for `AP_RO_ALL`, i.e. `mmu::user_flags::RO` and `RX`.
#[must_use]
#[inline]
pub fn mapping_is_read_only_to_user(map_flags: u64) -> bool {
    const AP_MASK: u64 = 3 << 6;
    (map_flags & AP_MASK) == mmu::flags::AP_RO_ALL
}

/// Is a page mapped with `map_flags` eligible for the shared file-page cache?
///
/// A writable private file mapping would need copy-on-write before sharing, so
/// ELF data segments carrying relocations stay private. Gated by
/// `config().shared_file_pages_enabled` (the `SHARED_FILE_PAGES_ENABLED` kill
/// switch), which makes every page ineligible when off.
#[must_use]
#[inline]
pub fn is_shareable_mapping(map_flags: u64) -> bool {
    config().shared_file_pages_enabled && mapping_is_read_only_to_user(map_flags)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() {
        crate::runtime::register_config_for_test();
    }

    #[test]
    fn only_user_read_only_mappings_are_shareable() {
        assert!(mapping_is_read_only_to_user(mmu::user_flags::RO));
        assert!(mapping_is_read_only_to_user(mmu::user_flags::RX));
        assert!(!mapping_is_read_only_to_user(mmu::user_flags::RW));
        assert!(!mapping_is_read_only_to_user(mmu::user_flags::RW_NO_EXEC));
    }

    /// The predicate must read *only* the AP field: a page that is RO to EL0 stays
    /// shareable whatever its execute/attr bits say, and a writable one is never
    /// rescued by them.
    #[test]
    fn predicate_ignores_bits_outside_the_ap_field() {
        let other = mmu::flags::UXN | mmu::flags::PXN | mmu::flags::AF;
        assert!(mapping_is_read_only_to_user(mmu::user_flags::RO | other));
        assert!(!mapping_is_read_only_to_user(mmu::user_flags::RW | other));
    }

    /// With the gate injected **on**, the gated form must agree with the pure
    /// predicate for every flag combination — i.e. the gate adds nothing but the
    /// kill switch.
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
            assert_eq!(is_shareable_mapping(flags), mapping_is_read_only_to_user(flags));
        }
    }
}
