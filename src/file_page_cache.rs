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

pub use akuma_fpcache::{
    insert, invalidate_inode, is_shareable_mapping, len, lookup_and_ref, mark_icache_clean,
    shrink, stats_line,
};

/// Size the cache from total RAM. Called once from `fs::init`.
///
/// Reads the `config` tunables and publishes them to the crate; see
/// [`akuma_fpcache::init`] for what the sizing means.
pub fn init(total_ram_bytes: usize) {
    akuma_fpcache::init(
        total_ram_bytes,
        akuma_fpcache::FpcacheConfig {
            enabled: crate::config::SHARED_FILE_PAGES_ENABLED,
            base_ram_divisor: crate::config::FPCACHE_BASE_RAM_DIVISOR,
            inflate_pct: crate::config::FPCACHE_INFLATE_PCT,
            inflate_headroom_mult: crate::config::FPCACHE_INFLATE_HEADROOM_MULT,
        },
    );
}
