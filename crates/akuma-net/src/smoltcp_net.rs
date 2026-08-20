//! Smoltcp Network Stack (Thread-Safe)
//!
//! Provides the core networking stack using smoltcp, protected by a Spinlock.
//! This allows any thread (kernel or userspace via syscall) to drive the network stack.

use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU16, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use spinning_top::Spinlock;

use smoltcp::iface::{Config, Interface, SocketSet, SocketStorage, PollResult};
pub use smoltcp::iface::SocketHandle;
use smoltcp::phy::Device;
use smoltcp::socket::{tcp, udp, dhcpv4, dns};
use smoltcp::time::Instant;
use smoltcp::wire::{EthernetAddress, HardwareAddress, IpAddress, IpCidr};

use virtio_drivers::device::net::VirtIONetRaw;
use akuma_virtio::VirtioTransport;

use akuma_virtio::VirtioHal;
use crate::runtime::runtime;
use crate::runtime::PreemptGuard;
use crate::nicstat;

// ============================================================================
// Constants
// ============================================================================

#[cfg(not(any(feature = "small-sockets", kernel_profile_extreme)))]
const MAX_SOCKETS: usize = 256;
#[cfg(all(
    any(feature = "small-sockets", kernel_profile_extreme),
    any(not(feature = "many-sessions"), kernel_profile_extreme)
))]
const MAX_SOCKETS: usize = 32;
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
const MAX_SOCKETS: usize = 512;

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
const SOCKET_SOFT_CAP_START: usize = if MAX_SOCKETS < 128 { MAX_SOCKETS } else { 128 };

// Reduced from 64KB to 16KB per direction to save heap memory.
// 40 sockets × 32KB = 1.25MB vs 40 × 128KB = 5MB.
// 16KB is still plenty for TLS handshakes and HTTP requests.
const TCP_RX_BUFFER_SIZE: usize = 16384;
const TCP_TX_BUFFER_SIZE: usize = 16384;
const EPHEMERAL_PORT_START: u16 = 49152;
const SOCKET_GC_TIMEOUT_US: u64 = 30_000_000; // 30 seconds

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
const CONNECT_TIMEOUT_US: u64 = 10_000_000; // 10 s, matching finish_connect_wait

static NEXT_EPHEMERAL_PORT: AtomicU16 = AtomicU16::new(EPHEMERAL_PORT_START);

fn alloc_ephemeral_port() -> u16 {
    let port = NEXT_EPHEMERAL_PORT.fetch_add(1, Ordering::Relaxed);
    if port == 65535 {
        NEXT_EPHEMERAL_PORT.store(EPHEMERAL_PORT_START, Ordering::Relaxed);
        EPHEMERAL_PORT_START
    } else {
        port
    }
}

/// Whether DHCP is enabled. Set during `init()`.
static DHCP_ENABLED: AtomicBool = AtomicBool::new(false);

// ============================================================================
// Global Network State
// ============================================================================

/// Atomic flag indicating the network stack is initialized and ready
static NETWORK_READY: AtomicBool = AtomicBool::new(false);

/// Atomic counter incremented when progress is made (e.g. packets processed)
static POLL_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Counter for silently dropped TX packets (`VirtIO` send failures)
static TX_DROP_COUNT: AtomicUsize = AtomicUsize::new(0);

pub fn is_ready() -> bool {
    NETWORK_READY.load(Ordering::Acquire)
}

pub fn tx_drop_count() -> usize {
    TX_DROP_COUNT.load(Ordering::Relaxed)
}

/// Returns true once DHCP has acquired a lease (Configured event was processed).
/// Returns true immediately if DHCP is disabled.
static DHCP_CONFIGURED: AtomicBool = AtomicBool::new(false);

pub fn is_dhcp_configured() -> bool {
    if !DHCP_ENABLED.load(Ordering::Relaxed) {
        return true;
    }
    DHCP_CONFIGURED.load(Ordering::Acquire)
}

pub fn poll_count() -> usize {
    POLL_COUNT.load(Ordering::Acquire)
}

// ============================================================================
// NIC interrupt
// ============================================================================

/// virtio-mmio `InterruptStatus` — which events the device is signalling.
const VIRTIO_MMIO_INTERRUPT_STATUS: usize = 0x060;
/// virtio-mmio `InterruptACK` — write back the bits read from `InterruptStatus`
/// to de-assert the line.
const VIRTIO_MMIO_INTERRUPT_ACK: usize = 0x064;

/// virtio-mmio `QueueNotify`. Writing a queue index tells the device that queue
/// has new available buffers. Only the async transmit path kicks it by hand.
#[cfg(feature = "net-noalloc")]
const VIRTIO_MMIO_QUEUE_NOTIFY: usize = 0x050;

/// virtio-net's transmit virtqueue index (receive is 0).
#[cfg(feature = "net-noalloc")]
const VIRTIO_NET_QUEUE_TRANSMIT: u32 = 1;

/// MMIO base of NIC0, captured during [`init`]. 0 until then.
///
/// Held separately from the `VirtIONetRaw` inside `NETWORK` because the IRQ
/// handler must reach it **without taking a lock**: the core it interrupted may
/// be the one holding `NETWORK`, and a handler that blocked on it would wedge
/// the machine. A raw MMIO base in an atomic is the same discipline the timer
/// IRQ uses for the GIC (see the `no-bkl-irq` feature notes).
static NIC_MMIO_BASE: AtomicUsize = AtomicUsize::new(0);

/// virtio-mmio slot index of NIC0, or [`NIC_SLOT_NONE`] before [`init`].
static NIC_SLOT: AtomicU32 = AtomicU32::new(NIC_SLOT_NONE);
/// Sentinel for "no NIC found / not initialised yet".
pub const NIC_SLOT_NONE: u32 = u32::MAX;

/// The virtio-mmio slot NIC0 was probed at, for the kernel to derive its GIC
/// INTID from. `None` when there is no NIC or the stack has not initialised.
///
/// The kernel owns the slot-to-INTID mapping (it is a property of the machine,
/// not of this crate) — see `src/main.rs`.
#[must_use]
pub fn nic_slot() -> Option<u32> {
    match NIC_SLOT.load(Ordering::Acquire) {
        NIC_SLOT_NONE => None,
        slot => Some(slot),
    }
}

/// Acknowledge NIC0's pending interrupt. **Safe to call from IRQ context.**
///
/// Reads `InterruptStatus` and writes it straight back to `InterruptACK`, which
/// is all the virtio-mmio spec requires to de-assert a level-triggered line.
/// Deliberately does nothing else: the *value* of the NIC interrupt is that it
/// makes a `WFI` return, so the netpoll loop runs immediately instead of waiting
/// for the next scheduler tick. Draining the queue here would need `NETWORK`,
/// which this context must never take.
///
/// A no-op before [`init`] has recorded a base, so an early spurious interrupt
/// cannot fault on a null pointer.
pub fn nic_irq_ack() {
    let base = NIC_MMIO_BASE.load(Ordering::Acquire);
    if base == 0 {
        return;
    }
    NIC_IRQS.fetch_add(1, Ordering::Relaxed);
    // SAFETY: `base` was recorded from `akuma_virtio::probe`, which only yields
    // addresses inside the kernel's device mapping, and is only ever stored
    // once. Both registers are 32-bit at fixed offsets in the virtio-mmio
    // layout.
    unsafe {
        let status: akuma_primitives::mmio::MmioReg<u32> =
            akuma_primitives::mmio::MmioReg::new(base + VIRTIO_MMIO_INTERRUPT_STATUS);
        let ack: akuma_primitives::mmio::MmioReg<u32> =
            akuma_primitives::mmio::MmioReg::new(base + VIRTIO_MMIO_INTERRUPT_ACK);
        let pending = status.read();
        if pending != 0 {
            ack.write(pending);
        }
    }
}

/// Kick the transmit queue unconditionally.
///
/// `transmit_begin` notifies only when `VirtQueue::should_notify()` allows, and
/// QEMU negotiates `VIRTIO_F_EVENT_IDX`, so that can be false. The blocking
/// `VirtIONetRaw::send` this replaced checks the same flag — but then *spins
/// until the used ring advances*, which waits the suppression out and forces
/// the host to pick the frame up. Async submit has no such backstop, so a
/// suppressed notify leaves the frame sitting in the avail ring.
///
/// Measured cost of not doing this (`[NICSTAT] tx_flight`, 2026-08-19):
/// **90.9 us average submit → completion, 6,486 us worst case**, against a
/// 9.1 us submit — and an HTTP p99 of 6,747 us that tracks the worst case
/// almost exactly.
///
/// A spurious notify is harmless by spec (it is a hint), so this is
/// unconditional rather than trying to second-guess `should_notify`.
#[cfg(feature = "net-noalloc")]
pub(crate) fn nic_kick_tx() {
    let base = NIC_MMIO_BASE.load(Ordering::Acquire);
    if base == 0 {
        return;
    }
    // SAFETY: same base and the same discipline as `nic_irq_ack` — recorded
    // once from `akuma_virtio::probe`, inside the kernel's device mapping.
    // `QueueNotify` is a 32-bit write-only register at a fixed offset.
    unsafe {
        let notify: akuma_primitives::mmio::MmioReg<u32> =
            akuma_primitives::mmio::MmioReg::new(base + VIRTIO_MMIO_QUEUE_NOTIFY);
        notify.write(VIRTIO_NET_QUEUE_TRANSMIT);
    }
}

/// Count of NIC interrupts taken. The first thing to check when a latency fix
/// that depends on the interrupt does not move: if this is 0, the SPI never
/// reached the CPU and the stack is still tick-driven.
static NIC_IRQS: AtomicU64 = AtomicU64::new(0);

/// How many NIC interrupts have been taken since boot.
#[must_use]
pub fn nic_irq_count() -> u64 {
    NIC_IRQS.load(Ordering::Relaxed)
}

/// QEMU user-mode networking DNS server address
const QEMU_DNS_SERVER: IpAddress = IpAddress::Ipv4(smoltcp::wire::Ipv4Address::new(10, 0, 2, 3));

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
static NETWORK: Spinlock<Option<NetworkState>> = Spinlock::new(None);

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
static POLL_ENTERED: AtomicU64 = AtomicU64::new(0);
/// Cumulative `poll()` exits (incremented after `pending_removal` sweep).
static POLL_EXITED: AtomicU64 = AtomicU64::new(0);

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
fn mark_acquire(site: NetSite) {
    // Best-effort: skip stamping if the runtime isn't registered yet
    // (host tests, very early boot). The site is the cheap part to set;
    // holder/locked_at need the runtime callbacks.
    if let Some(rt) = crate::runtime::try_runtime() {
        NETWORK_HOLDER.store((rt.current_thread_id)(), Ordering::Relaxed);
        NETWORK_LOCKED_AT_US.store((rt.uptime_us)(), Ordering::Relaxed);
    }
    NETWORK_LAST_SITE.store(site as u8, Ordering::Relaxed);
}

fn mark_release() {
    NETWORK_HOLDER.store(NETWORK_HOLDER_NONE, Ordering::Relaxed);
}

/// Static storage for sockets (required by smoltcp)
static mut SOCKET_STORAGE: [SocketStorage<'static>; MAX_SOCKETS] = [SocketStorage::EMPTY; MAX_SOCKETS];

// ============================================================================
// VirtIO Smoltcp Device Wrapper
// ============================================================================

pub struct VirtioSmoltcpDevice {
    inner: VirtIONetRaw<VirtioHal, VirtioTransport, 16>,
    /// The single receive buffer of the pre-`net-noalloc` path.
    #[cfg(not(feature = "net-noalloc"))]
    rx_buffer: [u8; 2048],
    /// The single transmit buffer of the pre-`net-noalloc` path. Also the
    /// saturation-fallback staging buffer's counterpart.
    #[cfg(not(feature = "net-noalloc"))]
    tx_buffer: [u8; 2048],
    /// Token for a pending `VirtIO` receive buffer that has been submitted to the device.
    /// `VirtIO` requires buffers to be posted via `receive_begin()` before the device can
    /// DMA received packets into them. We track the token so we can call `receive_complete()`
    /// once `poll_receive()` indicates the device has filled the buffer.
    #[cfg(not(feature = "net-noalloc"))]
    rx_token: Option<u16>,
    /// Receive slots posted to the device. Buffers live in BSS, not here — see
    /// `virtio_rings`.
    #[cfg(feature = "net-noalloc")]
    rx: crate::virtio_rings::RxRing,
    /// Transmit slots in flight.
    #[cfg(feature = "net-noalloc")]
    tx: crate::virtio_rings::TxRing,
}

impl VirtioSmoltcpDevice {
    #[must_use]
    pub const fn new(inner: VirtIONetRaw<VirtioHal, VirtioTransport, 16>) -> Self {
        Self {
            inner,
            #[cfg(not(feature = "net-noalloc"))]
            rx_buffer: [0u8; 2048],
            #[cfg(not(feature = "net-noalloc"))]
            tx_buffer: [0u8; 2048],
            #[cfg(not(feature = "net-noalloc"))]
            rx_token: None,
            #[cfg(feature = "net-noalloc")]
            rx: crate::virtio_rings::RxRing::new(),
            #[cfg(feature = "net-noalloc")]
            tx: crate::virtio_rings::TxRing::new(),
        }
    }

    #[must_use]
    pub fn mac_address(&self) -> [u8; 6] {
        self.inner.mac_address()
    }

    /// Take the next received frame, if the device has one ready.
    ///
    /// Returns a pointer to the L2 frame (virtio header already skipped) and its
    /// length. A raw pointer rather than a slice because the caller owns the
    /// lifetime: smoltcp's `RxToken` borrows it for exactly as long as
    /// `consume` runs, which is not a lifetime this function can name.
    ///
    /// The two implementations differ in one thing that matters: with
    /// `net-noalloc` the device always has a *ring* of buffers posted, so a
    /// burst drains without an MMIO notify per frame. Without it there is one
    /// buffer, and every single packet costs a fresh `receive_begin`.
    fn take_rx_frame(&mut self) -> Option<(*mut u8, usize)> {
        #[cfg(feature = "net-noalloc")]
        {
            // Reap first: this runs once per poll lap, which is the only place
            // TX completions get harvested promptly. Leaving it to the next
            // `claim` means a slot stays in flight for as long as nothing is
            // transmitted — which, on a request/response workload, is exactly
            // the gap between requests.
            self.tx.reap(&mut self.inner);
            // Re-post whatever the previous call released. Safe to do here and
            // not earlier: an outstanding `RxToken` has been consumed by the
            // time smoltcp asks for another frame.
            self.rx.refill(&mut self.inner);
            let Some((slot, hdr, len)) = self.rx.take_frame(&mut self.inner) else {
                nicstat::record_rx_empty();
                return None;
            };
            // SAFETY: `slot` came from the ring's own table and `NETWORK` is
            // held; `hdr + len <= FRAME_BUF` was checked by `take_frame`.
            let base = unsafe { crate::virtio_rings::rx_frame(slot) };
            Some((unsafe { base.as_mut_ptr().add(hdr) }, len))
        }
        #[cfg(not(feature = "net-noalloc"))]
        {
            // Phase 1: ensure a receive buffer is posted to the device.
            if self.rx_token.is_none() {
                let t = nicstat::start();
                match unsafe { self.inner.receive_begin(&mut self.rx_buffer) } {
                    Ok(token) => {
                        nicstat::record_rx_begin(t);
                        self.rx_token = Some(token);
                    }
                    Err(_) => return None,
                }
            }
            // Phase 2: has the device filled it?
            if self.inner.poll_receive().is_some() {
                let token = self.rx_token.take().unwrap();
                let t = nicstat::start();
                if let Ok((hdr_len, pkt_len)) =
                    unsafe { self.inner.receive_complete(token, &mut self.rx_buffer) }
                {
                    // A malformed VirtIO response could report a frame longer
                    // than the buffer; slicing on that corrupts adjacent memory.
                    if hdr_len.saturating_add(pkt_len) > self.rx_buffer.len() {
                        return None;
                    }
                    nicstat::record_rx_packet(t, pkt_len);
                    return Some((
                        unsafe { self.rx_buffer.as_mut_ptr().add(hdr_len) },
                        pkt_len,
                    ));
                }
                return None;
            }
            nicstat::record_rx_empty();
            None
        }
    }

    /// Fill one outbound frame and dispose of it.
    ///
    /// `fill` writes the L2 frame into the staging region. `divert` is then
    /// handed the filled frame and returns `true` if it must **not** reach the
    /// wire — that is how loopback traffic is intercepted without this function
    /// needing to know what loopback is.
    ///
    /// With `net-noalloc` the frame is staged directly in a ring slot and
    /// submitted with `transmit_begin`, which returns immediately; the device's
    /// completion is reaped on a later pass. Without it, every frame goes
    /// through `VirtIONetRaw::send`, which spins until the host consumes the
    /// descriptor — 20-26 us per packet with `NETWORK` held and IRQs masked
    /// (`docs/archive/AKUMA_NET_ISSUES.md` §3.2).
    fn emit_frame<R>(
        &mut self,
        len: usize,
        fill: impl FnOnce(&mut [u8]) -> R,
        divert: impl FnOnce(&[u8]) -> bool,
    ) -> R {
        #[cfg(feature = "net-noalloc")]
        {
            use crate::virtio_rings::{FRAME_BUF, tx_discard, tx_frame};
            if let Some(slot) = self.tx.claim(&mut self.inner) {
                // SAFETY: `slot < TX_RING` by construction; `NETWORK` is held.
                let frame = unsafe { tx_frame(slot) };
                // `transmit_begin` sends whatever is in the buffer verbatim, so
                // the virtio header has to be written into it here — unlike
                // `send`, which prepends its own.
                let hdr = self.inner.fill_buffer_header(frame).unwrap_or(0);
                let end = hdr.saturating_add(len).min(FRAME_BUF);
                let res = fill(&mut frame[hdr..end]);
                if divert(&frame[hdr..end]) {
                    // Never submitted, so the slot was never marked in flight —
                    // there is nothing to release.
                    return res;
                }
                let t = nicstat::start();
                let ok = self.tx.submit(&mut self.inner, slot, end);
                nicstat::record_tx(t, end - hdr, ok);
                if !ok {
                    TX_DROP_COUNT.fetch_add(1, Ordering::Relaxed);
                }
                return res;
            }
            // Every slot was still in flight after `CLAIM_SPINS` reaps. The
            // frame is dropped — smoltcp retransmits — but `consume`'s contract
            // still requires the fill closure to run, so it writes into a buffer
            // nothing reads. Falling back to `VirtIONetRaw::send` here would be
            // a bug, not a slow path: see `TxRing::claim`.
            //
            // A diverted (loopback) frame is unaffected by NIC saturation, so it
            // is still delivered.
            // SAFETY: `NETWORK` is held, so nothing else is touching the buffer.
            let discard = unsafe { tx_discard() };
            let end = len.min(FRAME_BUF);
            let res = fill(&mut discard[..end]);
            if divert(&discard[..end]) {
                return res;
            }
            TX_DROP_COUNT.fetch_add(1, Ordering::Relaxed);
            res
        }
        #[cfg(not(feature = "net-noalloc"))]
        {
            let end = len.min(self.tx_buffer.len());
            let res = fill(&mut self.tx_buffer[..end]);
            if divert(&self.tx_buffer[..end]) {
                return res;
            }
            let t = nicstat::start();
            let ok = self.inner.send(&self.tx_buffer[..end]).is_ok();
            nicstat::record_tx(t, end, ok);
            if !ok {
                TX_DROP_COUNT.fetch_add(1, Ordering::Relaxed);
            }
            res
        }
    }
}

// `deref_addrof`: `receive` must hand out an rx and a tx token from one
// `&mut self`, and the tx token holds the whole device — so unlike
// `LoopbackAwareDevice` below, disjoint field borrows cannot express it.
#[allow(clippy::deref_addrof)]
impl Device for VirtioSmoltcpDevice {
    type RxToken<'a> = VirtioRxToken<'a>;
    type TxToken<'a> = VirtioTxToken<'a>;

    fn receive(&mut self, _timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        let (ptr, len) = self.take_rx_frame()?;
        // SAFETY: `take_rx_frame` returned a live L2 frame of `len` bytes in a
        // buffer this device owns, and the token's lifetime is bounded by the
        // `&mut self` borrow. The `&raw mut` aliasing for the tx half is the
        // pre-existing pattern here: smoltcp's `Device` contract hands out an
        // rx and a tx token together, and they touch disjoint state (rx frame
        // storage vs the device/tx ring).
        let rx = VirtioRxToken { buffer: unsafe { core::slice::from_raw_parts_mut(ptr, len) } };
        let tx = VirtioTxToken { dev: unsafe { &mut *(&raw mut *self) } };
        Some((rx, tx))
    }

    fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
        Some(VirtioTxToken { dev: self })
    }

    fn capabilities(&self) -> smoltcp::phy::DeviceCapabilities {
        let mut caps = smoltcp::phy::DeviceCapabilities::default();
        caps.max_transmission_unit = 1514;
        caps
    }
}

pub struct VirtioRxToken<'a> {
    buffer: &'a mut [u8],
}

impl smoltcp::phy::RxToken for VirtioRxToken<'_> {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        f(self.buffer)
    }
}

pub struct VirtioTxToken<'a> {
    dev: &'a mut VirtioSmoltcpDevice,
}

impl smoltcp::phy::TxToken for VirtioTxToken<'_> {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        // No diversion: this device has no loopback queue.
        self.dev.emit_frame(len, f, |_| false)
    }
}

// ============================================================================
// Loopback-Aware Device Wrapper
// ============================================================================

/// Check if an Ethernet frame is destined for loopback (127.x.x.x).
///
/// Inspects the `EtherType` and the relevant IP address field:
/// - ARP (0x0806): target protocol address at bytes [38:42]
/// - IPv4 (0x0800): destination IP at bytes [30:34]
fn is_loopback_frame(frame: &[u8]) -> bool {
    if frame.len() < 14 {
        return false;
    }
    let ethertype = u16::from_be_bytes([frame[12], frame[13]]);
    match ethertype {
        // ARP: match if either sender (bytes 28) or target (bytes 38) IP is 127.x.x.x
        0x0806 => frame.len() >= 42 && (frame[28] == 127 || frame[38] == 127),
        // IPv4: match if either source (byte 26) or dest (byte 30) IP is 127.x.x.x
        0x0800 => frame.len() >= 34 && (frame[26] == 127 || frame[30] == 127),
        _ => false,
    }
}

/// Bytes per loopback frame slot. Loopback frames are pure L2 (no virtio
/// header, MTU 1514 — see `capabilities()`), but this matches
/// `virtio_rings::FRAME_BUF` and every other frame buffer in this file for
/// the same reason: one size means one set of bounds to reason about.
const LOOPBACK_FRAME_BUF: usize = 2048;

/// Loopback frames that may be queued at once. Deliberately the same order of
/// magnitude as `virtio_rings::RX_RING`/`TX_RING`: enough to cover one
/// TCP-handshake-shaped burst between two `poll()` calls, not a backlog.
const LOOPBACK_RING: usize = 32;

/// Frame storage for the loopback ring. Not a `LoopbackAwareDevice` field —
/// `NetworkState` (which owns the device) is built on the stack before being
/// moved into the `NETWORK` static, and `LOOPBACK_RING * LOOPBACK_FRAME_BUF`
/// (64 KiB) inline would push that far past a comfortable kernel stack frame.
/// Same reasoning as `virtio_rings`' `RX_BUFS`/`TX_BUFS`.
static mut LOOPBACK_BUFS: [[u8; LOOPBACK_FRAME_BUF]; LOOPBACK_RING] =
    [[0; LOOPBACK_FRAME_BUF]; LOOPBACK_RING];

/// Loopback frames dropped because the ring was full, or (should be
/// impossible — `capabilities()` caps the MTU well under this) too large for
/// a slot. `docs/archive/FREEZE_INSTRUMENTATION_PLAN.md` F5 flagged the old
/// `VecDeque` for growing without bound instead of ever hitting this path.
static LOOPBACK_DROP_COUNT: AtomicUsize = AtomicUsize::new(0);

#[must_use]
pub fn loopback_drop_count() -> usize {
    LOOPBACK_DROP_COUNT.load(Ordering::Relaxed)
}

/// Pointer to loopback slot `slot`.
///
/// # Safety
/// `slot < LOOPBACK_RING`, and the caller holds `NETWORK` — the lock that
/// serialises every push/pop below, same as `virtio_rings::rx_buf`/`tx_buf`.
unsafe fn loopback_buf(slot: usize) -> *mut u8 {
    unsafe { (&raw mut LOOPBACK_BUFS).cast::<u8>().add(slot * LOOPBACK_FRAME_BUF) }
}

/// A fixed-capacity ring of loopback frames, replacing what used to be a
/// `VecDeque<Vec<u8>>`.
///
/// The old queue paid a zeroing heap allocation and a copy for every loopback
/// frame (`docs/archive/AKUMA_NET_ISSUES.md` §"one per-packet allocation
/// remains", `docs/archive/BENCHMARK_PERFORMANCE_ATTEMPT_0.md` §6) and had no
/// capacity bound, which `docs/archive/SCHEDULING_INVESTIGATION.md` flagged as
/// an unbounded-queue-without-backpressure smell. This ring bounds depth at
/// [`LOOPBACK_RING`] and drops (counted) on overflow instead of growing —
/// the same backpressure a real NIC's finite ring already gives external
/// traffic.
///
/// # Why holding raw slices into a shared static is sound
///
/// Every push and pop happens under the `NETWORK` spinlock (`push` from
/// `TxToken::consume` during egress, `pop` from `Device::receive` during
/// ingress), so there is exactly one thread touching the ring at a time. A
/// slot popped by `pop` is only reused by a `push` after `LOOPBACK_RING`
/// further pushes advance `tail` all the way back around to it — and by then
/// the `RxToken::consume` call that borrowed it has long since returned,
/// because `receive()`/`consume()` are synchronous and non-reentrant on this
/// slot (the one case where a `push` runs "inside" an outstanding `pop` —
/// smoltcp generating an immediate reply, e.g. an ICMP echo, from within the
/// rx closure it was handed alongside the tx token — targets `tail`, a
/// different slot from the `head` slot still being read, as long as
/// `LOOPBACK_RING >= 2`).
struct LoopbackRing {
    /// Length of the frame in slot `i`, valid only while that slot is queued.
    lens: [u16; LOOPBACK_RING],
    /// Next slot to pop.
    head: usize,
    /// Next slot to push into.
    tail: usize,
    /// Frames currently queued.
    count: usize,
}

impl LoopbackRing {
    const fn new() -> Self {
        Self {
            lens: [0; LOOPBACK_RING],
            head: 0,
            tail: 0,
            count: 0,
        }
    }

    /// Copy `frame` into the next free slot. Drops and counts it
    /// (`LOOPBACK_DROP_COUNT`) if the ring is full or the frame does not fit
    /// a slot — the latter should be unreachable given the MTU, but a
    /// malformed frame must not overrun `LOOPBACK_BUFS`.
    fn push(&mut self, frame: &[u8]) {
        if self.count == LOOPBACK_RING || frame.len() > LOOPBACK_FRAME_BUF {
            LOOPBACK_DROP_COUNT.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let slot = self.tail;
        // SAFETY: `slot < LOOPBACK_RING` (`tail` is kept in range by the `%`
        // below); see the struct-level safety argument for exclusivity.
        let dst = unsafe { core::slice::from_raw_parts_mut(loopback_buf(slot), frame.len()) };
        dst.copy_from_slice(frame);
        self.lens[slot] = frame.len() as u16;
        self.tail = (self.tail + 1) % LOOPBACK_RING;
        self.count += 1;
        // A loopback frame never touches virtio, so unlike a real packet it
        // has no interrupt of its own to end a parked core's `wfi`/
        // `blocking_relax` halt — without this it rides the periodic timer
        // tick, the exact cost `AKUMA_NET_ISSUES.md` §3.1 removed for
        // external traffic. See `NetRuntime::wake_netpoll` and
        // `docs/archive/LOOPBACK_RING_CONVERSION.md`.
        (runtime().wake_netpoll)();
    }

    /// Hand back the oldest queued frame, if any, as a `'static` slice into
    /// `LOOPBACK_BUFS`.
    fn pop(&mut self) -> Option<&'static mut [u8]> {
        if self.count == 0 {
            return None;
        }
        let slot = self.head;
        let len = self.lens[slot] as usize;
        self.head = (self.head + 1) % LOOPBACK_RING;
        self.count -= 1;
        // SAFETY: as `push`.
        Some(unsafe { core::slice::from_raw_parts_mut(loopback_buf(slot), len) })
    }
}

/// A composite device that wraps `VirtIO` for external traffic and an internal
/// ring for loopback (127.x.x.x) traffic.
///
/// Outgoing frames destined for
/// loopback addresses are intercepted in `TxToken::consume()` and queued
/// internally rather than being sent through `VirtIO`. `receive()` checks
/// the loopback ring first, then falls back to `VirtIO`.
pub struct LoopbackAwareDevice {
    virtio: VirtioSmoltcpDevice,
    loopback: LoopbackRing,
}

impl LoopbackAwareDevice {
    #[must_use]
    pub const fn new(virtio: VirtioSmoltcpDevice) -> Self {
        Self {
            virtio,
            loopback: LoopbackRing::new(),
        }
    }

    #[must_use] 
    pub fn mac_address(&self) -> [u8; 6] {
        self.virtio.mac_address()
    }
}

impl Device for LoopbackAwareDevice {
    type RxToken<'a> = LoopbackAwareRxToken<'a>;
    type TxToken<'a> = LoopbackAwareTxToken<'a>;

    fn receive(&mut self, _timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        // Choose the frame BEFORE building the tx token. Doing it in this order
        // is what lets the two tokens be plain disjoint field borrows
        // (`&mut self.virtio` and `&mut self.loopback`) instead of the
        // `&raw mut` aliasing this used to need: the loopback pop wants the
        // ring mutably, and the tx token wants to keep it.
        let source = if let Some(frame) = self.loopback.pop() {
            // An internally queued frame is already in hand — no device round
            // trip, and `receive` drains these ahead of the wire.
            FrameSource::Loopback(frame)
        } else {
            let (ptr, len) = self.virtio.take_rx_frame()?;
            FrameSource::Virtio(ptr, len)
        };

        let tx = LoopbackAwareTxToken {
            virtio: &mut self.virtio,
            loopback: &mut self.loopback,
        };
        let rx = match source {
            FrameSource::Loopback(frame) => LoopbackAwareRxToken::Loopback(frame),
            // SAFETY: `take_rx_frame` returned a live L2 frame of `len` bytes in
            // storage this device owns — with `net-noalloc` a ring slot the ring
            // has already released and will not re-post until the next
            // `receive`, otherwise the single rx buffer. The token's lifetime is
            // bounded by the `&mut self` borrow.
            FrameSource::Virtio(ptr, len) => LoopbackAwareRxToken::Virtio(unsafe {
                core::slice::from_raw_parts_mut(ptr, len)
            }),
        };
        Some((rx, tx))
    }

    fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
        Some(LoopbackAwareTxToken {
            virtio: &mut self.virtio,
            loopback: &mut self.loopback,
        })
    }

    fn capabilities(&self) -> smoltcp::phy::DeviceCapabilities {
        self.virtio.capabilities()
    }
}

/// Where the frame `receive` is about to hand up came from.
///
/// Exists so the decision can be made before either token is built — see
/// `LoopbackAwareDevice::receive`.
enum FrameSource {
    /// A frame popped off the internal loopback ring, borrowed `'static` out
    /// of `LOOPBACK_BUFS`.
    Loopback(&'static mut [u8]),
    /// A pointer to the L2 frame in device-owned storage, and its length.
    Virtio(*mut u8, usize),
}

pub enum LoopbackAwareRxToken<'a> {
    /// A frame that was looped back internally.
    Loopback(&'a mut [u8]),
    /// A borrowed frame received from `VirtIO`.
    Virtio(&'a mut [u8]),
}

impl smoltcp::phy::RxToken for LoopbackAwareRxToken<'_> {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        match self {
            Self::Loopback(buf) => f(buf),
            Self::Virtio(buf) => f(buf),
        }
    }
}

pub struct LoopbackAwareTxToken<'a> {
    virtio: &'a mut VirtioSmoltcpDevice,
    loopback: &'a mut LoopbackRing,
}

impl smoltcp::phy::TxToken for LoopbackAwareTxToken<'_> {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let ring = self.loopback;
        self.virtio.emit_frame(len, f, |frame| {
            // Frames addressed to 127.x never reach the wire: copy them into the
            // internal ring, which `receive` drains ahead of the device.
            if !is_loopback_frame(frame) {
                return false;
            }
            ring.push(frame);
            nicstat::record_loopback(frame.len());
            true
        })
    }
}

// ============================================================================
// Initialization
// ============================================================================

#[allow(clippy::cast_possible_wrap)]
pub fn init(enable_dhcp: bool) -> Result<(), &'static str> {
    log::info!("[SmolNet] Initializing network stack...");
    DHCP_ENABLED.store(enable_dhcp, Ordering::Relaxed);

    let mut found_device: Option<VirtIONetRaw<VirtioHal, VirtioTransport, 16>> = None;

    if let Some((i, transport)) = akuma_virtio::probe::probe(akuma_virtio::device_id::NET) {
        log::info!("[SmolNet] Found virtio-net at slot {i}");
        if let Ok(dev) = VirtIONetRaw::new(transport) {
            // Record the slot and its MMIO base for the IRQ handler before the
            // device is moved into `NETWORK` — afterwards it is only reachable
            // under the lock, which IRQ context must not take.
            NIC_MMIO_BASE.store(akuma_virtio::slot_addr(i), Ordering::Release);
            NIC_SLOT.store(i as u32, Ordering::Release);
            found_device = Some(dev);
        }
    }

    let mut device = LoopbackAwareDevice::new(
        VirtioSmoltcpDevice::new(found_device.ok_or("No virtio-net device found")?)
    );
    let mac = device.mac_address();
    log::info!(
        "[SmolNet] MAC: {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
    );

    let timestamp = Instant::from_micros((runtime().uptime_us)() as i64);

    let mut config = Config::new(HardwareAddress::Ethernet(EthernetAddress(mac)));
    config.random_seed = (runtime().uptime_us)();
    
    let mut iface = Interface::new(config, &mut device, timestamp);
    
    iface.update_ip_addrs(|ip_addrs| {
        ip_addrs.push(IpCidr::new(IpAddress::v4(10, 0, 2, 15), 24)).unwrap();
        ip_addrs.push(IpCidr::new(IpAddress::v4(127, 0, 0, 1), 8)).unwrap();
    });
    iface.routes_mut().add_default_ipv4_route(smoltcp::wire::Ipv4Address::new(10, 0, 2, 2)).unwrap();

    let mut sockets = unsafe { SocketSet::new(&mut SOCKET_STORAGE[..]) };

    let dhcp_handle = if enable_dhcp {
        log::info!("[SmolNet] DHCP enabled");
        let dhcp_socket = dhcpv4::Socket::new();
        SOCKETS_LIVE.fetch_add(1, Ordering::Relaxed);
        Some(sockets.add(dhcp_socket))
    } else {
        None
    };

    let dns_servers = &[QEMU_DNS_SERVER];
    let dns_socket = dns::Socket::new(dns_servers, vec![]);
    SOCKETS_LIVE.fetch_add(1, Ordering::Relaxed);
    let dns_handle = sockets.add(dns_socket);
    log::info!("[SmolNet] DNS socket initialized (server: 10.0.2.3)");

    *NETWORK.lock() = Some(NetworkState {
        iface,
        sockets,
        device,
        dhcp_handle,
        dns_handle,
        pending_removal: Vec::new(),
        connecting: Vec::new(),
    });

    NETWORK_READY.store(true, Ordering::Release);
    log::info!("[SmolNet] Initialized successfully (VirtIO + Loopback)");
    Ok(())
}

// ============================================================================
// Public API
// ============================================================================

#[allow(clippy::cast_possible_wrap)]
pub fn poll() -> bool {
    POLL_ENTERED.fetch_add(1, Ordering::Relaxed);
    let poll_t = nicstat::start();
    let socket_state_changed = {
        // Hold preemption disabled for the whole NETWORK critical section so the
        // spinlock is never stranded across a context switch (fatal under the
        // BKL — see `PreemptGuard`). No-op on non-smp-shared builds.
        let _pg = PreemptGuard::new();
        // Time the acquisition separately: `NETWORK` is a plain (unfair)
        // `Spinlock`, so under 4-core contention a waiter can be starved, and
        // `poll_us` alone cannot distinguish that from a slow poll or a slow
        // wake pass.
        let wait_t = nicstat::start();
        let mut guard = NETWORK.lock();
        nicstat::record_poll_wait(wait_t);
        mark_acquire(NetSite::Poll);
        let result = if let Some(net) = guard.as_mut() {
            let timestamp = Instant::from_micros((runtime().uptime_us)() as i64);
            
            let p1 = net.iface.poll(timestamp, &mut net.device, &mut net.sockets);
            
            // Handle DHCP
            let mut dhcp_changed = false;
            if let Some(handle) = net.dhcp_handle {
                let event = net.sockets.get_mut::<dhcpv4::Socket>(handle).poll();
                if let Some(event) = event {
                    match event {
                        dhcpv4::Event::Configured(config) => {
                            log::info!("[SmolNet] DHCP configured");
                            net.iface.update_ip_addrs(|addrs| {
                                addrs.clear();
                                addrs.push(IpCidr::Ipv4(config.address)).unwrap();
                                addrs.push(IpCidr::new(IpAddress::v4(127, 0, 0, 1), 8)).unwrap();
                            });
                            if let Some(router) = config.router {
                                let _ = net.iface.routes_mut().add_default_ipv4_route(router);
                            }

                            log::info!("[SmolNet] IP: {}", config.address);
                            DHCP_CONFIGURED.store(true, Ordering::Release);
                        }
                        dhcpv4::Event::Deconfigured => {
                            DHCP_CONFIGURED.store(false, Ordering::Release);
                            log::info!("[SmolNet] DHCP deconfigured - reverting to static fallback");
                            net.iface.update_ip_addrs(|addrs| {
                                addrs.clear();
                                addrs.push(IpCidr::new(IpAddress::v4(10, 0, 2, 15), 24)).unwrap();
                                addrs.push(IpCidr::new(IpAddress::v4(127, 0, 0, 1), 8)).unwrap();
                            });
                            let _ = net.iface.routes_mut().add_default_ipv4_route(smoltcp::wire::Ipv4Address::new(10, 0, 2, 2));
                        }
                    }
                    dhcp_changed = true;
                }
            }

            // Re-poll after DHCP reconfiguration so the stack immediately processes
            // any in-flight packets (e.g. loopback TCP handshake) with the updated
            // IP configuration. Without this, the address change isn't picked up
            // until the next external poll() call, which can cause loopback TCP
            // connections to stall (server stuck in SynReceived).
            if dhcp_changed {
                let timestamp = Instant::from_micros((runtime().uptime_us)() as i64);
                net.iface.poll(timestamp, &mut net.device, &mut net.sockets);
            }

            // Garbage collect pending removals.
            // Force-abort sockets stuck in non-Closed states for longer than
            // SOCKET_GC_TIMEOUT_US to prevent slot exhaustion.
            //
            // Runs on EVERY poll. Rate-limiting it to 100 ms was tried
            // 2026-08-20 — reasonable on the face of it, since everything it
            // collects is on a 10 s (smoltcp `CLOSE_DELAY`) or 30 s clock — and
            // measured NO DIFFERENCE, so it was reverted as unearned complexity
            // rather than as a regression.
            //
            // Normalised by traffic (the only fair comparison — poll count tracks
            // packet count), gated vs ungated at ~12,160 rx packets/window:
            // 6.96 vs 6.87/6.70 polls per packet, 10.7 vs 11.0/11.5 us per poll.
            // That is noise.
            //
            // Why it is cheaper than it looks: `SocketSet::get` is an array index,
            // not a scan, so the sweep is ~100 cheap operations per poll. Counting
            // operations is not measuring them — see AKUMA_NET_ISSUES.md §10.
            let now_us = (runtime().uptime_us)();
            let mut i = 0;
            while i < net.pending_removal.len() {
                let (handle, added_at) = net.pending_removal[i];
                if !is_valid_handle(handle) {
                    log::warn!("[NET] CORRUPT HANDLE in poll GC: handle={handle}");
                    net.pending_removal.swap_remove(i);
                    continue;
                }
                let state = net.sockets.get::<tcp::Socket>(handle).state();
                let timed_out = now_us.saturating_sub(added_at) > SOCKET_GC_TIMEOUT_US;
                if state == tcp::State::Closed || timed_out {
                    if timed_out && state != tcp::State::Closed {
                        net.sockets.get_mut::<tcp::Socket>(handle).abort();
                    }
                    net.sockets.remove(handle);
                    SOCKETS_LIVE.fetch_sub(1, Ordering::Relaxed);
                    net.pending_removal.swap_remove(i);
                } else {
                    i += 1;
                }
            }

            // Bound `SynSent`. An entry leaves this list the moment the socket
            // is no longer shaking hands — connected, reset, or closed — so the
            // deadline never applies to an established connection. See
            // `CONNECT_TIMEOUT_US`.
            let mut i = 0;
            while i < net.connecting.len() {
                let (handle, started_at) = net.connecting[i];
                if !is_valid_handle(handle) {
                    net.connecting.swap_remove(i);
                    continue;
                }
                if net.sockets.get::<tcp::Socket>(handle).state() != tcp::State::SynSent {
                    net.connecting.swap_remove(i);
                    continue;
                }
                if now_us.saturating_sub(started_at) > CONNECT_TIMEOUT_US {
                    // `abort()` moves it to `Closed`, which surfaces to the
                    // caller as `EPOLLHUP`. The socket is flagged first so
                    // `SO_ERROR` can answer `ETIMEDOUT` rather than the
                    // `ECONNREFUSED` a plain `Closed` would imply — a connect
                    // that was never answered is not a connect that was refused,
                    // and telling them apart is the difference between reading a
                    // log correctly and not.
                    crate::socket::mark_connect_timed_out(handle);
                    net.sockets.get_mut::<tcp::Socket>(handle).abort();
                    net.connecting.swap_remove(i);
                    continue;
                }
                i += 1;
            }

            // Publish the set size: `iface.poll()` above walks all of it, so this
            // is the scaling term behind `poll_us`.
            // O(1): `SOCKETS_LIVE` is maintained on add/remove. It used to be
            // `iter().count()` here, which made the meter a material part of what
            // it measured — ~0.9 us/poll at 128 slots and ~14 us at 2048, which
            // inflated the first 2048-slot experiment.
            nicstat::record_sockets_live(sockets_live());

            if matches!(p1, PollResult::SocketStateChanged) {
                POLL_COUNT.fetch_add(1, Ordering::Release);
                true
            } else {
                false
            }
        } else {
            false
        };
        mark_release();
        drop(guard);
        POLL_EXITED.fetch_add(1, Ordering::Relaxed);
        result
    };
    // NETWORK lock is released here — safe to acquire SOCKET_TABLE.
    // Acquiring SOCKET_TABLE while holding NETWORK causes AB-BA deadlock
    // with socket_can_recv_tcp et al. which hold SOCKET_TABLE→NETWORK.

    if socket_state_changed {
        // Walks EVERY socket slot (MAX_SOCKETS) taking each one's waker lock,
        // with SOCKET_TABLE held. Timed separately from the poll itself.
        let wake_t = nicstat::start();
        crate::socket::with_table(|table| {
            for slot in table.iter().flatten() {
                slot.wake_all();
            }
        });
        nicstat::record_poll_wake(wake_t);
    }

    nicstat::record_poll(poll_t, socket_state_changed);
    socket_state_changed
}

pub fn with_network<F, R>(f: F) -> Option<R>
where
    F: FnOnce(&mut NetworkState) -> R,
{
    // Preemption disabled for the whole hold: the NETWORK spinlock must never be
    // stranded across a context switch under the BKL (see `PreemptGuard`).
    let _pg = PreemptGuard::new();
    let mut guard = NETWORK.lock();
    mark_acquire(NetSite::WithNetwork);
    let result = guard.as_mut().map(f);
    mark_release();
    drop(guard);
    result
}

// ============================================================================
// DNS Resolution
// ============================================================================

/// Blocking DNS query - resolves a hostname to an IPv4 address.
///
/// Polls the network stack and yields the current thread until a result is available.
/// Used by the syscall handler for userspace programs and by kernel services.
pub fn dns_query(hostname: &str) -> Result<smoltcp::wire::Ipv4Address, DnsQueryError> {
    // Fast path: try parsing as IP literal first
    if let Ok(ip) = hostname.parse::<smoltcp::wire::Ipv4Address>() {
        return Ok(ip);
    }
    if hostname == "localhost" {
        return Ok(smoltcp::wire::Ipv4Address::LOCALHOST);
    }

    // Start a DNS query
    let query_handle = with_network(|net| {
        let dns_socket = net.sockets.get_mut::<dns::Socket>(net.dns_handle);
        let cx = net.iface.context();
        dns_socket.start_query(cx, hostname, smoltcp::wire::DnsQueryType::A).ok()
    }).flatten().ok_or(DnsQueryError::StartFailed)?;

    // Poll until we get a result or timeout (10 seconds)
    let start = (runtime().uptime_us)();
    let timeout_us = 10_000_000u64;

    loop {
        poll();

        let result = with_network(|net| {
            let dns_socket = net.sockets.get_mut::<dns::Socket>(net.dns_handle);
            match dns_socket.get_query_result(query_handle) {
                Ok(addrs) => {
                    Some(
                        addrs.first().map_or(Err(DnsQueryError::NoRecords), |addr| {
                            let IpAddress::Ipv4(v4) = addr;
                            Ok(*v4)
                        }),
                    )
                }
                Err(dns::GetQueryResultError::Pending) => None,
                Err(dns::GetQueryResultError::Failed) => Some(Err(DnsQueryError::QueryFailed)),
            }
        }).flatten();

        match result {
            Some(Ok(addr)) => return Ok(addr),
            Some(Err(e)) => return Err(e),
            None => {
                if (runtime().uptime_us)() - start > timeout_us {
                    return Err(DnsQueryError::Timeout);
                }
                // Wait for the DNS response, DROPPING the Big Kernel Lock across the wait
                // under shared-kernel SMP. This loop does not poll itself — it relies on
                // the async-main poller (on a peer core) to drive the DNS RX, which cannot
                // happen if we spin holding the BKL. Same freeze as the socket wait; fires
                // first, on any connect-by-hostname. See docs/runbooks/debug-smp.md.
                (runtime().blocking_relax)();
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum DnsQueryError {
    StartFailed,
    QueryFailed,
    NoRecords,
    Timeout,
}

// ============================================================================
// Async TCP Connect
// ============================================================================

/// Async TCP connect - creates a socket, connects to the remote, and returns a `TcpStream`.
/// Suitable for use from async shell commands running in `block_on` contexts.
pub async fn tcp_connect(addr: IpAddress, port: u16) -> Result<(TcpStream, SocketHandle), TcpError> {
    let handle = socket_create().ok_or(TcpError::WriteError)?;
    let local_port = alloc_ephemeral_port();

    let connected = with_network(|net| {
        let socket = net.sockets.get_mut::<tcp::Socket>(handle);
        let cx = net.iface.context();
        socket.connect(cx, (addr, port), local_port).is_ok()
    }).unwrap_or(false);

    if !connected {
        socket_close(handle);
        return Err(TcpError::WriteError);
    }

    // Wait for connection to be established
    core::future::poll_fn(|cx| {
        if !is_valid_handle(handle) {
            return Poll::Ready(Err(TcpError::WriteError));
        }
        // Drive the network stack forward
        poll();
        with_network(|net| {
            let socket = net.sockets.get_mut::<tcp::Socket>(handle);
            match socket.state() {
                tcp::State::Established => Poll::Ready(Ok(())),
                tcp::State::Closed | tcp::State::Closing | tcp::State::TimeWait => {
                    Poll::Ready(Err(TcpError::WriteError))
                }
                _ => {
                    socket.register_send_waker(cx.waker());
                    Poll::Pending
                }
            }
        }).unwrap_or(Poll::Ready(Err(TcpError::WriteError)))
    }).await?;

    Ok((TcpStream::new(handle), handle))
}

// ============================================================================
// Socket API (Wrappers)
// ============================================================================

#[must_use] 
/// Free slots held by sockets that are only waiting out a protocol timer.
///
/// `socket_close` does not drop a socket immediately — it parks the handle in
/// `pending_removal` so smoltcp can finish the teardown handshake, and `poll`'s
/// sweep collects it once the state reaches `Closed` or `SOCKET_GC_TIMEOUT_US`
/// (**30 s**) expires. That is correct for a long-lived connection and badly
/// wrong for connection-per-request traffic: a socket in `TimeWait` holds a slot
/// for smoltcp's full close timeout, so at ~900 HTTP requests a second the
/// 128-slot budget drains in well under a second and every subsequent `accept`
/// fails. Measured 2026-08-19: **26 % of requests reset**
/// (`docs/archive/AKUMA_NET_ISSUES.md` §3.4).
///
/// Under pressure the states below are reclaimed early, oldest first:
///
/// - `TimeWait` — the connection is over. TIME-WAIT exists to absorb delayed
///   duplicates from the *old* incarnation of a 4-tuple; recycling it under slot
///   pressure is what every production stack does (Linux's `tcp_tw_recycle`
///   lineage, and its `tcp_max_tw_buckets` cap, which simply drops the oldest).
/// - `Closed` — already collectable; the periodic sweep just has not run.
///
/// Nothing else is touched. A socket still in `FinWait1`/`FinWait2`/`LastAck` is
/// waiting on the *peer*, and killing it would discard data the peer is still
/// entitled to send, so those keep the 30 s timeout they always had.
///
/// Returns how many slots it freed.
fn reclaim_pending_slots(net: &mut NetworkState, want: usize) -> usize {
    if want == 0 {
        return 0;
    }
    let mut freed = 0;
    // Oldest first: `pending_removal` holds the close timestamp, and the entry
    // that has waited longest is the one whose duplicates are least likely to
    // still be in flight.
    while freed < want {
        let mut oldest: Option<(usize, u64)> = None;
        for (i, &(handle, added_at)) in net.pending_removal.iter().enumerate() {
            if !is_valid_handle(handle) {
                continue;
            }
            let state = net.sockets.get::<tcp::Socket>(handle).state();
            if !matches!(state, tcp::State::TimeWait | tcp::State::Closed) {
                continue;
            }
            if oldest.is_none_or(|(_, t)| added_at < t) {
                oldest = Some((i, added_at));
            }
        }
        let Some((idx, _)) = oldest else { break };
        let (handle, _) = net.pending_removal[idx];
        net.sockets.get_mut::<tcp::Socket>(handle).abort();
        net.sockets.remove(handle);
        SOCKETS_LIVE.fetch_sub(1, Ordering::Relaxed);
        net.pending_removal.swap_remove(idx);
        freed += 1;
        RECLAIMED_SLOTS.fetch_add(1, Ordering::Relaxed);
    }
    freed
}

/// Slots to free per pressure-valve trip. Small next to `MAX_SOCKETS` (128 on
/// the devbox) so a burst never discards a meaningful fraction of the table,
/// large enough that a listener refilling an 8- or 32-deep backlog does not
/// re-scan `pending_removal` on every single `socket_create`.
const RECLAIM_BATCH: usize = 8;

/// The table size actually enforced right now. Starts at
/// [`SOCKET_SOFT_CAP_START`] and grows toward `MAX_SOCKETS` only under genuine
/// pressure — see [`grow_soft_cap`].
#[cfg(feature = "smoltcp")]
static SOCKET_SOFT_CAP: AtomicUsize = AtomicUsize::new(SOCKET_SOFT_CAP_START);

/// Grow the soft cap by 20 % (at least one slot), clamped to `MAX_SOCKETS`.
///
/// Called only when reclamation has already failed to free anything, i.e. the
/// table is full of genuinely live connections rather than `TimeWait` corpses.
/// That distinction is the whole design: growing on `TimeWait` pressure would
/// walk the cap straight up to the ceiling and inherit the 2048-slot result
/// (45 us per poll), whereas growing on *live* pressure trades a slightly more
/// expensive poll for connections that would otherwise be refused.
///
/// Returns the new cap.
#[cfg(feature = "smoltcp")]
fn grow_soft_cap() -> usize {
    let cur = SOCKET_SOFT_CAP.load(Ordering::Relaxed);
    if cur >= MAX_SOCKETS {
        return cur;
    }
    let next = (cur + cur / 5).max(cur + 1).min(MAX_SOCKETS);
    SOCKET_SOFT_CAP.store(next, Ordering::Relaxed);
    next
}

/// Live entries in the smoltcp `SocketSet`, maintained incrementally.
///
/// `SocketSet::iter().count()` is O(set) and there is no cheaper accessor, so
/// every "how full are we" question used to walk the whole table — up to three
/// times per `socket_create`, plus once per `poll()` for the profiler. At 128
/// slots and ~90,000 polls/s that is over 10M iterations/s inside the `NETWORK`
/// lock with IRQs masked; at 2048 slots it distorted the measurement it was
/// taken for. Maintained here instead: `+1` on `add`, `-1` on `remove`, read as
/// a single atomic load.
#[cfg(feature = "smoltcp")]
static SOCKETS_LIVE: AtomicUsize = AtomicUsize::new(0);

/// Live socket count. O(1) — see [`SOCKETS_LIVE`].
#[cfg(feature = "smoltcp")]
#[must_use]
pub fn sockets_live() -> usize {
    SOCKETS_LIVE.load(Ordering::Relaxed)
}

/// The enforced table size right now. Diagnostic + the `[NICSTAT]` dump.
#[cfg(feature = "smoltcp")]
#[must_use]
pub fn socket_soft_cap() -> usize {
    SOCKET_SOFT_CAP.load(Ordering::Relaxed)
}

/// Sockets reclaimed early by [`reclaim_pending_slots`]. A steadily climbing
/// value means the socket budget is genuinely too small for the workload rather
/// than merely churning; a flat zero under connection-per-request load means the
/// valve is not being reached.
static RECLAIMED_SLOTS: AtomicU64 = AtomicU64::new(0);

/// How many socket slots have been reclaimed under pressure since boot.
#[must_use]
pub fn reclaimed_slot_count() -> u64 {
    RECLAIMED_SLOTS.load(Ordering::Relaxed)
}

#[must_use]
pub fn socket_create() -> Option<SocketHandle> {
    with_network(|net| {
        let mut cap = SOCKET_SOFT_CAP.load(Ordering::Relaxed);
        if sockets_live() >= cap {
            // Pressure valve before giving up: a full table under
            // connection-per-request traffic is usually full of `TimeWait`,
            // not of live connections. Reclaim a small batch rather than the
            // one slot needed — the scan is O(pending) and the caller here is
            // the listener refilling its backlog, so it is about to ask again
            // several more times.
            let freed = reclaim_pending_slots(net, RECLAIM_BATCH);
            if sockets_live() >= cap {
                // Reclamation could not help: the table is full of LIVE
                // connections, not corpses. Widen rather than refuse — see
                // `grow_soft_cap` for why this is gated on `freed == 0` and not
                // simply on being full.
                if freed == 0 {
                    cap = grow_soft_cap();
                }
                if sockets_live() >= cap {
                    return None;
                }
            }
        }
        let rx_buffer = tcp::SocketBuffer::new(vec![0; TCP_RX_BUFFER_SIZE]);
        let tx_buffer = tcp::SocketBuffer::new(vec![0; TCP_TX_BUFFER_SIZE]);
        let mut socket = tcp::Socket::new(rx_buffer, tx_buffer);
        socket.set_nagle_enabled(false);
        // Disable delayed ACK so receive-heavy workloads aren't throttled
        // to ~65KB/10ms by piggyback waiting.
        socket.set_ack_delay(None);
        SOCKETS_LIVE.fetch_add(1, Ordering::Relaxed);
        Some(net.sockets.add(socket))
    }).flatten()
}

// ============================================================================
// UDP Socket API
// ============================================================================

const UDP_PACKET_COUNT: usize = 8;
/// DNS responses can exceed 512 bytes when there are many records (CNAME chains,
/// multiple A/AAAA records, TXT in additional section). 1500 bytes handles most cases
/// without exceeding typical MTU.
const UDP_PAYLOAD_SIZE: usize = 1500;

#[must_use] 
pub fn udp_socket_create() -> Option<SocketHandle> {
    with_network(|net| {
        if sockets_live() >= MAX_SOCKETS {
            return None;
        }
        let rx_meta = udp::PacketMetadata::EMPTY;
        let tx_meta = udp::PacketMetadata::EMPTY;
        let rx_buffer = udp::PacketBuffer::new(
            vec![rx_meta; UDP_PACKET_COUNT],
            vec![0u8; UDP_PACKET_COUNT * UDP_PAYLOAD_SIZE],
        );
        let tx_buffer = udp::PacketBuffer::new(
            vec![tx_meta; UDP_PACKET_COUNT],
            vec![0u8; UDP_PACKET_COUNT * UDP_PAYLOAD_SIZE],
        );
        let socket = udp::Socket::new(rx_buffer, tx_buffer);
        SOCKETS_LIVE.fetch_add(1, Ordering::Relaxed);
        Some(net.sockets.add(socket))
    }).flatten()
}

#[allow(clippy::result_unit_err)]
pub fn udp_socket_bind(handle: SocketHandle, port: u16) -> Result<(), ()> {
    with_network(|net| {
        let socket = net.sockets.get_mut::<udp::Socket>(handle);
        socket.bind(port).map_err(|_| ())
    }).unwrap_or(Err(()))
}

#[allow(clippy::result_unit_err)]
pub fn udp_socket_send(handle: SocketHandle, buf: &[u8], remote: smoltcp::wire::IpEndpoint) -> Result<usize, ()> {
    with_network(|net| {
        let socket = net.sockets.get_mut::<udp::Socket>(handle);
        socket.send_slice(buf, remote).map(|()| buf.len()).map_err(|_| ())
    }).unwrap_or(Err(()))
}

#[allow(clippy::result_unit_err)]
pub fn udp_socket_recv(handle: SocketHandle, buf: &mut [u8]) -> Result<(usize, smoltcp::wire::IpEndpoint), ()> {
    with_network(|net| {
        let socket = net.sockets.get_mut::<udp::Socket>(handle);
        match socket.recv_slice(buf) {
            Ok((len, meta)) => Ok((len, meta.endpoint)),
            Err(_) => Err(()),
        }
    }).unwrap_or(Err(()))
}

#[must_use]
pub fn udp_can_recv(handle: SocketHandle) -> bool {
    with_network(|net| {
        net.sockets.get::<udp::Socket>(handle).can_recv()
    }).unwrap_or(false)
}

#[must_use] 
pub fn udp_can_send(handle: SocketHandle) -> bool {
    with_network(|net| {
        net.sockets.get::<udp::Socket>(handle).can_send()
    }).unwrap_or(false)
}

#[must_use] 
pub fn get_local_ip() -> [u8; 4] {
    with_network(|net| {
            for cidr in net.iface.ip_addrs() {
                let IpCidr::Ipv4(v4) = cidr;
                let octets = v4.address().octets();
                if octets != [127, 0, 0, 1] {
                    return octets;
                }
            }
        [10, 0, 2, 15]
    }).unwrap_or([10, 0, 2, 15])
}

pub fn udp_socket_close(handle: SocketHandle) {
    with_network(|net| {
        let socket = net.sockets.get_mut::<udp::Socket>(handle);
        socket.close();
        net.sockets.remove(handle);
        SOCKETS_LIVE.fetch_sub(1, Ordering::Relaxed);
    });
}

/// Record that `handle` has just entered `SynSent`, so `poll()` can enforce
/// [`CONNECT_TIMEOUT_US`] on it.
pub fn note_connect_started(handle: SocketHandle) {
    with_network(|net| {
        let now = (runtime().uptime_us)();
        // A redial on the same handle replaces the old deadline rather than
        // stacking a second one.
        if let Some(e) = net.connecting.iter_mut().find(|(h, _)| *h == handle) {
            e.1 = now;
        } else {
            net.connecting.push((handle, now));
        }
    });
}

pub fn socket_close(handle: SocketHandle) {
    if !is_valid_handle(handle) {
        log::warn!("[NET] CORRUPT HANDLE in socket_close: handle={handle}");
        return;
    }
    with_network(|net| {
        let socket = net.sockets.get_mut::<tcp::Socket>(handle);
        socket.close();
        net.pending_removal.push((handle, (runtime().uptime_us)()));
    });
}


// ============================================================================
// Async TCP Stream (embedded-io-async)
// ============================================================================

use core::task::Poll;

#[derive(Debug, Clone, Copy)]
pub enum TcpError {
    ReadError,
    WriteError,
}

impl embedded_io_async::Error for TcpError {
    fn kind(&self) -> embedded_io_async::ErrorKind {
        embedded_io_async::ErrorKind::Other
    }
}

pub struct TcpStream {
    handle: SocketHandle,
    /// Cached socket index for corruption detection. Must always be < `MAX_SOCKETS`.
    handle_index: usize,
}

/// Extract the internal index from a `SocketHandle`.
///
/// `SocketHandle` is a newtype wrapper around a single `usize` field (the socket
/// set index). Since it has no public accessor, we use transmute to read it.
/// This is safe because `SocketHandle` contains exactly one usize and both types
/// have identical size and alignment.
fn socket_handle_index(handle: SocketHandle) -> usize {
    // Safety: SocketHandle(usize) is a single-field struct with the same
    // layout as usize. Verified by the static_assert below.
    const _: () = assert!(
        core::mem::size_of::<SocketHandle>() == core::mem::size_of::<usize>()
    );
    unsafe { core::mem::transmute::<SocketHandle, usize>(handle) }
}

/// Check if a `SocketHandle` index is within the valid range for our socket set.
fn is_valid_handle(handle: SocketHandle) -> bool {
    socket_handle_index(handle) < MAX_SOCKETS
}

impl TcpStream {
    #[must_use] 
    pub fn new(handle: SocketHandle) -> Self {
        Self {
            handle,
            handle_index: socket_handle_index(handle),
        }
    }
}

impl embedded_io_async::ErrorType for TcpStream {
    type Error = TcpError;
}

impl embedded_io_async::Read for TcpStream {
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
        core::future::poll_fn(|cx| {
            // Validate handle before accessing the socket set. A corrupted
            // async state machine could overwrite handle_index with garbage;
            // catch it here instead of panicking inside smoltcp's get_mut.
            if self.handle_index >= MAX_SOCKETS {
                log::warn!(
                    "[NET] CORRUPT HANDLE in TcpStream::read: index={}, handle={}",
                    self.handle_index,
                    self.handle
                );
                return Poll::Ready(Err(TcpError::ReadError));
            }
            with_network(|net| {
                let socket = net.sockets.get_mut::<tcp::Socket>(self.handle);
                if socket.can_recv() {
                    socket
                        .recv(|data| {
                            let len = data.len().min(buf.len());
                            buf[..len].copy_from_slice(&data[..len]);
                            (len, len)
                        })
                        .map_or(Poll::Ready(Err(TcpError::ReadError)), |n| Poll::Ready(Ok(n)))
                } else if socket.state() == tcp::State::Closed || socket.state() == tcp::State::CloseWait {
                    Poll::Ready(Ok(0)) // EOF
                } else {
                    socket.register_recv_waker(cx.waker());
                    Poll::Pending
                }
            }).unwrap_or(Poll::Ready(Err(TcpError::ReadError)))
        }).await
    }
}

impl embedded_io_async::Write for TcpStream {
    async fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
        core::future::poll_fn(|cx| {
            if self.handle_index >= MAX_SOCKETS {
                log::warn!(
                    "[NET] CORRUPT HANDLE in TcpStream::write: index={}, handle={}",
                    self.handle_index,
                    self.handle
                );
                return Poll::Ready(Err(TcpError::WriteError));
            }
            with_network(|net| {
                let socket = net.sockets.get_mut::<tcp::Socket>(self.handle);
                if socket.can_send() {
                    socket
                        .send_slice(buf)
                        .map_or(Poll::Ready(Err(TcpError::WriteError)), |n| Poll::Ready(Ok(n)))
                } else if socket.state() == tcp::State::Closed || socket.state() == tcp::State::CloseWait {
                    Poll::Ready(Err(TcpError::WriteError)) // Broken pipe
                } else {
                    socket.register_send_waker(cx.waker());
                    Poll::Pending
                }
            }).unwrap_or(Poll::Ready(Err(TcpError::WriteError)))
        }).await
    }
    
    async fn flush(&mut self) -> Result<(), Self::Error> {
        core::future::poll_fn(|cx| {
            if self.handle_index >= MAX_SOCKETS {
                log::warn!(
                    "[NET] CORRUPT HANDLE in TcpStream::flush: index={}, handle={}",
                    self.handle_index,
                    self.handle
                );
                return Poll::Ready(Err(TcpError::WriteError));
            }
            with_network(|net| {
                let socket = net.sockets.get_mut::<tcp::Socket>(self.handle);
                if socket.send_queue() == 0 {
                    Poll::Ready(Ok(()))
                } else if socket.state() == tcp::State::Closed || socket.state() == tcp::State::CloseWait {
                    Poll::Ready(Err(TcpError::WriteError))
                } else {
                    socket.register_send_waker(cx.waker());
                    Poll::Pending
                }
            }).unwrap_or(Poll::Ready(Err(TcpError::WriteError)))
        }).await
    }
}
