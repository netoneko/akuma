//! The `NETWORK` lock, the `NetworkState` it guards, and the holder
//! instrumentation the SSH stall watchdog reads.

use super::*;

// Global Network State
// ============================================================================

/// Atomic flag indicating the network stack is initialized and ready
pub(crate) static NETWORK_READY: AtomicBool = AtomicBool::new(false);

/// Atomic counter incremented when progress is made (e.g. packets processed)
pub(crate) static POLL_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Counter for silently dropped TX packets (`VirtIO` send failures)
pub(crate) static TX_DROP_COUNT: AtomicUsize = AtomicUsize::new(0);

pub fn is_ready() -> bool {
    NETWORK_READY.load(Ordering::Acquire)
}

pub fn tx_drop_count() -> usize {
    TX_DROP_COUNT.load(Ordering::Relaxed)
}

/// Returns true once DHCP has acquired a lease (Configured event was processed).
/// Returns true immediately if DHCP is disabled.
pub(crate) static DHCP_CONFIGURED: AtomicBool = AtomicBool::new(false);

pub fn is_dhcp_configured() -> bool {
    if !DHCP_ENABLED.load(Ordering::Relaxed) {
        return true;
    }
    DHCP_CONFIGURED.load(Ordering::Acquire)
}

pub fn poll_count() -> usize {
    POLL_COUNT.load(Ordering::Acquire)
}


/// QEMU user-mode networking DNS server address
pub(crate) const QEMU_DNS_SERVER: IpAddress = IpAddress::Ipv4(smoltcp::wire::Ipv4Address::new(10, 0, 2, 3));

pub struct NetworkState {
    pub iface: Interface,
    pub sockets: SocketSet<'static>,
    pub device: LoopbackAwareDevice,
    pub dhcp_handle: Option<SocketHandle>,
    pub dns_handle: SocketHandle,
    /// Sockets closed by the user, waiting for TCP teardown. Tuple: (handle, `close_timestamp_us`).
    pub pending_removal: Vec<(SocketHandle, u64)>,
    /// Handles currently in `SynSent`, with the microsecond the connect was
    /// issued. Swept in `poll()` to enforce [`CONNECT_TIMEOUT_US`]. Holds only
    /// in-flight connects, so it is empty on an idle system and never more than
    /// a few entries deep under load.
    pub connecting: Vec<(SocketHandle, u64)>,
}

/// Global network stack protected by a Spinlock.
pub(crate) static NETWORK: Spinlock<Option<NetworkState>> = Spinlock::new(None);

// ============================================================================
// NETWORK lock holder tracking (instrumentation for the SSH stall watchdog
// in src/main.rs::memory_monitor). All four atomics describe the *current*
// NETWORK lock holder; they are stamped on acquire and cleared on release.
// Reads are best-effort (no acquire ordering vs the spinlock itself); a
// stall report is allowed to see a torn snapshot — the supervisor flags
// long holds, not exact values.
// ============================================================================

/// Thread id of the current NETWORK holder, or `NETWORK_HOLDER_NONE` when
/// the lock is free. We use `u32::MAX` as the sentinel because thread 0 is
/// a real holder (kernel main).
pub const NETWORK_HOLDER_NONE: u32 = u32::MAX;
static NETWORK_HOLDER: AtomicU32 = AtomicU32::new(NETWORK_HOLDER_NONE);
/// Uptime (us) when the current holder acquired the lock. Stale if
/// `NETWORK_HOLDER == NETWORK_HOLDER_NONE`.
static NETWORK_LOCKED_AT_US: AtomicU64 = AtomicU64::new(0);
/// Last call site that acquired the lock. See [`NetSite`] for the enum.
static NETWORK_LAST_SITE: AtomicU8 = AtomicU8::new(NetSite::None as u8);

/// Tag for the last call site that acquired NETWORK. Kept in a u8 atomic so
/// the supervisor can snapshot it cheaply.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
#[repr(u8)]
pub enum NetSite {
    None = 0,
    Poll = 1,
    WithNetwork = 2,
    SocketClose = 3,
    UdpSocketClose = 4,
}

impl NetSite {
    #[must_use]
    pub const fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Poll,
            2 => Self::WithNetwork,
            3 => Self::SocketClose,
            4 => Self::UdpSocketClose,
            _ => Self::None,
        }
    }
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Poll => "poll",
            Self::WithNetwork => "with_network",
            Self::SocketClose => "socket_close",
            Self::UdpSocketClose => "udp_socket_close",
        }
    }
}

/// Cumulative `poll()` entries (incremented before `iface.poll()`).
pub(crate) static POLL_ENTERED: AtomicU64 = AtomicU64::new(0);
/// Cumulative `poll()` exits (incremented after `pending_removal` sweep).
pub(crate) static POLL_EXITED: AtomicU64 = AtomicU64::new(0);

/// Snapshot of the NETWORK lock holder, for use by the SSH stall watchdog.
///
/// Tuple: (`holder_tid`, `locked_at_us`, site, `poll_entered`, `poll_exited`).
/// `holder_tid == NETWORK_HOLDER_NONE` means the lock is currently free.
#[must_use]
pub fn network_holder_snapshot() -> (u32, u64, NetSite, u64, u64) {
    (
        NETWORK_HOLDER.load(Ordering::Relaxed),
        NETWORK_LOCKED_AT_US.load(Ordering::Relaxed),
        NetSite::from_u8(NETWORK_LAST_SITE.load(Ordering::Relaxed)),
        POLL_ENTERED.load(Ordering::Relaxed),
        POLL_EXITED.load(Ordering::Relaxed),
    )
}

/// Record that the current thread has just acquired NETWORK at `site`. Must
/// be paired with a matching `mark_release` before the lock guard is
/// dropped.
///
/// We expose these helpers so callers outside this module (kernel tests in
/// host mode) can exercise the holder tracking without a real spinlock.
pub(crate) fn mark_acquire(site: NetSite) {
    // Best-effort: skip stamping if the runtime isn't registered yet
    // (host tests, very early boot). The site is the cheap part to set;
    // holder/locked_at need the runtime callbacks.
    if let Some(rt) = crate::runtime::try_runtime() {
        NETWORK_HOLDER.store((rt.current_thread_id)(), Ordering::Relaxed);
        NETWORK_LOCKED_AT_US.store((rt.uptime_us)(), Ordering::Relaxed);
    }
    NETWORK_LAST_SITE.store(site as u8, Ordering::Relaxed);
}

pub(crate) fn mark_release() {
    NETWORK_HOLDER.store(NETWORK_HOLDER_NONE, Ordering::Relaxed);
}

/// Static storage for sockets (required by smoltcp).
///
/// Static rather than a field for the usual reason — at `MAX_SOCKETS` entries
/// it is far too large to build on the kernel stack in [`init`] — and a
/// [`TakeOnce`] rather than a `static mut` since 2026-08-30. The `&'static mut`
/// that `SocketSet::new` needs was previously minted with a bare `unsafe`,
/// sound only because `init` is the sole caller; that is now enforced instead
/// of assumed, and the call site is safe.
pub(crate) static SOCKET_STORAGE: TakeOnce<[SocketStorage<'static>; MAX_SOCKETS]> =
    TakeOnce::new([SocketStorage::EMPTY; MAX_SOCKETS]);
