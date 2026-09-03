//! Shared physical pages for read-only file-backed mappings.
//!
//! The cache itself is [`akuma_fpcache`]; this module is the `src/`-side shim
//! that keeps the ~30 `crate::file_page_cache::…` call sites spelled as they
//! were, exactly as `src/pmm.rs` does for `akuma-pmm`.
//!
//! The one thing that does not travel is configuration: the four tunables are
//! `src/config.rs` `const`s, and the crate sits below the module that owns
//! them, so [`init`] reads them here and hands them over as a
//! [`FpcacheConfig`](akuma_fpcache::FpcacheConfig). `config` therefore remains
//! the single source of truth for the values.

// `len` was dropped from this re-export when `/proc/meminfo` moved to
// `akuma-vfs-glue` and started calling `akuma_fpcache::len` directly. It is back
// for the `[PMM-BUDGET]` line in `akuma-kernel-glue`, which does NOT depend on
// `akuma-fpcache` and should not gain the edge for one diagnostic — it already
// reaches the cache through this shim for `shrink`/`stats_line`. Ungated like its
// two neighbours, and the caller is ungated too, so it cannot read as unused.
pub use akuma_fpcache::{shrink, stats_line};
/// Only the `pmm-instr` `[PMM-BUDGET]` line needs this; ungated it is an
/// unused re-export on profiles built with `-D unused-imports`.
#[cfg(feature = "pmm-instr")]
pub use akuma_fpcache::len;

/// Boot-suite only since `src/fs.rs` moved into `akuma-vfs-glue` and started
/// calling `akuma_fpcache::invalidate_inode` directly — an ungated re-export is
/// an unused import on `no-tests` profiles, which build with `-D unused-imports`.
#[cfg(kernel_tests)]
pub use akuma_fpcache::invalidate_inode;

// The rest of the shim's re-exports: exceptions.rs — their production caller —
// reached `akuma_fpcache` directly on 2026-09-01, and the only `src/` callers
// left are the fpcache boot self-tests in `process_tests.rs`, which `no-tests`
// builds (extreme-size, devbox) compile out while still denying unused
// imports. Same shape as `pmm.rs`'s `cow_ref_count`/`cow_ref_inc` re-exports.
#[allow(unused_imports)]
pub use akuma_fpcache::{insert, is_shareable_mapping, lookup_and_ref, mark_icache_clean};

/// Size the cache from total RAM. Called once from `fs::init`.
///
/// Reads the `config` tunables and publishes them to the crate; see
/// [`akuma_fpcache::init`] for what the sizing means.
pub fn init(total_ram_bytes: usize) {
    akuma_fpcache::init(total_ram_bytes);
}
