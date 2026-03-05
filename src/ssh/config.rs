//! SSH Server Configuration — kernel wrapper.
//!
//! Re-exports `SshdConfig` from `akuma_ssh::config` and provides async
//! filesystem loading/caching.

use spinning_top::Spinlock;

pub use akuma_ssh::config::SshdConfig;

static CACHED_CONFIG: Spinlock<Option<SshdConfig>> = Spinlock::new(None);

/// Get the cached configuration, or default if not loaded yet.
pub fn get_config() -> SshdConfig {
    let guard = CACHED_CONFIG.lock();
    guard.clone().unwrap_or_default()
}
