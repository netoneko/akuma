//! Shim over [`akuma_syscalls_log`].
//!
//! The rings moved to a crate on 2026-09-01 to break the `src/syscall/` ↔
//! `src/vfs/` dependency cycle (`docs/archive/SRC_SYSCALL_EXTRACTION.md`
//! Blocker 1). This file stays for one reason: **`src/config.rs` is the single
//! source of truth for the tunables**, and the crate cannot read it. `init` here
//! hands them over; the rest is a re-export, so every call site is unchanged.

// What the binary still calls: the epilogue records, `sys_exit` stamps, and a
// runtime hook hands `get_formatted` to `akuma-exec`. `/proc` reads
// `akuma_syscalls_log` directly — routing *that* back through here is what the
// cycle was.
pub use akuma_syscalls_log::{get_formatted, mark_exited, record};

/// Hand `src/config.rs`'s tunables to the crate. Called once from `kernel_main`.
pub fn init() {
    akuma_syscalls_log::init(akuma_syscalls_log::LogConfig {
        max_entries: crate::config::PROC_SYSCALL_LOG_MAX_ENTRIES,
        retain_ms: crate::config::PROC_SYSCALL_LOG_RETAIN_MS,
    });
}
