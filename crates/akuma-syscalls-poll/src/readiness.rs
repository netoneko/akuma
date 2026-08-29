//! The readiness map: already-probed fd state → poll event bits.
//!
//! # The seam
//!
//! `epoll_check_fd_readiness` used to do two jobs in one `match`, and only one
//! of them was testable. It **probed** the fd — resolving it through the
//! process's fd table, registering the caller's waker with the underlying
//! resource, asking a socket whether it can receive, asking the rump server
//! over a sysproxy round trip — and it **mapped** what it found onto
//! `EPOLLIN`/`EPOLLOUT`/`EPOLLHUP`/`EPOLLERR`/`EPOLLRDHUP`.
//!
//! Every incident in this family's history is in the second half. The probes
//! themselves were rarely wrong; what was wrong was which bits a given state
//! earns, whether a bit is maskable, and whether a resource has a state the map
//! forgot about. So the kernel keeps the probe, builds an [`FdState`] out of
//! what it learned, and calls [`readiness`].
//!
//! # Why the `requested` gating is repeated on the kernel side
//!
//! Several kernel arms probe *conditionally* — `PipeWrite` only asks
//! `pipe_can_write` when `EPOLLOUT` was requested, because the same branch also
//! registers a poller, and registering one for an event nobody asked about
//! would add a wakeup source out of nowhere. Those arms therefore report
//! `false` for facts they did not establish. That is safe here and only here:
//! every use of such a fact in this module is already `&&`-ed with the same
//! `requested` bit, so an unprobed `false` and a probed one produce the same
//! answer. The gating is duplicated on purpose — the kernel's copy decides
//! which *effects* run, this one decides which *bits* come out.

use akuma_syscalls_linux::flags::epoll::{EPOLLERR, EPOLLHUP, EPOLLIN, EPOLLOUT, EPOLLRDHUP};

/// What a probe found, per resource kind.
///
/// One variant per `FileDescriptor` arm the kernel actually models, plus the
/// two that are not resources at all: [`Self::Missing`] and
/// [`Self::Unmodelled`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FdState {
    /// The calling process has no fd by that number.
    ///
    /// Indistinguishable, to the caller, from a socket that died — which is why
    /// the kernel traces it. See
    /// `docs/runbooks/cargo-cannot-reach-crates-io.md` § 3.4.
    Missing,

    /// A UDP socket. Both facts are `false` when the socket has no smoltcp
    /// handle, which reports as "nothing ready" rather than as an error.
    Udp { can_recv: bool, can_send: bool },

    /// A TCP socket, connected or connecting.
    ///
    /// `dead` means "not connected and not connecting" and short-circuits
    /// everything else — see [`readiness`].
    Tcp { dead: bool, can_recv: bool, can_send: bool, peer_closed: bool },

    /// An eventfd. Always writable: the counter can always be added to as far
    /// as this kernel is concerned.
    EventFd { can_read: bool },

    /// A child process's stdout, read through its `ProcessChannel`.
    ///
    /// `channel_gone` is the channel having disappeared entirely, which is not
    /// the same as the child having exited — an exited child with buffered
    /// output is still readable, and `has_data` folds that in.
    ChildStdout { channel_gone: bool, has_data: bool },

    /// The read end of a pipe.
    ///
    /// `hup` is the last writer having closed, and it is a separate fact from
    /// `can_read` on purpose — see [`readiness`].
    PipeRead { can_read: bool, hup: bool },

    /// The write end of a pipe.
    PipeWrite { can_write: bool },

    /// A **listening** AF_UNIX socket: readable when its backlog is non-empty,
    /// and never writable.
    UnixListener { accept_ready: bool },

    /// A connected AF_UNIX endpoint, whose two directions are ordinary pipes.
    UnixStream { can_read: bool, can_write: bool },

    /// A timerfd. Readable once it has expirations to report; never writable.
    TimerFd { can_read: bool },

    /// A pidfd. Readable once the tracked process has exited.
    PidFd { can_read: bool },

    /// `stdin`, or `/dev/tty`, which shares fd 0's channel.
    Stdin { has_data: bool },

    /// `stdout`/`stderr`: always writable, never readable.
    Sink,

    /// A rump-box socket. `EPOLLOUT` is assumed — sends are
    /// blocking-synchronous through the sysproxy — and `readable` costs a
    /// round trip to the rump server (a non-blocking `MSG_PEEK`).
    RumpSocket { readable: bool },

    /// The raw `/dev/net/tap0` device the rump server's RX kthread blocks on.
    Tap { has_frame: bool },

    /// An fd kind with no readiness model — a plain file, most of them.
    /// Reported ready for whatever was asked, which is the POSIX answer for a
    /// regular file and a lie for anything else.
    ///
    /// It being the *default* is what made `Tap` a busy-spin: a
    /// `/dev/net/tap0` that fell through to here answered "a frame is waiting"
    /// on every call, turning a blocking-looking `poll()` into a spin. A new
    /// pollable fd kind must get its own variant, not this one.
    Unmodelled,
}

/// Map a probed state onto the poll event bits, masked by what was requested.
///
/// # Which bits are maskable
///
/// `EPOLLIN`, `EPOLLOUT` and `EPOLLRDHUP` are reported only when asked for.
/// `EPOLLHUP` and `EPOLLERR` are reported **whether or not** they were
/// requested — Linux does the same, and they are never maskable there either.
///
/// That is not parity for its own sake. `EPOLLHUP` is the only state change an
/// edge-triggered reader gets between "drained, writer still alive" and EOF:
/// `pipe_can_read` is already true in both, folding "has bytes" and "at EOF"
/// into one `EPOLLIN`, so without a distinct bit `revents & !last_ready` is 0
/// and the EOF edge is swallowed. `tokio`'s `read_to_end` then waits forever on
/// a pipe that is sitting at EOF. See `docs/archive/TOKIO_PIPE_EPOLL_HANG.md`.
#[must_use]
pub const fn readiness(state: FdState, requested: u32) -> u32 {
    let wants_in = requested & EPOLLIN != 0;
    let wants_out = requested & EPOLLOUT != 0;
    let wants_rdhup = requested & EPOLLRDHUP != 0;

    let mut ready = 0u32;
    match state {
        // Unmaskable, and both bits: a caller polling an fd it does not have is
        // told the fd is finished, not that nothing happened, so it cannot
        // block forever on a number that will never be ready.
        FdState::Missing => return EPOLLHUP | EPOLLERR,

        FdState::Udp { can_recv, can_send } => {
            if wants_in && can_recv {
                ready |= EPOLLIN;
            }
            if wants_out && can_send {
                ready |= EPOLLOUT;
            }
        }

        // A fully dead socket reports HUP and *only* HUP. Reporting readiness
        // bits alongside it would put a caller into a read loop on a socket
        // that can never produce anything — the epoll spin this bit was added
        // to end.
        FdState::Tcp { dead: true, .. } => return EPOLLHUP,
        FdState::Tcp { dead: false, can_recv, can_send, peer_closed } => {
            if wants_in && can_recv {
                ready |= EPOLLIN;
            }
            if wants_out && can_send {
                ready |= EPOLLOUT;
            }
            // Reported *with* EPOLLIN, not instead of it: a peer that closed
            // after sending leaves readable bytes behind, and a caller told
            // only "read-closed" discards them.
            if wants_rdhup && peer_closed {
                ready |= EPOLLRDHUP;
            }
        }

        FdState::EventFd { can_read } => {
            if wants_in && can_read {
                ready |= EPOLLIN;
            }
            if wants_out {
                ready |= EPOLLOUT;
            }
        }

        FdState::ChildStdout { channel_gone, has_data } => {
            if wants_in {
                if channel_gone {
                    ready |= EPOLLHUP;
                } else if has_data {
                    ready |= EPOLLIN;
                }
            }
        }

        FdState::PipeRead { can_read, hup } => {
            if wants_in && can_read {
                ready |= EPOLLIN;
            }
            if hup {
                ready |= EPOLLHUP;
            }
        }

        FdState::PipeWrite { can_write } => {
            if wants_out && can_write {
                ready |= EPOLLOUT;
            }
        }

        // No EPOLLOUT arm, deliberately: a listener has no transmit side, and
        // it is checked before the stream arms because its `rx`/`tx` pipe ids
        // are 0 — falling through to `UnixStream` would ask `pipe_can_read(0)`,
        // get `false`, and leave an accept-ready listener polling as "nothing"
        // forever.
        FdState::UnixListener { accept_ready } => {
            if wants_in && accept_ready {
                ready |= EPOLLIN;
            }
        }
        FdState::UnixStream { can_read, can_write } => {
            if wants_in && can_read {
                ready |= EPOLLIN;
            }
            if wants_out && can_write {
                ready |= EPOLLOUT;
            }
        }

        FdState::TimerFd { can_read } | FdState::PidFd { can_read } => {
            if wants_in && can_read {
                ready |= EPOLLIN;
            }
        }

        FdState::Stdin { has_data } => {
            if wants_in && has_data {
                ready |= EPOLLIN;
            }
        }

        FdState::Sink => {
            if wants_out {
                ready |= EPOLLOUT;
            }
        }

        FdState::RumpSocket { readable } => {
            if wants_in && readable {
                ready |= EPOLLIN;
            }
            if wants_out {
                ready |= EPOLLOUT;
            }
        }
        FdState::Tap { has_frame } => {
            if wants_in && has_frame {
                ready |= EPOLLIN;
            }
            if wants_out {
                ready |= EPOLLOUT;
            }
        }

        FdState::Unmodelled => {
            if wants_in {
                ready |= EPOLLIN;
            }
            if wants_out {
                ready |= EPOLLOUT;
            }
        }
    }
    ready
}
