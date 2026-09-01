//! Shim over [`akuma_vfs_glue::fs`].
//!
//! The sync filesystem facade moved in with the layer it delegates to on
//! 2026-09-01 — it is a thin wrapper over the mount table, and keeping it in the
//! binary meant `src/syscall/` reached 13 symbols across a crate boundary for
//! functions whose whole body is a `with_fs` call.

pub use akuma_vfs_glue::fs::*;
