//! Kernel Socket Management
//!
//! Provides socket abstractions for userspace programs via syscalls.
//! Wraps smoltcp sockets via the thread-safe `smoltcp_net` module.

use alloc::vec::Vec;
#[cfg(feature = "smoltcp")]
use alloc::collections::VecDeque;
#[cfg(feature = "smoltcp")]
use core::sync::atomic::{AtomicU16, Ordering};
#[cfg(feature = "smoltcp")]
use core::task::Waker;
#[cfg(feature = "smoltcp")]
use spinning_top::Spinlock;

#[cfg(feature = "smoltcp")]
use crate::smoltcp_net::{self, SocketHandle, with_network};

/// The extracted readiness state machine. Every rule this loop enforces lives
/// there and has host tests; this module supplies only the effects.
#[cfg(feature = "smoltcp")]
use akuma_net_yarn::{
    Observation, ParkKind, RelapReason, WaitError, WaitMachine, WaitPolicy, WaitStep,
};
#[cfg(feature = "smoltcp")]
use crate::runtime::runtime;
// Only `with_table` uses this, and that is smoltcp-only — without the gate a
// rump-only build (scripts/build_devbox.sh) fails on `unused_imports = "deny"`.
#[cfg(feature = "smoltcp")]
use crate::runtime::PreemptGuard;
#[cfg(feature = "smoltcp")]
use smoltcp::socket::tcp;
#[cfg(feature = "smoltcp")]
use smoltcp::time::Duration;

// ============================================================================
// Constants
// ============================================================================

/// Maximum number of concurrent sockets (FDs)
pub const MAX_SOCKETS: usize = 128;

/// Maximum number of sockets to pre-allocate for a listener's backlog.
///
/// There is no SYN queue in this stack: a listener *is* its pool of pre-created
/// smoltcp sockets already in `Listen` state, and `socket_accept` replenishes
/// the pool one socket at a time as connections are taken off it. So this is
/// not a soft hint like Linux's `backlog` — it is a hard ceiling on how many
/// connections can arrive *before the server accepts any of them*. Past it, the
/// peer gets a RST, which a client reports as
/// `kex_exchange_identification: read: Connection reset by peer`.
///
/// It is 8 unless the `many-sessions` feature is on, which caps simultaneous
/// arrivals at 8 regardless of how fast the server accepts — measured directly
/// against `userspace/sshd`: clean at 8 concurrent connections, dropping 2-4 of
/// 16. That is the binding limit on sshd's concurrency, and it sits well below
/// sshd's `max_sessions` default of 24, so the process-per-session sshd
/// (`userspace/sshd`'s `fork-sessions` feature) cannot reach its own limit
/// without this raised to match.
///
/// Each entry costs one smoltcp socket, i.e. `TCP_RX_BUFFER_SIZE +
/// TCP_TX_BUFFER_SIZE` = 32 KB of heap held for the listener's whole life, so
/// 32 entries is ~1 MB per listening socket, paid by every listener in the image
/// (`httpd` included), plus ~44 KB of BSS for the larger socket table.
/// `many-sessions` raises `smoltcp_net::MAX_SOCKETS` alongside it — a 32-deep
/// backlog is meaningless if the total socket budget is 32.
///
/// `kernel_profile_extreme` overrides the feature and keeps 8. That profile
/// builds `--no-default-features` so it cannot pick `many-sessions` up by
/// accident today, but the override is written explicitly so that adding the
/// feature to its list later cannot quietly cost it a megabyte per listener
/// against a 4 MB floor.
#[cfg(all(feature = "smoltcp", feature = "many-sessions", not(kernel_profile_extreme)))]
const MAX_BACKLOG: usize = 32;
#[cfg(all(
    feature = "smoltcp",
    any(not(feature = "many-sessions"), kernel_profile_extreme)
))]
const MAX_BACKLOG: usize = 8;

/// Ephemeral port range start
pub const EPHEMERAL_PORT_START: u16 = 49152;
/// Ephemeral port range end
pub const EPHEMERAL_PORT_END: u16 = 65535;

/// Global atomic for ephemeral port allocation
#[cfg(feature = "smoltcp")]
static NEXT_EPHEMERAL_PORT: AtomicU16 = AtomicU16::new(EPHEMERAL_PORT_START);

// ============================================================================
// Socket Address Types
// ============================================================================

/// IPv4 socket address
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SocketAddrV4 {
    pub ip: [u8; 4],
    pub port: u16,
}

impl SocketAddrV4 {
    #[must_use] 
    pub const fn new(ip: [u8; 4], port: u16) -> Self {
        Self { ip, port }
    }
}

/// Linux `sockaddr_in` structure (for syscall interface)
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct SockAddrIn {
    pub sin_family: u16,    // AF_INET = 2
    pub sin_port: u16,      // Network byte order (big-endian)
    pub sin_addr: u32,      // Network byte order (big-endian)
    pub sin_zero: [u8; 8],  // Padding
}

impl SockAddrIn {
    /// Convert to `SocketAddrV4`.
    /// `sin_addr` is in network byte order — raw memory bytes ARE the IP octets.
    #[must_use] 
    pub const fn to_addr(&self) -> SocketAddrV4 {
        let ip_bytes = self.sin_addr.to_ne_bytes();
        let port = u16::from_be(self.sin_port);
        SocketAddrV4::new(ip_bytes, port)
    }

    /// Create from `SocketAddrV4`.
    /// Store IP octets directly as network byte order in `sin_addr`.
    #[must_use] 
    pub const fn from_addr(addr: &SocketAddrV4) -> Self {
        Self {
            sin_family: 2, // AF_INET
            sin_port: addr.port.to_be(),
            sin_addr: u32::from_ne_bytes(addr.ip),
            sin_zero: [0u8; 8],
        }
    }
}

// ============================================================================
// Socket Constants
// ============================================================================

pub mod socket_const {
    pub const AF_INET: i32 = 2;
    pub const SOCK_STREAM: i32 = 1;
    pub const SOCK_DGRAM: i32 = 2;
}

// ============================================================================
// Socket Type
// ============================================================================

#[cfg(feature = "smoltcp")]
pub enum SocketType {
    /// A connected or connecting socket (one smoltcp handle)
    Stream(SocketHandle),
    /// A listening socket (manages a pool of smoltcp handles)
    Listener {
        local_port: u16,
        handles: VecDeque<SocketHandle>,
        /// How deep the pool is *supposed* to be. `handles.len()` can fall
        /// below it when `socket_create` fails while refilling after an
        /// `accept`, and without a target depth to compare against there was
        /// nothing that could ever put the lost slot back — see
        /// [`listener_refresh`].
        backlog: usize,
    },
    /// A UDP socket with an optional default peer (set by connect)
    Datagram {
        handle: SocketHandle,
        peer: Option<SocketAddrV4>,
    },
}

// ============================================================================
// Kernel Socket
// ============================================================================

#[cfg(feature = "smoltcp")]
// Five flags, each an independent per-fd fact with no state to share:
// `tcp_nodelay`/`keepalive` are socket options, `connect_timed_out` and
// `was_connected` are connection history. Bit-packing them would only make the
// call sites that read exactly one of them harder to read.
#[allow(clippy::struct_excessive_bools)]
pub struct KernelSocket {
    pub inner: SocketType,
    pub bind_port: Option<u16>,
    pub box_id: u64,
    /// `TCP_NODELAY` option (disable Nagle's algorithm)
    pub tcp_nodelay: bool,
    /// `SO_KEEPALIVE` option
    pub keepalive: bool,
    /// `SO_RCVTIMEO` / `SO_SNDTIMEO`, in microseconds. `None` is POSIX's
    /// default and POSIX's meaning of a zero timeval: **wait indefinitely**.
    ///
    /// These used to not exist, and the blocking paths carried hardcoded caps
    /// instead — 30 s on recv, 5 s on send — which meant a client got
    /// `ETIMEDOUT` at a deadline it never set, and setting `SO_RCVTIMEO`
    /// (silently accepted, silently dropped) changed nothing. Measured
    /// 2026-08-17 with `nettest-std`: a blocking read of a response delayed
    /// 35 s died at 30069 ms, and a 2 s `SO_RCVTIMEO` fired at 30041 ms.
    pub rcvtimeo_us: Option<u64>,
    pub sndtimeo_us: Option<u64>,
    /// Threads waiting for I/O on this socket (epoll, blocking recv/send)
    pub wakers: Spinlock<Vec<Waker>>,
    /// Number of fd-table references to this socket (fork-inherited fds, dup'd
    /// fds). [`remove_socket`] only destroys the socket when this drops to zero.
    /// Without it, the FIRST close (a fork child's exit, exec's cloexec sweep, a
    /// plain `close(2)` of a dup) destroyed the socket under every other fd still
    /// using it; the freed table slot AND smoltcp handle were then reused by the
    /// next connection, splicing two unrelated TCP streams together (observed as
    /// TLS record bytes inside an SSH session → "message authentication code
    /// incorrect" on the client). Guarded by the SOCKET_TABLE lock, so a plain
    /// integer suffices.
    pub refs: u32,
    /// Set when `poll()` abandoned this socket's connect at
    /// `CONNECT_TIMEOUT_US`. Read (and cleared) by `SO_ERROR`, so an unanswered
    /// connect reports `ETIMEDOUT` instead of the `ECONNREFUSED` a bare `Closed`
    /// socket would otherwise imply.
    pub connect_timed_out: bool,
    /// This socket's TCP connection reached `Established` at least once —
    /// set by `accept` (which only ever hands out a connection that did) and
    /// by a `connect` that succeeded.
    ///
    /// smoltcp keeps no history: a connection **reset** after it was up lands
    /// in `Closed`, which is also where a socket that has never been used
    /// sits, so the state alone cannot tell "no more data ever" from "nothing
    /// yet". [`tcp_reached_established`] therefore has to call `Closed`
    /// not-established, and a blocking `recv()` on a reset connection waited
    /// forever instead of reporting the reset. This flag is the missing
    /// history — see `docs/archive/NGINX_MISSING_SYSCALLS.md` Issue E1.
    pub was_connected: bool,
    /// `shutdown(fd, SHUT_RD)` was called: every later `recv` reports EOF
    /// without looking at the wire. smoltcp has no half-close for the receive
    /// direction (TCP has no such thing on the wire either — it is purely a
    /// local promise), so this bit is the whole implementation.
    pub recv_shutdown: bool,
}

#[cfg(feature = "smoltcp")]
impl KernelSocket {
    #[must_use] 
    pub fn new_stream() -> Option<Self> {
        let handle = smoltcp_net::socket_create()?;
        let box_id = (runtime().current_box_id)();
        Some(Self {
            inner: SocketType::Stream(handle),
            bind_port: None,
            box_id,
            tcp_nodelay: true,  // We disable Nagle by default
            keepalive: false,
            rcvtimeo_us: None,
            sndtimeo_us: None,
            wakers: Spinlock::new(Vec::new()),
            refs: 1,
            connect_timed_out: false,
            was_connected: false,
            recv_shutdown: false,
        })
    }

    #[must_use] 
    pub fn new_datagram() -> Option<Self> {
        let handle = smoltcp_net::udp_socket_create()?;
        let box_id = (runtime().current_box_id)();
        Some(Self {
            inner: SocketType::Datagram { handle, peer: None },
            bind_port: None,
            box_id,
            tcp_nodelay: false,
            keepalive: false,
            rcvtimeo_us: None,
            sndtimeo_us: None,
            wakers: Spinlock::new(Vec::new()),
            refs: 1,
            connect_timed_out: false,
            was_connected: false,
            recv_shutdown: false,
        })
    }

    #[must_use] 
    pub fn new_listener(port: u16, backlog: usize) -> Option<Self> {
        let actual_backlog = backlog.min(MAX_BACKLOG);
        let mut handles = VecDeque::new();
        
        for _ in 0..actual_backlog {
            if let Some(handle) = smoltcp_net::socket_create() {
                with_network(|net| {
                    let socket = net.sockets.get_mut::<tcp::Socket>(handle);
                    let _ = socket.listen(port);
                });
                handles.push_back(handle);
            }
        }
        
        if handles.is_empty() {
            return None;
        }
        
        let box_id = (runtime().current_box_id)();
        
        Some(Self {
            inner: SocketType::Listener { local_port: port, handles, backlog: actual_backlog },
            bind_port: Some(port),
            box_id,
            tcp_nodelay: true,
            keepalive: false,
            rcvtimeo_us: None,
            sndtimeo_us: None,
            wakers: Spinlock::new(Vec::new()),
            refs: 1,
            connect_timed_out: false,
            was_connected: false,
            recv_shutdown: false,
        })
    }

    /// Register a waker for the current thread to be notified of I/O events.
    pub fn add_waker(&self, waker: Waker) {
        let mut wakers = self.wakers.lock();
        // Avoid duplicates (wakers for the same thread)
        if !wakers.iter().any(|w| w.will_wake(&waker)) {
            wakers.push(waker);
        }
    }

    /// How many wakers are registered on this socket right now.
    ///
    /// Diagnostic, and the boot suite's evidence that a blocking waiter actually
    /// announced itself before parking: this read zero for `accept`/`recv` until
    /// `wait_until` started registering (docs/archive/AKUMA_NET_ISSUES.md §8).
    #[must_use]
    pub fn waker_count(&self) -> usize {
        self.wakers.lock().len()
    }

    /// Wake all threads waiting for I/O on this socket.
    pub fn wake_all(&self) {
        let mut wakers = self.wakers.lock();
        for waker in wakers.drain(..) {
            waker.wake();
        }
    }
}

// ============================================================================
// Helpers
// ============================================================================

/// Allocate an ephemeral port
#[cfg(feature = "smoltcp")]
fn alloc_ephemeral_port() -> u16 {
    let port = NEXT_EPHEMERAL_PORT.fetch_add(1, Ordering::Relaxed);
    if port == EPHEMERAL_PORT_END {
        NEXT_EPHEMERAL_PORT.store(EPHEMERAL_PORT_START, Ordering::Relaxed);
        EPHEMERAL_PORT_START
    } else {
        port
    }
}

// ============================================================================
// Global Socket Table
// ============================================================================

/// Global table of sockets (indexed by integer "socket descriptor").
///
/// Fixed at `MAX_SOCKETS` slots rather than a heap `Vec` that grows one `push`
/// at a time up to that same cap — `smoltcp_net.rs`'s `SOCKET_STORAGE` already
/// uses this pattern for the same problem (`docs/archive/VEC_AUDIT.md` #2).
#[cfg(feature = "smoltcp")]
static SOCKET_TABLE: Spinlock<[Option<KernelSocket>; MAX_SOCKETS]> =
    Spinlock::new([const { None }; MAX_SOCKETS]);

#[cfg(feature = "smoltcp")]
pub(crate) fn with_table<F, R>(f: F) -> R
where F: FnOnce(&mut [Option<KernelSocket>; MAX_SOCKETS]) -> R
{
    // Preemption disabled for the whole hold: the SOCKET_TABLE spinlock (and the
    // NETWORK lock nested under it via socket ops) must never be stranded across a
    // context switch under the BKL (see `PreemptGuard`). The closure `f` must not
    // yield — the same discipline the native stack already followed single-core.
    let _pg = PreemptGuard::new();
    let mut guard = SOCKET_TABLE.lock();
    f(&mut guard)
}

/// Allocate a socket index
#[cfg(feature = "smoltcp")]
#[must_use]
pub fn alloc_socket(socket_type: i32) -> Option<usize> {
    let socket = match socket_type {
        socket_const::SOCK_STREAM => KernelSocket::new_stream()?,
        socket_const::SOCK_DGRAM => KernelSocket::new_datagram()?,
        _ => return None,
    };

    with_table(|table| {
        for (i, slot) in table.iter_mut().enumerate() {
            if slot.is_none() {
                *slot = Some(socket);
                return Some(i);
            }
        }
        None
    })
}

#[cfg(feature = "smoltcp")]
pub fn with_socket<F, R>(idx: usize, f: F) -> Option<R>
where F: FnOnce(&KernelSocket) -> R
{
    with_table(|table| {
        table.get(idx).and_then(|slot| slot.as_ref()).map(f)
    })
}

/// Boot-suite hook: run [`wait_until`]'s exact wait discipline against a
/// caller-supplied predicate.
///
/// Exists because the properties that matter — a parked waiter registers itself
/// so `wake_all` can reach it, and a waiter nothing ever wakes still returns on
/// the backstop instead of hanging — are not reachable through the public socket
/// API without a live TCP peer. Used by `src/process_tests.rs`.
#[cfg(feature = "smoltcp")]
#[doc(hidden)]
pub fn wait_until_for_boot_test(
    idx: usize,
    condition: impl FnMut() -> bool,
    timeout_us: Option<u64>,
) -> Result<(), i32> {
    wait_until(idx, condition, timeout_us)
}

/// Park a waker on a native (smoltcp) socket, so `wake_all` can reach it.
///
/// `smoltcp`-gated because everything it touches is: `Waker` is imported under
/// that gate, and `with_table` is the smoltcp socket table. It was ungated,
/// which was invisible for as long as nothing built without smoltcp — the
/// rump-only devbox target had been failing to compile before this, on a
/// separate lost gate in `lib.rs`, so this one never got the chance to be
/// reported. Both callers (`src/syscall/poll.rs`'s `FileDescriptor::Socket` arm
/// and `wait_until` below) are themselves smoltcp-only.
#[cfg(feature = "smoltcp")]
pub fn socket_add_waker(idx: usize, waker: Waker) {
    with_table(|table| {
        if let Some(Some(sock)) = table.get(idx) {
            sock.add_waker(waker);
        }
    });
}

/// Flag the socket owning `handle` as having timed out mid-connect.
///
/// Called from `smoltcp_net::poll()`'s `SynSent` sweep just before it aborts the
/// socket. Linear over the socket table, but only ever on the timeout path.
#[cfg(feature = "smoltcp")]
pub fn mark_connect_timed_out(handle: SocketHandle) {
    with_table(|table| {
        for slot in table.iter_mut().flatten() {
            if let SocketType::Stream(h) = slot.inner
                && h == handle
            {
                slot.connect_timed_out = true;
                return;
            }
        }
    });
}

/// Consume the connect-timeout flag for socket `idx`, if set.
///
/// Consuming matches Linux: `SO_ERROR` reports a pending socket error exactly
/// once and clears it.
#[cfg(feature = "smoltcp")]
#[must_use]
pub fn take_connect_timed_out(idx: usize) -> bool {
    with_table(|table| {
        if let Some(Some(sock)) = table.get_mut(idx) {
            core::mem::take(&mut sock.connect_timed_out)
        } else {
            false
        }
    })
}

/// Add one fd-table reference to socket `idx` (see [`KernelSocket::refs`]).
///
/// Called whenever a `FileDescriptor::Socket` entry is duplicated: fork's fd-table
/// deep copy, `dup`/`dup2`/`F_DUPFD`. Mirrors `pipe_clone_ref` for pipe fds.
#[cfg(feature = "smoltcp")]
pub fn socket_clone_ref(idx: usize) {
    with_table(|table| {
        if let Some(Some(sock)) = table.get_mut(idx) {
            sock.refs += 1;
        }
    });
}

/// No-op when the smoltcp stack is compiled out — see [`remove_socket`]'s shim.
#[cfg(not(feature = "smoltcp"))]
pub fn socket_clone_ref(_idx: usize) {}

/// Drop one fd-table reference to socket `idx`; destroy it on the last close.
///
/// Every fd close path (process-exit `close_all`, exec's
/// cloexec sweep, `close(2)`, `dup2` replacing an entry) calls this, so a fork
/// child's exit no longer tears the socket out from under the parent's live fd
/// (the smoltcp handle would be GC'd and reused by the next connection, splicing
/// two TCP streams together).
#[cfg(feature = "smoltcp")]
pub fn remove_socket(idx: usize) {
    with_table(|table| {
        if idx >= table.len() {
            return;
        }
        if let Some(sock) = table[idx].as_mut() {
            if sock.refs > 1 {
                sock.refs -= 1;
                return;
            }
        } else {
            return;
        }
        if let Some(sock) = table[idx].take() {
            match sock.inner {
                SocketType::Stream(h) => smoltcp_net::socket_close(h),
                SocketType::Listener { handles, .. } => {
                    for h in handles {
                        smoltcp_net::socket_close(h);
                    }
                }
                SocketType::Datagram { handle, .. } => smoltcp_net::udp_socket_close(handle),
            }
        }
    });
}

/// No-op when the smoltcp stack is compiled out (devbox / rump-only).
///
/// There is no smoltcp socket table, and `FileDescriptor::Socket` fds are never
/// created. Kept so the unconditional FD-teardown / `ExecRuntime` callers link.
#[cfg(not(feature = "smoltcp"))]
pub fn remove_socket(_idx: usize) {}

// ============================================================================
// Socket Option Setters
// ============================================================================

/// Set `TCP_NODELAY` option on a socket
#[cfg(feature = "smoltcp")]
pub fn set_tcp_nodelay(idx: usize, enabled: bool) {
    with_table(|table| {
        if let Some(Some(sock)) = table.get_mut(idx) {
            sock.tcp_nodelay = enabled;
        }
    });
}

/// Linux's `tcp_keepalive_time`: idle seconds before the first probe.
///
/// smoltcp has a single interval rather than Linux's time/intvl/probes triple,
/// so this is also what an armed socket repeats at.
#[cfg(feature = "smoltcp")]
pub const KEEPALIVE_IDLE_SECS: u64 = 7200;

/// Set `SO_KEEPALIVE` on a socket, and actually arm it.
///
/// This used to write `sock.keepalive` and stop there. The field was never read
/// anywhere, and smoltcp's `set_keep_alive` — the call that winds the
/// `keep_alive_at` timer — was never reached from anywhere in the crate, so
/// `setsockopt(SO_KEEPALIVE)` reported success and Akuma emitted no keepalive
/// probe, ever (docs/archive/DEVBOX_ISSUES.md Issue 19).
///
/// What this buys is Akuma noticing a peer that vanished without a FIN; it is
/// **not** a fix for a connection Akuma itself tears down, so it does not by
/// itself explain the 300 s `rc=255` in that issue.
///
/// A listener's pooled handles are armed too, so a connection inherits the
/// option from the socket it was accepted on rather than silently losing it.
#[cfg(feature = "smoltcp")]
pub fn set_socket_keepalive(idx: usize, enabled: bool) {
    let interval = enabled.then(|| Duration::from_secs(KEEPALIVE_IDLE_SECS));
    with_table(|table| {
        let Some(Some(sock)) = table.get_mut(idx) else {
            return;
        };
        sock.keepalive = enabled;
        with_network(|net| match &sock.inner {
            SocketType::Stream(h) => {
                net.sockets.get_mut::<tcp::Socket>(*h).set_keep_alive(interval);
            }
            SocketType::Listener { handles, .. } => {
                for h in handles {
                    net.sockets.get_mut::<tcp::Socket>(*h).set_keep_alive(interval);
                }
            }
            // UDP has no connection to keep alive.
            SocketType::Datagram { .. } => {}
        });
    });
}

/// Read back the keep-alive interval smoltcp is actually holding for `idx`.
///
/// Deliberately reads *smoltcp*, not `KernelSocket::keepalive` — the whole
/// defect was those two disagreeing, so a getter that returned the local bool
/// would pass whether or not the option was ever armed.
#[cfg(feature = "smoltcp")]
#[must_use]
pub fn socket_keepalive_interval(idx: usize) -> Option<Duration> {
    with_table(|table| {
        let Some(Some(sock)) = table.get(idx) else {
            return None;
        };
        let SocketType::Stream(h) = sock.inner else {
            return None;
        };
        with_network(|net| net.sockets.get::<tcp::Socket>(h).keep_alive()).flatten()
    })
}

/// Set `SO_RCVTIMEO` (`recv`) or `SO_SNDTIMEO` (`send`), in microseconds.
///
/// `None` means "wait indefinitely", which is both POSIX's default and what
/// POSIX says a zero `timeval` requests — so callers translate a zero timeval
/// to `None` before getting here.
#[cfg(feature = "smoltcp")]
pub fn set_socket_timeout(idx: usize, is_recv: bool, timeout_us: Option<u64>) {
    with_table(|table| {
        if let Some(Some(sock)) = table.get_mut(idx) {
            if is_recv {
                sock.rcvtimeo_us = timeout_us;
            } else {
                sock.sndtimeo_us = timeout_us;
            }
        }
    });
}

/// Read back `SO_RCVTIMEO` / `SO_SNDTIMEO` in microseconds.
///
/// `None` means no timeout. Needed by `getsockopt`: a client that cannot read
/// the option back cannot tell "honoured" from "silently dropped" — which is
/// exactly how the missing implementation stayed invisible.
#[cfg(feature = "smoltcp")]
#[must_use]
pub fn socket_timeout(idx: usize, is_recv: bool) -> Option<u64> {
    with_table(|table| {
        table
            .get(idx)
            .and_then(|slot| slot.as_ref())
            .and_then(|sock| if is_recv { sock.rcvtimeo_us } else { sock.sndtimeo_us })
    })
}

// ============================================================================
// Socket Operations (Blocking with Yield)
// ============================================================================

// The backstop sleep, the 64-poll drain budget and the 4-lap fruitless escape
// all moved to `akuma_net_yarn` (`DEFAULT_BACKSTOP_US`, `DEFAULT_DRAIN_BUDGET`,
// `DEFAULT_FRUITLESS_LIMIT`), where each one has host tests naming the incident
// that produced it.

/// Block until `condition` holds, driving the network stack while we wait.
///
/// Drains all pending network work before checking the condition, since the
/// calling thread is about to block anyway. This ensures TCP ACKs, window
/// updates, and retransmissions are processed promptly.
///
/// `idx` is the socket the caller is waiting on: before parking we register the
/// current thread's waker there, so `smoltcp_net::poll()`'s `wake_all()` can
/// wake us directly instead of leaving us to notice on a timer.
///
/// **Ordering is load-bearing.** Register, THEN re-check, THEN park. A wake that
/// lands between a check and the park is lost if the waker is not in the list
/// yet, and a lost wake here is a hang, not a delay — the same trap
/// `pipe_check_set_reader` (`src/syscall/pipe.rs`) was written to close.
/// `schedule_blocking` adds a second layer: it consults the sticky
/// `WOKEN_STATES` flag and returns without parking if a wake already landed.
#[cfg(feature = "smoltcp")]
fn wait_until<F>(idx: usize, mut condition: F, timeout_us: Option<u64>) -> Result<(), i32>
where F: FnMut() -> bool
{
    let start = (runtime().uptime_us)();
    let mut machine = WaitMachine::new(start, timeout_us, active_wait_policy());

    loop {
        // Wake epoch, read BEFORE we poll. `POLL_COUNT` is bumped by
        // `smoltcp_net::poll()` on every `SocketStateChanged` — by ANY caller,
        // including the netpoll drain thread and other sockets' waiters — so it
        // is an accumulating record of "the stack moved", not a one-shot signal.
        //
        // That is the property `net-waker-park` (§8) failed to get from a waker
        // list: `wake_all()` DRAINS, so a waiter had to re-register every lap and
        // any wake landing during the poll x64 below was simply lost. A counter
        // cannot be lost — if it moved, we notice, whenever we get around to
        // looking.
        let budget = machine.lap_start(smoltcp_net::poll_count() as u64);

        // Drain all pending network work (not just one poll)
        let mut progress = false;
        for _ in 0..budget {
            if !smoltcp_net::poll() {
                break;
            }
            progress = true;
        }

        // `condition` first, and `is_current_interrupted` only if it does not
        // hold. The short-circuit is deliberate: a wait that is already
        // satisfied must never report EINTR for work it actually completed,
        // and this keeps the syscall off the interrupt check on the fast path.
        let condition_met = condition();
        let obs = Observation {
            now_us: (runtime().uptime_us)(),
            poll_epoch: smoltcp_net::poll_count() as u64,
            progress,
            condition_met,
            interrupted: !condition_met && (runtime().is_current_interrupted)(),
        };

        match machine.lap_end(&obs) {
            WaitStep::Ready => return Ok(()),
            WaitStep::Failed(WaitError::Interrupted) => return Err(libc_errno::EINTR),
            WaitStep::Failed(WaitError::TimedOut) => return Err(libc_errno::ETIMEDOUT),
            WaitStep::Relap(RelapReason::EpochMoved) => {
                crate::nicstat::record_epoch_save();
            }
            // poll() made progress but not the progress WE need, and the
            // fruitless budget is not spent. Spin: the condition is usually
            // about to hold.
            WaitStep::Relap(RelapReason::FruitlessProgress) => {}
            WaitStep::Park { kind, deadline_us } => {
                let relax_t = crate::nicstat::start();
                wait_park(idx, &mut condition, kind, deadline_us);
                crate::nicstat::record_relax(relax_t);
            }
        }
    }
}

/// Which park policy this build selects.
///
/// The three arms are mutually exclusive and ordered by specificity, so a build
/// that somehow enables both features gets the newer one rather than a compile
/// error in a cfg thicket. [`ParkKind::LightSleep`] is deliberately
/// unreachable: it has no kernel implementation (see [`wait_park`]).
#[cfg(feature = "smoltcp")]
const fn active_wait_policy() -> WaitPolicy {
    #[cfg(feature = "net-direct-waker")]
    {
        WaitPolicy::direct_waker()
    }
    #[cfg(all(not(feature = "net-direct-waker"), feature = "net-waker-park"))]
    {
        WaitPolicy::targeted()
    }
    #[cfg(all(not(feature = "net-direct-waker"), not(feature = "net-waker-park")))]
    {
        WaitPolicy::promiscuous()
    }
}

/// Register, re-check, park. The tail of both [`wait_until`] wait arms.
///
/// Returns nothing: the caller loops and re-evaluates `condition` immediately,
/// so the re-check here exists only to close the lost-wake window, not to report
/// a result.
/// The three park mechanisms, one arm each. `deadline_us` is absolute and has
/// already been clamped to the caller's own timeout by [`WaitMachine`].
#[cfg(feature = "smoltcp")]
fn wait_park<F>(idx: usize, condition: &mut F, kind: ParkKind, deadline_us: u64)
where F: FnMut() -> bool
{
    match kind {
        ParkKind::Promiscuous => {
            // Yield, then halt until ANY interrupt. Imprecise — nothing can
            // target this thread, because it never enters WAITING — but under
            // load the NIC raises ~6,300 interrupts per 5 s window, so the wake
            // is plentiful. It measured FASTER than the targeted park; see the
            // `net-waker-park` feature notes in the root Cargo.toml.
            //
            // Under shared-kernel SMP this DROPS the Big Kernel Lock across the
            // wait (a plain `yield_now` would spin holding it, freezing every
            // peer core — the meow->LLM `connect`+recv wedge), so a peer's
            // async-main poller can drive the RX that satisfies `condition`.
            (runtime().blocking_relax)();
        }

        // Register, re-check, park. The ordering is the correctness argument,
        // not an optimisation — see this module's `wait_until` header. Both
        // targeted kinds share it and differ only in WHERE the waker is parked.
        ParkKind::Targeted | ParkKind::DirectWaker => {
            // 1. Announce ourselves BEFORE the final check.
            if matches!(kind, ParkKind::DirectWaker) {
                // On the smoltcp socket itself, so the wake fires at the state
                // transition instead of from a list `poll()` walks after it has
                // released NETWORK.
                // A listener has a pool of handles rather than one, so there is
                // nothing to hang a waker on; that waiter rides the backstop.
                if let Some((handle, is_udp)) = smoltcp_handle_for(idx) {
                    let _registered = smoltcp_net::register_socket_waker(
                        handle,
                        is_udp,
                        &(runtime().current_waker)(),
                    );
                }
            } else {
                socket_add_waker(idx, (runtime().current_waker)());
            }

            // 2. Re-check. A state change that landed during the poll loop is
            //    caught here; anything later is caught by the waker we just
            //    registered.
            if (*condition)() {
                return;
            }

            // 3. Park. The deadline is the backstop; the waker is the mechanism.
            (runtime().park_until)(deadline_us);
        }

        // The epoll family's kind: the waker was registered during the
        // readiness scan, so there is nothing to announce here. `wait_until`
        // never selects it (its condition closure is opaque — there is no scan
        // to fold a registration into), so reaching this arm from here is a
        // policy bug; park on the deadline, which is the safe reading.
        ParkKind::ScanRegistered => (runtime().park_until)(deadline_us),

        // No kernel implementation — it needs a scheduler "light sleep" state
        // (targetable AND ended by any interrupt). `active_wait_policy` never
        // selects it; degrade to the shipping default rather than hanging on a
        // park nothing can end.
        ParkKind::LightSleep => (runtime().blocking_relax)(),
    }
}

/// The smoltcp handle behind socket-table slot `idx`, and whether it is UDP.
///
/// `None` for a listener (a pool of handles, not one) and for AF_UNIX, neither
/// of which has a single smoltcp socket to hang a waker on. Callers fall back
/// to the backstop in that case, which is why this returns an `Option` rather
/// than panicking.
#[cfg(feature = "smoltcp")]
fn smoltcp_handle_for(idx: usize) -> Option<(SocketHandle, bool)> {
    with_table(|table| match table.get(idx)?.as_ref()?.inner {
        SocketType::Stream(h) => Some((h, false)),
        SocketType::Datagram { handle, .. } => Some((handle, true)),
        SocketType::Listener { .. } => None,
    })
}

/// The port a `bind` records: the requested one, or a freshly allocated
/// ephemeral when the caller asked for 0.
///
/// Port 0 means "pick one for me". The TCP arm used to store the literal 0, so
/// the *next* `connect` handed smoltcp `local_port = 0` — which it rejects as
/// `Unaddressable` — and every client that binds before connecting failed
/// against a healthy listener. Pure (the allocator is a callback) so that rule
/// has a test that needs no network, and lazy so an explicit port does not burn
/// an ephemeral number on the way past.
#[cfg(feature = "smoltcp")]
#[must_use]
pub fn bind_port_for(requested: u16, ephemeral: impl FnOnce() -> u16) -> u16 {
    if requested == 0 { ephemeral() } else { requested }
}

/// What `connect(2)` must do next, given the socket's current TCP state.
///
/// The redial case is the whole reason this exists: the standard non-blocking
/// idiom is `connect` -> `EINPROGRESS` -> poll -> `connect` again to collect the
/// result, and hiredis (so `redis-cli`) does exactly that. Handing that second
/// call to `smoltcp::tcp::Socket::connect` gets `InvalidState`, which used to be
/// reported as ECONNREFUSED — so a healthy listener looked refused.
#[cfg(feature = "smoltcp")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectStep {
    /// Socket is idle: issue the SYN.
    Dial,
    /// Already established. POSIX says EISCONN, but a redial here is how the
    /// non-blocking idiom asks "did it work?", so report success.
    AlreadyConnected,
    /// A connect is in flight. EALREADY for a non-blocking caller; a blocking
    /// one waits for completion WITHOUT re-issuing the SYN.
    InProgress,
}

/// True once the connection has actually come up, i.e. the socket has left the
/// pre-established states.
///
/// This exists because `smoltcp::tcp::Socket::may_recv()` answers "can I read
/// right now?", not "is the read side finished?", and those differ in exactly
/// one place that matters: a socket still completing its handshake. In
/// `SynSent`, `is_active()` is `true` (it is not `Closed`/`Listen`/`TimeWait`)
/// while `may_recv()` is `false` (it is not `Established` and has nothing
/// buffered) — the same pair of answers a peer's FIN produces.
///
/// Every caller that read `!may_recv()` as "EOF / peer closed" therefore said
/// so about connections that had never been open. Concretely, during the SYN
/// window a client would see:
///
/// - `EPOLLIN` with `recv()` returning `Ok(0)` — a spurious end-of-stream on a
///   socket that had not yet carried a byte, and
/// - `EPOLLRDHUP` — "peer closed its write half" before the peer had accepted.
///
/// A client that polled inside that window concluded the connection was dead
/// and parked forever without ever sending its request. Reproduced 2026-08-17
/// with `nettest-reqwest post <url> 64` (tokio + hyper): roughly 1 run in 3
/// hung with the socket ESTABLISHED and **zero** bytes delivered to the
/// server. The window is one SLIRP round trip wide, which is why it presented
/// as an intermittent hang rather than a consistent failure.
#[cfg(feature = "smoltcp")]
#[must_use]
pub const fn tcp_reached_established(state: tcp::State) -> bool {
    !matches!(
        state,
        tcp::State::Closed
            | tcp::State::Listen
            | tcp::State::SynSent
            | tcp::State::SynReceived
    )
}

/// Whether a TCP socket should report "readable" to a poller, and whether a
/// non-blocking `recv` on it should report EOF rather than `EAGAIN`.
///
/// Buffered data always wins. Absent data, EOF is only real once the
/// connection has been up — see [`tcp_reached_established`].
///
/// `was_connected` is [`KernelSocket::was_connected`], and it is what covers
/// the one state `tcp_reached_established` cannot classify on its own:
/// `Closed`. A connection **reset** after it was serving lands in `Closed`,
/// which is indistinguishable by state alone from a socket that has never been
/// connected — so without this bit a blocking `recv()` on a reset connection
/// waited for readability that could never arrive, and the reader hung
/// forever. That is not hypothetical: it is how `userspace/httpd` died after
/// 24 connect-then-RST connections, accepting one and then blocking on it for
/// good (`docs/archive/NGINX_MISSING_SYSCALLS.md` Issue E1).
#[cfg(feature = "smoltcp")]
#[must_use]
pub const fn tcp_recv_ready(
    can_recv: bool,
    may_recv: bool,
    state: tcp::State,
    was_connected: bool,
) -> bool {
    can_recv || (!may_recv && (was_connected || tcp_reached_established(state)))
}

/// Classify a socket's state into the action `connect` should take.
#[cfg(feature = "smoltcp")]
#[must_use]
pub const fn connect_step(state: tcp::State) -> ConnectStep {
    match state {
        tcp::State::Established => ConnectStep::AlreadyConnected,
        tcp::State::SynSent | tcp::State::SynReceived => ConnectStep::InProgress,
        _ => ConnectStep::Dial,
    }
}

/// Map the end of a blocking connect onto a return value.
///
/// `waited` is what the poll loop reported (a signal or its own deadline);
/// `state` is where the socket actually landed. Every non-`Established` outcome
/// used to collapse into ECONNREFUSED, which made "nothing is listening" and
/// "the connect never completed" indistinguishable from userspace — the reason
/// two separate bugs hid behind one errno.
#[cfg(feature = "smoltcp")]
pub fn connect_outcome(waited: Result<(), i32>, state: Option<tcp::State>) -> Result<(), i32> {
    match (waited, state) {
        (_, Some(tcp::State::Established)) => Ok(()),
        (Err(e), _) => Err(e),
        (Ok(()), Some(tcp::State::Closed)) => Err(libc_errno::ECONNREFUSED),
        (Ok(()), Some(_)) => Err(libc_errno::ETIMEDOUT),
        (Ok(()), None) => Err(libc_errno::ENETDOWN),
    }
}

#[cfg(feature = "smoltcp")]
pub fn socket_bind(idx: usize, addr: SocketAddrV4) -> Result<(), i32> {
    with_table(|table| {
        if let Some(Some(sock)) = table.get_mut(idx) {
            // Port 0 means "pick one for me", and it means that for TCP exactly as
            // it does for UDP. The TCP arm used to store the literal 0, so the next
            // `connect` handed smoltcp `local_port = 0` — rejected as
            // `Unaddressable` — and every client that binds before connecting
            // (busybox `nc`, anything setting a source address) failed with
            // EADDRNOTAVAIL against a healthy listener. A `listen` after a port-0
            // bind now also gets a real ephemeral port instead of listening on 0.
            let port = bind_port_for(addr.port, alloc_ephemeral_port);
            sock.bind_port = Some(port);
            if let SocketType::Datagram { handle, .. } = &sock.inner {
                smoltcp_net::udp_socket_bind(*handle, port).map_err(|()| libc_errno::EINVAL)?;
            }
            Ok(())
        } else {
            Err(libc_errno::EBADF)
        }
    })
}

#[cfg(feature = "smoltcp")]
pub fn socket_listen(idx: usize, backlog: usize) -> Result<(), i32> {
    with_table(|table| {
        if idx >= table.len() || table[idx].is_none() {
            return Err(libc_errno::EBADF);
        }
        
        let port = table[idx].as_ref().unwrap().bind_port.ok_or(libc_errno::EINVAL)?;
        
        if let Some(sock) = table[idx].take() {
            match sock.inner {
                SocketType::Stream(h) => smoltcp_net::socket_close(h),
                // A second `listen()` on an already-listening socket must not
                // orphan the first call's backlog: `listen()` is idempotent on
                // real Linux (it can only adjust the backlog of a live queue),
                // but this used to unconditionally replace the table entry
                // with a freshly allocated `Listener`, leaking the previous
                // `handles` — still bound and `Listen`-ing on the same port
                // inside smoltcp's `SocketSet`, just no longer referenced by
                // anything that calls `accept()`. A SYN smoltcp matched to one
                // of those orphaned handles went Established and then sat
                // invisible forever: nginx's real `ngx_open_listening_sockets`
                // calls `listen()` twice on the master's listening fd, and
                // every connection had ~50% odds of landing on the orphaned
                // set — see `NGINX_MISSING_SYSCALLS.md` Issue D.
                SocketType::Listener { handles, .. } => {
                    for h in handles {
                        smoltcp_net::socket_close(h);
                    }
                }
                _ => {}
            }

            KernelSocket::new_listener(port, backlog).map_or(Err(libc_errno::ENOMEM), |new_sock| {
                table[idx] = Some(new_sock);
                Ok(())
            })
        } else {
            Err(libc_errno::EBADF)
        }
    })
}

/// Can this backlog handle still take part in serving a connection?
///
/// `Listen` is waiting for a SYN, `SynReceived` is mid-handshake, and
/// `Established`/`CloseWait` are connections `accept()` is about to hand out
/// (`CloseWait` included deliberately: a client that sent its request and then
/// closed leaves the handle there with the request still buffered — dropping
/// it would lose a real, complete request).
///
/// Everything else — `Closed` above all — is a handle that will never serve
/// another connection and must be recycled by [`listener_refresh`].
#[cfg(feature = "smoltcp")]
#[must_use]
pub const fn backlog_handle_is_live(state: tcp::State) -> bool {
    matches!(
        state,
        tcp::State::Listen
            | tcp::State::SynReceived
            | tcp::State::Established
            | tcp::State::CloseWait
    )
}

/// Put a listener's backlog back the way `listen()` left it, and report whether
/// a connection is waiting to be accepted.
///
/// **This is what keeps a port answering.** A listener here is not one socket
/// but a pool of `MAX_BACKLOG` pre-`listen()`ed smoltcp sockets, and a pool
/// entry leaves `Listen` the moment a SYN lands on it. `accept()` replaces the
/// handles it hands out, but a handle that reaches `Established` and is then
/// **reset before anyone accepts it** just goes to `Closed` and stays there:
/// nothing in the old code ever called `listen()` on a pool handle again. Under
/// connect-then-RST churn (`SO_LINGER 0`, which is what a load generator and
/// any impatient client do) the pool eroded one handle at a time until zero
/// were listening, at which point the kernel answered every SYN on that port
/// with a RST — permanently, for every process, until the listening fd was
/// closed and rebuilt. Measured directly on 2026-08-20 via the `BACKLOG`
/// column this function's failure made necessary: `32/0/0` → `27/0/5` →
/// … → `0/0/32`, nginx dead at the end of it
/// (`docs/archive/NGINX_MISSING_SYSCALLS.md` Issue E1,
/// `scripts/probes/listener_backlog_churn.py`).
///
/// Recycling is `abort()` + `listen()` on the same handle rather than a fresh
/// `socket_create`: it reuses the buffers already allocated for that slot, so a
/// listener under churn does no allocation at all and cannot be starved by a
/// full socket table. The refill loop below is the second half — it only has
/// work to do when an earlier `accept` could not allocate a replacement.
#[cfg(feature = "smoltcp")]
fn listener_refresh(idx: usize) -> bool {
    let mut pending = false;
    with_table(|table| {
        let Some(Some(KernelSocket {
            inner: SocketType::Listener { handles, local_port, backlog },
            ..
        })) = table.get_mut(idx) else { return };
        let port = *local_port;
        let want = *backlog;

        with_network(|net| {
            for &handle in handles.iter() {
                let socket = net.sockets.get_mut::<tcp::Socket>(handle);
                let state = socket.state();
                if backlog_handle_is_live(state) {
                    if matches!(state, tcp::State::Established | tcp::State::CloseWait) {
                        pending = true;
                    }
                    continue;
                }
                // Dead slot: hand it back to the pool as a fresh listener.
                socket.abort();
                let _ = socket.listen(port);
            }
        });

        // Slots lost entirely (an `accept` whose replacement `socket_create`
        // came back empty). Cheap no-op in the normal case.
        while handles.len() < want {
            let Some(new_h) = smoltcp_net::socket_create() else { break };
            with_network(|net| {
                let _ = net.sockets.get_mut::<tcp::Socket>(new_h).listen(port);
            });
            handles.push_back(new_h);
        }
    });
    pending
}

#[cfg(feature = "smoltcp")]
fn has_pending_connection(idx: usize) -> bool {
    listener_refresh(idx)
}

/// Listener readiness for a poller (`epoll`/`poll`), reaping dead backlog
/// handles on the way past — the same maintenance `accept()` does, on the path
/// an event-driven server actually spends its time in.
///
/// `None` means "not a listening socket", so the caller can fall through to its
/// stream handling. Takes the socket-table lock itself: call it with no table
/// lock held.
#[cfg(feature = "smoltcp")]
#[must_use]
pub fn listener_ready(idx: usize) -> Option<bool> {
    let is_listener = with_socket(idx, |sock| matches!(sock.inner, SocketType::Listener { .. }))?;
    if !is_listener {
        return None;
    }
    Some(listener_refresh(idx))
}

/// The `(listening, pending, dead)` census of a listener's backlog pool, for
/// `/proc/net/tcp` and tests. `None` for anything that is not a listener.
#[cfg(feature = "smoltcp")]
#[must_use]
pub fn listener_backlog_census(idx: usize) -> Option<(u16, u16, u16)> {
    with_socket(idx, |sock| {
        let SocketType::Listener { handles, .. } = &sock.inner else { return None };
        let mut census = (0u16, 0u16, 0u16);
        for &h in handles {
            match with_network(|net| net.sockets.get::<tcp::Socket>(h).state()) {
                Some(tcp::State::Listen) => census.0 += 1,
                Some(s) if backlog_handle_is_live(s) => census.1 += 1,
                _ => census.2 += 1,
            }
        }
        Some(census)
    })?
}

#[cfg(feature = "smoltcp")]
pub fn socket_accept(idx: usize, nonblock: bool) -> Result<(usize, SocketAddrV4), i32> {
    if nonblock {
        if !has_pending_connection(idx) {
            return Err(libc_errno::EAGAIN);
        }
    } else {
        wait_until(idx, || has_pending_connection(idx), None)?;
    }

    let (handle, addr) = with_table(|table| {
        if let Some(Some(KernelSocket { inner: SocketType::Listener { handles, local_port, .. }, .. })) = table.get_mut(idx) {
             let port = *local_port;
             for (i, &handle) in handles.iter().enumerate() {
                let state = with_network(|net| net.sockets.get::<tcp::Socket>(handle).state());
                // `CloseWait` as well as `Established`: the client may have sent
                // its whole request and closed before anyone got here, and that
                // request is still sitting in this handle's receive buffer.
                if matches!(state, Some(tcp::State::Established | tcp::State::CloseWait)) {
                    let h = handles.remove(i).unwrap();
                    if let Some(new_h) = smoltcp_net::socket_create() {
                        with_network(|net| { let _ = net.sockets.get_mut::<tcp::Socket>(new_h).listen(port); });
                        handles.push_back(new_h);
                    }
                    let remote = with_network(|net| {
                        let socket = net.sockets.get::<tcp::Socket>(h);
                        socket.remote_endpoint().map(|ep| {
                            let smoltcp::wire::IpAddress::Ipv4(addr) = ep.addr;
                            SocketAddrV4 { ip: addr.octets(), port: ep.port }
                        })
                    }).flatten().unwrap_or(SocketAddrV4::new([0;4], 0));
                    return Some((h, remote));
                }
             }
        }
        None
    }).ok_or(libc_errno::ECONNABORTED)?;

    let current_box_id = (runtime().current_box_id)();
    let new_sock = KernelSocket { 
        inner: SocketType::Stream(handle), 
        bind_port: None,
        box_id: current_box_id,
        tcp_nodelay: true,
        keepalive: false,
        rcvtimeo_us: None,
        sndtimeo_us: None,
        wakers: Spinlock::new(Vec::new()),
        refs: 1,
        connect_timed_out: false,
        // By construction: `accept` only hands out a handle that reached
        // `Established`, so a later `Closed` on it is a reset, not a socket
        // that never connected.
        was_connected: true,
        recv_shutdown: false,
    };
    let new_idx = with_table(|table| {
        for (i, slot) in table.iter_mut().enumerate() {
            if slot.is_none() { *slot = Some(new_sock); return Some(i); }
        }
        None
    }).ok_or(libc_errno::ENOMEM)?;

    Ok((new_idx, addr))
}

#[cfg(feature = "smoltcp")]
pub fn socket_connect(idx: usize, addr: SocketAddrV4, nonblock: bool) -> Result<(), i32> {
    let is_dgram = with_table(|table| {
        if let Some(Some(sock)) = table.get(idx) {
            matches!(&sock.inner, SocketType::Datagram { .. })
        } else {
            false
        }
    });
    if is_dgram {
        return with_table(|table| {
            if let Some(Some(sock)) = table.get_mut(idx)
                && let SocketType::Datagram { peer, handle } = &mut sock.inner {
                    *peer = Some(addr);
                    if sock.bind_port.is_none() {
                        let port = alloc_ephemeral_port();
                        sock.bind_port = Some(port);
                        let _ = smoltcp_net::udp_socket_bind(*handle, port);
                    }
                    return Ok(());
                }
            Err(libc_errno::EBADF)
        });
    }

    let (h, bound_port): (SocketHandle, Option<u16>) = with_table(|table| {
        if let Some(Some(sock)) = table.get(idx)
            && let SocketType::Stream(handle) = sock.inner {
                return Some((handle, sock.bind_port));
            }
        None
    }).ok_or(libc_errno::EBADF)?;

    let local_port = bound_port.unwrap_or_else(|| {
        let p = alloc_ephemeral_port();
        with_table(|table| {
            if let Some(Some(sock)) = table.get_mut(idx) {
                sock.bind_port = Some(p);
            }
        });
        p
    });

    // A REDIAL — connect(2) called again on a fd whose first connect is still in
    // flight (the standard non-blocking idiom: connect -> EINPROGRESS -> poll ->
    // connect again to collect the result) — must NOT be handed to
    // `smoltcp::connect`, which rejects any non-`Closed` socket with
    // `InvalidState`. Reporting that as ECONNREFUSED made every such caller
    // (`redis-cli`/hiredis is the reference case) fail against a listener that was
    // up and healthy. POSIX: EISCONN once established, EALREADY while connecting.
    let Some(state_before) = with_network(|net| net.sockets.get::<tcp::Socket>(h).state()) else {
        return Err(libc_errno::ENETDOWN);
    };
    match connect_step(state_before) {
        ConnectStep::AlreadyConnected => { mark_was_connected(idx); return Ok(()); }
        ConnectStep::InProgress if nonblock => return Err(libc_errno::EALREADY),
        // Blocking redial: wait for the in-flight connect to finish rather than
        // re-issuing the SYN.
        ConnectStep::InProgress => return finish_connect_wait(idx, h),
        ConnectStep::Dial => {}
    }

    let res = with_network(|net| {
        let socket = net.sockets.get_mut::<tcp::Socket>(h);
        let cx = net.iface.context();
        socket.connect(cx,
            (smoltcp::wire::IpAddress::Ipv4(smoltcp::wire::Ipv4Address::from(addr.ip)), addr.port),
            local_port
        )
    });

    match res {
        Some(Ok(())) => {},
        // Distinguish the two smoltcp refusals: an unroutable/unaddressable remote
        // is EADDRNOTAVAIL, a socket that is already open is EISCONN. Collapsing
        // both into ECONNREFUSED hid which one was happening.
        Some(Err(smoltcp::socket::tcp::ConnectError::Unaddressable)) => {
            return Err(libc_errno::EADDRNOTAVAIL)
        }
        Some(Err(smoltcp::socket::tcp::ConnectError::InvalidState)) => {
            return Err(libc_errno::EISCONN)
        }
        None => return Err(libc_errno::ENETDOWN),
    }

    // The SYN is out; arm the connect deadline (see `CONNECT_TIMEOUT_US`). The
    // blocking path below has its own 10 s cap, but it shares the deadline so a
    // socket abandoned by `poll()` is reported the same way whoever asked.
    smoltcp_net::note_connect_started(h);

    if nonblock {
        return Err(libc_errno::EINPROGRESS);
    }

    finish_connect_wait(idx, h)
}

/// Block until a TCP socket that is already in `SynSent` either establishes or
/// dies, and map the outcome to a POSIX errno via [`connect_outcome`].
///
/// Split out of [`socket_connect`] so a blocking *redial* takes the same path as
/// the first call.
#[cfg(feature = "smoltcp")]
fn finish_connect_wait(idx: usize, h: SocketHandle) -> Result<(), i32> {
    let waited = wait_until(idx, || {
        with_network(|net| {
            let socket = net.sockets.get::<tcp::Socket>(h);
            matches!(socket.state(), tcp::State::Established | tcp::State::Closed | tcp::State::Closing | tcp::State::TimeWait)
        }).unwrap_or(true)
    }, Some(10_000_000));

    let outcome = connect_outcome(waited, with_network(|net| net.sockets.get::<tcp::Socket>(h).state()));
    if outcome.is_ok() {
        mark_was_connected(idx);
    }
    outcome
}

/// Record that this fd's connection reached `Established`. See
/// [`KernelSocket::was_connected`].
#[cfg(feature = "smoltcp")]
fn mark_was_connected(idx: usize) {
    with_table(|table| {
        if let Some(Some(sock)) = table.get_mut(idx) {
            sock.was_connected = true;
        }
    });
}

/// `shutdown(2)` on a TCP socket: half-close without giving up the fd.
///
/// This was a `return 0` stub, and the missing FIN cost a full five seconds per
/// request against nginx. nginx finishes a response with a *lingering close*
/// (`ngx_http_set_lingering_close`): `shutdown(SHUT_WR)` first — which is what
/// tells the client "response complete, EOF" — then it keeps reading whatever
/// the client still has in flight for up to `lingering_timeout`, default 5 s.
/// With the FIN silently dropped, a client reading to end-of-response saw
/// nothing at all until nginx's 5 s timer expired and it closed for real:
/// `--mode http` "times out waiting for connection close" in
/// `docs/archive/NGINX_MISSING_SYSCALLS.md` Issue E, measured here at
/// 5007 ms/request against 3 ms for `userspace/httpd`, which happens to
/// `close()` immediately after its own `shutdown`.
///
/// `SHUT_WR` sends the FIN (smoltcp `close()`, so the socket lands in
/// `FinWait1` and can still *receive* — nginx's lingering read depends on
/// that). `SHUT_RD` is local-only bookkeeping, see
/// [`KernelSocket::recv_shutdown`]. Neither frees the fd; only `close(2)` does.
#[cfg(feature = "smoltcp")]
pub fn socket_shutdown(idx: usize, how: i32) -> Result<(), i32> {
    const SHUT_RD: i32 = 0;
    const SHUT_WR: i32 = 1;
    const SHUT_RDWR: i32 = 2;
    if !matches!(how, SHUT_RD | SHUT_WR | SHUT_RDWR) {
        return Err(libc_errno::EINVAL);
    }

    let handle = with_table(|table| {
        let sock = table.get_mut(idx)?.as_mut()?;
        if matches!(how, SHUT_RD | SHUT_RDWR) {
            sock.recv_shutdown = true;
        }
        match sock.inner {
            SocketType::Stream(h) => Some(h),
            _ => None,
        }
    });

    let Some(handle) = handle else {
        // A listener or a UDP socket: nothing to half-close, but the fd is
        // real, so this is not an error the caller should act on.
        return Ok(());
    };

    if matches!(how, SHUT_WR | SHUT_RDWR) {
        with_network(|net| net.sockets.get_mut::<tcp::Socket>(handle).close());
        // Put the FIN on the wire now rather than at the next poll: the whole
        // point of this call is that the peer learns about it promptly.
        smoltcp_net::poll();
    }

    with_table(|table| {
        if let Some(Some(sock)) = table.get(idx) {
            sock.wake_all();
        }
    });
    Ok(())
}

#[cfg(feature = "smoltcp")]
pub fn socket_send(idx: usize, buf: &[u8], nonblock: bool) -> Result<usize, i32> {
    let (handle, sndtimeo) = with_table(|table| {
        if let Some(Some(sock @ KernelSocket { inner: SocketType::Stream(h), .. })) = table.get(idx) {
            Some((*h, sock.sndtimeo_us))
        } else {
            None
        }
    }).ok_or(libc_errno::EBADF)?;

    if nonblock {
        let can = with_network(|net| net.sockets.get::<tcp::Socket>(handle).can_send()).unwrap_or(false);
        if !can { return Err(libc_errno::EAGAIN); }
    } else {
        // `sndtimeo` (None = forever) replaces what used to be an unconditional
        // 5 s cap. A blocking write that cannot make room in the transmit
        // buffer must block, not invent an `ETIMEDOUT` the caller never asked
        // for — see `KernelSocket::sndtimeo_us`.
        wait_until(idx, || with_network(|net| net.sockets.get::<tcp::Socket>(handle).can_send()).unwrap_or(true), sndtimeo)?;
    }

    let res = with_network(|net| {
        let socket = net.sockets.get_mut::<tcp::Socket>(handle);
        if !socket.can_send() { return Err(libc_errno::EPIPE); }
        socket.send_slice(buf).map_err(|_| libc_errno::EIO)
    });
    
    if matches!(res, Some(Ok(_))) {
        with_table(|table| {
            if let Some(Some(sock)) = table.get(idx) {
                sock.wake_all();
            }
        });
    }

    smoltcp_net::poll();
    
    res.unwrap_or(Err(libc_errno::ENETDOWN))
}

#[cfg(feature = "smoltcp")]
pub fn socket_recv(idx: usize, buf: &mut [u8], nonblock: bool) -> Result<usize, i32> {
    let (handle, rcvtimeo) = with_table(|table| {
        if let Some(Some(sock @ KernelSocket { inner: SocketType::Stream(h), .. })) = table.get(idx) {
            Some((*h, sock.rcvtimeo_us))
        } else {
            None
        }
    }).ok_or(libc_errno::EBADF)?;

    // Latch the connection's history before parking on it. `accept`/`connect`
    // already set this, but a fd that finished a NON-blocking connect has only
    // ever been observed by the poller, so catch that here too: once the socket
    // is seen in a post-handshake state, a later `Closed` on it is a reset and
    // must wake this read rather than silently never satisfying it.
    if with_socket(idx, |sock| sock.recv_shutdown).unwrap_or(false) {
        return Ok(0);
    }

    let was_connected = with_table(|table| {
        let sock = table.get_mut(idx)?.as_mut()?;
        if !sock.was_connected
            && with_network(|net| tcp_reached_established(net.sockets.get::<tcp::Socket>(handle).state()))
                .unwrap_or(false)
        {
            sock.was_connected = true;
        }
        Some(sock.was_connected)
    }).unwrap_or(false);

    // EOF is signalled ONLY by `!may_recv()` — the peer closed its send half (FIN) or the
    // connection was reset. `!is_active()` is deliberately NOT treated as EOF here: a live
    // ESTABLISHED socket can momentarily read `!is_active()` across smoltcp poll boundaries,
    // and reporting that as `Ok(0)` gives a SPURIOUS end-of-stream — fatal for an interactive
    // reader (e.g. sshd's bridge treats one `recv()==0` as "client closed" and stops reading
    // input). `may_recv()` stays false once a real FIN/RST arrives, so genuine EOF still works.
    if nonblock {
        smoltcp_net::poll();
        let ready = with_network(|net| {
            let socket = net.sockets.get::<tcp::Socket>(handle);
            tcp_recv_ready(socket.can_recv(), socket.may_recv(), socket.state(), was_connected)
        }).unwrap_or(true);
        if !ready { return Err(libc_errno::EAGAIN); }
    } else {
        wait_until(idx, || with_network(|net| {
            let socket = net.sockets.get::<tcp::Socket>(handle);
            tcp_recv_ready(socket.can_recv(), socket.may_recv(), socket.state(), was_connected)
        // `rcvtimeo` (None = forever) replaces what used to be an unconditional
        // 30 s cap — the one a 35 s-delayed response tripped at 30069 ms, and
        // the one that made a 2 s `SO_RCVTIMEO` fire at 30041 ms. POSIX: a
        // blocking read with no `SO_RCVTIMEO` blocks until data, EOF or a
        // signal.
        }).unwrap_or(true), rcvtimeo)?;
    }

    let res = with_network(|net| {
        let socket = net.sockets.get_mut::<tcp::Socket>(handle);
        if socket.can_recv() {
            socket.recv(|data| {
                let len = data.len().min(buf.len());
                buf[..len].copy_from_slice(&data[..len]);
                (len, len)
            }).map_err(|_| libc_errno::EIO)
        } else if !socket.may_recv() && socket.state() == tcp::State::Closed && was_connected {
            // A connection that was up and is now `Closed` was ABORTED — reset by
            // the peer, or given up on by smoltcp's own timeout. A graceful FIN
            // parks the socket in `CloseWait` and never reaches here, so this is
            // never a clean end-of-stream and must not be reported as one:
            // `Ok(0)` would tell an HTTP server "request complete" about a
            // request that was cut in half.
            Err(libc_errno::ECONNRESET)
        } else if !socket.may_recv() && tcp_reached_established(socket.state()) {
            // Real EOF: the peer closed its write half of a connection that was
            // up. A socket still in SynSent also answers `!may_recv()`, and
            // reporting THAT as `Ok(0)` handed the caller an end-of-stream
            // before the handshake had finished.
            Ok(0)
        } else { Err(libc_errno::EAGAIN) }
    });

    if matches!(res, Some(Ok(_))) {
        with_table(|table| {
            if let Some(Some(sock)) = table.get(idx) {
                sock.wake_all();
            }
        });
    }
    
    smoltcp_net::poll();
    
    res.unwrap_or(Err(libc_errno::ENETDOWN))
}

// ============================================================================
// UDP Socket Operations
// ============================================================================

#[cfg(feature = "smoltcp")]
pub fn socket_send_udp(idx: usize, buf: &[u8], dest: SocketAddrV4) -> Result<usize, i32> {
    let handle = with_table(|table| {
        if let Some(Some(KernelSocket { inner: SocketType::Datagram { handle, .. }, bind_port, .. })) = table.get_mut(idx) {
            if bind_port.is_none() {
                let port = alloc_ephemeral_port();
                *bind_port = Some(port);
                let _ = smoltcp_net::udp_socket_bind(*handle, port);
            }
            Some(*handle)
        } else {
            None
        }
    }).ok_or(libc_errno::EBADF)?;

    let endpoint = smoltcp::wire::IpEndpoint {
        addr: smoltcp::wire::IpAddress::Ipv4(smoltcp::wire::Ipv4Address::from(dest.ip)),
        port: dest.port,
    };

    smoltcp_net::udp_socket_send(handle, buf, endpoint).map_err(|()| libc_errno::EIO)?;

    with_table(|table| {
        if let Some(Some(sock)) = table.get(idx) {
            sock.wake_all();
        }
    });

    smoltcp_net::poll();
    Ok(buf.len())
}

#[cfg(feature = "smoltcp")]
pub fn socket_recv_udp(idx: usize, buf: &mut [u8], nonblock: bool) -> Result<(usize, SocketAddrV4), i32> {
    let handle = with_table(|table| {
        if let Some(Some(KernelSocket { inner: SocketType::Datagram { handle, .. }, .. })) = table.get(idx) {
            Some(*handle)
        } else {
            None
        }
    }).ok_or(libc_errno::EBADF)?;

    if nonblock {
        smoltcp_net::poll();
        if !smoltcp_net::udp_can_recv(handle) {
            return Err(libc_errno::EAGAIN);
        }
    } else {
        wait_until(idx, || {
            smoltcp_net::poll();
            smoltcp_net::udp_can_recv(handle)
        }, Some(10_000_000))?;
    }

    let (len, endpoint) = smoltcp_net::udp_socket_recv(handle, buf).map_err(|()| libc_errno::EIO)?;

    with_table(|table| {
        if let Some(Some(sock)) = table.get(idx) {
            sock.wake_all();
        }
    });

    let smoltcp::wire::IpAddress::Ipv4(ip) = endpoint.addr;
    let src = SocketAddrV4::new(ip.octets(), endpoint.port);
    Ok((len, src))
}

/// Check if a socket index refers to a UDP socket
#[must_use] 
#[cfg(feature = "smoltcp")]
pub fn is_udp_socket(idx: usize) -> bool {
    with_table(|table| {
        if let Some(Some(sock)) = table.get(idx) {
            matches!(&sock.inner, SocketType::Datagram { .. })
        } else {
            false
        }
    })
}

/// Get the default peer for a connected UDP socket
#[must_use] 
#[cfg(feature = "smoltcp")]
pub fn udp_default_peer(idx: usize) -> Option<SocketAddrV4> {
    with_table(|table| {
        if let Some(Some(KernelSocket { inner: SocketType::Datagram { peer, .. }, .. })) = table.get(idx) {
            *peer
        } else {
            None
        }
    })
}

pub struct SocketStat {
    pub local_port: u16,
    pub remote_ip: [u8; 4],
    pub remote_port: u16,
    pub state: &'static str,
    pub box_id: u64,
    /// Backlog census, listener rows only (`(listening, pending, dead)`); all
    /// zero for every other socket type.
    ///
    /// A listener on Akuma is not one socket but a *pool* of `MAX_BACKLOG`
    /// pre-`listen()`ed smoltcp sockets ([`SocketType::Listener`]), and the
    /// only thing that keeps the port answering is that pool still holding
    /// handles in `Listen`. That number was invisible until this column
    /// existed: the row said `LISTEN` whether the pool had 32 listening
    /// handles or none, which is exactly the state
    /// `docs/archive/NGINX_MISSING_SYSCALLS.md` Issue E1 had to be diagnosed
    /// blind. `dead` counts handles in a state that can never serve a
    /// connection again (`Closed`, `TimeWait`, ...) — a non-zero, climbing
    /// `dead` is a listener eroding toward permanent deafness.
    pub backlog: (u16, u16, u16),
}

#[must_use]
#[cfg(feature = "smoltcp")]
pub fn list_sockets() -> Vec<SocketStat> {
    let mut stats = Vec::new();
    let current_box_id = (runtime().current_box_id)();

    with_table(|table| {
        for slot in table.iter().flatten() {
            // Isolation: only show sockets from current box (unless Box 0)
            if current_box_id != 0 && slot.box_id != current_box_id {
                continue;
            }

            match slot.inner {
                SocketType::Stream(h) => {
                    with_network(|net| {
                        let socket = net.sockets.get::<tcp::Socket>(h);
                        let remote = socket.remote_endpoint().map_or(([0;4], 0), |ep| {
                            let smoltcp::wire::IpAddress::Ipv4(addr) = ep.addr;
                            (addr.octets(), ep.port)
                        });
                        
                        let tcp_state = match socket.state() {
                            tcp::State::Closed => "CLOSED",
                            tcp::State::Listen => "LISTEN",
                            tcp::State::SynSent => "SYN_SENT",
                            tcp::State::SynReceived => "SYN_RECV",
                            tcp::State::Established => "ESTABLISHED",
                            tcp::State::FinWait1 => "FIN_WAIT1",
                            tcp::State::FinWait2 => "FIN_WAIT2",
                            tcp::State::CloseWait => "CLOSE_WAIT",
                            tcp::State::Closing => "CLOSING",
                            tcp::State::LastAck => "LAST_ACK",
                            tcp::State::TimeWait => "TIME_WAIT",
                        };

                        stats.push(SocketStat {
                            local_port: slot.bind_port.unwrap_or(0),
                            remote_ip: remote.0,
                            remote_port: remote.1,
                            state: tcp_state,
                            box_id: slot.box_id,
                            backlog: (0, 0, 0),
                        });
                    });
                }
                SocketType::Listener { local_port, ref handles, .. } => {
                    let mut listening = 0u16;
                    let mut pending = 0u16;
                    let mut dead = 0u16;
                    for &h in handles {
                        match with_network(|net| net.sockets.get::<tcp::Socket>(h).state()) {
                            Some(tcp::State::Listen) => listening += 1,
                            Some(st) if backlog_handle_is_live(st) => pending += 1,
                            _ => dead += 1,
                        }
                    }
                    stats.push(SocketStat {
                        local_port,
                        remote_ip: [0;4],
                        remote_port: 0,
                        state: "LISTEN",
                        box_id: slot.box_id,
                        backlog: (listening, pending, dead),
                    });
                }
                SocketType::Datagram { peer, .. } => {
                    let (ip, port) = peer.map_or(([0;4], 0), |p| (p.ip, p.port));
                    stats.push(SocketStat {
                        local_port: slot.bind_port.unwrap_or(0),
                        remote_ip: ip,
                        remote_port: port,
                        state: "UDP",
                        box_id: slot.box_id,
                        backlog: (0, 0, 0),
                    });
                }
            }
        }
    });
    stats
}

/// Empty when the smoltcp stack is compiled out (devbox / rump-only): no smoltcp
/// socket table exists, so `/proc/net/tcp` shows just its header. Kept so the
/// unconditional procfs caller still links.
#[cfg(not(feature = "smoltcp"))]
#[must_use]
pub fn list_sockets() -> Vec<SocketStat> {
    Vec::new()
}

// ============================================================================
// Error Numbers
// ============================================================================

/// Positive `libc` errno values, as the socket layer's `Result<_, i32>` errors
/// carry them.
///
/// These 25 consts used to be defined here, and that was the reason the bin crate
/// kept a second, pre-negated table of its own: a library crate cannot reach the
/// bin crate's privates, so whichever side wrote first, the other side copied. The
/// table now lives in the dependency-free leaf and this is an alias, kept because
/// `libc_errno::EINVAL` reads correctly at the ~100 call sites that already spell
/// it that way. New code may use either path — they are the same items.
///
/// See `akuma_primitives::errno` and
/// `docs/archive/TRIM_FAT_EMBARASSING_DUPLICATIONS.md` §5.7.
pub use akuma_primitives::errno as libc_errno;
