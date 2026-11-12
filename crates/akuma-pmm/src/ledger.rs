//! The free and CoW-event ledgers: rings of recent frees and reference events,
//! read back by the premature-free and use-after-free reports. Compiled only with
//! `pmm-forensics`; without it the crate's stubs record nothing.

use core::sync::atomic::{AtomicUsize, Ordering};

use alloc::vec::Vec;

use super::{config, FreeSite, PAGE_SIZE};
use spinning_top::Spinlock;

const FREE_LEDGER_SLOTS: usize = 4096;

static FREE_LEDGER_PA: [AtomicUsize; FREE_LEDGER_SLOTS] =
    [const { AtomicUsize::new(0) }; FREE_LEDGER_SLOTS];
/// `tid << 32 | seq` for the matching `FREE_LEDGER_PA` slot.
static FREE_LEDGER_META: [AtomicUsize; FREE_LEDGER_SLOTS] =
    [const { AtomicUsize::new(0) }; FREE_LEDGER_SLOTS];
static FREE_LEDGER_NEXT: AtomicUsize = AtomicUsize::new(0);

impl FreeSite {
    const fn from_bits(b: u8) -> Self {
        match b {
            1 => Self::Munmap,
            2 => Self::AsTeardown,
            3 => Self::CowBreak,
            4 => Self::FaultRaceLost,
            5 => Self::FaultPoolSurplus,
            6 => Self::FpcacheEvict,
            7 => Self::FpcacheInvalidate,
            8 => Self::MadviseDontneed,
            9 => Self::Mremap,
            10 => Self::SurplusRef,
            11 => Self::MunmapRegion,
            12 => Self::MunmapPartial,
            13 => Self::MunmapSpan,
            _ => Self::Unknown,
        }
    }
}

/// Note that `pa` was returned to the PMM by thread `tid`, from `site`. Lock-free
/// and IRQ-safe: two relaxed stores and a fetch_add.
///
/// `META` packs `site << 48 | tid << 32 | seq`. `tid` is bounded by `MAX_THREADS`
/// (≤ 256), so the 16 bits above it were dead space and the site rides for free —
/// no extra array, no extra store on the free path.
pub fn record_free_at(pa: usize, tid: u32, site: FreeSite) {
    let seq = FREE_LEDGER_NEXT.fetch_add(1, Ordering::Relaxed);
    let idx = seq & (FREE_LEDGER_SLOTS - 1);
    FREE_LEDGER_META[idx].store(
        ((site as usize) << 48) | ((tid as usize & 0xFFFF) << 32) | (seq & 0xFFFF_FFFF),
        Ordering::Relaxed,
    );
    FREE_LEDGER_PA[idx].store(pa, Ordering::Release);
}

/// Most recent ledger entry for `pa`, as `(tid, seq)` — `None` if this frame has
/// not been freed inside the ring's window.
#[must_use]
pub fn last_free_record(pa: usize) -> Option<(u32, u32)> {
    last_free_record_at(pa).map(|(tid, seq, _)| (tid, seq))
}

/// As [`last_free_record`], plus **which code path** did the freeing.
#[must_use]
pub fn last_free_record_at(pa: usize) -> Option<(u32, u32, FreeSite)> {
    let mut best: Option<(u32, u32, FreeSite)> = None;
    for i in 0..FREE_LEDGER_SLOTS {
        if FREE_LEDGER_PA[i].load(Ordering::Acquire) != pa {
            continue;
        }
        let meta = FREE_LEDGER_META[i].load(Ordering::Relaxed);
        let tid = ((meta >> 32) & 0xFFFF) as u32;
        let seq = (meta & 0xFFFF_FFFF) as u32;
        let site = FreeSite::from_bits(((meta >> 48) & 0xFF) as u8);
        if best.is_none_or(|(_, prev, _)| seq.wrapping_sub(prev) < u32::MAX / 2) {
            best = Some((tid, seq, site));
        }
    }
    best
}

/// Sequence number the next free will be stamped with.
#[must_use]
pub fn free_ledger_seq() -> u32 {
    (FREE_LEDGER_NEXT.load(Ordering::Relaxed) & 0xFFFF_FFFF) as u32
}

// ============================================================================
// CoW/share refcount EVENT ledger — NOT the refcount table itself (that is
// `COW_REFCOUNTS`, staying in `src/pmm.rs` until Step 3). This records every
// inc/dec event so an anomaly report can print a frame's whole reference
// history; the table it is recording events *about* is a separate concern.
// ============================================================================

static COW_EVER: Spinlock<Vec<u64>> = Spinlock::new(Vec::new());
static COW_EVER_BASE: AtomicUsize = AtomicUsize::new(0);

fn cow_ever_index(pa: usize, len_words: usize) -> Option<usize> {
    let base = COW_EVER_BASE.load(Ordering::Relaxed);
    let idx = pa.checked_sub(base)? / PAGE_SIZE;
    (idx / 64 < len_words).then_some(idx)
}

fn cow_ever_mark(pa: usize) {
    akuma_primitives::irq::with_irqs_disabled(|| {
        let mut bits = COW_EVER.lock();
        let Some(idx) = cow_ever_index(pa, bits.len()) else { return };
        bits[idx / 64] |= 1u64 << (idx % 64);
    });
}

/// Has `pa` ever taken part in a reference event? `None` when the instrument is
/// off.
#[must_use]
pub fn cow_ever_touched(pa: usize) -> Option<bool> {
    akuma_primitives::irq::with_irqs_disabled(|| {
        let bits = COW_EVER.lock();
        if bits.is_empty() {
            return None;
        }
        let Some(idx) = cow_ever_index(pa, bits.len()) else { return Some(false) };
        Some(bits[idx / 64] & (1u64 << (idx % 64)) != 0)
    })
}

const COW_LEDGER_SLOTS: usize = 4096;
static COW_LEDGER_PA: [AtomicUsize; COW_LEDGER_SLOTS] =
    [const { AtomicUsize::new(0) }; COW_LEDGER_SLOTS];
/// `tid << 32 | op << 24 | before << 12 | after` (op: 0 = inc, 1 = dec).
static COW_LEDGER_META: [AtomicUsize; COW_LEDGER_SLOTS] =
    [const { AtomicUsize::new(0) }; COW_LEDGER_SLOTS];
static COW_LEDGER_SEQ: [AtomicUsize; COW_LEDGER_SLOTS] =
    [const { AtomicUsize::new(0) }; COW_LEDGER_SLOTS];
static COW_LEDGER_NEXT: AtomicUsize = AtomicUsize::new(0);

/// Called by `src/pmm.rs`'s still-local `cow_ref_inc`/`cow_ref_dec` after they
/// update `COW_REFCOUNTS`, so the event ledger stays populated across the Step
/// 2/3 boundary with zero behaviour change.
pub fn cow_ledger_record(pa: usize, is_dec: bool, before: u16, after: u16) {
    if !config().cow_ref_ledger {
        return;
    }
    cow_ever_mark(pa);
    let seq = COW_LEDGER_NEXT.fetch_add(1, Ordering::Relaxed);
    let idx = seq & (COW_LEDGER_SLOTS - 1);
    let tid = akuma_primitives::preempt::current_tid();
    let meta = (tid << 32)
        | (usize::from(is_dec) << 24)
        | (((before as usize) & 0xFFF) << 12)
        | ((after as usize) & 0xFFF);
    COW_LEDGER_SEQ[idx].store(seq, Ordering::Relaxed);
    COW_LEDGER_META[idx].store(meta, Ordering::Relaxed);
    COW_LEDGER_PA[idx].store(pa, Ordering::Release);
}

// No `#[cfg(kernel_tests)]` here: that cfg is emitted only by the kernel
// binary's own `build.rs` (`src/`), unreachable from this crate. The
// kernel-test-suite-only call site (`crates/akuma-kernel-tests-aarch64/src/process_tests.rs` via
// `crate::pmm::cow_event_count`) is gated in `src/pmm.rs`'s thin wrapper
// instead; this stays always-compiled and relies on LTO to strip it from
// builds where that wrapper compiles out (`no-tests`).
pub fn cow_event_count(pa: usize) -> usize {
    if !config().cow_ref_ledger {
        return 0;
    }
    (0..COW_LEDGER_SLOTS)
        .filter(|&i| COW_LEDGER_PA[i].load(Ordering::Acquire) == pa)
        .count()
}

/// Print every recorded reference event for `pa`, oldest first (up to 12).
pub fn print_cow_history(pa: usize) {
    if !config().cow_ref_ledger {
        return;
    }
    let mut printed = 0usize;
    let mut last_seq: Option<usize> = None;
    while printed < 12 {
        let mut best: Option<(usize, usize)> = None;
        for i in 0..COW_LEDGER_SLOTS {
            if COW_LEDGER_PA[i].load(Ordering::Acquire) != pa {
                continue;
            }
            let seq = COW_LEDGER_SEQ[i].load(Ordering::Relaxed);
            if last_seq.is_some_and(|l| seq <= l) {
                continue;
            }
            if best.is_none_or(|(bs, _)| seq < bs) {
                best = Some((seq, i));
            }
        }
        let Some((seq, idx)) = best else { break };
        let meta = COW_LEDGER_META[idx].load(Ordering::Relaxed);
        akuma_primitives::safe_print!(160, "  [COW-HIST] pa={:#x} seq={} tid={} {} {}->{}\n",
            pa, seq, meta >> 32,
            if (meta >> 24) & 1 == 1 { "dec" } else { "inc" },
            (meta >> 12) & 0xFFF, meta & 0xFFF);
        last_seq = Some(seq);
        printed += 1;
    }
    if printed == 0 {
        let verdict = match cow_ever_touched(pa) {
            Some(false) => "NEVER shared (durable bitset clear)",
            Some(true) => "shared at some point (frame, not necessarily this owner)",
            None => "instrument off — says nothing",
        };
        akuma_primitives::safe_print!(160, "  [COW-HIST] pa={:#x} no events in window: {}\n", pa, verdict);
    }
}


/// Size the "ever shared" bitset for an arena of `total_pages` from `base`.
pub fn init(base: usize, total_pages: usize) {
    if config().cow_ref_ledger {
        COW_EVER_BASE.store(base, Ordering::Release);
        *COW_EVER.lock() = alloc::vec![0u64; total_pages.div_ceil(64)];
    }
}
