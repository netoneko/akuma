//! sqld library - SQLite for Akuma
//!
//! This library provides SQLite database access for Akuma userspace applications
//! through a custom VFS implementation.

#![no_std]

extern crate alloc;

pub mod vfs;
