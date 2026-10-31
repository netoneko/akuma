//! `epoll(7)` — `epoll_create1`(291), `epoll_ctl`(233), `epoll_pwait`(281),
//! plus the two legacy x86_64 narrowings `epoll_create`(213) and
//! `epoll_wait`(232) the dispatcher routes here.
//!
//! The second fd-notification family on this kernel, after eventfd (session
//! Q): `goose` is a tokio/mio program and dies at its first `epoll_create1`
//! (`Failed to build Tokio runtime: Os { code: 38 }`). Per the composition
//! rule the decisions are PULLED, not written:
//!
//! - `akuma-syscalls-poll` owns the interest list (`InterestList`/`Entry`,
//!   including the `EPOLLET` armed state and `EPOLLONESHOT` disarm),
//!   `epoll_ctl`'s op decode and errno set (`decode`/`CtlOutcome::errno`,
//!   including the pinned ADD-is-not-EEXIST divergence), and the readiness
//!   map (`FdState` + `readiness`, including the unmaskable HUP/ERR rule and
//!   the pipe-EOF edge).
//! - `akuma-net-yarn` owns the wait loop (`WaitMachine` under
//!   `WaitPolicy::epoll`): drain budget, the inclusive zero-timeout, the
//!   timeout-before-signal tie-break, and the park-deadline arithmetic.
//!
//! What is written here is only the effects: the fd-table wiring, the 12-byte
//! packed x86_64 `struct epoll_event` copies, and `fd_state` — the probe that
//! turns one of THIS kernel's `OpenFile` kinds into the crate's `FdState`. It
//! asks the same facts `sock.rs::fd_events` asks (akuma-net's per-protocol
//! probes, akuma-pipes' own predicates, `eventfd_can_read`), plus two EOF
//! facts those probes never needed before: a TCP socket's peer FIN and full
//! death (`akuma-net`'s `socket_tcp_eof`), a pipe end's writer-gone
//! (`pipes::read_hup`), and a spawned child's buffer state
//! (`child::child_out_state`).
//!
//! NOT folded in, on purpose: `akuma-syscalls-glue`'s `poll::sys_epoll_*`
//! would drag in ~20 crates and amd64's `FileDescriptor`/`Process` model this
//! kernel does not have (see RESULT-X12.md for the count).
//!
//! # Wait design (the slice in the tree)
//!
//! The simple one, as the brief prescribes: `epoll_pwait` re-checks readiness
//! every lap and parks the calling THREAD between laps — `net::sleep_until`
//! at yarn's deadline (10 ms backstop, capped by the caller's timeout). The
//! park drives `netpoll` and yields to any runnable slot, so (a) TCP makes
//! progress while nobody is running, and (b) the other threads of the process
//! keep running. There are no producer-side wakers yet (no socket/pipe wake
//! registration on this kernel); a latency-sensitive waker design is a second
//! slice if goose's idle CPU ever makes it matter.
//!
//! # The one locking rule
//!
//! `EPOLLS` (the instance table) is never held across a probe — the same
//! `EPOLL_TABLE` discipline `docs/reference/subsystems/syscalls/poll.md`
//! records, and the reason nested epolls work at all: each lap first
//! snapshots `(fd, Entry)` pairs under the cell, releases it, then probes
//! each fd. An `OpenFile::Epoll` fd probed from an outer instance re-enters
//! `EPOLLS` (read-only, depth-capped at `EPOLL_MAX_NEST`) with no cell held.

use akuma_net_yarn::{Observation, WaitError, WaitMachine, WaitPolicy, WaitStep};
use akuma_syscalls_linux::flags::epoll::EPOLL_CLOEXEC;
use akuma_syscalls_poll::{decode, readiness, CtlOutcome, Entry, FdState, InterestList};

/// The epoll instances: the `Socket(usize)`/`EventFd(slot)` treatment — the
/// state lives here, the fd carries the index, a dup'd fd names the same
/// instance, the slot frees when the owning fd closes.
///
/// Four slots (goose's mio reactor creates one; the spare three are the same
/// headroom `EVENTFDS` keeps).
static EPOLLS: akuma_primitives::cell::BorrowCell<[Option<InterestList>; 4]> =
    akuma_primitives::cell::BorrowCell::new([None, None, None, None]);

/// Run `f` on the instance table; nothing holds it across a user copy or a
/// probe — see the locking rule above.
fn epolls<R>(f: impl FnOnce(&mut [Option<InterestList>; 4]) -> R) -> R {
    EPOLLS.with_mut(f)
}

/// How deep a nested probe may descend before it reports the level not ready.
/// amd64's `EPOLL_MAX_NEST`; nothing in this guest nests (mio never does) —
/// the cap exists so a self-registered epoll cannot recurse forever.
const EPOLL_MAX_NEST: u32 = 4;

/// The park backstop: amd64's `effective_poll_interval_us()` for a set with
/// no rump fd in it. This kernel has no rump fds.
const BACKSTOP_US: u64 = 10_000;

/// The interest-list snapshot bound (amd64's stack-array policy): up to this
/// many `(fd, Entry)` pairs are copied to the stack per lap; a larger
/// interest list falls back to one heap `Vec` (allocation justified in
/// RESULT: only for lists past 128 fds, which goose's reactor never grows).
const SNAP_MAX: usize = 128;

/// A zero `Entry` for the snapshot arrays' spare slots (every field is
/// `pub`; a real entry always overwrites before a read).
const EMPTY_ENTRY: Entry = Entry { events: 0, data: 0, last_ready: 0, disarmed: false };

/// The 12-byte packed x86_64 `struct epoll_event` (`events: u32`, `data:
/// u64`, `__attribute__((packed))`). NOT the 16-byte aarch64 layout
/// `akuma-syscalls-linux::io::EpollEvent` spells — this guest is x86_64, and
/// the padding word would put every event after the first at the wrong
/// offset for mio. Parsed from bytes, never from a pointer cast: the packed
/// layout has no alignment guarantee.
const EVENT_SIZE: usize = 12;

/// `epoll_create1(flags)` — and `epoll_create(213)` narrowed here with
/// `flags = 0` by the dispatcher, the way musl's own wrapper narrows.
/// `EPOLL_CLOEXEC` is honored through the fd table's flag bits; every other
/// bit is silently accepted (Linux's answer).
pub fn epoll_create1_arm(flags: u64) -> u64 {
    let slot = epolls(|t| {
        let slot = t.iter().position(core::option::Option::is_none)?;
        t[slot] = Some(InterestList::new());
        Some(slot)
    });
    let Some(slot) = slot else {
        return crate::e(crate::ring3::err::EMFILE);
    };
    let desc = crate::OpenFile::Epoll(slot);
    let min = crate::new_fd_min();
    let fd = crate::with_fds(|table| {
        let fd = table.alloc_fd_from(min, desc);
        if flags & u64::from(EPOLL_CLOEXEC) != 0 {
            table.set_cloexec(fd);
        }
        fd
    });
    u64::from(fd)
}

/// Which instance does `epfd` name? `None` for an unbound number or any
/// other kind (Linux's `EBADF` — epoll_ctl on a non-epoll fd).
fn epfd_slot(epfd: u64) -> Option<usize> {
    crate::with_fds(|t| {
        t.with(epfd as u32, |d| match d {
            crate::OpenFile::Epoll(slot) => Some(*slot),
            _ => None,
        })
    })
    .flatten()
}

/// `epoll_ctl(epfd, op, fd, event)`. The errno set and the ADD/MOD/DEL
/// decisions (including ADD-on-present overwriting) are the crate's
/// (`decode` + `InterestList::apply` + `CtlOutcome::errno`); this arm does
/// the fd-table checks and the 12-byte event copy.
pub fn epoll_ctl_arm(epfd: u64, op: u64, fd: u64, event_va: u64) -> u64 {
    let ctl = decode(op as i32);
    let Some(slot) = epfd_slot(epfd) else {
        return crate::e(crate::ring3::err::EBADF);
    };
    // The target fd must exist (Linux checks before touching the interest
    // list; for `DEL` the event pointer is routinely NULL, so this check
    // runs BEFORE the copy-in — the membership-ahead-of-EFAULT order the
    // reference doc pins). `with` answers None past the table's ceiling.
    if crate::with_fds(|t| t.with(fd as u32, |_| ())).is_none() {
        return crate::e(crate::ring3::err::EBADF);
    }
    let (events, data) = if ctl.needs_event() {
        let mut b = [0u8; EVENT_SIZE];
        if !crate::copy_from_user(&mut b, event_va) {
            return crate::e(crate::ring3::err::EFAULT);
        }
        (
            u32::from_le_bytes(b[0..4].try_into().expect("u32")),
            u64::from_le_bytes(b[4..12].try_into().expect("u64")),
        )
    } else {
        (0, 0)
    };
    let outcome = epolls(|t| match t[slot].as_mut() {
        Some(list) => list.apply(ctl, fd as u32, events, data),
        None => CtlOutcome::NotFound, // raced close; Linux would have said EBADF above
    });
    outcome.errno()
}

/// `epoll_pwait(epfd, events, maxevents, timeout, sigmask, sigsetsize)`.
/// The `sigmask` pair is accepted and discarded, exactly like amd64's and
/// like `ppoll`/`pselect6` here (a pinned divergence in the reference doc).
/// `timeout < 0` waits forever, `== 0` is the non-blocking poll (yarn's
/// inclusive timeout), `> 0` is milliseconds.
pub fn epoll_pwait_arm(epfd: u64, events_va: u64, maxevents: u64, timeout: u64) -> u64 {
    if maxevents == 0 || maxevents > i32::MAX as u64 {
        return crate::e(crate::ring3::err::EINVAL);
    }
    let Some(slot) = epfd_slot(epfd) else {
        return crate::e(crate::ring3::err::EBADF);
    };
    // The report buffer is validated up front (amd64's order; Linux's too):
    // `maxevents * 12` bytes must be writable, or EFAULT before any waiting.
    if !crate::user_ok(events_va, maxevents.saturating_mul(EVENT_SIZE as u64)) {
        return crate::e(crate::ring3::err::EFAULT);
    }
    // musl widens the int timeout sign-extended; the low 32 bits are the value.
    let tmo_ms = timeout as u32 as i64;
    let timeout_us = if tmo_ms < 0 {
        None
    } else {
        Some(tmo_ms as u64 * 1000)
    };
    let mut machine = WaitMachine::new(crate::net::uptime_us(), timeout_us, WaitPolicy::epoll(BACKSTOP_US));
    loop {
        let _budget = machine.lap_start(0); // this kernel has no poll epoch; the epoch guard is off in the epoll policy
        // One poll per lap: the stack is driven BEFORE the readiness scan,
        // so a datagram or ACK that landed while parked is visible now —
        // the same order amd64's loop runs in (drain_budget 1).
        crate::net::netpoll();
        let reported = scan_into(slot, events_va, maxevents as usize);

        let interrupted = {
            let s = crate::exec_twin::current_slot();
            s < crate::child::NSLOTS && akuma_threading::take_thread_interrupted(s)
        };
        let obs = Observation {
            now_us: crate::net::uptime_us(),
            poll_epoch: 0,
            progress: false,
            condition_met: reported > 0,
            interrupted,
        };
        match machine.lap_end(&obs) {
            WaitStep::Ready => return reported,
            WaitStep::Failed(WaitError::Interrupted) => return crate::e(crate::EINTR),
            // TimedOut: zero events, no error — and the park deadline can
            // never be past the caller's timeout, so Failed is TimedOut here.
            WaitStep::Failed(_) => return 0,
            // The park drives netpoll and yields to runnable slots (other
            // threads keep running); the deadline is yarn's arithmetic.
            WaitStep::Park { deadline_us, .. } => crate::net::sleep_until(deadline_us),
            WaitStep::Relap(_) => {}
        }
    }
}

/// One lap: snapshot the interest list under the cell, release it, probe
/// each fd through `fd_state`, and let the crate's edge decision say what
/// each entry contributes. Reports ascend by fd and stop at `maxevents` —
/// the pinned deterministic-truncation divergence (scanning past the stop
/// would record edges nobody receives).
fn scan_into(slot: usize, events_va: u64, maxevents: usize) -> u64 {
    let mut fds = [0u32; SNAP_MAX];
    let mut ents = [EMPTY_ENTRY; SNAP_MAX];
    let n = epolls(|t| {
        let Some(list) = t[slot].as_ref() else { return 0 };
        let mut n = 0;
        for fd in list.fds() {
            if n == SNAP_MAX {
                break; // >128: the heap path below; never reached by goose
            }
            fds[n] = fd;
            ents[n] = list.get(fd).unwrap_or(EMPTY_ENTRY);
            n += 1;
        }
        n
    });
    // > SNAP_MAX entries: one heap snapshot of the fd keys, same walk; the
    // entries are re-fetched under the cell per fd. (Allocation on the
    // syscall path is justified in RESULT — only for lists past 128 fds,
    // which goose's reactor never grows.)
    if n == SNAP_MAX {
        let vfds = epolls(|t| match t[slot].as_ref() {
            Some(list) => list.fds().collect::<alloc::vec::Vec<_>>(),
            None => alloc::vec![],
        });
        return scan_pairs(slot, &vfds, &[], events_va, maxevents);
    }
    scan_pairs(slot, &fds[..n], &ents[..n], events_va, maxevents)
}

/// The probe-and-report walk over one snapshot. `heap_fds` is the >128
/// fallback's fd vector (its entries are re-fetched under the cell per fd).
fn scan_pairs(
    slot: usize,
    fds: &[u32],
    ents: &[Entry],
    events_va: u64,
    maxevents: usize,
) -> u64 {
    let mut reported = 0usize;
    for (i, &fd) in fds.iter().enumerate() {
        if reported == maxevents {
            break;
        }
        // A closed fd is pruned here — Linux drops it at close()
        // (`eventpoll_release_file`), this kernel prunes lazily (the pinned
        // divergence): an unpruned entry would synthesise HUP|ERR for an fd
        // the caller already closed.
        if crate::with_fds(|t| t.with(fd, |_| ())).is_none() {
            epolls(|t| {
                if let Some(list) = t[slot].as_mut() {
                    list.prune(fd);
                }
            });
            continue;
        }
        let entry = if i < ents.len() { ents[i] } else {
            match epolls(|t| t[slot].as_ref().and_then(|l| l.get(fd))) {
                Some(e) => e,
                None => continue,
            }
        };
        let requested = entry.requested();
        let rev = readiness(fd_state(fd as i32, requested, 0), requested);
        let report = epolls(|t| {
            t[slot]
                .as_mut()
                .and_then(|l| l.scan_entry(fd, rev))
                .map(|s| s.report)
                .unwrap_or(0)
        });
        if report == 0 {
            continue;
        }
        // The packed 12-byte report: `events` then `data`, little-endian.
        let mut b = [0u8; EVENT_SIZE];
        b[0..4].copy_from_slice(&report.to_le_bytes());
        b[4..12].copy_from_slice(&entry.data.to_le_bytes());
        if !crate::copy_to_user(events_va + (reported as u64) * EVENT_SIZE as u64, &b) {
            // The buffer was validated up front; this is a torn write on a
            // racing close, not a reportable errno — stop here with what is
            // already out.
            break;
        }
        reported += 1;
    }
    reported as u64
}

/// Is the epoll instance named by `slot` readable from an outer epoll —
/// "does its interest list have something that would report?" Probes the
/// inner fds through the same `fd_state` (re-entering `EPOLLS` only for a
/// deeper `Epoll` fd, never with the cell held), and decides with
/// `Entry::would_report` — the pure edge decision WITHOUT recording, so a
/// probe never consumes an inner edge.
pub(crate) fn epoll_pending(slot: usize, depth: u32) -> bool {
    if depth >= EPOLL_MAX_NEST {
        return false;
    }
    let mut fds = [0u32; SNAP_MAX];
    let mut ents = [EMPTY_ENTRY; SNAP_MAX];
    let n = epolls(|t| {
        let Some(list) = t[slot].as_ref() else { return 0 };
        let mut n = 0;
        for fd in list.fds() {
            if n == SNAP_MAX {
                break;
            }
            fds[n] = fd;
            ents[n] = list.get(fd).unwrap_or(EMPTY_ENTRY);
            n += 1;
        }
        n
    });
    for i in 0..n {
        let requested = ents[i].requested();
        let rev = readiness(fd_state(fds[i] as i32, requested, depth + 1), requested);
        if ents[i].would_report(rev) {
            return true;
        }
    }
    false
}

/// The probe: one of this kernel's `OpenFile` kinds → the crate's
/// `FdState`. Asks the same facts `sock.rs::fd_events` asks, plus the three
/// EOF facts epoll needs and poll(2) never did (TCP peer FIN / death, pipe
/// writer-gone, child-buffer drained-and-gone). `depth` bounds nested-epoll
/// recursion; the kinds this kernel has no readiness model for (a regular
/// file) map to `Unmodelled` — always ready, POSIX's honest answer.
///
/// One `with_fds` lookup, facts captured out of the closure; every probe
/// below runs with the task record released (`with_fds`'s own rule).
pub(crate) fn fd_state(fd: i32, requested: u32, depth: u32) -> FdState {
    if fd < 0 {
        return FdState::Missing;
    }
    let mut sidx = usize::MAX;
    let mut cout = usize::MAX;
    let mut efd = usize::MAX;
    let mut eslot = usize::MAX;
    let mut pipe = (0u32, false);
    let mut pair = (0u32, 0u32);
    let kind = crate::with_fds(|t| {
        t.with(fd as u32, |d| match d {
            crate::OpenFile::Socket(i) => {
                sidx = *i;
                1
            }
            crate::OpenFile::File { .. } | crate::OpenFile::Dir { .. } => 2,
            crate::OpenFile::Null | crate::OpenFile::Random | crate::OpenFile::Session => 3,
            crate::OpenFile::ChildOut(i) => {
                cout = *i;
                4
            }
            crate::OpenFile::Pipe(id, is_write) => {
                pipe = (*id, *is_write);
                5
            }
            crate::OpenFile::Pair(r, w) => {
                pair = (*r, *w);
                6
            }
            crate::OpenFile::EventFd(slot) => {
                efd = *slot;
                7
            }
            crate::OpenFile::Epoll(slot) => {
                eslot = *slot;
                8
            }
        })
    });
    match kind {
        Some(1) => {
            let Some(idx) = crate::sock::fd_socket(fd as u64) else {
                return FdState::Missing;
            };
            if akuma_net::socket::is_udp_socket(idx) {
                FdState::Udp {
                    can_recv: akuma_net::socket::socket_udp_recv_ready(idx),
                    can_send: true,
                }
            } else {
                let (dead, peer_closed) = akuma_net::socket::socket_tcp_eof(idx);
                let (can_recv, can_send) = akuma_net::socket::socket_tcp_ready(idx);
                FdState::Tcp { dead, can_recv, can_send, peer_closed }
            }
        }
        Some(2) => FdState::Unmodelled,
        Some(3) => FdState::Sink,
        Some(4) => {
            let (has_data, gone) = crate::child::child_out_state(cout);
            FdState::ChildStdout { channel_gone: gone, has_data }
        }
        Some(5) => {
            if pipe.1 {
                FdState::PipeWrite { can_write: crate::pipes::writable(pipe.0) }
            } else {
                FdState::PipeRead {
                    can_read: crate::pipes::readable(pipe.0),
                    hup: crate::pipes::read_hup(pipe.0),
                }
            }
        }
        Some(6) => FdState::UnixStream {
            can_read: crate::pipes::readable(pair.0),
            can_write: crate::pipes::writable(pair.1),
        },
        Some(7) => FdState::EventFd { can_read: crate::eventfd_can_read(efd) },
        Some(8) => FdState::Epoll { pending: epoll_pending(eslot, depth) },
        // An unbound number: HUP|ERR unmaskable, the same "finished, not
        // quiet" answer `FdState::Missing` encodes.
        _ => {
            let _ = requested;
            FdState::Missing
        }
    }
}

/// The instance frees when any fd naming it closes (the eventfd treatment:
/// fork strips epoll fds from a child's table, so the only closes are the
/// owner's).
pub(crate) fn epoll_close_slot(slot: usize) {
    epolls(|t| t[slot] = None);
}

/// The drain hook (amd64's `epoll_on_fd_drained`; poll.md's table): a
/// successful read **or** an EAGAIN on `fd` witnesses the IN readiness going
/// away, so the next arrival counts as a fresh edge — for every instance's
/// entry, edge-triggered ones only (`reset_edge`'s own guard). Without it a
/// reader that drained and let the peer write again waits forever: the scan
/// recorded the old IN as already-reported and nothing re-arms it (the exact
/// hang `epollops`' et_in_rearms_after_drain probes).
pub(crate) fn on_fd_drained(fd: u64) {
    let fd = fd as u32;
    epolls(|t| {
        for slot in t.iter_mut() {
            if let Some(list) = slot {
                list.reset_edge(fd, akuma_syscalls_linux::flags::epoll::EPOLLIN);
            }
        }
    });
}

/// The write-side hook (amd64's `epoll_on_fd_write_blocked`): a short write
/// **or** an EAGAIN on `fd` witnesses the OUT readiness going away. A caller
/// that filled the buffer and waits for `EPOLLOUT` would otherwise wait
/// forever — the intermittent half of the same incident class.
pub(crate) fn on_fd_write_blocked(fd: u64) {
    let fd = fd as u32;
    epolls(|t| {
        for slot in t.iter_mut() {
            if let Some(list) = slot {
                list.reset_edge(fd, akuma_syscalls_linux::flags::epoll::EPOLLOUT);
            }
        }
    });
}
