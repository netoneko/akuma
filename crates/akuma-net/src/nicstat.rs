//! NIC-level traffic and latency instrumentation (measurement builds only).
//!
//! Answers one question the BKL profiler cannot: **where does a network round
//! trip actually spend its microseconds inside the virtio-net driver?**
//!
//! The BKL profiler (`src/bkl_profile.rs`) attributes *lock* time by syscall
//! tag. It says `netpoll` is the top holder but not why a `netpoll` lap costs
//! what it does. These counters split one lap into its device-level pieces:
//!
//! | counter family | what it isolates |
//! |---|---|
//! | `rx_*`   | posting a receive buffer (an MMIO notify → vmexit) and completing one |
//! | `tx_*`   | `VirtIONetRaw::send`, which is `add_notify_wait_pop` — a **blocking** spin until the host consumes the descriptor, executed with `NETWORK` held and IRQs masked |
//! | `poll_*` | a whole `smoltcp_net::poll()`, so device time can be subtracted from stack time |
//! | `relax_*`| how often a blocked socket parked in `blocking_relax` (WFI), whose only wakeup is the timer tick — there is no virtio-net IRQ |
//!
//! # Cost and gating
//!
//! Everything here is behind the `net-profile` feature. With it off every
//! function below is an empty `#[inline(always)]` body and the statics are not
//! emitted, so a normal build is byte-for-byte unchanged — the same discipline
//! `bkl-profile` uses. With it on, each instrumented site costs two
//! `uptime_us()` reads (CNTVCT) plus relaxed atomic adds, which is enough to
//! perturb a sub-microsecond path: read the *ratios* between counters, not the
//! absolute wall time of the workload.
//!
//! # Reading it
//!
//! [`snapshot`] returns a plain `Copy` struct; the kernel diffs two snapshots
//! and prints the delta with `safe_print!` (no allocation on a console path —
//! see CLAUDE.md § "Kernel conventions"). `scripts/benchmarks/bench_nic_rtt.py`
//! parses those `[NICSTAT]` lines out of the serial log and lines them up with
//! host-side round-trip latency.

#[cfg(feature = "net-profile")]
use core::sync::atomic::{AtomicU64, Ordering};

/// One reading of every NIC counter. Deltas between two of these are what the
/// kernel prints; the absolute values are monotonic since boot.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct NicStat {
    /// Frames handed up from virtio (loopback excluded).
    pub rx_pkts: u64,
    /// Bytes in those frames (L2, excluding the virtio net header).
    pub rx_bytes: u64,
    /// Calls to `receive_begin` — each posts a buffer and may notify the device.
    pub rx_begin: u64,
    /// Cumulative µs inside `receive_begin` (the MMIO notify → vmexit).
    pub rx_begin_us: u64,
    /// Cumulative µs inside `receive_complete`.
    pub rx_done_us: u64,
    /// `Device::receive()` calls that found no packet. The ratio
    /// `rx_empty / rx_pkts` is how much of the drain loop is wasted probing.
    pub rx_empty: u64,
    /// Frames pushed to virtio (loopback excluded).
    pub tx_pkts: u64,
    /// Bytes in those frames.
    pub tx_bytes: u64,
    /// Cumulative µs blocked inside `VirtIONetRaw::send`. **This is the one to
    /// watch**: it is spent with `NETWORK` held and IRQs masked.
    pub tx_us: u64,
    /// Worst single `send` in µs.
    pub tx_max_us: u64,
    /// Sends that returned an error (queue full) and were dropped.
    pub tx_drops: u64,
    /// Frames whose device completion was reaped (`net-noalloc` only).
    pub tx_flight: u64,
    /// Cumulative µs from `transmit_begin` to that completion being observed.
    ///
    /// The async-TX equivalent of `tx_us`, and the one number that says whether
    /// the host is picking submitted frames up promptly. `tx_us` cannot answer
    /// that once transmit stops blocking: it only covers the submit itself.
    /// Reap runs once per poll lap, and there are tens of thousands of those
    /// per window, so the observation delay is not what this measures.
    pub tx_flight_us: u64,
    /// Worst single submit → completion in µs.
    pub tx_flight_max_us: u64,
    /// Frames short-circuited into the loopback queue.
    pub lo_pkts: u64,
    /// Bytes in those frames.
    pub lo_bytes: u64,
    /// Calls to `smoltcp_net::poll()`.
    pub poll_calls: u64,
    /// Those that reported `SocketStateChanged`.
    pub poll_progress: u64,
    /// Cumulative µs inside `poll()`, device time included.
    pub poll_us: u64,
    /// Worst single `poll()` in µs.
    pub poll_max_us: u64,
    /// Cumulative µs spent WAITING for the `NETWORK` spinlock inside `poll()`.
    ///
    /// Split out of `poll_us` because that number conflates three unrelated
    /// things — lock wait, lock hold, and the post-drop `wake_all` pass over
    /// every socket — and a `poll_max` of 3.7 ms could be any of them.
    pub poll_wait_us: u64,
    /// Worst single `NETWORK` acquisition wait in µs.
    pub poll_wait_max_us: u64,
    /// Times a waiter was about to park, noticed the wake epoch had moved, and
    /// looped to re-check instead. Zero means the epoch guard never fires and the
    /// window it closes does not exist in practice.
    pub epoch_saves: u64,
    /// Live entries in the smoltcp `SocketSet` at the last dump — a LEVEL, not a
    /// delta, so `delta()` passes it through unchanged.
    ///
    /// `iface.poll()` walks the whole set on every call, so this is the scaling
    /// term behind `poll_us`. Under connection-per-request load the set fills
    /// with `TimeWait`/`pending_removal` entries and per-poll cost climbs (10.5
    /// -> 15.6 us measured across three benchmark runs).
    pub sockets_live: u64,
    /// Cumulative µs in `poll()`'s post-drop `wake_all` pass. Runs with `NETWORK`
    /// released but takes `SOCKET_TABLE` and every slot's waker lock.
    pub poll_wake_us: u64,
    /// Times a blocking socket op parked in `blocking_relax` (yield + WFI).
    pub relax: u64,
    /// Cumulative µs spent parked there — the wake-latency budget.
    pub relax_us: u64,
}

impl NicStat {
    /// Field-wise `self - base`, for printing a windowed delta. Saturating so a
    /// torn read (the counters are not snapshot atomically) can never wrap.
    #[must_use]
    pub fn delta(&self, base: &Self) -> Self {
        Self {
            rx_pkts: self.rx_pkts.saturating_sub(base.rx_pkts),
            rx_bytes: self.rx_bytes.saturating_sub(base.rx_bytes),
            rx_begin: self.rx_begin.saturating_sub(base.rx_begin),
            rx_begin_us: self.rx_begin_us.saturating_sub(base.rx_begin_us),
            rx_done_us: self.rx_done_us.saturating_sub(base.rx_done_us),
            rx_empty: self.rx_empty.saturating_sub(base.rx_empty),
            tx_pkts: self.tx_pkts.saturating_sub(base.tx_pkts),
            tx_bytes: self.tx_bytes.saturating_sub(base.tx_bytes),
            tx_us: self.tx_us.saturating_sub(base.tx_us),
            // A max is not additive: report the window's own high-water mark,
            // which `reset_maxima` re-arms after every dump.
            tx_max_us: self.tx_max_us,
            tx_drops: self.tx_drops.saturating_sub(base.tx_drops),
            tx_flight: self.tx_flight.saturating_sub(base.tx_flight),
            tx_flight_us: self.tx_flight_us.saturating_sub(base.tx_flight_us),
            tx_flight_max_us: self.tx_flight_max_us,
            lo_pkts: self.lo_pkts.saturating_sub(base.lo_pkts),
            lo_bytes: self.lo_bytes.saturating_sub(base.lo_bytes),
            poll_calls: self.poll_calls.saturating_sub(base.poll_calls),
            poll_progress: self.poll_progress.saturating_sub(base.poll_progress),
            poll_us: self.poll_us.saturating_sub(base.poll_us),
            poll_max_us: self.poll_max_us,
            poll_wait_us: self.poll_wait_us.saturating_sub(base.poll_wait_us),
            poll_wait_max_us: self.poll_wait_max_us,
            poll_wake_us: self.poll_wake_us.saturating_sub(base.poll_wake_us),
            // A level, not a counter: report it as-is.
            epoch_saves: self.epoch_saves.saturating_sub(base.epoch_saves),
            sockets_live: self.sockets_live,
            relax: self.relax.saturating_sub(base.relax),
            relax_us: self.relax_us.saturating_sub(base.relax_us),
        }
    }

    /// True when nothing at all happened in this window — the kernel skips the
    /// dump so an idle system does not scroll the log.
    #[must_use]
    pub const fn is_idle(&self) -> bool {
        self.rx_pkts == 0 && self.tx_pkts == 0 && self.lo_pkts == 0
    }
}

// ============================================================================
// Enabled build
// ============================================================================

#[cfg(feature = "net-profile")]
mod imp {
    use super::{AtomicU64, NicStat, Ordering};

    macro_rules! counters {
        ($($name:ident),* $(,)?) => {
            $(pub(super) static $name: AtomicU64 = AtomicU64::new(0);)*
        };
    }

    counters!(
        RX_PKTS, RX_BYTES, RX_BEGIN, RX_BEGIN_US, RX_DONE_US, RX_EMPTY,
        TX_PKTS, TX_BYTES, TX_US, TX_MAX_US, TX_DROPS,
        TX_FLIGHT, TX_FLIGHT_US, TX_FLIGHT_MAX_US,
        LO_PKTS, LO_BYTES,
        POLL_CALLS, POLL_PROGRESS, POLL_US, POLL_MAX_US,
        POLL_WAIT_US, POLL_WAIT_MAX_US, POLL_WAKE_US, SOCKETS_LIVE, EPOCH_SAVES,
        RELAX, RELAX_US,
    );

    #[inline]
    pub(super) fn add(c: &AtomicU64, v: u64) {
        c.fetch_add(v, Ordering::Relaxed);
    }

    #[inline]
    pub(super) fn max(c: &AtomicU64, v: u64) {
        c.fetch_max(v, Ordering::Relaxed);
    }

    /// `uptime_us`, or `None` before the runtime seam is registered (early boot
    /// and host tests). Returning `None` rather than 0 keeps a bogus
    /// "elapsed = now" out of the sums.
    #[inline]
    pub(super) fn now_us() -> Option<u64> {
        crate::runtime::try_runtime().map(|rt| (rt.uptime_us)())
    }

    pub(super) fn snapshot() -> NicStat {
        let g = |c: &AtomicU64| c.load(Ordering::Relaxed);
        NicStat {
            rx_pkts: g(&RX_PKTS),
            rx_bytes: g(&RX_BYTES),
            rx_begin: g(&RX_BEGIN),
            rx_begin_us: g(&RX_BEGIN_US),
            rx_done_us: g(&RX_DONE_US),
            rx_empty: g(&RX_EMPTY),
            tx_pkts: g(&TX_PKTS),
            tx_bytes: g(&TX_BYTES),
            tx_us: g(&TX_US),
            tx_max_us: g(&TX_MAX_US),
            tx_drops: g(&TX_DROPS),
            tx_flight: g(&TX_FLIGHT),
            tx_flight_us: g(&TX_FLIGHT_US),
            tx_flight_max_us: g(&TX_FLIGHT_MAX_US),
            lo_pkts: g(&LO_PKTS),
            lo_bytes: g(&LO_BYTES),
            poll_calls: g(&POLL_CALLS),
            poll_progress: g(&POLL_PROGRESS),
            poll_us: g(&POLL_US),
            poll_max_us: g(&POLL_MAX_US),
            poll_wait_us: g(&POLL_WAIT_US),
            poll_wait_max_us: g(&POLL_WAIT_MAX_US),
            poll_wake_us: g(&POLL_WAKE_US),
            epoch_saves: g(&EPOCH_SAVES),
            sockets_live: g(&SOCKETS_LIVE),
            relax: g(&RELAX),
            relax_us: g(&RELAX_US),
        }
    }

    pub(super) fn reset_maxima() {
        TX_MAX_US.store(0, Ordering::Relaxed);
        TX_FLIGHT_MAX_US.store(0, Ordering::Relaxed);
        POLL_MAX_US.store(0, Ordering::Relaxed);
        POLL_WAIT_MAX_US.store(0, Ordering::Relaxed);
    }
}

/// A start timestamp for one of the `record_*` pairs below.
///
/// `None` when the runtime seam is not registered yet, or when profiling is
/// compiled out — in both cases the matching `record_*` call is a no-op.
#[derive(Clone, Copy)]
pub struct Started(#[cfg(feature = "net-profile")] Option<u64>);

/// Open a timing window. Pair with exactly one `record_*`.
///
/// `inline(always)` throughout this module is deliberate, not a micro-
/// optimisation: with `net-profile` off every one of these is an *empty* body,
/// and the whole no-cost claim rests on the call disappearing at every site —
/// including the per-packet ones inside `Device::receive`/`TxToken::consume`.
#[inline(always)]
#[allow(clippy::inline_always)]
#[must_use]
pub fn start() -> Started {
    #[cfg(feature = "net-profile")]
    {
        Started(imp::now_us())
    }
    #[cfg(not(feature = "net-profile"))]
    {
        Started()
    }
}

#[cfg(feature = "net-profile")]
#[inline]
fn elapsed(s: Started) -> Option<u64> {
    let t0 = s.0?;
    Some(imp::now_us()?.saturating_sub(t0))
}

/// Declare the no-op / real body of a `record_*` in one place, so the two
/// builds can never drift apart in signature.
macro_rules! recorder {
    ($(#[$m:meta])* $vis:vis fn $name:ident($($arg:ident : $ty:ty),* $(,)?) $body:block) => {
        $(#[$m])*
        #[inline(always)]
        #[allow(clippy::inline_always)]
        $vis fn $name($($arg: $ty),*) {
            #[cfg(feature = "net-profile")]
            $body
            #[cfg(not(feature = "net-profile"))]
            { $(let _ = $arg;)* }
        }
    };
}

recorder! {
    /// One `receive_begin`: a buffer posted, possibly an MMIO notify.
    pub fn record_rx_begin(s: Started) {
        imp::add(&imp::RX_BEGIN, 1);
        if let Some(us) = elapsed(s) { imp::add(&imp::RX_BEGIN_US, us); }
    }
}

recorder! {
    /// One frame completed out of the receive queue.
    pub fn record_rx_packet(s: Started, len: usize) {
        imp::add(&imp::RX_PKTS, 1);
        imp::add(&imp::RX_BYTES, len as u64);
        if let Some(us) = elapsed(s) { imp::add(&imp::RX_DONE_US, us); }
    }
}

recorder! {
    /// `Device::receive()` found nothing to hand up.
    pub fn record_rx_empty() {
        imp::add(&imp::RX_EMPTY, 1);
    }
}

recorder! {
    /// One frame pushed to virtio. `s` must have been opened immediately before
    /// the (blocking) send so `tx_us` measures the device wait and nothing else.
    pub fn record_tx(s: Started, len: usize, ok: bool) {
        imp::add(&imp::TX_PKTS, 1);
        imp::add(&imp::TX_BYTES, len as u64);
        if !ok { imp::add(&imp::TX_DROPS, 1); }
        if let Some(us) = elapsed(s) {
            imp::add(&imp::TX_US, us);
            imp::max(&imp::TX_MAX_US, us);
        }
    }
}

recorder! {
    /// One asynchronously submitted frame, completed by the device.
    ///
    /// `s` is the `Started` opened at `transmit_begin`, so this measures how
    /// long the host took to consume the descriptor — the cost `transmit_begin`
    /// stopped charging the caller for, but that did not stop existing.
    pub fn record_tx_complete(s: Started) {
        imp::add(&imp::TX_FLIGHT, 1);
        if let Some(us) = elapsed(s) {
            imp::add(&imp::TX_FLIGHT_US, us);
            imp::max(&imp::TX_FLIGHT_MAX_US, us);
        }
    }
}

recorder! {
    /// One frame short-circuited into the loopback queue.
    pub fn record_loopback(len: usize) {
        imp::add(&imp::LO_PKTS, 1);
        imp::add(&imp::LO_BYTES, len as u64);
    }
}

recorder! {
    /// One whole `smoltcp_net::poll()`.
    pub fn record_poll(s: Started, progress: bool) {
        imp::add(&imp::POLL_CALLS, 1);
        if progress { imp::add(&imp::POLL_PROGRESS, 1); }
        if let Some(us) = elapsed(s) {
            imp::add(&imp::POLL_US, us);
            imp::max(&imp::POLL_MAX_US, us);
        }
    }
}

recorder! {
    /// A waiter re-checked instead of parking because the wake epoch moved.
    pub fn record_epoch_save() {
        imp::add(&imp::EPOCH_SAVES, 1);
    }
}

recorder! {
    /// Publish the current live socket-set size (a level, overwritten each call).
    pub fn record_sockets_live(n: usize) {
        imp::SOCKETS_LIVE.store(n as u64, core::sync::atomic::Ordering::Relaxed);
    }
}

recorder! {
    /// Time spent waiting to acquire `NETWORK` in one `poll()`.
    pub fn record_poll_wait(s: Started) {
        if let Some(us) = elapsed(s) {
            imp::add(&imp::POLL_WAIT_US, us);
            imp::max(&imp::POLL_WAIT_MAX_US, us);
        }
    }
}

recorder! {
    /// Time spent in `poll()`'s post-drop `wake_all` pass.
    pub fn record_poll_wake(s: Started) {
        if let Some(us) = elapsed(s) { imp::add(&imp::POLL_WAKE_US, us); }
    }
}

recorder! {
    /// One `blocking_relax` park in a socket wait loop. There is no virtio-net
    /// IRQ (`src/main.rs` registers only IRQ 27, the timer), so the wake that
    /// ends this park comes from the timer tick or a peer core — which is
    /// exactly the latency this counter exists to expose.
    pub fn record_relax(s: Started) {
        imp::add(&imp::RELAX, 1);
        if let Some(us) = elapsed(s) { imp::add(&imp::RELAX_US, us); }
    }
}

/// Read every counter. All-zero when profiling is compiled out.
#[must_use]
#[inline(always)]
#[allow(clippy::inline_always)]
pub fn snapshot() -> NicStat {
    #[cfg(feature = "net-profile")]
    {
        imp::snapshot()
    }
    #[cfg(not(feature = "net-profile"))]
    {
        NicStat::default()
    }
}

/// Re-arm the high-water marks so the next window reports its own maxima
/// rather than the largest ever seen. Call right after taking a snapshot.
#[inline(always)]
#[allow(clippy::inline_always)]
pub fn reset_maxima() {
    #[cfg(feature = "net-profile")]
    imp::reset_maxima();
}

/// Whether this build carries the counters at all — so a report can say
/// "profiling off" instead of quoting a page of zeroes.
#[must_use]
pub const fn enabled() -> bool {
    cfg!(feature = "net-profile")
}
