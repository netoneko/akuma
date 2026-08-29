//! Host tests for the epoll/poll/select family.
//!
//! Each test is named for the thing that actually went wrong, not for the
//! method it calls. Every incident named here previously cost a live VM, a
//! network client (`bun`, `tokio`, `nginx`, `redis`, cargo's libcurl) and a
//! wait to see whether it hung.

use akuma_primitives::errno::negated::{EINVAL, ENOENT};
use akuma_syscalls_linux::flags::epoll::{
    EPOLLERR, EPOLLET, EPOLLHUP, EPOLLIN, EPOLLONESHOT, EPOLLOUT, EPOLLRDHUP, EPOLL_EVENT_MASK,
};
use akuma_syscalls_linux::flags::poll::{POLLERR, POLLHUP, POLLIN, POLLOUT, POLLPRI, POLLRDHUP};

use crate::ctl::{Ctl, CtlOutcome, decode};
use crate::edge::{Scan, scan};
use crate::fdset;
use crate::interest::InterestList;
use crate::pollfd;
use crate::readiness::{FdState, readiness};

/// Everything this kernel can report, for the "was it maskable?" tests.
const ALL: u32 = EPOLL_EVENT_MASK;
/// Nothing requested at all — what a registration of `EPOLLET` alone amounts to.
const NONE: u32 = 0;

// ---------------------------------------------------------------------------
// readiness: the state -> bits map
// ---------------------------------------------------------------------------

/// Bug 5 in `BUG_FIX_LIST.md`: a fully-closed TCP socket emitted no `EPOLLHUP`,
/// so an epoll loop watching a dead connection spun — readiness never arrived
/// and nothing ever said the fd was finished.
///
/// The bit is unmaskable, which is the half that is easy to get wrong: a caller
/// that registered `EPOLLIN` only must still be told.
#[test]
fn a_dead_tcp_socket_reports_hup_whether_or_not_it_was_asked_for() {
    let dead = FdState::Tcp { dead: true, can_recv: false, can_send: false, peer_closed: false };
    assert_eq!(readiness(dead, EPOLLIN), EPOLLHUP);
    assert_eq!(readiness(dead, NONE), EPOLLHUP);
    assert_eq!(readiness(dead, ALL), EPOLLHUP);
}

/// The other half of Bug 5: a dead socket reports `EPOLLHUP` and *nothing else*.
///
/// Adding readiness bits alongside it is what the spin was — a caller told
/// "readable" on a socket that can never produce a byte reads, gets nothing,
/// and comes straight back.
#[test]
fn a_dead_socket_reports_only_hup_so_a_caller_cannot_spin_on_epollin() {
    // Even with the probe claiming both directions are live, `dead` wins.
    let dead = FdState::Tcp { dead: true, can_recv: true, can_send: true, peer_closed: true };
    assert_eq!(readiness(dead, ALL), EPOLLHUP);
}

/// Bug 1: `EPOLLIN` was never reported for a *listening* TCP socket, so no
/// epoll-driven server could ever accept.
///
/// The fix was in the probe (`socket_can_recv_tcp` answering for a listener
/// with a non-empty backlog); what this pins is the half in the map — there is
/// no separate "listening" state here, and adding one would re-open it.
#[test]
fn a_listening_socket_is_readable_through_the_same_can_recv_fact() {
    let listening =
        FdState::Tcp { dead: false, can_recv: true, can_send: false, peer_closed: false };
    assert_eq!(readiness(listening, EPOLLIN), EPOLLIN);
}

/// Bug 4: `EPOLLIN` was not reported after the remote peer closed.
///
/// A peer that closes after sending leaves readable bytes behind, so the two
/// bits are reported **together**. A map that returned `EPOLLRDHUP` instead of
/// `EPOLLIN` makes a client discard the last response it was sent.
#[test]
fn a_peer_close_reports_in_and_rdhup_together() {
    let closed =
        FdState::Tcp { dead: false, can_recv: true, can_send: true, peer_closed: true };
    assert_eq!(readiness(closed, EPOLLIN | EPOLLRDHUP), EPOLLIN | EPOLLRDHUP);
    // `EPOLLRDHUP` is maskable, unlike HUP/ERR: a caller that never asked for
    // it gets `EPOLLIN` alone.
    assert_eq!(readiness(closed, EPOLLIN), EPOLLIN);
}

/// A socket still in `SynSent` answers `is_active() && !may_recv()` — the same
/// pair a peer's FIN produces. Reporting that as read-closed told a tokio/hyper
/// client that a *connecting* socket was already dead, and it parked forever
/// without ever sending its request (~1 run in 3 at a 64 KiB POST body).
///
/// The fix gates both predicates on `tcp_reached_established`, in the probe.
/// What the map must not do is manufacture either bit from the absence of the
/// other.
#[test]
fn a_connecting_socket_reports_nothing_rather_than_read_closed() {
    let connecting =
        FdState::Tcp { dead: false, can_recv: false, can_send: false, peer_closed: false };
    assert_eq!(readiness(connecting, ALL), 0);
}

/// Fix #10: `epoll_check_fd_readiness` had no arm for `FileDescriptor::Tap`, so
/// `/dev/net/tap0` fell through to the always-ready catch-all — and the rump
/// server's RX kthread, which blocks on exactly that fd for every inbound
/// frame, got an instant return every time. A busy-spin hidden behind a
/// blocking-looking `poll()`.
///
/// The catch-all is asserted alongside it, because the bug was not that the
/// catch-all is wrong — it is right for a plain file — but that a pollable fd
/// kind reached it.
#[test]
fn a_tap_with_no_frame_is_not_ready_unlike_the_catch_all() {
    assert_eq!(readiness(FdState::Tap { has_frame: false }, EPOLLIN), 0);
    assert_eq!(readiness(FdState::Tap { has_frame: true }, EPOLLIN), EPOLLIN);
    // ...and the default it used to take.
    assert_eq!(readiness(FdState::Unmodelled, EPOLLIN), EPOLLIN);
    // Both are unconditionally writable, which is the part that made the bug
    // hard to spot: only the read direction differed.
    assert_eq!(readiness(FdState::Tap { has_frame: false }, EPOLLOUT), EPOLLOUT);
}

/// `TOKIO_PIPE_EPOLL_HANG.md`: `pipe_can_read` answers true both for "has
/// bytes" and "at EOF", so an edge-triggered reader that drained a child's
/// stdout and went back for EOF saw no new bit and hung — a healthy-looking
/// process that had simply stopped.
///
/// `EPOLLHUP` is the bit that makes the EOF transition an edge at all, and like
/// the TCP one it is unmaskable.
#[test]
fn a_pipe_at_eof_reports_hup_so_the_eof_transition_is_an_edge() {
    let drained_eof = FdState::PipeRead { can_read: true, hup: true };
    assert_eq!(readiness(drained_eof, EPOLLIN), EPOLLIN | EPOLLHUP);
    assert_eq!(readiness(drained_eof, NONE), EPOLLHUP);

    // Writer still alive: the same `can_read` fact, no HUP — which is exactly
    // why one bit could not carry both states.
    let has_bytes = FdState::PipeRead { can_read: true, hup: false };
    assert_eq!(readiness(has_bytes, EPOLLIN), EPOLLIN);
}

/// A poll on an fd the calling process does not have is answered
/// `EPOLLHUP|EPOLLERR`, unmaskably, so the caller cannot block forever on a
/// number that will never be ready.
#[test]
fn a_missing_fd_reports_hup_and_err_unmaskably() {
    assert_eq!(readiness(FdState::Missing, NONE), EPOLLHUP | EPOLLERR);
    assert_eq!(readiness(FdState::Missing, EPOLLOUT), EPOLLHUP | EPOLLERR);
}

/// A listening AF_UNIX socket has no transmit side. It is checked ahead of the
/// stream arms because its `rx`/`tx` pipe ids are 0: falling through would ask
/// `pipe_can_read(0)`, get `false`, and leave an accept-ready listener polling
/// as "nothing" forever while every event-loop server on it hung.
#[test]
fn a_unix_listener_is_readable_but_never_writable() {
    let ready = FdState::UnixListener { accept_ready: true };
    assert_eq!(readiness(ready, EPOLLIN | EPOLLOUT), EPOLLIN);
    assert_eq!(readiness(ready, EPOLLOUT), 0);
    assert_eq!(readiness(FdState::UnixListener { accept_ready: false }, EPOLLIN), 0);
}

/// A child that exited with buffered output is still readable; a child whose
/// channel is gone entirely is a hangup. Both are gated on `EPOLLIN` having
/// been asked for, which is where this arm differs from the pipe one.
#[test]
fn a_lost_child_channel_is_a_hangup_but_a_buffered_exit_is_still_readable() {
    assert_eq!(
        readiness(FdState::ChildStdout { channel_gone: true, has_data: false }, EPOLLIN),
        EPOLLHUP
    );
    assert_eq!(
        readiness(FdState::ChildStdout { channel_gone: false, has_data: true }, EPOLLIN),
        EPOLLIN
    );
    assert_eq!(
        readiness(FdState::ChildStdout { channel_gone: true, has_data: false }, EPOLLOUT),
        0
    );
}

/// No state may produce a *registration* bit. `EPOLLET` is `1 << 31`; echoed
/// back in a `revents` it reads to userspace as an event.
#[test]
fn no_state_ever_reports_a_registration_only_bit() {
    let states = [
        FdState::Missing,
        FdState::Udp { can_recv: true, can_send: true },
        FdState::Tcp { dead: true, can_recv: true, can_send: true, peer_closed: true },
        FdState::Tcp { dead: false, can_recv: true, can_send: true, peer_closed: true },
        FdState::EventFd { can_read: true },
        FdState::ChildStdout { channel_gone: true, has_data: true },
        FdState::PipeRead { can_read: true, hup: true },
        FdState::PipeWrite { can_write: true },
        FdState::UnixListener { accept_ready: true },
        FdState::UnixStream { can_read: true, can_write: true },
        FdState::TimerFd { can_read: true },
        FdState::PidFd { can_read: true },
        FdState::Stdin { has_data: true },
        FdState::Sink,
        FdState::RumpSocket { readable: true },
        FdState::Tap { has_frame: true },
        FdState::Unmodelled,
    ];
    for s in states {
        let r = readiness(s, u32::MAX);
        assert_eq!(r & EPOLLET, 0, "{s:?} reported EPOLLET");
        assert_eq!(r & EPOLLONESHOT, 0, "{s:?} reported EPOLLONESHOT");
        assert_eq!(r & !EPOLL_EVENT_MASK, 0, "{s:?} reported a bit outside EPOLL_EVENT_MASK");
    }
}

/// The kernel probes some facts only when the matching bit was requested, and
/// reports `false` for the rest. That is only safe because every use of such a
/// fact here is already `&&`-ed with the same bit — so an unprobed `false` and
/// a probed `true` give the same answer when the bit was not asked for.
#[test]
fn an_unprobed_fact_cannot_change_the_answer_for_an_unrequested_bit() {
    // PipeWrite's `can_write` is probed only under EPOLLOUT.
    assert_eq!(readiness(FdState::PipeWrite { can_write: true }, EPOLLIN), 0);
    assert_eq!(readiness(FdState::PipeWrite { can_write: false }, EPOLLIN), 0);
    // TimerFd/PidFd/Stdin, likewise, under EPOLLIN.
    assert_eq!(readiness(FdState::TimerFd { can_read: true }, EPOLLOUT), 0);
    assert_eq!(readiness(FdState::PidFd { can_read: true }, EPOLLOUT), 0);
    assert_eq!(readiness(FdState::Stdin { has_data: true }, EPOLLOUT), 0);
}

/// An eventfd and a rump socket are always writable; stdout/stderr are writable
/// and never readable. Stated because each is an assumption, not a probe.
#[test]
fn the_always_writable_states_are_writable_and_nothing_more() {
    assert_eq!(readiness(FdState::EventFd { can_read: false }, ALL), EPOLLOUT);
    assert_eq!(readiness(FdState::RumpSocket { readable: false }, ALL), EPOLLOUT);
    assert_eq!(readiness(FdState::Sink, ALL), EPOLLOUT);
}

// ---------------------------------------------------------------------------
// edge: the EPOLLET armed-state decision
// ---------------------------------------------------------------------------

/// A level-triggered entry reports whatever is ready on every pass and stores
/// nothing — a caller that does not drain is told again, which is the contract.
#[test]
fn a_level_triggered_entry_reports_every_pass_and_records_nothing() {
    let s = scan(EPOLLIN, EPOLLIN, EPOLLIN);
    assert_eq!(s, Scan { report: EPOLLIN, record: None });
}

/// The bug that is one character away: recording the *reported subset* rather
/// than the full `revents`.
///
/// A bit that stays ready across two passes would then drop out of the mask on
/// the second pass and re-fire on the third, so an edge-triggered fd would
/// spuriously re-arm itself every other pass — the opposite failure from a lost
/// edge, and just as silent.
#[test]
fn an_edge_triggered_entry_records_the_full_revents_not_the_reported_subset() {
    // First pass: nothing recorded yet, both bits are new.
    let first = scan(EPOLLET | EPOLLIN | EPOLLOUT, EPOLLIN | EPOLLOUT, 0);
    assert_eq!(first, Scan { report: EPOLLIN | EPOLLOUT, record: Some(EPOLLIN | EPOLLOUT) });

    // Second pass, still ready: nothing new, and the record must stay complete.
    let second = scan(EPOLLET | EPOLLIN | EPOLLOUT, EPOLLIN | EPOLLOUT, EPOLLIN | EPOLLOUT);
    assert_eq!(second, Scan { report: 0, record: Some(EPOLLIN | EPOLLOUT) });

    // Third pass: had the record been the reported subset (0), this would
    // report both bits again.
    let third = scan(EPOLLET | EPOLLIN | EPOLLOUT, EPOLLIN | EPOLLOUT, EPOLLIN | EPOLLOUT);
    assert_eq!(third.report, 0);
}

/// Readiness *going away* has to be written down, or the next arrival is not a
/// new bit and the edge is lost.
#[test]
fn readiness_going_away_is_recorded_so_the_next_arrival_is_a_new_edge() {
    let gone = scan(EPOLLET | EPOLLIN, 0, EPOLLIN);
    assert_eq!(gone, Scan { report: 0, record: Some(0) });

    let back = scan(EPOLLET | EPOLLIN, EPOLLIN, 0);
    assert_eq!(back.report, EPOLLIN);
}

/// A partially-new mask reports only the new half.
#[test]
fn only_the_bits_that_were_not_ready_last_pass_are_reported() {
    let s = scan(EPOLLET | EPOLLIN | EPOLLOUT, EPOLLIN | EPOLLOUT, EPOLLIN);
    assert_eq!(s, Scan { report: EPOLLOUT, record: Some(EPOLLIN | EPOLLOUT) });
}

// ---------------------------------------------------------------------------
// ctl + interest list
// ---------------------------------------------------------------------------

#[test]
fn only_add_and_mod_read_an_event_struct_from_userspace() {
    assert!(decode(1).needs_event()); // EPOLL_CTL_ADD
    assert!(decode(3).needs_event()); // EPOLL_CTL_MOD
    // DEL's fourth argument has been ignored by Linux since 2.6.9 and callers
    // pass NULL; reading it would turn a correct program into EFAULT.
    assert!(!decode(2).needs_event()); // EPOLL_CTL_DEL
    assert!(!decode(99).needs_event());
    assert_eq!(decode(99), Ctl::Unknown);
}

/// Known divergence, pinned so a change to it is deliberate: Linux answers
/// `EEXIST` and leaves the registration alone; this kernel overwrites it, resets
/// the edge state, and answers success.
///
/// A program that *tests* for `EEXIST` to discover whether it already
/// registered an fd concludes it has not.
#[test]
fn an_add_on_a_present_fd_overwrites_instead_of_reporting_eexist() {
    let mut list = InterestList::new();
    assert_eq!(list.apply(Ctl::Add, 7, EPOLLIN, 0xaaaa), CtlOutcome::Added);
    list.record_ready(7, EPOLLIN);

    let again = list.apply(Ctl::Add, 7, EPOLLOUT, 0xbbbb);
    assert_eq!(again, CtlOutcome::AddedOverExisting);
    assert_eq!(again.errno(), 0, "not EEXIST — see CtlOutcome::errno");
    assert_eq!(again.trace_tag(), Some("ADD->MOD"));

    let e = list.get(7).expect("still registered");
    assert_eq!(e.events, EPOLLOUT);
    assert_eq!(e.data, 0xbbbb);
    assert_eq!(e.last_ready, 0, "the edge state is reset, as MOD does");
    assert_eq!(list.len(), 1, "an overwrite, not a second entry");
}

#[test]
fn mod_and_del_on_an_absent_fd_are_enoent_and_an_unknown_op_is_einval() {
    let mut list = InterestList::new();
    assert_eq!(list.apply(Ctl::Mod, 3, EPOLLIN, 0).errno(), ENOENT);
    assert_eq!(list.apply(Ctl::Del, 3, 0, 0).errno(), ENOENT);
    assert_eq!(list.apply(Ctl::Unknown, 3, 0, 0).errno(), EINVAL);
    assert!(list.is_empty());
}

/// A `MOD` clearing `last_ready` is what makes re-registering the documented
/// way to re-arm an edge-triggered fd by hand.
#[test]
fn a_mod_resets_the_edge_state_so_a_re_registration_re_arms() {
    let mut list = InterestList::new();
    list.apply(Ctl::Add, 4, EPOLLET | EPOLLIN, 1);
    list.record_ready(4, EPOLLIN);
    assert_eq!(scan(EPOLLET | EPOLLIN, EPOLLIN, list.get(4).unwrap().last_ready).report, 0);

    assert_eq!(list.apply(Ctl::Mod, 4, EPOLLET | EPOLLIN, 1), CtlOutcome::Modified);
    assert_eq!(scan(EPOLLET | EPOLLIN, EPOLLIN, list.get(4).unwrap().last_ready).report, EPOLLIN);
}

/// The `bun` HTTPS hang: `EPOLLET`'s `EPOLLIN` edge was not re-armed after a
/// drained `recvfrom`/`recvmsg`, so a caller that read one TLS record at a time
/// and never drained to `EAGAIN` never saw `EPOLLIN` fire again for data that
/// had arrived in the same poll window.
///
/// The hook runs on *every* successful read as well as every `EAGAIN`, which is
/// what "never drains to EAGAIN" forces, and it touches only edge-triggered
/// entries — a level-triggered one never consults `last_ready` at all.
#[test]
fn a_drained_read_rearms_the_in_edge_only_for_et_entries() {
    let mut list = InterestList::new();
    list.apply(Ctl::Add, 5, EPOLLET | EPOLLIN | EPOLLOUT, 0);
    list.record_ready(5, EPOLLIN | EPOLLOUT);

    assert!(list.reset_edge(5, EPOLLIN));
    let e = list.get(5).unwrap();
    assert_eq!(e.last_ready, EPOLLOUT, "only the read edge is re-armed");
    assert_eq!(scan(e.events, EPOLLIN, e.last_ready).report, EPOLLIN);

    // Level-triggered: nothing to re-arm, and saying so is the point.
    let mut lt = InterestList::new();
    lt.apply(Ctl::Add, 6, EPOLLIN, 0);
    lt.record_ready(6, EPOLLIN);
    assert!(!lt.reset_edge(6, EPOLLIN));
    assert!(!lt.reset_edge(99, EPOLLIN), "an unregistered fd is a no-op");
}

/// The mirror hook, and the one whose absence was an *intermittent* hang: a
/// client that filled the 16 KB TCP transmit buffer and waited for `EPOLLOUT`
/// could wait forever, because `epoll_pwait` drives `smoltcp_net::poll()` at the
/// top of its own loop and usually flushed the buffer before `can_send()` was
/// ever observed false. Reproduced with `nettest-reqwest post <url> 64`, 2 runs
/// in 3 at a 64 KiB body.
#[test]
fn a_short_write_rearms_the_out_edge_without_disturbing_the_in_edge() {
    let mut list = InterestList::new();
    list.apply(Ctl::Add, 5, EPOLLET | EPOLLIN | EPOLLOUT, 0);
    list.record_ready(5, EPOLLIN | EPOLLOUT);

    assert!(list.reset_edge(5, EPOLLOUT));
    assert_eq!(list.get(5).unwrap().last_ready, EPOLLIN);
}

/// Real Linux drops an fd from every interest list the instant it is `close()`d.
/// This kernel's `close()` does not, so `epoll_pwait` prunes — and it must,
/// because an unpruned entry resolves to `FdState::Missing` and synthesises
/// `EPOLLHUP|EPOLLERR` for a fd the caller already closed, an event real Linux
/// can never produce.
///
/// nginx creates, registers and closes a socketpair in one breath; its
/// crash-recovery path ORs `EPOLLIN|EPOLLOUT` into `revents` on HUP/ERR and
/// dereferenced a connection object it had torn down with the fd.
#[test]
fn pruning_a_closed_fd_is_what_stops_a_synthetic_hup_err() {
    let mut list = InterestList::new();
    list.apply(Ctl::Add, 9, EPOLLIN, 0);

    // What the scan would report if the entry survived the close.
    assert_eq!(readiness(FdState::Missing, EPOLLIN), EPOLLHUP | EPOLLERR);

    assert!(list.prune(9));
    assert!(!list.prune(9), "pruning twice is a no-op, not an error");
    assert!(list.is_empty());
}

/// `epoll_pwait` fills the caller's buffer by walking this order and stops when
/// it is full, so with more ready fds than `maxevents` the order decides who is
/// reported. Ascending, because the backing map is ordered — a divergence from
/// Linux's fairer ready-list rotation, and one worth knowing is deterministic.
#[test]
fn the_interest_list_is_walked_in_ascending_fd_order() {
    let mut list = InterestList::new();
    for fd in [42, 3, 17, 1] {
        list.apply(Ctl::Add, fd, EPOLLIN, 0);
    }
    let fds: alloc::vec::Vec<u32> = list.fds().collect();
    assert_eq!(fds, alloc::vec![1, 3, 17, 42]);
}

/// `Entry::requested` must not hand a registration bit to a readiness probe,
/// and must not let one back out.
#[test]
fn a_registration_word_is_masked_before_it_reaches_a_probe() {
    let mut list = InterestList::new();
    list.apply(Ctl::Add, 1, EPOLLET | EPOLLONESHOT | EPOLLIN, 0);
    let e = list.get(1).unwrap();
    assert!(e.is_edge_triggered());
    assert_eq!(e.requested(), EPOLLIN);
}

// ---------------------------------------------------------------------------
// fdset: select(2) marshalling
// ---------------------------------------------------------------------------

#[test]
fn nfds_above_the_hard_cap_is_rejected() {
    assert!(fdset::nfds_ok(0));
    assert!(fdset::nfds_ok(fdset::MAX_FDS));
    // A cap `ppoll`/`epoll` do not have: the same program `select()`s and gets
    // EINVAL where it would `poll()` fine.
    assert!(!fdset::nfds_ok(fdset::MAX_FDS + 1));
}

/// The copy is in whole words, so a `nfds` that is not a multiple of 64 still
/// moves the whole partial word — which is why the write-back buffers have to
/// be zeroed rather than assumed clean.
#[test]
fn the_word_count_covers_the_partial_last_word() {
    assert_eq!(fdset::words(0), 0);
    assert_eq!(fdset::bytes(0), 0);
    assert_eq!(fdset::words(1), 1);
    assert_eq!(fdset::bytes(1), 8);
    assert_eq!(fdset::words(64), 1);
    assert_eq!(fdset::words(65), 2);
    assert_eq!(fdset::words(fdset::MAX_FDS), fdset::MAX_WORDS);
}

/// `select(2)` returns the number of **bits** left set across all three sets,
/// not the number of fds. A caller that sized a loop by the return value and
/// got one per fd would stop early.
#[test]
fn an_fd_ready_in_both_directions_counts_twice() {
    let mut read = [0u64; fdset::MAX_WORDS];
    let mut write = [0u64; fdset::MAX_WORDS];
    fdset::set(&mut read, 5);
    fdset::set(&mut write, 5);

    let interests: alloc::vec::Vec<_> = fdset::interests(&read, &write, 64).collect();
    assert_eq!(interests.len(), 1);
    assert_eq!(interests[0].requested(), EPOLLIN | EPOLLOUT);

    let mut out_read = [0u64; fdset::MAX_WORDS];
    let mut out_write = [0u64; fdset::MAX_WORDS];
    let n = interests[0].record(EPOLLIN | EPOLLOUT, &mut out_read, &mut out_write);
    assert_eq!(n, 2);
    assert!(fdset::is_set(&out_read, 5));
    assert!(fdset::is_set(&out_write, 5));
}

/// An fd asked about in `readfds` only is never reported writable, however the
/// probe answers — and many fd kinds answer `EPOLLOUT` unconditionally.
#[test]
fn a_read_only_interest_is_never_reported_writable() {
    let mut read = [0u64; fdset::MAX_WORDS];
    let write = [0u64; fdset::MAX_WORDS];
    fdset::set(&mut read, 2);

    let i = fdset::interests(&read, &write, 64).next().unwrap();
    assert_eq!(i.requested(), EPOLLIN);

    let mut out_read = [0u64; fdset::MAX_WORDS];
    let mut out_write = [0u64; fdset::MAX_WORDS];
    // `Sink`-like answer: writable, not readable.
    assert_eq!(i.record(EPOLLOUT, &mut out_read, &mut out_write), 0);
    assert!(!fdset::is_set(&out_write, 2));
}

/// Bits set past `nfds` are never probed — probing registers a waker, so an fd
/// outside the caller's range would gain a wakeup source out of nowhere.
#[test]
fn bits_at_or_above_nfds_are_never_probed() {
    let mut read = [0u64; fdset::MAX_WORDS];
    let write = [0u64; fdset::MAX_WORDS];
    fdset::set(&mut read, 3);
    fdset::set(&mut read, 70); // same word count, past nfds

    let fds: alloc::vec::Vec<usize> = fdset::set_fds(&read, &write, 64).collect();
    assert_eq!(fds, alloc::vec![3]);
}

/// Ascending, across the word boundary, and each fd yielded once even when it
/// is in both sets.
#[test]
fn interests_are_yielded_once_each_in_ascending_order() {
    let mut read = [0u64; fdset::MAX_WORDS];
    let mut write = [0u64; fdset::MAX_WORDS];
    fdset::set(&mut read, 0);
    fdset::set(&mut read, 63);
    fdset::set(&mut write, 63);
    fdset::set(&mut write, 64);
    fdset::set(&mut read, 200);

    let fds: alloc::vec::Vec<usize> = fdset::set_fds(&read, &write, 256).collect();
    assert_eq!(fds, alloc::vec![0, 63, 64, 200]);
}

// ---------------------------------------------------------------------------
// pollfd: poll(2) marshalling
// ---------------------------------------------------------------------------

/// `POLLHUP`/`POLLERR` cannot be requested and are always possible in
/// `revents`. A `poll()` that reported a hangup only when asked would leave a
/// caller waiting on a dead fd forever.
#[test]
fn pollhup_and_pollerr_are_reported_without_being_requested() {
    assert_eq!(pollfd::report(POLLIN, EPOLLHUP), POLLHUP);
    assert_eq!(pollfd::report(0, EPOLLHUP | EPOLLERR), POLLHUP | POLLERR);
    // ...while POLLIN/POLLOUT are masked by what was asked.
    assert_eq!(pollfd::report(POLLIN, EPOLLIN | EPOLLOUT), POLLIN);
    assert_eq!(pollfd::report(POLLOUT, EPOLLIN | EPOLLOUT), POLLOUT);
}

/// Known divergence: `POLLRDHUP` is dropped in both directions, so `poll()`
/// never reports a half-close that `epoll` on the same socket does. `POLLPRI`
/// likewise, though that one is honest — Akuma has no out-of-band TCP data.
#[test]
fn pollrdhup_and_pollpri_are_dropped_in_both_directions() {
    assert_eq!(pollfd::requested(POLLRDHUP), 0);
    assert_eq!(pollfd::requested(POLLPRI), 0);
    assert_eq!(pollfd::requested(POLLIN | POLLRDHUP), EPOLLIN);
    assert_eq!(pollfd::report(POLLIN | POLLRDHUP, EPOLLIN | EPOLLRDHUP), POLLIN);
}

/// Known divergence: an fd the process does not have reaches the map as
/// `Missing` and comes back `EPOLLHUP|EPOLLERR`, so `poll()` reports
/// `POLLHUP|POLLERR` where Linux reports `POLLNVAL`. A caller that
/// distinguishes "bad fd" from "peer hung up" sees the wrong one.
#[test]
fn a_bad_fd_reports_pollhup_pollerr_rather_than_pollnval() {
    let revents = readiness(FdState::Missing, pollfd::requested(POLLIN));
    assert_eq!(pollfd::report(POLLIN, revents), POLLHUP | POLLERR);
}

/// The two vocabularies agree numerically for the bits both define, which is
/// what makes the translation look like a cast. It is not one: `revents` is an
/// `i16` and `EPOLLRDHUP` is `0x2000`, `EPOLLET` `1 << 31`.
#[test]
fn the_poll_and_epoll_bit_values_agree_where_both_define_them() {
    assert_eq!(pollfd::requested(POLLIN | POLLOUT), EPOLLIN | EPOLLOUT);
    assert_eq!(u32::from(POLLIN.cast_unsigned()), EPOLLIN);
    assert_eq!(u32::from(POLLOUT.cast_unsigned()), EPOLLOUT);
}
