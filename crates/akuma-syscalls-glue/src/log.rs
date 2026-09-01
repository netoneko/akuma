//! Shim over [`akuma_syscalls_log`].
//!
//! The rings moved to a crate on 2026-09-01 to break the `src/syscall/` ↔
//! `src/vfs/` dependency cycle (`docs/archive/SRC_SYSCALL_EXTRACTION.md`
//! Blocker 1). It is now a pure re-export: the `init` that handed `src/config.rs`
//! over went away with `akuma-config`, which the crate reads directly.

// What the binary still calls: the epilogue records, `sys_exit` stamps, and a
// runtime hook hands `get_formatted` to `akuma-exec`. `/proc` reads
// `akuma_syscalls_log` directly — routing *that* back through here is what the
// cycle was.
pub use akuma_syscalls_log::{get_formatted, mark_exited, record};

