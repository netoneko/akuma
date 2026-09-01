//! Shim over [`akuma_syscalls_ipc`].
//!
//! The family moved to a crate on 2026-09-01 to break the `src/syscall/` ↔
//! `src/vfs/` dependency cycle (`docs/archive/SRC_SYSCALL_EXTRACTION.md`
//! Blocker 1) — `/proc` lists the queues. This file stays so **`src/config.rs`
//! remains the single source of truth** for the trace toggle, which the crate
//! cannot read; everything else is a re-export and no call site changed.

// The four dispatch arms — always reachable. `/proc` reads
// `akuma_syscalls_ipc::list_msg_queues` directly; routing that back through here
// is what the cycle was.
pub use akuma_syscalls_ipc::{sys_msgctl, sys_msgget, sys_msgrcv, sys_msgsnd};

/// Poller and direct-queue helpers, driven only by the boot suite.
///
/// `kernel_tests`-gated because `-D unused-imports` is on and a `no-tests` build
/// (devbox, devbox-smoltcp) calls none of them — an ungated re-export fails
/// those profiles while `--release` and `extreme-size` stay green, which is the
/// most annoying shape of build break to discover.
#[cfg(kernel_tests)]
pub use akuma_syscalls_ipc::{
    msgqueue_add_recv_poller, msgqueue_add_send_poller, msgqueue_is_recv_poller,
    msgqueue_message_count, msgqueue_pop_direct, msgqueue_push_direct,
    msgqueue_recv_pollers_count, msgqueue_send_pollers_count,
};

/// Hand `src/config.rs`'s trace toggle to the crate. Called once from `kernel_main`.
pub fn init() {
    akuma_syscalls_ipc::init(crate::config::SYSCALL_DEBUG_INFO_ENABLED);
}
