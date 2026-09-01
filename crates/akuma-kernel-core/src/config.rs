//! Shim over [`akuma_config`].
//!
//! The tunables became a crate on 2026-09-01 so that crates could fold against
//! them instead of being handed them at runtime — see `akuma_config`'s header
//! for the reasoning, including why they are not in `akuma-primitives`.
//!
//! **This file is a re-export and should stay one.** Add new tunables to
//! `crates/akuma-config/src/lib.rs`; adding them here would put a const the
//! crates cannot see in the file people look in.

pub use akuma_config::*;
