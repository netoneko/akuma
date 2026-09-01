//! `[NICSTAT]` dump — the console half of `akuma_net::nicstat` (`net-profile`).
//!
//! **Measurement builds only.** The counters live in `crates/akuma-net`; this
//! module owns the previous-window snapshot and the printing, because printing
//! is a kernel-crate concern: `safe_print!` allocates nothing, which is the rule
//! for anything that ends at the console (CLAUDE.md § "Kernel conventions").
//!
//! Deltas, not totals — same reason `bkl_profile` prints deltas: a window has to
//! be attributable to the workload that ran during it, not to boot noise.
//!
//! # The three lines, and what each one answers
//!
//! ```text
//! [NICSTAT] w=3 dt=5000ms rx=12043p/1584kB tx=12038p/802kB lo=0p drop=0
//! [NICSTAT] w=3 tx_wait=61.2ms(5.0us/pkt max=137us) rx_post=18.1ms(1.5us) rx_done=4.4ms
//! [NICSTAT] w=3 poll=48211c/12040prog 96.7ms(2.0us/c max=241us) empty=36171 relax=118/9.7ms
//! ```
//!
//! - **line 1** — did traffic actually flow, and in which direction.
//! - **line 2** — `tx_wait` is the headline. `VirtIONetRaw::send` is
//!   `add_notify_wait_pop`: it busy-spins until the host consumes the
//!   descriptor, and it runs with `NETWORK` held and IRQs masked. Every
//!   microsecond here is a microsecond no core can enter the network stack.
//!   `rx_post` is the matching cost on the receive side — one MMIO notify per
//!   *packet*, because only a single 2 KB buffer is ever posted.
//! - **line 3** — `empty` counts drain-loop probes that found nothing (pure
//!   overhead), and `relax` counts parks in `blocking_relax`. There is no
//!   virtio-net IRQ — `src/main.rs` registers only IRQ 27, the timer — so a
//!   park ends on the tick or on a peer core, and `relax_us/relax` is the
//!   wake-latency a blocked reader pays.
//!
//! Parsed by `scripts/benchmarks/bench_nic_rtt.py`, which lines these windows up
//! against host-measured round-trip latency over the QEMU port forward.

use akuma_net::nicstat::{self, NicStat};
use akuma_net::smoltcp_net;
use core::sync::atomic::{AtomicU64, Ordering};

/// How often to print a window. Matches `bkl_profile::DUMP_INTERVAL_US`'s intent
/// (one workload step per window) but is shorter: a round-trip benchmark run is
/// seconds, not tens of seconds, and lining the windows up with the host-side
/// samples needs finer granularity than the BKL histogram does.
const DUMP_INTERVAL_US: u64 = 5_000_000;

static LAST_DUMP_US: AtomicU64 = AtomicU64::new(0);
static WINDOW: AtomicU64 = AtomicU64::new(0);

/// Previous window's reading. A plain set of atomics rather than a
/// `Spinlock<NicStat>`: this runs on the async-main loop only, and a torn read
/// costs one skewed window, never a stall.
macro_rules! prev {
    ($($f:ident),* $(,)?) => {
        mod prev {
            use core::sync::atomic::AtomicU64;
            $(pub static $f: AtomicU64 = AtomicU64::new(0);)*
        }
    };
}
/// Previous window's NIC interrupt count. Kept here rather than in `nicstat`
/// because the counter lives with the acknowledge path, not with the packet
/// counters — but it is the first thing to read when a latency fix that depends
/// on the interrupt does not move the number: `irq=0` means the SPI never
/// reached the CPU and the stack is still tick-driven.
static PREV_NIC_IRQS: AtomicU64 = AtomicU64::new(0);

/// async-main loop iterations — one wake/drain/halt cycle each. Incremented in
/// `src/main.rs`; printed as a windowed delta so `laps` can be compared against
/// `nic_irq` (what should be waking it) and `poll` (calls from all callers).
pub static NETPOLL_LAPS: AtomicU64 = AtomicU64::new(0);
static PREV_NETPOLL_LAPS: AtomicU64 = AtomicU64::new(0);

prev!(
    RX_PKTS, RX_BYTES, RX_BEGIN, RX_BEGIN_US, RX_DONE_US, RX_EMPTY,
    TX_PKTS, TX_BYTES, TX_US, TX_DROPS, TX_FLIGHT, TX_FLIGHT_US,
    LO_PKTS, LO_BYTES,
    POLL_CALLS, POLL_PROGRESS, POLL_US, POLL_WAIT_US, POLL_WAKE_US, EPOCH_SAVES,
    RELAX, RELAX_US,
);

fn load_prev() -> NicStat {
    let g = |c: &AtomicU64| c.load(Ordering::Relaxed);
    NicStat {
        rx_pkts: g(&prev::RX_PKTS),
        rx_bytes: g(&prev::RX_BYTES),
        rx_begin: g(&prev::RX_BEGIN),
        rx_begin_us: g(&prev::RX_BEGIN_US),
        rx_done_us: g(&prev::RX_DONE_US),
        rx_empty: g(&prev::RX_EMPTY),
        tx_pkts: g(&prev::TX_PKTS),
        tx_bytes: g(&prev::TX_BYTES),
        tx_us: g(&prev::TX_US),
        tx_max_us: 0,
        tx_drops: g(&prev::TX_DROPS),
        tx_flight: g(&prev::TX_FLIGHT),
        tx_flight_us: g(&prev::TX_FLIGHT_US),
        tx_flight_max_us: 0,
        lo_pkts: g(&prev::LO_PKTS),
        lo_bytes: g(&prev::LO_BYTES),
        poll_calls: g(&prev::POLL_CALLS),
        poll_progress: g(&prev::POLL_PROGRESS),
        poll_us: g(&prev::POLL_US),
        poll_max_us: 0,
        poll_wait_us: g(&prev::POLL_WAIT_US),
        poll_wait_max_us: 0,
        poll_wake_us: g(&prev::POLL_WAKE_US),
        epoch_saves: g(&prev::EPOCH_SAVES),
        sockets_live: 0,
        relax: g(&prev::RELAX),
        relax_us: g(&prev::RELAX_US),
    }
}

fn store_prev(s: &NicStat) {
    let p = |c: &AtomicU64, v: u64| c.store(v, Ordering::Relaxed);
    p(&prev::RX_PKTS, s.rx_pkts);
    p(&prev::RX_BYTES, s.rx_bytes);
    p(&prev::RX_BEGIN, s.rx_begin);
    p(&prev::RX_BEGIN_US, s.rx_begin_us);
    p(&prev::RX_DONE_US, s.rx_done_us);
    p(&prev::RX_EMPTY, s.rx_empty);
    p(&prev::TX_PKTS, s.tx_pkts);
    p(&prev::TX_BYTES, s.tx_bytes);
    p(&prev::TX_US, s.tx_us);
    p(&prev::TX_DROPS, s.tx_drops);
    p(&prev::TX_FLIGHT, s.tx_flight);
    p(&prev::TX_FLIGHT_US, s.tx_flight_us);
    p(&prev::LO_PKTS, s.lo_pkts);
    p(&prev::LO_BYTES, s.lo_bytes);
    p(&prev::POLL_CALLS, s.poll_calls);
    p(&prev::POLL_PROGRESS, s.poll_progress);
    p(&prev::POLL_US, s.poll_us);
    p(&prev::POLL_WAIT_US, s.poll_wait_us);
    p(&prev::POLL_WAKE_US, s.poll_wake_us);
    p(&prev::EPOCH_SAVES, s.epoch_saves);
    p(&prev::RELAX, s.relax);
    p(&prev::RELAX_US, s.relax_us);
}

/// Tenths of `num/den`, saturating, with `den == 0` reading as 0. Used so the
/// per-packet averages can be printed as `5.0us` without floating point (which
/// would pull in soft-float formatting on a console path).
fn tenths(num: u64, den: u64) -> (u64, u64) {
    if den == 0 {
        return (0, 0);
    }
    let t = num.saturating_mul(10) / den;
    (t / 10, t % 10)
}

/// Print one window if `DUMP_INTERVAL_US` has elapsed. Called from the
/// async-main loop next to `bkl_profile::maybe_dump`.
pub fn maybe_dump(now_us: u64) {
    let last = LAST_DUMP_US.load(Ordering::Relaxed);
    if now_us.saturating_sub(last) < DUMP_INTERVAL_US {
        return;
    }
    // First call only stamps the baseline: window 0 would otherwise report the
    // whole boot, which is not a workload.
    if last == 0 {
        LAST_DUMP_US.store(now_us, Ordering::Relaxed);
        store_prev(&nicstat::snapshot());
        PREV_NIC_IRQS.store(smoltcp_net::nic_irq_count(), Ordering::Relaxed);
        nicstat::reset_maxima();
        return;
    }
    let dt_us = now_us - last;
    LAST_DUMP_US.store(now_us, Ordering::Relaxed);

    let cur = nicstat::snapshot();
    let d = cur.delta(&load_prev());
    store_prev(&cur);
    nicstat::reset_maxima();

    if d.is_idle() {
        return;
    }
    let w = WINDOW.fetch_add(1, Ordering::Relaxed);

    crate::safe_print!(
        160,
        "[NICSTAT] w={} dt={}ms rx={}p/{}kB tx={}p/{}kB lo={}p/{}kB drop={}\n",
        w,
        dt_us / 1000,
        d.rx_pkts,
        d.rx_bytes / 1024,
        d.tx_pkts,
        d.tx_bytes / 1024,
        d.lo_pkts,
        d.lo_bytes / 1024,
        d.tx_drops
    );

    let irqs_now = smoltcp_net::nic_irq_count();
    let irqs = irqs_now.saturating_sub(PREV_NIC_IRQS.swap(irqs_now, Ordering::Relaxed));
    // `orphan` and `tx_stall` are cumulative, not windowed: both should be
    // exactly zero forever, so the useful question is "has it ever happened",
    // not "how often this window". A non-zero `tx_stall` means `TX_RING` is too
    // shallow for the offered load and those frames took the old blocking path.
    #[cfg(feature = "net-noalloc")]
    let (orphan, tx_stall) = akuma_net::virtio_rings::ring_health();
    #[cfg(not(feature = "net-noalloc"))]
    let (orphan, tx_stall) = (0u64, 0u64);
    let laps_now = NETPOLL_LAPS.load(Ordering::Relaxed);
    let laps = laps_now.saturating_sub(PREV_NETPOLL_LAPS.swap(laps_now, Ordering::Relaxed));
    crate::safe_print!(
        160,
        "[NICSTAT] w={} nic_irq={} laps={} orphan={} tx_stall={}\n",
        w,
        irqs,
        laps,
        orphan,
        tx_stall
    );

    // Async TX only: how long the host actually took to consume a submitted
    // descriptor. `tx_wait` above stops being the whole cost once transmit no
    // longer blocks — this is the rest of it.
    if d.tx_flight > 0 {
        let (fa, fb) = tenths(d.tx_flight_us, d.tx_flight);
        crate::safe_print!(
            128,
            "[NICSTAT] w={} tx_flight={}p {}ms({}.{}us/pkt max={}us)\n",
            w,
            d.tx_flight,
            d.tx_flight_us / 1000,
            fa,
            fb,
            d.tx_flight_max_us
        );
    }

    // Decompose `poll`: waiting for NETWORK vs the post-drop wake_all pass. The
    // remainder is the poll itself. A `poll_max` in the milliseconds is one of
    // these three and they need different fixes.
    if d.poll_calls > 0 {
        let (wa, wb) = tenths(d.poll_wait_us, d.poll_calls);
        crate::safe_print!(
            160,
            "[NICSTAT] w={} poll_wait={}ms({}.{}us/c max={}us) wake={}ms sockets={}/{} epoch_saves={}\n",
            w,
            d.poll_wait_us / 1000,
            wa,
            wb,
            d.poll_wait_max_us,
            d.poll_wake_us / 1000,
            d.sockets_live,
            smoltcp_net::socket_soft_cap(),
            d.epoch_saves
        );
    }

    let (txa, txb) = tenths(d.tx_us, d.tx_pkts);
    let (rxa, rxb) = tenths(d.rx_begin_us, d.rx_begin);
    crate::safe_print!(
        160,
        "[NICSTAT] w={} tx_wait={}ms({}.{}us/pkt max={}us) rx_post={}ms({}.{}us) rx_done={}ms\n",
        w,
        d.tx_us / 1000,
        txa,
        txb,
        d.tx_max_us,
        d.rx_begin_us / 1000,
        rxa,
        rxb,
        d.rx_done_us / 1000
    );

    let (pa, pb) = tenths(d.poll_us, d.poll_calls);
    let (ra, rb) = tenths(d.relax_us, d.relax);
    crate::safe_print!(
        192,
        "[NICSTAT] w={} poll={}c/{}prog {}ms({}.{}us/c max={}us) empty={} relax={}/{}ms({}.{}us)\n",
        w,
        d.poll_calls,
        d.poll_progress,
        d.poll_us / 1000,
        pa,
        pb,
        d.poll_max_us,
        d.rx_empty,
        d.relax,
        d.relax_us / 1000,
        ra,
        rb
    );
}
