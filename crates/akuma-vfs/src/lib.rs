#![cfg_attr(not(test), no_std)]
// This crate is unsafe-free by design and stays that way: `forbid` (not
// `deny`) so no module can opt back in with a local `allow`. Cargo cannot
// host this per-crate — `[lints] workspace = true` and crate-local lints are
// mutually exclusive ("cannot override `workspace.lints` in `lints`"), and
// spelling the ban in Cargo.toml would mean duplicating the whole workspace
// lint table into every crate that wants it.
#![forbid(unsafe_code)]
//! Virtual Filesystem (VFS) Layer
//!
//! Provides the `Filesystem` trait, common types (`FsError`, `DirEntry`, `Metadata`,
//! `FsStats`), path utilities, a mount table, and an in-memory filesystem
//! implementation — all usable in `no_std` environments.

extern crate alloc;

pub mod dev;
mod memfs;
mod mount;
mod path;
mod types;

pub use dev::{DevNode, DevProbe};
pub use memfs::MemoryFilesystem;
pub use mount::{MountSet, MountSnapshot, MountTable, ResolvedMount, MS_RDONLY, MS_REMOUNT, ST_RDONLY};
pub use path::{canonicalize_path, path_components, resolve_path, split_path};
pub use types::{DirEntry, Filesystem, FsError, FsStats, Metadata, MountInfo};

pub const FS_MAX_PATH_SIZE: usize = 512;

#[cfg(test)]
mod tests;
