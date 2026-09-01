//! Shim over [`akuma_syscalls_ipc`].
//!
//! The family moved to a crate on 2026-09-01 to break the `src/syscall/` ↔
//! `src/vfs/` dependency cycle (`docs/archive/SRC_SYSCALL_EXTRACTION.md`
//! Blocker 1) — `/proc` lists the queues. It is now a pure re-export: the `init`
//! that handed the trace toggle over went away with `akuma-config`.

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

