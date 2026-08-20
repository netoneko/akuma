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
#[cfg(feature = "smoltcp")]
use crate::runtime::runtime;
// Only `with_table` uses this, and that is smoltcp-only — without the gate a
// rump-only build (scripts/build_devbox.sh) fails on `unused_imports = "deny"`.
#[cfg(feature = "smoltcp")]
use crate::runtime::PreemptGuard;
#[cfg(feature = "smoltcp")]
use smoltcp::socket::tcp;

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
            inner: SocketType::Listener { local_port: port, handles },
            bind_port: Some(port),
            box_id,
            tcp_nodelay: true,
            keepalive: false,
            rcvtimeo_us: None,
            sndtimeo_us: None,
            wakers: Spinlock::new(Vec::new()),
            refs: 1,
            connect_timed_out: false,
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

/// Global table of sockets (indexed by integer "socket descriptor")
#[cfg(feature = "smoltcp")]
static SOCKET_TABLE: Spinlock<Option<Vec<Option<KernelSocket>>>> = Spinlock::new(None);

#[cfg(feature = "smoltcp")]
pub(crate) fn with_table<F, R>(f: F) -> R
where F: FnOnce(&mut Vec<Option<KernelSocket>>) -> R 
{
    // Preemption disabled for the whole hold: the SOCKET_TABLE spinlock (and the
    // NETWORK lock nested under it via socket ops) must never be stranded across a
    // context switch under the BKL (see `PreemptGuard`). The closure `f` must not
    // yield — the same discipline the native stack already followed single-core.
    let _pg = PreemptGuard::new();
    let mut guard = SOCKET_TABLE.lock();
    if guard.is_none() {
        *guard = Some(Vec::new());
    }
    f(guard.as_mut().unwrap())
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
        if table.len() < MAX_SOCKETS {
            table.push(Some(socket));
            Some(table.len() - 1)
        } else {
            None
        }
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

/// Set `SO_KEEPALIVE` option on a socket
#[cfg(feature = "smoltcp")]
pub fn set_socket_keepalive(idx: usize, enabled: bool) {
    with_table(|table| {
        if let Some(Some(sock)) = table.get_mut(idx) {
            sock.keepalive = enabled;
        }
    });
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

/// Longest a parked socket waiter sleeps before re-checking on its own.
///
/// A BACKSTOP for the directed wake, not the mechanism. Deliberately the
/// scheduler tick and not `poll.rs`'s 10 ms `BLOCKING_POLL_INTERVAL_US`: this
/// path used to land on the tick via `blocking_relax`, so anchoring here means
/// a wake that never arrives is no worse than the old behaviour, while a wake
/// that does arrive is immediate.
#[cfg(all(feature = "smoltcp", feature = "net-waker-park"))]
const WAIT_PARK_BACKSTOP_US: u64 = 3_000;

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

    // Consecutive rounds where poll() reported progress but `condition` stayed
    // false. See the relax logic below.
    let mut fruitless_progress_rounds: u32 = 0;

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
        let epoch = smoltcp_net::poll_count();

        // Drain all pending network work (not just one poll)
        let mut any_progress = false;
        for _ in 0..64 {
            if !smoltcp_net::poll() {
                break;
            }
            any_progress = true;
        }

        if condition() {
            return Ok(());
        }

        if (runtime().is_current_interrupted)() {
            return Err(libc_errno::EINTR);
        }

        if let Some(timeout) = timeout_us
            && (runtime().uptime_us)() - start > timeout {
                return Err(libc_errno::ETIMEDOUT);
            }

        if !any_progress {
            fruitless_progress_rounds = 0;
            // Someone ELSE advanced the stack while we were polling and checking.
            // Our own `any_progress` is false because our polls found nothing,
            // but the state we just judged `condition` against is already stale.
            // Re-check rather than parking on it: parking here is precisely how a
            // wake gets missed and the wait lands on the 3 ms tick instead.
            if smoltcp_net::poll_count() != epoch {
                crate::nicstat::record_epoch_save();
                continue;
            }
            let relax_t = crate::nicstat::start();
            // Tell the NIC interrupt this core has someone waiting on it, so it
            // can target its wake instead of broadcasting to every core.
            // Park properly instead of the old `blocking_relax()` (yield + WFI).
            // That left this thread READY, so nothing could target it: `wake_all`
            // walked an empty list and the only thing that ended the halt was an
            // unrelated interrupt — measured as httpd's `accept` phase costing
            // 80-88 % of a request (docs/archive/AKUMA_NET_ISSUES.md §8).
            //
            // Register first, re-check, then park — see this function's header
            // for why that order is the correctness argument.
            //
            // Parking still drops the BKL (`schedule_blocking` switches away), so
            // a peer core can drive the RX that satisfies `condition` — the
            // property the old `blocking_relax` was chosen for is preserved.
            wait_park(idx, &mut condition, start, timeout_us);
            crate::nicstat::record_relax(relax_t);
        } else {
            // poll() made progress but not the progress WE need. Under sustained
            // unrelated traffic (a torrent's dozens of peers, DHT chatter) poll()
            // reports progress on nearly every call, so the `!any_progress` branch
            // never runs — and under shared-kernel SMP this loop then busy-spins
            // HOLDING the Big Kernel Lock for the entire wait (an accept with no
            // timeout: forever), starving every peer core. Reproduced 2026-07-24:
            // baseline SMP=4 hard-wedged ([BKL] stuck, owner frozen) the moment
            // aria2c's swarm traffic started. Bound the hold: after a few fruitless
            // progress rounds, relax anyway. `blocking_relax` wakes on the next IRQ
            // (RX under active traffic, else the 10ms tick), so the added latency on
            // a soon-to-be-ready socket is small, and the fast path — condition met
            // within the first rounds — is unchanged.
            fruitless_progress_rounds = fruitless_progress_rounds.wrapping_add(1);
            if fruitless_progress_rounds >= 4 {
                let relax_t = crate::nicstat::start();
                wait_park(idx, &mut condition, start, timeout_us);
                crate::nicstat::record_relax(relax_t);
            }
        }
    }
}

/// Register, re-check, park. The tail of both [`wait_until`] wait arms.
///
/// Returns nothing: the caller loops and re-evaluates `condition` immediately,
/// so the re-check here exists only to close the lost-wake window, not to report
/// a result.
#[cfg(feature = "smoltcp")]
fn wait_park<F>(idx: usize, condition: &mut F, start: u64, timeout_us: Option<u64>)
where F: FnMut() -> bool
{
    #[cfg(feature = "net-waker-park")]
    {
        // 1. Announce ourselves BEFORE the final check.
        socket_add_waker(idx, (runtime().current_waker)());

        // 2. Re-check. A state change that landed during the poll loop above is
        //    caught here; anything later is caught by the waker we just
        //    registered.
        if (*condition)() {
            return;
        }

        // 3. Park. The deadline is the backstop; the waker is the mechanism.
        let now = (runtime().uptime_us)();
        let backstop = now.saturating_add(WAIT_PARK_BACKSTOP_US);
        let deadline = match timeout_us {
            // Never sleep past the caller's own timeout, or a `SO_RCVTIMEO`
            // would fire a whole backstop late.
            Some(t) => backstop.min(start.saturating_add(t)),
            None => backstop,
        };
        (runtime().park_until)(deadline);
    }
    #[cfg(not(feature = "net-waker-park"))]
    {
        let _ = (idx, start, timeout_us);
        let _ = &mut *condition;
        // Yield, then halt until ANY interrupt. Imprecise — nothing can target
        // this thread, because it never enters WAITING — but under load the NIC
        // raises ~6,300 interrupts per 5 s window, so the wake is plentiful. It
        // measured FASTER than the targeted park; see the `net-waker-park`
        // feature notes in the root Cargo.toml.
        //
        // Under shared-kernel SMP this DROPS the Big Kernel Lock across the wait
        // (a plain `yield_now` would spin holding it, freezing every peer core —
        // the meow->LLM `connect`+recv wedge), so a peer's async-main poller can
        // drive the RX that satisfies `condition`.
        (runtime().blocking_relax)();
    }
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
#[cfg(feature = "smoltcp")]
#[must_use]
pub const fn tcp_recv_ready(can_recv: bool, may_recv: bool, state: tcp::State) -> bool {
    can_recv || (!may_recv && tcp_reached_established(state))
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
            if let SocketType::Stream(h) = sock.inner {
                smoltcp_net::socket_close(h);
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

#[cfg(feature = "smoltcp")]
fn has_pending_connection(idx: usize) -> bool {
    let mut result = false;
    with_table(|table| {
        if let Some(Some(KernelSocket { inner: SocketType::Listener { handles, .. }, .. })) = table.get(idx) {
            for &handle in handles {
                let state = with_network(|net| net.sockets.get::<tcp::Socket>(handle).state());
                if state == Some(tcp::State::Established) {
                    result = true;
                    break;
                }
            }
        }
    });
    result
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
        if let Some(Some(KernelSocket { inner: SocketType::Listener { handles, local_port }, .. })) = table.get_mut(idx) {
             let port = *local_port;
             for (i, &handle) in handles.iter().enumerate() {
                let state = with_network(|net| net.sockets.get::<tcp::Socket>(handle).state());
                if state == Some(tcp::State::Established) {
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
    };
    let new_idx = with_table(|table| {
        for (i, slot) in table.iter_mut().enumerate() {
            if slot.is_none() { *slot = Some(new_sock); return Some(i); }
        }
        if table.len() < MAX_SOCKETS { table.push(Some(new_sock)); Some(table.len() - 1) } else { None }
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
        ConnectStep::AlreadyConnected => return Ok(()),
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

    connect_outcome(waited, with_network(|net| net.sockets.get::<tcp::Socket>(h).state()))
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
            tcp_recv_ready(socket.can_recv(), socket.may_recv(), socket.state())
        }).unwrap_or(true);
        if !ready { return Err(libc_errno::EAGAIN); }
    } else {
        wait_until(idx, || with_network(|net| {
            let socket = net.sockets.get::<tcp::Socket>(handle);
            tcp_recv_ready(socket.can_recv(), socket.may_recv(), socket.state())
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
                        });
                    });
                }
                SocketType::Listener { local_port, .. } => {
                    stats.push(SocketStat {
                        local_port,
                        remote_ip: [0;4],
                        remote_port: 0,
                        state: "LISTEN",
                        box_id: slot.box_id,
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
