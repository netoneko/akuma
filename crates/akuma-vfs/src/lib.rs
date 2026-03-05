#![no_std]
//! Virtual Filesystem (VFS) Layer
//!
//! Provides the `Filesystem` trait, common types (`FsError`, `DirEntry`, `Metadata`,
//! `FsStats`), path utilities, a mount table, and an in-memory filesystem
//! implementation — all usable in `no_std` environments.

extern crate alloc;

mod memfs;
mod mount;
mod path;
mod types;

pub use memfs::MemoryFilesystem;
pub use mount::MountTable;
pub use path::{canonicalize_path, path_components, resolve_path, split_path};
pub use types::{DirEntry, Filesystem, FsError, FsStats, Metadata, MountInfo};

#[cfg(test)]
mod tests;
