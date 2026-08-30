//! Tunables: socket-set capacity, buffer sizes, timeouts, ephemeral ports.

use super::*;

// Constants
// ============================================================================

#[cfg(not(any(feature = "small-sockets", kernel_profile_extreme)))]
pub(crate) const MAX_SOCKETS: usize = 256;
#[cfg(all(
    any(feature = "small-sockets", kernel_profile_extreme),
    any(not(feature = "many-sessions"), kernel_profile_extreme)
))]
pub(crate) const MAX_SOCKETS: usize = 32;
/// The size-constrained profiles' budget, raised for `many-sessions`.
///
/// `devbox-smoltcp` pulls in `no-tests` → `small-sockets`, so it lands on the
/// 32-socket arm. That is not enough to host a 32-deep listener backlog *and*
/// two dozen accepted connections (see `socket::MAX_BACKLOG`) — the listener
/// alone would consume the entire budget and every `accept()` would fail. 128
/// covers a full backlog plus sshd's 24 sessions plus the rest of the system,
/// at 32 KB per socket ≈ 4 MB worst case (plus ~44 KB of BSS for the table
/// itself), which the devbox's RAM allows and `extreme-size`'s 4 MB floor would
/// not — hence `kernel_profile_extreme` overriding the feature above.
#[cfg(all(
    feature = "small-sockets",
    feature = "many-sessions",
    not(kernel_profile_extreme)
))]
pub(crate) const MAX_SOCKETS: usize = 512;

/// Where [`SOCKET_SOFT_CAP`] starts, and the size the table behaves as until
/// pressure forces it wider.
///
/// `MAX_SOCKETS` is a *ceiling on the static storage*, not the operating size.
/// Sizing the operating table generously is actively harmful: measured
/// 2026-08-20, `iface.poll()` walks the whole `SocketSet` on every call, so
/// per-poll cost tracks the table — **10.6 us at 128 slots, 45 us at 2048** —
/// and the set saturates to whatever cap it is given (2047/2048 observed),
/// because `TimeWait` accumulates faster than the 30 s timeout releases. A
/// 2048-slot table measured **848 req/s against 1,052 at 128**.
///
/// Derived rather than per-profile so `extreme-size` (32 slots) starts at its
/// own ceiling and never grows, while the devbox starts at the 128 that
/// measured best and keeps the rest as headroom.
pub(crate) const SOCKET_SOFT_CAP_START: usize = if MAX_SOCKETS < 128 { MAX_SOCKETS } else { 128 };

// Reduced from 64KB to 16KB per direction to save heap memory.
// 40 sockets × 32KB = 1.25MB vs 40 × 128KB = 5MB.
// 16KB is still plenty for TLS handshakes and HTTP requests.
/// RX-path counters. Plain atomics rather than log statements on purpose: this
/// path runs inside the `NETWORK` critical section with preemption disabled, where
/// console I/O can deadlock (see `poll()`). Read them with [`rx_counters`].
pub(crate) static RX_BUFFERS_POSTED: AtomicUsize = AtomicUsize::new(0);
pub(crate) static RX_BEGIN_FAILURES: AtomicUsize = AtomicUsize::new(0);
pub(crate) static RX_FRAMES_RECEIVED: AtomicUsize = AtomicUsize::new(0);

/// `(buffers posted, receive_begin failures, frames received)`.
///
/// A receive path that posts buffers but never receives a frame is otherwise
/// invisible: the device has nowhere to put frames, drops them all, and it reads
/// as "the network is down" rather than "nothing consumed our buffer".
#[must_use]
pub fn rx_counters() -> (usize, usize, usize) {
    (
        RX_BUFFERS_POSTED.load(Ordering::Relaxed),
        RX_BEGIN_FAILURES.load(Ordering::Relaxed),
        RX_FRAMES_RECEIVED.load(Ordering::Relaxed),
    )
}

pub(crate) const TCP_RX_BUFFER_SIZE: usize = 16384;
pub(crate) const TCP_TX_BUFFER_SIZE: usize = 16384;
pub(crate) const EPHEMERAL_PORT_START: u16 = 49152;
pub(crate) const SOCKET_GC_TIMEOUT_US: u64 = 30_000_000; // 30 seconds

/// How long a TCP socket may sit in `SynSent` before the connect is abandoned.
///
/// Without this a non-blocking connect to a black hole **never** fails: smoltcp
/// retransmits the SYN forever unless a `timeout` is set, and nothing in this
/// tree sets one. The *blocking* path already gave up — `finish_connect_wait`
/// caps its wait at 10 s — so the two paths disagreed, and only the caller who
/// asked for the non-blocking version got an unbounded hang. This makes them
/// agree.
///
/// smoltcp's own `Socket::set_timeout` is deliberately NOT used for this. It is
/// an *inactivity* timeout that stays armed in every state, so it would also
/// abort an idle `Established` connection — an ssh session at a prompt, a
/// keep-alive pool, or a stream whose next byte is slow. That last one is the
/// exact failure class `docs/archive/SOCKET_DELAYED_FIRST_BYTE_HANG.md` spent
/// four bugs eliminating. Scoping the deadline to `SynSent` here keeps the
/// connect bounded without putting a ceiling on idle time.
pub(crate) const CONNECT_TIMEOUT_US: u64 = 10_000_000; // 10 s, matching finish_connect_wait

pub(crate) static NEXT_EPHEMERAL_PORT: AtomicU16 = AtomicU16::new(EPHEMERAL_PORT_START);

pub(crate) fn alloc_ephemeral_port() -> u16 {
    let port = NEXT_EPHEMERAL_PORT.fetch_add(1, Ordering::Relaxed);
    if port == 65535 {
        NEXT_EPHEMERAL_PORT.store(EPHEMERAL_PORT_START, Ordering::Relaxed);
        EPHEMERAL_PORT_START
    } else {
        port
    }
}

/// Whether DHCP is enabled. Set during `init()`.
pub(crate) static DHCP_ENABLED: AtomicBool = AtomicBool::new(false);
