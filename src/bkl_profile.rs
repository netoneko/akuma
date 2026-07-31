//! BKL-hold attribution dump (`bkl-profile` feature → `cfg(kernel_bkl_profile)`).
//!
//! **Measurement builds only.** `akuma_exec::sync` already carries a per-tag profiler:
//! a core waiting on the BKL samples what the *owning* core is doing (syscall number,
//! fault, or IRQ/scheduler) when it first observes contention, and on acquiring adds its
//! spin count to that tag's bucket. Until now the only consumer was a boot self-test, so
//! the histogram was never read under a real userspace workload — which is exactly what
//! decides whether more subsystems are worth carving out of the BKL
//! (docs/archive/BKL_FINE_GRAINED_LOCKING_PLAN.md: the Phase 0 "scheduler/IRQ holds ~70%
//! of contended time" figure is an estimate, not a measurement).
//!
//! This module turns the profiler on for the whole boot and prints a **delta** histogram
//! every [`DUMP_INTERVAL_US`] from the async-main loop. Deltas (not totals) so a window
//! can be attributed to the workload that ran during it rather than to boot noise.
//!
//! Cost — and why this is not in `smp-shared`: with profiling on, every kernel entry
//! stores to a shared per-core tag line, and every contended acquire does an extra atomic
//! add. That is enough to perturb timing-sensitive tests, so the feature is opt-in.

use akuma_exec::sync::{
    contention_spins, set_profiling, wait_by_holder, HOLD_TAG_FAULT, HOLD_TAG_IDLE,
    HOLD_TAG_IRQ, HOLD_TAG_NETPOLL, HOLD_TAG_NETPOLL_DRAIN, HOLD_TAG_NETPOLL_HERD,
    HOLD_TAG_NETPOLL_MAINT, HOLD_TAG_NETPOLL_MEMMON, HOLD_TAG_UNKNOWN,
};
use core::sync::atomic::{AtomicU64, Ordering};

/// Tag-bucket count in `akuma_exec::sync` (0..=499 syscall nrs, 500 fault, 501 IRQ,
/// 502 idle, 503 netpoll, 504-507 netpoll sub-phases, 511 unknown). Kept in sync by the
/// `buckets` assertion in the boot self-test.
const BUCKETS: usize = 512;

/// How often to print a window. Short enough that a single workload step (one bulk
/// transfer, one `cp`) lands in an identifiable window, long enough not to spam.
const DUMP_INTERVAL_US: u64 = 10_000_000;

/// How many tags to print per window, ranked by wait attributed to them.
const TOP_N: usize = 6;

/// Previous-window snapshot of each bucket, so we can print deltas.
static PREV: [AtomicU64; BUCKETS] = [const { AtomicU64::new(0) }; BUCKETS];
static PREV_TOTAL: AtomicU64 = AtomicU64::new(0);
static PREV_PRESERVED: AtomicU64 = AtomicU64::new(0);
static LAST_DUMP_US: AtomicU64 = AtomicU64::new(0);
static WINDOW: AtomicU64 = AtomicU64::new(0);

/// Turn the profiler on. Called once from `kernel_main` before userspace starts.
pub fn init() {
    set_profiling(true);
    crate::safe_print!(
        160,
        "[BKLPROF] enabled: per-tag BKL-hold attribution, {}s windows\n",
        DUMP_INTERVAL_US / 1_000_000
    );
}

/// Human label for a tag bucket. Syscall numbers fall back to `nr<N>` when
/// `syscall_name` has no entry, so an unattributed hot path is still identifiable.
fn tag_label(tag: usize, buf: &mut [u8; 16]) -> &str {
    match tag as u64 {
        HOLD_TAG_FAULT => return "fault",
        HOLD_TAG_IRQ => return "irq/sched",
        HOLD_TAG_IDLE => return "idle",
        HOLD_TAG_NETPOLL => return "netpoll",
        HOLD_TAG_NETPOLL_MAINT => return "netpoll_maint",
        HOLD_TAG_NETPOLL_DRAIN => return "netpoll_drain",
        HOLD_TAG_NETPOLL_MEMMON => return "netpoll_memmon",
        HOLD_TAG_NETPOLL_HERD => return "netpoll_herd",
        HOLD_TAG_UNKNOWN => return "unknown",
        _ => {}
    }
    let name = akuma_exec::process::syscall_name(tag);
    if !name.is_empty() {
        return name;
    }
    // `nr<tag>` without allocating.
    buf[0] = b'n';
    buf[1] = b'r';
    let mut n = tag;
    let mut digits = [0u8; 8];
    let mut d = 0;
    loop {
        digits[d] = b'0' + u8::try_from(n % 10).unwrap_or(0);
        n /= 10;
        d += 1;
        if n == 0 {
            break;
        }
    }
    for (out, digit) in buf[2..2 + d].iter_mut().zip(digits[..d].iter().rev()) {
        *out = *digit;
    }
    core::str::from_utf8(&buf[..2 + d]).unwrap_or("nr?")
}

/// Print one window if the interval has elapsed. Cheap when it hasn't (one atomic load).
///
/// Called from the async-main poll loop, which runs on the BSP with the BKL held — so the
/// snapshot is not atomic across cores. That is fine: the numbers are spin *counts* over a
/// 10 s window, and a few thousand in-flight spins cannot change which tag dominates.
pub fn maybe_dump(now_us: u64) {
    let last = LAST_DUMP_US.load(Ordering::Relaxed);
    if now_us.saturating_sub(last) < DUMP_INTERVAL_US {
        return;
    }
    LAST_DUMP_US.store(now_us, Ordering::Relaxed);
    let window = WINDOW.fetch_add(1, Ordering::Relaxed);

    // Collect this window's per-tag delta, and rank.
    let mut top: [(usize, u64); TOP_N] = [(0, 0); TOP_N];
    let mut window_total: u64 = 0;
    for tag in 0..BUCKETS {
        let now = wait_by_holder(tag);
        let prev = PREV[tag].swap(now, Ordering::Relaxed);
        let delta = now.saturating_sub(prev);
        if delta == 0 {
            continue;
        }
        window_total = window_total.saturating_add(delta);
        // Insertion into the fixed top-N (BUCKETS is 512; a sort would need alloc).
        if delta > top[TOP_N - 1].1 {
            let mut pos = TOP_N - 1;
            while pos > 0 && delta > top[pos - 1].1 {
                top[pos] = top[pos - 1];
                pos -= 1;
            }
            top[pos] = (tag, delta);
        }
    }

    let total_now = contention_spins();
    let total_delta = total_now.saturating_sub(PREV_TOTAL.swap(total_now, Ordering::Relaxed));
    let preserved_now = akuma_exec::bkl::dropped_windows_preserved();
    let preserved_delta =
        preserved_now.saturating_sub(PREV_PRESERVED.swap(preserved_now, Ordering::Relaxed));

    crate::safe_print!(
        200,
        "[BKLPROF] w{} t={}s spins={} attributed={} windows_preserved={}\n",
        window,
        now_us / 1_000_000,
        total_delta,
        window_total,
        preserved_delta
    );
    if window_total == 0 {
        return;
    }
    for &(tag, delta) in &top {
        if delta == 0 {
            continue;
        }
        let mut buf = [0u8; 16];
        let label = tag_label(tag, &mut buf);
        // Percent to one decimal without floats: (delta * 1000) / total.
        let permille = delta.saturating_mul(1000) / window_total;
        crate::safe_print!(
            200,
            "[BKLPROF]   {} tag={} {}.{}% spins={}\n",
            label,
            tag,
            permille / 10,
            permille % 10,
            delta
        );
    }
}
