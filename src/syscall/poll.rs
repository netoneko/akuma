use super::*;
#[cfg(feature = "smoltcp")]
use akuma_net::socket;
/// The readiness wait loop, shared with `akuma_net::socket::wait_until`. All
/// three syscalls here drive the same machine under
/// [`WaitPolicy::epoll`](akuma_net_yarn::WaitPolicy::epoll); every way this
/// family differs from the socket family is a field on that policy rather than
/// a difference between three open-coded loops.
use akuma_net_yarn::{Observation, WaitError, WaitMachine, WaitPolicy, WaitStep};
/// The family's pure logic: the state -> event-bits readiness map, the interest
/// list and its `epoll_ctl` errno set, the `EPOLLET` armed-state decision, and
/// the `ppoll`/`pselect6` wire marshalling. Every probe, lock, waker
/// registration and user-memory access stays here — see the crate docs for the
/// seam, and note that it stops at the readiness edge: the wait loop above is
/// `akuma-net-yarn` and this change did not touch it.
use akuma_syscalls_poll::readiness::{FdState, readiness};
use akuma_syscalls_poll::{fdset, pollfd};
#[cfg(feature = "sc-epoll")]
use akuma_syscalls_poll::{InterestList, ctl, edge};
#[cfg(feature = "sc-epoll")]
use core::sync::atomic::AtomicU64;
use core::task::Waker;
#[cfg(feature = "sc-epoll")]
use alloc::collections::BTreeMap;

/// The interest lists, keyed by the internal epoll id an `EpollFd(id)`
/// descriptor is a handle into.
///
/// The list itself — the registrations, `epoll_ctl`'s errno set and the
/// `EPOLLET` bookkeeping — is
/// [`akuma_syscalls_poll::InterestList`]. What stays here is the lock around it
/// and the IRQ discipline that lock needs; see `epoll_reset_edge`.
#[cfg(feature = "sc-epoll")]
static EPOLL_TABLE: Spinlock<BTreeMap<u32, InterestList>> = Spinlock::new(BTreeMap::new());
#[cfg(feature = "sc-epoll")]
static NEXT_EPOLL_ID: AtomicU32 = AtomicU32::new(1);
/// Counts `epoll_pwait(timeout=0)` returns with `nready=0` for rate-limited logging.
#[cfg(feature = "sc-epoll")]
static EPOLL_PWAIT_ZERO_ZERO_COUNT: AtomicU64 = AtomicU64::new(0);

// The bit values live in `akuma-syscalls-linux` (2026-08-27), unconditionally
// — a bit is not feature-dependent. The `#[cfg]` survives on the *import*,
// because `unused_imports` is `deny` here and a name nothing in this build
// mentions is a hard error.
//
// Only the two edge re-arm hooks name a bit directly now: everything else
// either goes through `akuma_syscalls_poll` (which owns the masking rules) or
// through `Entry::requested`, which is what keeps a registration-only bit out
// of a `revents`.
use akuma_syscalls_linux::flags::epoll as ep;
const BLOCKING_POLL_INTERVAL_US: u64 = 10_000;

/// Shorter per-iteration sleep ceiling when a polled fd is a rump socket.
/// Rump sockets have no push readiness from the tap RX path yet (readiness is
/// discovered only by re-polling via MSG_PEEK), so the default 10 ms cap means
/// each round-trip on the rump sysproxy path pays up to 10 ms of idle wait
/// before the poller re-checks. Dropping to 1 ms tightens the per-call cost
/// for rump boxes without disturbing non-rump paths (pipes/eventfds/timerfds
/// keep their existing wakers + 10 ms safety ceiling). See
/// `docs/runbooks/debug-devbox.md` "SSH slow / first-connection lag" and
/// `docs/reference/subsystems/rump-stack.md` "Known limitations".
const RUMP_BLOCKING_POLL_INTERVAL_US: u64 = 1_000;

/// Per-iteration blocking-poll sleep ceiling. Rump fds get a shorter interval
/// because they have no push readiness yet; everything else keeps the 10 ms
/// default (the `KNOWN_ISSUES.md` #6/#7 fix that bounds per-iteration sleep
/// when no waker fires).
pub fn effective_poll_interval_us(has_rump_fd: bool) -> u64 {
    if has_rump_fd {
        RUMP_BLOCKING_POLL_INTERVAL_US
    } else {
        BLOCKING_POLL_INTERVAL_US
    }
}

/// True if `fd` in the current process is a rump-box fd with no push readiness
/// (a `RumpSocket`, or the raw `/dev/net/tap0` device rump_server's RX kthread
/// blocks on) — used to pick the shorter poll cadence for these. Tap matters
/// as much as sockets here: `rumpcomp_tap.c`'s `rcvthread` blocks on the tap fd
/// via `rumpuser_akuma_wait_fd` (fiber idle-path `poll()`, or a direct host
/// `poll()` under the pthread backend) for EVERY inbound frame, so leaving it
/// out silently downgrades tap RX to the 10 ms default floor regardless of how
/// tight `RUMP_BLOCKING_POLL_INTERVAL_US` is set.
#[cfg(feature = "rump")]
fn fd_wants_rump_poll_interval(fd: i32) -> bool {
    if fd < 0 {
        return false;
    }
    let Some(proc) = akuma_exec::process::current_process_shared() else { return false };
    matches!(
        proc.get_fd(fd as u32),
        Some(
            akuma_exec::process::FileDescriptor::RumpSocket { .. }
                | akuma_exec::process::FileDescriptor::Tap { .. }
        )
    )
}

// No rump feature → no rump sockets or tap fds exist, so nothing ever wants the
// tightened cadence. Stub keeps `sys_ppoll` compiling on a rump-free build (e.g.
// the smoltcp-only devbox-smoltcp overlay).
#[cfg(not(feature = "rump"))]
fn fd_wants_rump_poll_interval(_fd: i32) -> bool {
    false
}

/// True if any of the polled raw fd numbers (epoll interest list / ppoll fds)
/// wants the tightened rump poll cadence (see `fd_wants_rump_poll_interval`).
#[cfg(feature = "rump")]
fn any_fd_wants_rump_poll_interval(fds: &[u32]) -> bool {
    fds.iter().any(|&fd| fd_wants_rump_poll_interval(fd as i32))
}

// Sole caller is `sys_epoll_pwait`; the extreme build (no `sc-epoll`, no `rump`)
// compiles neither the rump variant above nor the caller. Allow rather than
// mirroring the sc-epoll gate here.
#[allow(dead_code)]
#[cfg(not(feature = "rump"))]
fn any_fd_wants_rump_poll_interval(_fds: &[u32]) -> bool {
    false
}

/// True if any fd in the select(2) fd_set bitmaps wants the tightened rump
/// poll cadence (see `fd_wants_rump_poll_interval`). Walks only the set bits
/// (typically a handful).
#[cfg(feature = "rump")]
fn fd_set_wants_rump_poll_interval(readfds: &[u64], writefds: &[u64], nfds: usize) -> bool {
    fdset::set_fds(readfds, writefds, nfds).any(|fd| fd_wants_rump_poll_interval(fd as i32))
}

#[cfg(not(feature = "rump"))]
fn fd_set_wants_rump_poll_interval(_readfds: &[u64], _writefds: &[u64], _nfds: usize) -> bool {
    false
}

// `epoll_wait_deadline` lived here and computed the same thing
// `akuma_net_yarn::WaitMachine::park_deadline` now computes for all three wait
// loops. It was deleted 2026-08-24 rather than left beside the machine: a
// second implementation of the deadline, with its own tests, is how the three
// loops drifted apart in the first place. Its `timeout == 0` sentinel is gone
// too — that case is now the policy's inclusive `>=` expiring on lap one.

// `struct pollfd` and `struct epoll_event` moved to `akuma-syscalls-linux` on
// 2026-08-27, with the "NOT packed on ARM64, unlike x86_64" note and a test
// that proves the 16-byte stride. Re-exported so
// `crate::syscall::poll::EpollEvent` (kernel tests) keeps its spelling — and no
// longer behind `sc-epoll`, since `EpollEvent`'s *layout* does not depend on
// whether this build dispatches `epoll_ctl`.
pub use super::PollFd;
#[cfg(feature = "sc-epoll")]
pub use super::EpollEvent;

/// One line per epoll_pwait return. Suppresses most `timeout=0, nready=0` returns (see config).
#[cfg(feature = "sc-epoll")]
fn log_epoll_pwait_return(
    epfd: u32,
    timeout: i32,
    ready_count: usize,
    iterations: u64,
    start_time: u64,
    interest_fd_count: usize,
    kernel_events: &[EpollEvent],
    note: &'static str,
) {
    if !crate::config::SYSCALL_DEBUG_NET_ENABLED {
        return;
    }
    let pid = akuma_exec::process::read_current_pid().unwrap_or(0);
    let elapsed_us = akuma_primitives::clock::uptime_us().saturating_sub(start_time);
    let nready = ready_count;
    let every = crate::config::EPOLL_ZERO_SAMPLE_INTERVAL.max(1);

    if timeout == 0 && nready == 0 && iterations == 1 && note.is_empty() {
        let n = EPOLL_PWAIT_ZERO_ZERO_COUNT.fetch_add(1, Ordering::Relaxed);
        if !n.is_multiple_of(every) {
            return;
        }
        akuma_primitives::tprint!(
            224,
            "[epoll] pwait zero-sample#{} pid={} epfd={} nready=0 timeout=0ms (interval={} ~{} suppressed)\n",
            n / every,
            pid,
            epfd,
            every,
            every.saturating_sub(1),
        );
        return;
    }

    akuma_primitives::tprint!(
        224,
        "[epoll] pwait ret pid={} epfd={} timeout_ms={} nready={} iters={} dur_us={} interest_fds={} {}\n",
        pid,
        epfd,
        timeout,
        nready,
        iterations,
        elapsed_us,
        interest_fd_count,
        note,
    );
    if nready == 0 || kernel_events.is_empty() {
        return;
    }
    for (i, ev) in kernel_events.iter().take(6).enumerate() {
        let in_flag = if ev.events & ep::EPOLLIN != 0 { "IN" } else { "" };
        let out_flag = if ev.events & ep::EPOLLOUT != 0 { "OUT" } else { "" };
        let hup_flag = if ev.events & ep::EPOLLHUP != 0 { "HUP" } else { "" };
        let err_flag = if ev.events & ep::EPOLLERR != 0 { "ERR" } else { "" };
        akuma_primitives::tprint!(
            128,
            "[epoll]    ev[{}] data=0x{:x} {}{}{}{}\n",
            i,
            ev.data,
            in_flag,
            out_flag,
            hup_flag,
            err_flag
        );
    }
}

#[cfg(feature = "sc-epoll")]
pub fn epoll_destroy(epoll_id: u32) {
    akuma_primitives::irq::with_irqs_disabled(|| {
        EPOLL_TABLE.lock().remove(&epoll_id);
    });
}

/// No-ops when epoll is gated out: there is no interest table to reset, and the
/// net/fs hooks call these unconditionally. Keeping the symbols avoids
/// sprinkling `#[cfg]` across every caller in net.rs/fs.rs.
#[cfg(not(feature = "sc-epoll"))]
pub fn epoll_on_fd_drained(_fd: u32) {}
#[cfg(not(feature = "sc-epoll"))]
pub fn epoll_on_fd_write_blocked(_fd: u32) {}

/// Clear `bits` from the edge-triggered "already reported" mask of every epoll
/// instance watching `fd`, so the next time those bits go ready they count as a
/// fresh edge.
///
/// Why the mask needs an outside-in reset at all — and why the level-triggered
/// guard is stated rather than dropped — is in
/// [`InterestList::reset_edge`](akuma_syscalls_poll::InterestList::reset_edge).
/// What is kernel-only, and stays here, is the loop over instances and the
/// locking discipline below.
#[cfg(feature = "sc-epoll")]
fn epoll_reset_edge(fd: u32, bits: u32) {
    // IRQ-masked holds: this is called from the TCP send/recv syscalls, which
    // under `no-bkl-network` run with the Big Kernel Lock DROPPED. A nested IRQ
    // there does an unconditional `enter_kernel()` hard-spin; if it lands while
    // this core holds EPOLL_TABLE and the BKL owner is blocked on EPOLL_TABLE
    // (any epoll_wait/epoll_ctl syscall), the cores deadlock AB-BA. Masking
    // IRQs for the (tiny) holds makes them nest-free. Harmless on other builds.
    //
    // One hold, no allocation. This used to snapshot the instance IDs into a
    // `Vec` and then re-take the lock once per instance, "so the per-instance
    // holds stay short and independent". Both halves of that were wrong:
    //
    // * `table.keys().copied().collect()` allocated **inside** the IRQ-masked
    //   `EPOLL_TABLE` hold, which is the rule in `locking.md` ("the allocator is
    //   a lock re-entrant too") — any allocation in a locked section is a call
    //   into the OOM path, which is allowed to tear down processes.
    // * N lock acquisitions plus a heap round-trip is strictly *more* total
    //   IRQ-masked time than one pass, not less. The per-instance work is a
    //   `BTreeMap` lookup and a mask clear; there is nothing to break up.
    //
    // Both only mattered once this became hot. It was socket-only until
    // 2026-08-17, when the pipe read path started calling it — putting an
    // allocation and N spinlock round-trips on every `read()` of a pipe, i.e.
    // on sshd's session bridge, every byte a TUI writes, and every busybox
    // pipeline. See `docs/archive/TOKIO_PIPE_EPOLL_HANG.md`.
    akuma_primitives::irq::with_irqs_disabled(|| {
        let mut table = EPOLL_TABLE.lock();
        for inst in table.values_mut() {
            inst.reset_edge(fd, bits);
        }
    });
}

/// Called when a socket read drained the receive buffer (an `EAGAIN`, or any
/// successful read — BoringSSL/bun read one TLS record at a time and never
/// drain to `EAGAIN`). Re-arms the `EPOLLIN` edge so the next arrival fires.
#[cfg(feature = "sc-epoll")]
pub fn epoll_on_fd_drained(fd: u32) {
    epoll_reset_edge(fd, ep::EPOLLIN);
}

/// Called when a socket write could not take everything the caller offered —
/// an `EAGAIN`, or a **short** write. Re-arms the `EPOLLOUT` edge.
///
/// This is the exact mirror of [`epoll_on_fd_drained`], and its absence was a
/// real, reproducible hang: a client that filled the 16 KB TCP transmit buffer
/// and then waited for `EPOLLOUT` could wait forever. The window is small but
/// wide open —
///
/// 1. an `epoll_pwait` pass sees room in the transmit buffer, reports
///    `EPOLLOUT`, and records `last_ready |= EPOLLOUT`;
/// 2. the client writes until the buffer is full and gets a short write /
///    `EAGAIN`, then goes back into `epoll_pwait`;
/// 3. that call drives `smoltcp_net::poll()` at the top of its loop, which
///    flushes the buffer to the wire, so by the time readiness is computed
///    `can_send()` is already true again;
/// 4. `revents & !last_ready` is therefore 0 — `EPOLLOUT` was never *observed*
///    to go false, so no new edge is reported, and the client sleeps forever
///    holding a half-written request.
///
/// Step 3 is why this is intermittent rather than deterministic: whether any
/// pass lands while the buffer is genuinely full is a race against the flush.
/// Reproduced 2026-08-17 with `nettest-reqwest post <url> 64` (hyper + tokio,
/// edge-triggered mio) — 2 of 3 runs at a 64 KiB body hung with the connection
/// ESTABLISHED and the request never delivered, while 16 KiB bodies always
/// completed. See `docs/runbooks/debug-delayed-first-byte.md`.
#[cfg(feature = "sc-epoll")]
pub fn epoll_on_fd_write_blocked(fd: u32) {
    epoll_reset_edge(fd, ep::EPOLLOUT);
}

#[cfg(feature = "sc-epoll")]
pub fn sys_epoll_create1(flags: u32) -> u64 {
    if let Some(proc) = akuma_exec::process::current_process_shared() {
        let epoll_id = NEXT_EPOLL_ID.fetch_add(1, Ordering::SeqCst);
        akuma_primitives::irq::with_irqs_disabled(|| {
            EPOLL_TABLE.lock().insert(epoll_id, InterestList::new());
        });
        let fd = proc.alloc_fd(akuma_exec::process::FileDescriptor::EpollFd(epoll_id));
        if flags & ep::EPOLL_CLOEXEC != 0 {
            proc.set_cloexec(fd);
        }
        if crate::config::SYSCALL_DEBUG_NET_ENABLED {
            akuma_primitives::tprint!(96, "[epoll] create1() id={} fd={} cloexec={}\n", epoll_id, fd, flags & ep::EPOLL_CLOEXEC != 0);
        }
        u64::from(fd)
    } else {
        EBADF
    }
}

/// Forktest Pattern 2: Go reports crashes at **`PC≈0x13060`** for both **`read`** and **`epoll_ctl`**
/// (shared syscall trampoline). **`[sigsegv-syscall]`** serial (`src/exceptions.rs`) keys off **`x8`**.
/// This path uses **`read_user_into`** (validate + prefault + copy) for the 16-byte
/// AArch64 **`epoll_event`** — see **`docs/GO_FORKTEST_DEBUG.md`** if **`x8==EPOLL_CTL`** at SIGSEGV.
#[cfg(feature = "sc-epoll")]
pub fn sys_epoll_ctl(epfd: u32, op: i32, fd: u32, event_ptr: usize) -> u64 {
    let epoll_id = match akuma_exec::process::current_process_shared().and_then(|p| p.get_fd(epfd)) {
        Some(akuma_exec::process::FileDescriptor::EpollFd(id)) => id,
        _ => return EBADF,
    };

    // Decoded before the hold because the answer decides whether a user copy
    // happens at all — `EPOLL_CTL_DEL`'s fourth argument is routinely NULL.
    let ctl = ctl::decode(op);

    // The user copy is hoisted out of the EPOLL_TABLE hold, and must stay there:
    // `read_user_into` prefaults, and the prefault's `LazySource::File` arm reads the
    // page in through the VFS — while the hold below masks IRQs, where block I/O is
    // barred outright (locking.md). The membership probe keeps EBADF ahead of EFAULT,
    // which is the precedence the un-hoisted code had.
    let event = if ctl.needs_event() {
        if !akuma_primitives::irq::with_irqs_disabled(|| EPOLL_TABLE.lock().contains_key(&epoll_id)) {
            return EBADF;
        }
        let mut ev = EpollEvent { events: 0, _pad: 0, data: 0 };
        if read_user_into(&mut ev, event_ptr as u64).is_err() {
            return EFAULT;
        }
        Some(({ ev.events }, { ev.data }))
    } else {
        None
    };
    let (ev_events, ev_data) = event.unwrap_or((0, 0));

    // Outcome, logged after the hold is released — a UART write is far too slow
    // to sit inside an IRQ-masked spinlock hold.
    let mut outcome = None;

    let result = akuma_primitives::irq::with_irqs_disabled(|| {
        let mut table = EPOLL_TABLE.lock();
        let Some(instance) = table.get_mut(&epoll_id) else { return EBADF };
        let o = instance.apply(ctl, fd, ev_events, ev_data);
        outcome = Some(o);
        o.errno()
    });

    // Gated, like every other trace in this file. Ungated, this fires on EVERY
    // successful `epoll_ctl` ADD/MOD — and an epoll-driven server re-arms its
    // interest per request, so a redis PING round trip emitted ~3 of these. Each
    // line is ~40 bytes out the emulated 16550, one MMIO trap per byte, INSIDE
    // the request path. Measured 2026-08-24: 244,414 of 246,045 lines of a
    // running guest's console output (99.3 %) were this single statement, and
    // removing it took the c=1 redis round trip from 303 us to the numbers in
    // `docs/archive/LONG_ROAD_TO_REDIS_PART_2.md` §9. Debug tracing must never
    // be on by default on a request path.
    if crate::config::SYSCALL_DEBUG_NET_ENABLED
        && let Some(kind) = outcome.and_then(akuma_syscalls_poll::CtlOutcome::trace_tag)
    {
        akuma_primitives::tprint!(96, "[epoll] ctl {} epfd={} fd={} events=0x{:x}\n", kind, epfd, fd, ev_events);
    }
    result
}

/// Probe `fd_num` and map what it finds onto poll event bits.
///
/// The single readiness dispatch behind `epoll_pwait`, `ppoll` and `pselect6`
/// alike. It does two things, and only one of them is here: it **probes** the
/// fd — resolving it, registering `waker` with the underlying resource so a
/// producer-side write wakes this thread directly instead of waiting for the
/// next poll tick, and asking the resource what it can do — and then hands the
/// facts to [`akuma_syscalls_poll::readiness`], which decides the bits.
///
/// That split is where the family's whole bug history lives. Which bits a
/// state earns, which of them are maskable, and whether a resource has a state
/// the map forgot about are all decisions with host tests now; the probes are
/// effects and stay here. See the crate docs for the seam, and note the
/// `requested` gating below: an arm that probes conditionally does so because
/// the same branch registers a poller, and registering one for an event nobody
/// asked about would add a wakeup source out of nowhere.
pub fn epoll_check_fd_readiness(fd_num: u32, requested: u32, waker: Option<&Waker>) -> u32 {
    let fd_entry = akuma_exec::process::current_process_shared().and_then(|p| p.get_fd(fd_num));
    let Some(fd_entry) = fd_entry else {
        // A poll on an fd the calling process cannot see. Rare and always worth
        // knowing about: it is indistinguishable, to the caller, from a socket
        // that died — see `docs/runbooks/cargo-cannot-reach-crates-io.md` § 3.4.
        if crate::config::SYSCALL_DEBUG_NET_ENABLED {
            akuma_primitives::tprint!(96, "[pollmiss] fd={} -> EPOLLHUP|EPOLLERR (no fd entry)\n", fd_num);
        }
        return readiness(FdState::Missing, requested);
    };

    let tid = akuma_exec::threading::current_thread_id();
    // Deferred so the trace can report what came *out* of the map, which is the
    // only place a lost edge is visible at all. Emitted below, after the map runs.
    #[cfg(feature = "smoltcp")]
    let mut tcp_trace: Option<(usize, bool)> = None;

    let state = match fd_entry {
        #[cfg(feature = "smoltcp")]
        akuma_exec::process::FileDescriptor::Socket(idx) => {
            if let Some(w) = waker {
                socket::socket_add_waker(idx, w.clone());
            }

            if socket::is_udp_socket(idx) {
                match super::net::socket_get_udp_handle(idx) {
                    Some(handle) => {
                        let can_recv = akuma_net::smoltcp_net::udp_can_recv(handle);
                        if crate::config::SYSCALL_DEBUG_NET_ENABLED {
                            akuma_primitives::tprint!(96, "[epoll] check UDP fd={} can_recv={}\n", fd_num, can_recv);
                        }
                        FdState::Udp {
                            can_recv,
                            can_send: requested & ep::EPOLLOUT != 0
                                && akuma_net::smoltcp_net::udp_can_send(handle),
                        }
                    }
                    // No smoltcp handle: nothing ready, not an error.
                    None => FdState::Udp { can_recv: false, can_send: false },
                }
            } else if super::net::socket_is_dead_tcp(idx) {
                if crate::config::SYSCALL_DEBUG_NET_ENABLED {
                    akuma_primitives::tprint!(96, "[pollhup] fd={} idx={} state={} req=0x{:x}\n",
                        fd_num, idx, super::net::socket_tcp_state_str(idx), requested);
                }
                FdState::Tcp { dead: true, can_recv: false, can_send: false, peer_closed: false }
            } else {
                let can_recv = super::net::socket_can_recv_tcp(idx);
                tcp_trace = Some((idx, can_recv));
                FdState::Tcp {
                    dead: false,
                    can_recv,
                    can_send: requested & ep::EPOLLOUT != 0
                        && super::net::socket_can_send_tcp(idx),
                    peer_closed: requested & ep::EPOLLRDHUP != 0
                        && super::net::socket_peer_closed_tcp(idx),
                }
            }
        }
        #[cfg(feature = "sc-eventfd")]
        akuma_exec::process::FileDescriptor::EventFd(efd_id) => {
            if waker.is_some() {
                super::eventfd::eventfd_add_poller(efd_id, tid);
            }
            FdState::EventFd { can_read: super::eventfd::eventfd_can_read(efd_id) }
        }
        akuma_exec::process::FileDescriptor::ChildStdout(child_pid) => {
            if requested & ep::EPOLLIN == 0 {
                FdState::ChildStdout { channel_gone: false, has_data: false }
            } else if let Some(ch) = akuma_exec::process::get_child_channel(child_pid) {
                if waker.is_some() {
                    ch.add_poller(tid);
                }
                FdState::ChildStdout {
                    channel_gone: false,
                    has_data: ch.has_stdout_data() || ch.has_exited(),
                }
            } else {
                FdState::ChildStdout { channel_gone: true, has_data: false }
            }
        }
        akuma_exec::process::FileDescriptor::PipeRead(pipe_id) => {
            // Register for wakeup notifications
            if waker.is_some() {
                super::pipe::pipe_add_poller(pipe_id, tid);
            }
            FdState::PipeRead {
                can_read: requested & ep::EPOLLIN != 0 && super::pipe::pipe_can_read(pipe_id),
                hup: super::pipe::pipe_hup(pipe_id),
            }
        }
        akuma_exec::process::FileDescriptor::PipeWrite(pipe_id) => {
            if requested & ep::EPOLLOUT == 0 {
                FdState::PipeWrite { can_write: false }
            } else {
                super::pipe::pipe_add_poller(pipe_id, tid);
                FdState::PipeWrite { can_write: super::pipe::pipe_can_write(pipe_id) }
            }
        }
        // AF_UNIX socket. A connected endpoint is readable when `rx` has data
        // and writable when `tx`'s peer is still open; a **listener** has no
        // pipes at all and is readable when its backlog is non-empty. The
        // listener probe runs first for the reason its `FdState` variant
        // records: its `rx`/`tx` are 0, so falling through would ask
        // `pipe_can_read(0)`.
        akuma_exec::process::FileDescriptor::UnixSocket { rx, tx, .. } => {
            if let Some(accept_ready) =
                super::unixsock::listener_ready(fd_num, waker.is_some().then_some(tid))
            {
                FdState::UnixListener { accept_ready }
            } else {
                let can_read = if requested & ep::EPOLLIN == 0 {
                    false
                } else {
                    if waker.is_some() {
                        super::pipe::pipe_add_poller(rx, tid);
                    }
                    super::pipe::pipe_can_read(rx)
                };
                let can_write = if requested & ep::EPOLLOUT == 0 {
                    false
                } else {
                    super::pipe::pipe_add_poller(tx, tid);
                    super::pipe::pipe_can_write(tx)
                };
                FdState::UnixStream { can_read, can_write }
            }
        }
        #[cfg(feature = "sc-timerfd")]
        akuma_exec::process::FileDescriptor::TimerFd(timer_id) => {
            if requested & ep::EPOLLIN == 0 {
                FdState::TimerFd { can_read: false }
            } else {
                if waker.is_some() {
                    super::timerfd::timerfd_add_poller(timer_id, tid);
                }
                FdState::TimerFd { can_read: super::timerfd::timerfd_can_read(timer_id) }
            }
        }
        #[cfg(feature = "sc-pidfd")]
        akuma_exec::process::FileDescriptor::PidFd(pidfd_id) => {
            // A pidfd becomes readable (EPOLLIN) when the tracked process has exited.
            if requested & ep::EPOLLIN == 0 {
                FdState::PidFd { can_read: false }
            } else {
                if let Some(target_pid) = super::pidfd::pidfd_get_pid(pidfd_id)
                    && let Some(ch) = akuma_exec::process::get_child_channel(target_pid)
                        && waker.is_some() {
                            ch.add_poller(tid);
                        }
                FdState::PidFd { can_read: super::pidfd::pidfd_can_read(pidfd_id) }
            }
        }
        // `/dev/tty` shares fd 0's channel: same readiness, same waker.
        akuma_exec::process::FileDescriptor::Stdin
        | akuma_exec::process::FileDescriptor::DevTty => {
            if requested & ep::EPOLLIN != 0
                && let Some(ch) = akuma_exec::process::current_channel() {
                    if waker.is_some() {
                        ch.add_poller(tid);
                    }
                    FdState::Stdin { has_data: ch.has_stdin_data() }
                } else {
                    FdState::Stdin { has_data: false }
                }
        }
        akuma_exec::process::FileDescriptor::Stdout | akuma_exec::process::FileDescriptor::Stderr => {
            FdState::Sink
        }
        // A rump socket (stack=rump box): POLLIN comes from a non-blocking
        // MSG_PEEK probe forwarded to the rump server; POLLOUT is assumed ready
        // (sends are blocking-synchronous through the proxy). This lets a client
        // like sic multiplex stdin + the IRC socket instead of blocking in recv.
        // Each readiness check is a sysproxy round-trip (proxy latency applies),
        // which is why it is skipped when EPOLLIN was not asked for.
        #[cfg(feature = "rump")]
        akuma_exec::process::FileDescriptor::RumpSocket { rump_fd, .. } => FdState::RumpSocket {
            readable: requested & ep::EPOLLIN != 0
                && crate::rump_proxy::rump_socket_readable(rump_fd),
        },
        #[cfg(feature = "rump")]
        akuma_exec::process::FileDescriptor::Tap { .. } => FdState::Tap {
            has_frame: requested & ep::EPOLLIN != 0
                && akuma_net::rump_tap::has_frame(),
        },
        _ => FdState::Unmodelled,
    };

    let ready = readiness(state, requested);

    #[cfg(feature = "smoltcp")]
    if crate::config::SYSCALL_DEBUG_EPOLL_EDGE
        && let Some((idx, can_recv)) = tcp_trace
    {
        akuma_primitives::tprint!(96, "[epoll-tcp] fd={} idx={} req=0x{:x} can_recv={} ready=0x{:x}\n",
            fd_num, idx, requested, can_recv, ready);
    }

    ready
}

#[cfg(feature = "sc-epoll")]
pub fn sys_epoll_pwait(epfd: u32, events_ptr: usize, maxevents: i32, timeout: i32) -> u64 {
    const EPOLL_EVENT_SIZE: usize = core::mem::size_of::<EpollEvent>();  // 16 on ARM64
    
    if maxevents <= 0 { return EINVAL; }
    let maxevents = maxevents as usize;
    let out_size = maxevents * EPOLL_EVENT_SIZE;
    if !validate_user_ptr(events_ptr as u64, out_size) { return EFAULT; }

    let epoll_id = match akuma_exec::process::current_process_shared().and_then(|p| p.get_fd(epfd)) {
        Some(akuma_exec::process::FileDescriptor::EpollFd(id)) => id,
        _ => return EBADF,
    };

    let timeout_us = if timeout > 0 {
        (timeout as u64) * 1000
    } else {
        0
    };
    let start_time = akuma_primitives::clock::uptime_us();
    let waker = akuma_exec::threading::current_thread_waker();

    let mut iterations = 0u64;

    // The wait policy this syscall has always had, stated instead of
    // open-coded — shared with `sys_ppoll` and `sys_pselect6`. See
    // `akuma_net_yarn::WaitPolicy::epoll`. The backstop is refreshed per lap
    // below, because the interest list can gain or lose a rump fd underneath
    // us and rump fds want a 1 ms cadence.
    //
    // `timeout == 0` maps to `Some(0)`, which the policy's inclusive `>=`
    // comparison expires on the first lap — that is the non-blocking
    // `epoll_wait(.., 0)` contract.
    let mut machine = WaitMachine::new(
        start_time,
        if timeout < 0 { None } else { Some(timeout_us) },
        WaitPolicy::epoll(effective_poll_interval_us(false)),
    );

    loop {
        iterations += 1;
        let budget = machine.lap_start(0);

        // Drive network stack (only once per loop). BKL carve-out (Phase 7b piece 1,
        // docs/archive/BKL_PHASE7_AUDIT.md §3): every piece of state `poll()` touches is
        // already behind its own `PreemptGuard`-protected `NETWORK`/`SOCKET_TABLE` lock —
        // same precedent as the `netpoll_drain` carve in `src/main.rs`
        // (`BKL_VFS_CARVE_OUT.md` §19–20), whose mechanism this reuses directly. Gated on
        // `kernel_no_bkl_network` specifically, not `kernel_smp_shared` alone: that is what
        // makes `PreemptGuard::new()` mask IRQs for the inner holds, which is what keeps a
        // nested IRQ from ever observing this core "holding NETWORK, wanting the BKL".
        #[cfg(all(kernel_smp_shared, kernel_no_bkl_network))]
        akuma_exec::bkl::dropped_window_open();
        // The drain below is `#[cfg(feature = "smoltcp")]`, so on a rump-only
        // build nothing ever assigns this and the `mut` is genuinely unused.
        #[cfg_attr(not(feature = "smoltcp"), allow(unused_mut))]
        let mut progress = false;
        for _ in 0..budget {
            #[cfg(feature = "smoltcp")]
            {
                progress = akuma_net::smoltcp_net::poll();
            }
        }
        #[cfg(all(kernel_smp_shared, kernel_no_bkl_network))]
        akuma_exec::bkl::dropped_window_close();

        let mut kernel_events = alloc::vec![];
        let mut ready_count = 0usize;

        // Snapshot interest list to avoid holding EPOLL_TABLE lock during readiness checks.
        // This prevents deadlock with PROCESS_TABLE lock (which readiness checks need).
        // We use a stack-allocated array for common small interest lists (up to 128).
        const STACK_SNAPSHOT_SIZE: usize = 128;
        let mut stack_snapshot = [0u32; STACK_SNAPSHOT_SIZE];

        let snapshot = akuma_primitives::irq::with_irqs_disabled(|| {
            let table = EPOLL_TABLE.lock();
            let instance = table.get(&epoll_id)?;

            let count = instance.len();
            if count <= STACK_SNAPSHOT_SIZE {
                for (i, fd) in instance.fds().enumerate() {
                    stack_snapshot[i] = fd;
                }
                Some((count, None))
            } else {
                Some((count, Some(instance.fds().collect::<alloc::vec::Vec<u32>>())))
            }
        });
        let (snapshot_count, heap_snapshot) = match snapshot {
            Some(s) => s,
            None => return EBADF,
        };

        let fds: &[u32] = if let Some(ref h) = heap_snapshot { 
            h 
        } else { 
            &stack_snapshot[..snapshot_count] 
        };

        for &fd in fds {
            if ready_count >= maxevents { break; }

            // Re-acquire lock to get entry details (MUST NOT hold during readiness check)
            let entry_info = akuma_primitives::irq::with_irqs_disabled(|| {
                let table = EPOLL_TABLE.lock();
                table.get(&epoll_id).and_then(|inst| inst.get(fd))
            });

            let Some(entry) = entry_info else {
                continue; // FD removed from epoll interest during loop
            };

            // Real Linux implicitly drops a fd from every epoll instance's interest
            // list the instant the fd is close()'d (ep_free/eventpoll_release_file
            // walk back-references from the file to its epitems). Akuma's close()
            // does not do the equivalent, so a fd closed after being added here can
            // leave a stale interest-list entry behind. Left unchecked,
            // epoll_check_fd_readiness's "no fd entry" fallback synthesizes
            // EPOLLHUP|EPOLLERR for it — a real event delivered to userspace for a
            // fd the caller already closed, which real Linux can never produce.
            // nginx hit exactly this (creates+registers+closes a socketpair in one
            // breath): its crash-recovery path ORs EPOLLIN|EPOLLOUT into revents on
            // HUP/ERR to force both handlers to run, and dereferenced a connection
            // object it had already torn down along with the fd. Prune instead.
            if akuma_exec::process::current_process_shared().is_none_or(|p| p.get_fd(fd).is_none()) {
                akuma_primitives::irq::with_irqs_disabled(|| {
                    if let Some(inst) = EPOLL_TABLE.lock().get_mut(&epoll_id) {
                        inst.prune(fd);
                    }
                });
                continue;
            }

            // Pass waker to register interest for event-driven wakeups.
            // epoll_check_fd_readiness locks PROCESS_TABLE.
            let revents = epoll_check_fd_readiness(fd, entry.requested(), Some(&waker));

            // Level- vs edge-triggered, and what the entry remembers afterwards.
            // See `akuma_syscalls_poll::edge::scan`.
            let step = edge::scan(entry.events, revents, entry.last_ready);

            if let Some(record) = step.record {
                akuma_primitives::irq::with_irqs_disabled(|| {
                    if let Some(inst) = EPOLL_TABLE.lock().get_mut(&epoll_id) {
                        inst.record_ready(fd, record);
                    }
                });
                // One line per ready fd, showing whether the edge bookkeeping
                // let this event through. A lost edge is invisible in every
                // other trace: the fd stays ready, the watcher stays parked,
                // and nothing reports an error. See `SYSCALL_DEBUG_EPOLL_EDGE`.
                if crate::config::SYSCALL_DEBUG_EPOLL_EDGE && revents != 0 {
                    akuma_primitives::tprint!(
                        160,
                        "[epoll] ET epfd={} fd={} rev=0x{:x} last=0x{:x} new=0x{:x} {}\n",
                        epfd,
                        fd,
                        revents,
                        entry.last_ready,
                        step.report,
                        if step.report == 0 { "SUPPRESSED" } else { "deliver" }
                    );
                }
            }

            if step.report != 0 {
                kernel_events.push(EpollEvent { events: step.report, _pad: 0, data: entry.data });
                ready_count += 1;
            }
        }

        // The per-lap backstop: `epoll_ctl` can change the interest list under
        // us, and a rump fd in it wants 1 ms rather than 10 ms.
        machine.set_backstop(effective_poll_interval_us(any_fd_wants_rump_poll_interval(fds)));

        let ready = ready_count > 0;
        let obs = Observation {
            now_us: akuma_primitives::clock::uptime_us(),
            poll_epoch: 0, // no epoch guard in this family — see WaitPolicy::epoll
            progress,
            condition_met: ready,
            interrupted: !ready && akuma_exec::process::should_interrupt_blocking_syscall(),
        };

        // Periodic log for long waits (every ~5 seconds = 500 iterations x 10ms)
        if crate::config::SYSCALL_DEBUG_NET_ENABLED && iterations.is_multiple_of(500) {
            let elapsed = akuma_primitives::clock::uptime_us() - start_time;
            let pid = akuma_exec::process::read_current_pid().unwrap_or(0);
            akuma_primitives::tprint!(192, "[epoll] pwait still waiting: pid={} epfd={} {}us elapsed\n",
                pid, epfd, elapsed);
        }

        match machine.lap_end(&obs) {
            WaitStep::Ready => {
                if copy_to_user(
                    events_ptr as u64,
                    &as_user_bytes(&kernel_events)[..ready_count * EPOLL_EVENT_SIZE],
                )
                .is_err()
                {
                    return EFAULT;
                }
                log_epoll_pwait_return(
                    epfd, timeout, ready_count, iterations, start_time, 0, &kernel_events, "",
                );
                return ready_count as u64;
            }
            WaitStep::Failed(WaitError::TimedOut) => {
                // Two tags, as before: a `timeout == 0` probe is a different
                // event from a real wait that ran out.
                let tag = if timeout == 0 { "" } else { "timeout_expired" };
                log_epoll_pwait_return(epfd, timeout, 0, iterations, start_time, 0, &[], tag);
                return 0;
            }
            WaitStep::Failed(WaitError::Interrupted) => {
                log_epoll_pwait_return(epfd, timeout, 0, iterations, start_time, 0, &[], "EINTR");
                return EINTR;
            }
            WaitStep::Relap(_) => {}
            WaitStep::Park { deadline_us, .. } => {
                // With the waker mechanism this blocks efficiently; the backstop
                // is a cap for resources that do not support wakers (TimerFd),
                // and network events wake us immediately.
                akuma_exec::threading::schedule_blocking(deadline_us);
            }
        }
    }
}

/// `pselect6(2)`.
///
/// # `exceptfds` must be cleared, not ignored
///
/// Akuma has no out-of-band/urgent TCP data, so no fd ever has an exceptional
/// condition and the honest answer for `exceptfds` is "none of them". That is
/// **not** the same as leaving the caller's set alone: `select` reports its
/// results by *overwriting* all three sets, so a kernel that never writes
/// `exceptfds` hands every fd back still flagged, exactly as the caller passed
/// it in.
///
/// That is not theoretical. It is the whole of
/// `docs/runbooks/cargo-cannot-reach-crates-io.md`: `curl-sys`' vendored libcurl
/// — the one the nightly Rust toolchain's cargo links — defines `HAVE_POLL_H`
/// and `HAVE_POLL_FINE` but **not** plain `HAVE_POLL`, so `Curl_poll()` compiles
/// its `select()` branch. `Curl_socket_check()` asks for
/// `POLLWRNORM|POLLOUT|POLLPRI` on a connecting socket, and the select branch
/// puts a fd with `POLLPRI` into `exceptfds`. With the set returned untouched,
/// `FD_ISSET(sock, &fds_err)` stayed true, libcurl synthesised `POLLPRI`, mapped
/// it to `CURL_CSELECT_ERR`, and `cf_tcp_connect()` — which tests
/// `rc == CURL_CSELECT_OUT` by equality — took the error branch on a socket that
/// had just reached `Established` with `SO_ERROR == 0`. Every `cargo fetch`
/// failed with `[7] Could not connect to server` about one RTT into a connection
/// that had in fact succeeded. `poll(2)` callers were unaffected, which is why
/// apk cargo and `/bin/curl` always worked.
pub(super) fn sys_pselect6(nfds: usize, readfds_ptr: u64, writefds_ptr: u64, exceptfds_ptr: u64, timeout_ptr: u64, _sigmask_ptr: u64) -> SysResult {
    if nfds == 0 { return Ok(0); }
    // The cap, the word arithmetic and the bit marshalling are
    // `akuma_syscalls_poll::fdset` — pure, and untested until it existed. The
    // buffers stay here: they are stack storage the copy-in/copy-out below
    // reads and writes, which is a user-memory access, not arithmetic.
    if !fdset::nfds_ok(nfds) { return Err(EINVAL); }
    let fd_set_bytes = fdset::bytes(nfds);

    let mut orig_read = [0u64; fdset::MAX_WORDS];
    let mut orig_write = [0u64; fdset::MAX_WORDS];

    if readfds_ptr != 0
        && copy_from_user(&mut as_user_bytes_mut(&mut orig_read)[..fd_set_bytes], readfds_ptr)
            .is_err()
    {
        return Err(EFAULT);
    }
    if writefds_ptr != 0
        && copy_from_user(&mut as_user_bytes_mut(&mut orig_write)[..fd_set_bytes], writefds_ptr)
            .is_err()
    {
        return Err(EFAULT);
    }

    // NULL timeout = block indefinitely. `infinite`/`timeout_us` stay two
    // locals because the wait loops below read them separately; the pair is
    // derived from one `Option` so they cannot disagree.
    let (infinite, timeout_us) = match time::read_timeout_us(timeout_ptr)? {
        Some(us) => (false, us),
        None => (true, 0),
    };

    let start_time = akuma_primitives::clock::uptime_us();

    // Register the calling thread as a waker on each polled fd, exactly as
    // `sys_epoll_pwait` and `sys_ppoll` do, so a peer write wakes us
    // IMMEDIATELY (sticky WOKEN_STATES -> schedule_blocking returns at once)
    // instead of only at the next `BLOCKING_POLL_INTERVAL_US` re-poll.
    //
    // This was passing `None` — alone among the three wait loops — which capped
    // every `select(2)` wait at the 10 ms tick no matter how fast the peer
    // answered. It is the same drift that left the EINTR check below missing:
    // three copies of one loop, and a fix applied to the copies someone
    // remembered. The victim is named in this function's doc comment above:
    // cargo's vendored libcurl compiles the `select()` branch, so every cargo
    // network wait rode the tick while `poll(2)` callers were woken at once.
    let waker = akuma_exec::threading::current_thread_waker();

    // Rump sockets have no push readiness yet (MSG_PEEK probe only), so a
    // shorter per-iteration sleep ceiling keeps their poll cadence tight.
    let has_rump_fd = fd_set_wants_rump_poll_interval(&orig_read, &orig_write, nfds);

    // Same policy as `sys_ppoll` and `sys_epoll_pwait` — see
    // `akuma_net_yarn::WaitPolicy::epoll`. A zero timeout expiring on the first
    // lap (the `>=` comparison) is what makes `select(fds, 0)` non-blocking.
    let mut machine = WaitMachine::new(
        start_time,
        if infinite { None } else { Some(timeout_us) },
        WaitPolicy::epoll(effective_poll_interval_us(has_rump_fd)),
    );

    loop {
        let budget = machine.lap_start(0);

        // BKL carve-out (Phase 7b piece 1) — see the identical comment on the
        // `sys_epoll_pwait` poll() call above.
        #[cfg(all(kernel_smp_shared, kernel_no_bkl_network))]
        akuma_exec::bkl::dropped_window_open();
        // The drain below is `#[cfg(feature = "smoltcp")]`, so on a rump-only
        // build nothing ever assigns this and the `mut` is genuinely unused.
        #[cfg_attr(not(feature = "smoltcp"), allow(unused_mut))]
        let mut progress = false;
        for _ in 0..budget {
            #[cfg(feature = "smoltcp")]
            {
                progress = akuma_net::smoltcp_net::poll();
            }
        }
        #[cfg(all(kernel_smp_shared, kernel_no_bkl_network))]
        akuma_exec::bkl::dropped_window_close();
        let mut ready_count: u64 = 0;
        let mut out_read = [0u64; fdset::MAX_WORDS];
        let mut out_write = [0u64; fdset::MAX_WORDS];

        for interest in fdset::interests(&orig_read, &orig_write, nfds) {
            let revents =
                epoll_check_fd_readiness(interest.fd as u32, interest.requested(), Some(&waker));
            // Adds TWO for an fd ready in both directions: `select(2)` returns
            // the number of bits left set, not the number of fds.
            ready_count += interest.record(revents, &mut out_read, &mut out_write);
        }

        let ready = ready_count > 0;
        let obs = Observation {
            now_us: akuma_primitives::clock::uptime_us(),
            poll_epoch: 0, // no epoch guard in this family — see WaitPolicy::epoll
            progress,
            condition_met: ready,
            interrupted: !ready && akuma_exec::process::should_interrupt_blocking_syscall(),
        };
        let step = machine.lap_end(&obs);

        if matches!(step, WaitStep::Ready) {
            if readfds_ptr != 0
                && copy_to_user(readfds_ptr, &as_user_bytes(&out_read)[..fd_set_bytes]).is_err()
            {
                return Err(EFAULT);
            }
            if writefds_ptr != 0
                && copy_to_user(writefds_ptr, &as_user_bytes(&out_write)[..fd_set_bytes]).is_err()
            {
                return Err(EFAULT);
            }
            // No exceptional conditions exist here, but the set still has to be
            // written — see this function's doc comment.
            if exceptfds_ptr != 0 {
                let cleared = [0u8; fdset::MAX_FDS / 8];
                if copy_to_user(exceptfds_ptr, &cleared[..fd_set_bytes]).is_err() {
                    return Err(EFAULT);
                }
            }
            return Ok(ready_count);
        }

        if matches!(step, WaitStep::Failed(WaitError::TimedOut)) {
            // select(2) reports its result by OVERWRITING all three sets, so
            // "nothing ready" has to be written down, not left alone — see this
            // function's doc comment and `run_pselect6_exceptfds_test`.
            let cleared = [0u8; fdset::MAX_FDS / 8];
            if readfds_ptr != 0 && copy_to_user(readfds_ptr, &cleared[..fd_set_bytes]).is_err() {
                return Err(EFAULT);
            }
            if writefds_ptr != 0 && copy_to_user(writefds_ptr, &cleared[..fd_set_bytes]).is_err() {
                return Err(EFAULT);
            }
            if exceptfds_ptr != 0 && copy_to_user(exceptfds_ptr, &cleared[..fd_set_bytes]).is_err() {
                return Err(EFAULT);
            }
            return Ok(0);
        }

        // Without this, a pending signal (e.g. SIGALRM from setitimer/alarm())
        // just wakes schedule_blocking below, finds nothing ready, and goes
        // right back to sleep — the signal is never delivered because nothing
        // ever returns to the syscall-return path that dispatches it, so
        // `alarm()` + `select()` hangs instead of interrupting.
        //
        // `sys_epoll_pwait` and `sys_ppoll` both make this check; ppoll's
        // comment records that it was *added* there after exactly this bug.
        // pselect6 was the third copy and never got the fix.
        //
        // Return without writing the fd sets: Linux leaves them unmodified on
        // EINTR, and that is what ppoll does too.
        match step {
            // Linux leaves the fd sets unmodified on EINTR, and so does ppoll.
            WaitStep::Failed(WaitError::Interrupted) => return Err(EINTR),
            WaitStep::Park { deadline_us, .. } => {
                akuma_exec::threading::schedule_blocking(deadline_us);
            }
            // Ready and TimedOut are handled above; Relap just re-drains.
            WaitStep::Ready | WaitStep::Failed(WaitError::TimedOut) | WaitStep::Relap(_) => {}
        }
    }
}

/// Regression: `select(2)` must **write** all three fd sets, including
/// `exceptfds`.
///
/// `sys_pselect6` used to take `_exceptfds_ptr` and never touch it, so the
/// caller's exceptional-condition set came back exactly as passed in — every fd
/// in it still flagged. Akuma has no out-of-band data, so the correct answer is
/// always "none", but "none" has to be written down.
///
/// This is the whole of `docs/runbooks/cargo-cannot-reach-crates-io.md`: the
/// nightly toolchain's cargo links a libcurl whose `Curl_poll()` compiles its
/// `select()` branch (curl-sys' build.rs defines `HAVE_POLL_H`/`HAVE_POLL_FINE`
/// but not plain `HAVE_POLL`), and `Curl_socket_check()` asks for `POLLPRI` on a
/// connecting socket, which the select branch puts into `exceptfds`. The stale
/// set made libcurl read `POLLPRI` back, map it to `CURL_CSELECT_ERR`, and
/// abandon a socket that had just reached `Established` with `SO_ERROR == 0` —
/// so every `cargo fetch` died with `[7] Could not connect to server` about one
/// RTT into a connection that had actually succeeded. Verified 2026-08-20:
/// `nettest-connect one index.crates.io 443 --wait select` returned
/// `revents=PRI|OUT` before the fix and `revents=OUT` after, with `--wait poll`
/// connecting cleanly in both.
#[cfg(kernel_tests)]
pub fn run_pselect6_exceptfds_test() {
    use akuma_exec::process::{
        register_process, register_thread_pid, unregister_process, unregister_thread_pid,
        FileDescriptor,
    };

    let _bypass = akuma_exec::process::user_access::BypassValidationGuard::new();

    let tid = akuma_exec::threading::current_thread_id();
    let pid = 8041u32;
    register_process(pid, akuma_exec::process::make_test_process(pid));
    register_thread_pid(tid, pid);

    let proc = akuma_exec::process::current_process_shared().unwrap();
    // A pipe's write end is unconditionally writable while the reader lives, so
    // it drives the ready path without needing a peer on the network.
    let pipe_id = super::pipe::pipe_create();
    let wfd = proc.alloc_fd(FileDescriptor::PipeWrite(pipe_id));
    let rfd = proc.alloc_fd(FileDescriptor::PipeRead(pipe_id));

    let bit = |fd: u32| -> [u64; 16] {
        let mut set = [0u64; 16];
        set[(fd / 64) as usize] |= 1u64 << (fd % 64);
        set
    };
    // Zero timeout — exactly what libcurl's `SOCKET_WRITABLE(sock, 0)` passes.
    let mut ts = Timespec { tv_sec: 0, tv_nsec: 0 };

    // Ready path: the write fd is writable, so pselect6 returns > 0.
    let mut wset = bit(wfd);
    let mut eset = bit(wfd);
    let rc_ready = flat(sys_pselect6(
        (wfd + 1) as usize,
        0,
        &raw mut wset as u64,
        &raw mut eset as u64,
        &raw mut ts as u64,
        0,
    ));
    let ready_ok = rc_ready > 0 && wset[(wfd / 64) as usize] & (1u64 << (wfd % 64)) != 0;
    let ready_except_cleared = eset[(wfd / 64) as usize] & (1u64 << (wfd % 64)) == 0;

    // Timeout path: an empty pipe is not readable, so pselect6 returns 0 — and
    // must still have cleared the set it was handed.
    let mut rset = bit(rfd);
    let mut eset2 = bit(rfd);
    let rc_timeout = flat(sys_pselect6(
        (rfd + 1) as usize,
        &raw mut rset as u64,
        0,
        &raw mut eset2 as u64,
        &raw mut ts as u64,
        0,
    ));
    let timeout_ok = rc_timeout == 0;
    let timeout_except_cleared = eset2[(rfd / 64) as usize] & (1u64 << (rfd % 64)) == 0;

    proc.remove_fd(wfd);
    proc.remove_fd(rfd);
    unregister_thread_pid(tid);
    unregister_process(pid);

    let pass = ready_ok && ready_except_cleared && timeout_ok && timeout_except_cleared;
    if pass {
        akuma_primitives::safe_print!(128, "  [PASS] pselect6_clears_exceptfds\n");
    } else {
        akuma_primitives::safe_print!(160,
            "  [FAIL] pselect6_clears_exceptfds ready_ok={} ready_cleared={} timeout_ok={} timeout_cleared={}\n",
            ready_ok, ready_except_cleared, timeout_ok, timeout_except_cleared);
    }
}

/// Regression: `pselect6` must register the calling thread as a waker.
///
/// It passed `None` to [`epoll_check_fd_readiness`] — alone among the three
/// wait loops in this file — so a `select(2)` waiter announced itself to
/// nothing and could only ever be woken by the `BLOCKING_POLL_INTERVAL_US`
/// 10 ms tick, however fast the peer answered. `sys_epoll_pwait` (`Some(&waker)`)
/// and `sys_ppoll` (`Some(&waker)`) both did it correctly; this was the third
/// copy of a loop that had drifted.
///
/// The victim is the same one named in `sys_pselect6`'s doc comment: cargo's
/// vendored libcurl compiles `Curl_poll()`'s `select()` branch, so every cargo
/// network wait rode the tick.
///
/// A zero timeout is enough to prove it — the readiness scan runs before the
/// timeout check, so one non-blocking call registers if it is going to.
#[cfg(kernel_tests)]
pub fn run_pselect6_registers_waker_test() {
    use akuma_exec::process::{
        register_process, register_thread_pid, unregister_process, unregister_thread_pid,
        FileDescriptor,
    };

    let _bypass = akuma_exec::process::user_access::BypassValidationGuard::new();

    let tid = akuma_exec::threading::current_thread_id();
    let pid = 8042u32;
    register_process(pid, akuma_exec::process::make_test_process(pid));
    register_thread_pid(tid, pid);

    let proc = akuma_exec::process::current_process_shared().unwrap();
    let pipe_id = super::pipe::pipe_create();
    let wfd = proc.alloc_fd(FileDescriptor::PipeWrite(pipe_id));
    let rfd = proc.alloc_fd(FileDescriptor::PipeRead(pipe_id));

    let before = super::pipe::pipe_poller_count(pipe_id);

    // An empty pipe is not readable, so this takes the not-ready path — which
    // is precisely the path that has to have registered a waker before it
    // decides to sleep.
    let mut rset = [0u64; 16];
    rset[(rfd / 64) as usize] |= 1u64 << (rfd % 64);
    let mut ts = Timespec { tv_sec: 0, tv_nsec: 0 };
    let rc = flat(sys_pselect6(
        (rfd + 1) as usize,
        &raw mut rset as u64,
        0,
        0,
        &raw mut ts as u64,
        0,
    ));

    let after = super::pipe::pipe_poller_count(pipe_id);

    proc.remove_fd(wfd);
    proc.remove_fd(rfd);
    unregister_thread_pid(tid);
    unregister_process(pid);

    let pass = rc == 0 && before == 0 && after >= 1;
    if pass {
        akuma_primitives::safe_print!(128, "  [PASS] pselect6_registers_waker\n");
    } else {
        akuma_primitives::safe_print!(
            160,
            "  [FAIL] pselect6_registers_waker rc={} pollers {} -> {} (want 0 -> >=1)\n",
            rc,
            before,
            after
        );
    }
}

/// Regression: `pselect6` must return `EINTR` when a signal is pending.
///
/// `sys_epoll_pwait` and `sys_ppoll` both call
/// `should_interrupt_blocking_syscall()` before parking; pselect6 did not. The
/// comment on ppoll's check records that it was *added* there after exactly
/// this bug — "alarm()+pause() hang instead of interrupting" — and pselect6,
/// the third copy, never got the fix. So `alarm()` + `select()` slept through
/// its own signal: `schedule_blocking` woke, found nothing ready, and went
/// straight back to sleep, because nothing ever returned to the syscall-return
/// path that dispatches signals.
///
/// The timeout is short so that a regression is a slow test rather than a hang.
#[cfg(kernel_tests)]
pub fn run_pselect6_eintr_test() {
    use alloc::sync::Arc;
    use akuma_exec::process::{
        register_process, register_thread_pid, unregister_process, unregister_thread_pid,
        FileDescriptor,
    };

    let _bypass = akuma_exec::process::user_access::BypassValidationGuard::new();

    let tid = akuma_exec::threading::current_thread_id();
    let pid = 8043u32;
    register_process(pid, akuma_exec::process::make_test_process(pid));
    register_thread_pid(tid, pid);

    // `interrupt_thread` sets the flag on the channel registered for `tid`, and
    // the boot test thread has none — without this it is a silent no-op and the
    // test can never observe the interrupt. Same setup as
    // `process_tests::test_sys_kill_sets_interrupted_flag`.
    let prior = akuma_exec::process::channel::get_channel(tid);
    if prior.is_none() {
        akuma_exec::process::channel::register_channel(
            tid,
            Arc::new(akuma_exec::process::ProcessChannel::new()),
        );
    }

    let proc = akuma_exec::process::current_process_shared().unwrap();
    let pipe_id = super::pipe::pipe_create();
    let wfd = proc.alloc_fd(FileDescriptor::PipeWrite(pipe_id));
    let rfd = proc.alloc_fd(FileDescriptor::PipeRead(pipe_id));

    akuma_exec::process::interrupt_thread(tid);

    // An empty pipe never becomes readable, so without the EINTR check this
    // sleeps to the 300 ms timeout and returns 0.
    let mut rset = [0u64; 16];
    rset[(rfd / 64) as usize] |= 1u64 << (rfd % 64);
    let mut ts = Timespec { tv_sec: 0, tv_nsec: 300_000_000 };
    let started = akuma_primitives::clock::uptime_us();
    let rc = flat(sys_pselect6(
        (rfd + 1) as usize,
        &raw mut rset as u64,
        0,
        0,
        &raw mut ts as u64,
        0,
    ));
    let elapsed = akuma_primitives::clock::uptime_us().saturating_sub(started);

    if let Some(ch) = akuma_exec::process::current_channel() {
        ch.clear_interrupted();
    }
    if prior.is_none() {
        let _ = akuma_exec::process::channel::remove_channel(tid);
    }
    proc.remove_fd(wfd);
    proc.remove_fd(rfd);
    unregister_thread_pid(tid);
    unregister_process(pid);

    // Both halves matter: the right errno, and returning it promptly rather
    // than after a full timeout that happened to be reported as an error.
    let pass = rc == EINTR && elapsed < 200_000;
    if pass {
        akuma_primitives::safe_print!(128, "  [PASS] pselect6_returns_eintr\n");
    } else {
        akuma_primitives::safe_print!(
            160,
            "  [FAIL] pselect6_returns_eintr rc={} (want {}) elapsed={}us\n",
            rc,
            EINTR,
            elapsed
        );
    }
}

pub(super) fn sys_ppoll(fds_ptr: u64, nfds: usize, timeout_ptr: u64, _sigmask: u64) -> SysResult {
    // nfds == 0 is NOT "nothing to do, return immediately" — Linux blocks until
    // the timeout (or forever, if timeout is NULL) or a signal, and musl's
    // pause() is implemented as exactly `ppoll(NULL, 0, NULL, ...)`. Returning 0
    // here made pause() (and therefore alarm()+pause()) a no-op that returned
    // instantly instead of blocking. fds_ptr/fds_size stay unvalidated/unused
    // below when nfds == 0, matching musl passing a NULL fds pointer.
    let fds_size = nfds * core::mem::size_of::<PollFd>();
    if nfds > 0 && !validate_user_ptr(fds_ptr, fds_size) { return Err(EFAULT); }

    // NULL timeout = block indefinitely. `infinite`/`timeout_us` stay two
    // locals because the wait loops below read them separately; the pair is
    // derived from one `Option` so they cannot disagree.
    let (infinite, timeout_us) = match time::read_timeout_us(timeout_ptr)? {
        Some(us) => (false, us),
        None => (true, 0),
    };

    if crate::config::SYSCALL_DEBUG_NET_ENABLED && nfds > 0 {
        let pid = akuma_exec::process::read_current_pid().unwrap_or(0);
        akuma_primitives::tprint!(128, "[ppoll] enter: pid={} nfds={} timeout_us={}\n", pid, nfds, 
            if infinite { u64::MAX } else { timeout_us });
    }

    let start_time = akuma_primitives::clock::uptime_us();
    let mut kernel_fds = alloc::vec![PollFd { fd: 0, events: 0, revents: 0 }; nfds];
    if nfds > 0 && copy_from_user(as_user_bytes_mut(&mut kernel_fds), fds_ptr).is_err() {
        return Err(EFAULT);
    }

    // Register the calling thread as a waker on each polled fd's underlying
    // primitive (pipe/socket/etc.) so a peer write wakes us IMMEDIATELY (sticky
    // WOKEN_STATES → schedule_blocking returns at once), instead of only at the
    // next BLOCKING_POLL_INTERVAL_US re-poll. Without this, a request/response
    // protocol over a pipe (e.g. the rump sysproxy channel: server blocks in
    // poll(INFTIM) on the channel fd) pays ~interval+scheduling latency PER
    // transfer — turning each forwarded syscall into hundreds of ms. epoll
    // already does this (passes Some(&waker)); ppoll did not.
    let waker = akuma_exec::threading::current_thread_waker();

    // Rump sockets have no push readiness yet (MSG_PEEK probe only), so a
    // shorter per-iteration sleep ceiling keeps their poll cadence tight.
    let has_rump_fd = kernel_fds.iter().any(|p| fd_wants_rump_poll_interval(p.fd));

    // The wait policy this syscall has always had, now stated instead of
    // open-coded: one poll per lap, no fruitless-progress spin, no epoch guard,
    // `>=` timeout, and a park whose waker was registered during the scan
    // above. See `akuma_net_yarn::WaitPolicy::epoll`.
    let mut machine = WaitMachine::new(
        start_time,
        if infinite { None } else { Some(timeout_us) },
        WaitPolicy::epoll(effective_poll_interval_us(has_rump_fd)),
    );

    loop {
        let budget = machine.lap_start(0);

        // BKL carve-out (Phase 7b piece 1) — see the identical comment on the
        // `sys_epoll_pwait` poll() call above.
        #[cfg(all(kernel_smp_shared, kernel_no_bkl_network))]
        akuma_exec::bkl::dropped_window_open();
        // The drain below is `#[cfg(feature = "smoltcp")]`, so on a rump-only
        // build nothing ever assigns this and the `mut` is genuinely unused.
        #[cfg_attr(not(feature = "smoltcp"), allow(unused_mut))]
        let mut progress = false;
        for _ in 0..budget {
            #[cfg(feature = "smoltcp")]
            {
                progress = akuma_net::smoltcp_net::poll();
            }
        }
        #[cfg(all(kernel_smp_shared, kernel_no_bkl_network))]
        akuma_exec::bkl::dropped_window_close();
        let mut ready_count = 0;

        for fd in &mut kernel_fds {
            fd.revents = 0;
            if fd.fd < 0 { continue; }

            // `POLL*` <-> `EPOLL*` in both directions, including which bits are
            // maskable and which are not — see `akuma_syscalls_poll::pollfd`,
            // which also records what this translation drops (`POLLRDHUP`,
            // `POLLPRI`, `POLLNVAL`). The bit numbers used to be written here
            // as `1`/`4`/`16`/`8` with the names in trailing comments.
            let revents = epoll_check_fd_readiness(
                fd.fd as u32,
                pollfd::requested(fd.events),
                Some(&waker),
            );
            fd.revents = pollfd::report(fd.events, revents);

            if fd.revents != 0 {
                ready_count += 1;
            }
        }

        let ready = ready_count > 0;
        let obs = Observation {
            now_us: akuma_primitives::clock::uptime_us(),
            poll_epoch: 0, // no epoch guard in this family — see WaitPolicy::epoll
            progress,
            condition_met: ready,
            // Short-circuit, as the open-coded loop did: a poll that is already
            // satisfied must not report EINTR for work it actually completed.
            interrupted: !ready && akuma_exec::process::should_interrupt_blocking_syscall(),
        };

        match machine.lap_end(&obs) {
            WaitStep::Ready => {
                if nfds > 0 && copy_to_user(fds_ptr, as_user_bytes(&kernel_fds)).is_err() {
                    return Err(EFAULT);
                }
                return Ok(ready_count as u64);
            }
            // A timeout is a normal return for poll(2): zero fds ready.
            WaitStep::Failed(WaitError::TimedOut) => return Ok(0),
            // The signal is delivered by the syscall-return path; without
            // returning to it the wake is consumed and ppoll sleeps through
            // its own SIGALRM.
            WaitStep::Failed(WaitError::Interrupted) => return Err(EINTR),
            WaitStep::Relap(_) => {}
            WaitStep::Park { deadline_us, .. } => {
                akuma_exec::threading::schedule_blocking(deadline_us);
            }
        }
    }
}
