#![no_std]
// This crate is unsafe-free by design and stays that way: `forbid` (not `deny`)
// so no module can opt back in with a local `allow`. Cargo cannot host this
// per-crate — `[lints] workspace = true` and crate-local lints are mutually
// exclusive — so the ban is spelled here.
#![forbid(unsafe_code)]
//! The unsafe-free core of `akuma-exec`.
//!
//! `akuma-exec` is the tree's largest crate (9,300 production lines) and holds
//! 119 `unsafe` sites, woven through its two central files rather than isolated
//! in a hardware surface — so unlike `akuma-kernel-glue` it cannot be made to
//! `forbid` by lifting one module out (`docs/archive/AKUMA_EXEC_AUDIT.md` §1).
//! This crate is the other direction: pull the parts that are *already* pure
//! **down** into a crate that can carry the ban, and grow it.
//!
//! What qualifies is narrow on purpose: code whose inputs are plain values. If
//! something moved here starts wanting a `Process`, a page table, a lock or the
//! MMU, the seam is in the wrong place — move the seam, do not add the
//! dependency. That rule is why the only dependency is `akuma-primitives`.
//!
//! Extracted 2026-09-01 from `akuma-exec/src/threading/types.rs`, which was
//! already `alloc`-and-`akuma-primitives` only and already carried 199 lines of
//! host tests. `akuma-exec` re-exports this module as
//! `akuma_exec::threading::types`, so every existing call site — including
//! `akuma-exceptions`' `MAX_THREADS` arrays and `akuma-vfs-glue`'s `MAX_CORES`
//! — resolves unchanged.

extern crate alloc;

pub mod thread;
