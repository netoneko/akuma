//! Re-export of the kernel callback table, which now lives in `akuma-primitives`.
//!
//! `NetRuntime` moved down on 2026-08-30 (`docs/archive/AKUMA_NET_SPLIT.md`
//! §5.1c step 1). It had to: `net-sockets`, `net-smoltcp` and `net-nic` all need
//! it and none may depend on another, so the table has to sit below all three —
//! and it was already only a `Copy` struct of `fn` pointers behind
//! `Registered`, which lives in `akuma-primitives` too.
//!
//! This module stays as a re-export so the call sites inside this crate keep
//! reading `crate::runtime::runtime()`. It costs nothing and disappears when the
//! crate splits.

pub use akuma_primitives::PreemptGuard;
pub use akuma_primitives::net_runtime::{NetRuntime, register, runtime, try_runtime};
