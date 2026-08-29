// Unsafe-free by design, and `forbid` so no module can opt back in with a
// local `allow`. Same reasoning as `akuma-syscalls-sync` and `akuma-net-yarn`,
// and spelled here rather than in Cargo.toml for the same reason: a
// crate-local `[lints]` table and `[lints] workspace = true` are mutually
// exclusive.
#![forbid(unsafe_code)]
#![no_std]
//! The `epoll`/`ppoll`/`pselect6` family's pure logic.
//!
//! The second family taken on `AKUMA_EXTRACT_SYSCALLS.md` §8's opportunistic
//! rule, after [`akuma_syscalls_sync`](../akuma_syscalls_sync/index.html), and
//! chosen on the same criterion: **falsifiability**, not size.
//!
//! # Why this family
//!
//! Every epoll incident in `docs/archive/BUG_FIX_LIST.md` except the lock
//! inversion is a state→event-bits mapping or an edge re-arm decision — and
//! every one of them was found by pointing `bun`, `tokio` or `redis` at a live
//! socket and waiting for a hang:
//!
//! | incident | what was actually wrong | now a host test |
//! |---|---|---|
//! | bun HTTPS fetch hang | `EPOLLET`'s edge was not re-armed after a drained `recvfrom`/`recvmsg` | `a_drained_read_rearms_the_in_edge_only_for_et_entries` |
//! | epoll spin on a dead connection | `EPOLLHUP` was not emitted for a fully-closed TCP socket | `a_dead_tcp_socket_reports_hup_whether_or_not_it_was_asked_for` |
//! | a server that never accepts | `EPOLLIN` was never reported for a listening TCP socket | `a_listening_socket_is_readable_through_the_same_can_recv_fact` |
//! | a client that never sees EOF | `EPOLLIN` was not reported after the peer closed | `a_peer_close_reports_in_and_rdhup_together` |
//! | tap RX busy-spin behind a blocking `poll()` | `epoll_check_fd_readiness` had no arm for `FileDescriptor::Tap`, so it fell to the always-ready catch-all | `a_tap_with_no_frame_is_not_ready_unlike_the_catch_all` |
//! | `tokio`'s `read_to_end` waits forever on a pipe at EOF | `pipe_can_read` folds "has bytes" and "at EOF" into one bit, so the EOF transition had no edge | `a_pipe_at_eof_reports_hup_so_the_eof_transition_is_an_edge` |
//!
//! The one that is *not* here is the sixth from that list — `epoll_pwait`
//! computing an absolute deadline instead of a per-iteration sleep. That
//! arithmetic left this file in 2026-08-24 and lives in
//! `akuma_net_yarn::WaitMachine::park_deadline`, which is the whole reason this
//! extraction stops at the readiness edge; see [the wait loop][waitloop] below.
//!
//! [waitloop]: #what-deliberately-did-not-move
//!
//! # The shape: decisions and one data structure
//!
//! Following `akuma-net-yarn`, `akuma-syscalls` and `akuma-syscalls-sync`: no
//! `trait Effects`, no generic effect parameter, no `dyn`. The kernel performs
//! every effect — resolving the fd, probing the resource, registering the
//! waker, taking `EPOLL_TABLE`, masking IRQs, copying user memory, tracing —
//! and calls pure functions in between.
//!
//! The split that matters is in [`readiness`]. `epoll_check_fd_readiness` used
//! to do two jobs in one `match`: *probe* the fd (an effect, and one that can
//! recurse into `PROCESS_TABLE`, a socket's own lock or a rump sysproxy round
//! trip) and *map* what it found onto event bits (pure, and where the bugs
//! were). The kernel keeps the probe and hands the facts over as an
//! [`FdState`](readiness::FdState).
//!
//! # What deliberately did not move
//!
//! - **The wait loop.** It is already extracted, as `akuma-net-yarn`, and it is
//!   driven by four call sites whose policies differ in six fields — each
//!   difference a real divergence. Nothing here touches it. See
//!   `docs/reference/subsystems/syscalls/poll.md` § "The wait loop is one
//!   machine, not three".
//! - **`EPOLL_TABLE`'s lock and its IRQ discipline.** `epoll_reset_edge` is
//!   called from the TCP send/recv path, which under `no-bkl-network` runs with
//!   the BKL dropped; the hold masks local IRQs to avoid an AB-BA against a
//!   nested IRQ that hard-spins for the BKL, and there is a known
//!   `EPOLL_TABLE` ↔ `PROCESS_TABLE` inversion to keep on the right side of
//!   (`docs/archive/EPOLL_PERFORMANCE.md`). That is a locking argument about
//!   *this kernel's* IRQ discipline, not about interest lists. This crate
//!   cannot take a lock, because it has none.
//! - **Every probe.** `socket_can_recv_tcp`, `pipe_hup`, `listener_ready`,
//!   `rump_socket_readable` and the rest read live kernel state; a rump probe
//!   is a sysproxy round trip. They stay next to the state they read.
//! - **The interest-list snapshot.** `sys_epoll_pwait` copies the fd keys into
//!   a 128-entry stack array and only falls back to a heap `Vec` past that, to
//!   keep an allocation out of an IRQ-masked hold. That is an allocation
//!   policy, so it stays in the kernel; this crate exposes
//!   [`InterestList::fds`](interest::InterestList::fds) and lets the caller
//!   choose where to put them.
//! - **The `EBADF` checks.** Resolving `epfd` to a `FileDescriptor::EpollFd`,
//!   and the membership probe that keeps `EBADF` ahead of `EFAULT` in
//!   `epoll_ctl`, are fd-table lookups. Only the errno set *after* the instance
//!   is in hand is here.

extern crate alloc;

pub mod ctl;
pub mod edge;
pub mod fdset;
pub mod interest;
pub mod pollfd;
pub mod readiness;

pub use ctl::{Ctl, CtlOutcome, decode};
pub use edge::{Scan, scan};
pub use interest::{Entry, InterestList};
pub use readiness::{FdState, readiness};

#[cfg(test)]
mod tests;
