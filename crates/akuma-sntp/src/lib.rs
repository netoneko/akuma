// This crate is unsafe-free by design and stays that way: `forbid` (not
// `deny`) so no module can opt back in with a local `allow`.
#![forbid(unsafe_code)]
#![no_std]
//! Pure SNTP (RFC 4330) client protocol ([`sntp`]) and the boot-time
//! bootstrap loop that drives it over caller-supplied effects ([`boot`]).
//!
//! Extracted 2026-09-05 from `akuma-syscalls-time`, which still re-exports
//! both modules unchanged (`pub use akuma_sntp::{boot, sntp};`) — every
//! existing `akuma_syscalls_time::{boot, sntp}` call site is unaffected.
//! Pulled out because a second, architecturally unrelated consumer showed up:
//! `amd64`'s own SNTP-based wall clock (`amd64/src/clock.rs`) needs exactly
//! this protocol/retry logic and nothing else `akuma-syscalls-time` carries —
//! that crate depends on `akuma-exec`, which does not build for
//! `x86_64-unknown-none` at all, so amd64 could not have depended on
//! `akuma-syscalls-time` itself no matter how the one module it wanted was
//! reached. Both `sntp.rs` and `boot.rs` were already fully self-contained
//! (`core` only, no `akuma_exec`/`akuma_net`/anything arch-specific — `boot`'s
//! own module doc says as much: "this crate deliberately has no `akuma-net`
//! dependency"), so this is a pure move, not a rewrite — `[dependencies]`
//! here is empty and stays that way.

pub mod boot;
pub mod sntp;
