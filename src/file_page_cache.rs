//! Shared physical pages for read-only file-backed mappings.
//!
//! # The problem this solves
//!
//! Before this cache, every file-backed demand fault allocated a **private** PMM
//! frame and copied the file bytes into it (`exceptions.rs`, Pass B). Two
//! processes mapping the same page of the same file therefore held two physical
//! copies, filled by two separate `read_at` calls.
//!
//! That is the mechanism behind "`-j4` is slower than `-j1`" on the self-host
//! build. Four concurrent `rustc`s map the same toolchain — `librustc_driver.so`
//! is 295 MB and `rust-lld` 154 MB — so the *same* text pages were replicated
//! four times in RAM and re-read four times from ext2. Physical memory then runs
//! short, `reclaim_clean_file_pages` evicts clean RO file pages, and every
//! eviction buys a fresh disk read on the next touch. More jobs → more copies →
//! more pressure → more eviction → more I/O, a self-reinforcing thrash loop that
//! `-j1` never enters because one copy of the working set fits.
//!
//! Deduplicating on `(inode, file_offset)` collapses all of it at once: one fill,
//! one frame, one I-cache maintenance pass, however many mappers.
//!
//! # Why reusing the CoW refcount is the whole trick
//!
//! `pmm::free_page` already routes through `cow_ref_dec` and declines to free a
//! frame that still has references. So teardown needs **no** new code: process
//! exit, `munmap`, and `try_evict_ro_page` all free shared frames through their
//! existing paths and simply drop a reference instead.
//!
//! The invariant is `refcount = (1 if cached) + (number of mappings)`:
//!
//! - miss  → fill, map (1 mapping), `insert` does one `cow_ref_inc` → 2 ✓
//! - hit   → `cow_ref_inc` per additional mapper → 3, 4, … ✓
//! - unmap → `free_page` → `cow_ref_dec`; never reaches 0 while cached ✓
//! - evict → drop the entry, then `free_page` for the cache's own reference;
//!   frees only if nobody has it mapped, otherwise the last unmapper frees it ✓
//!
//! # Eligibility (deliberately narrow)
//!
//! A page is shareable only when all of the following hold, because each one
//! rules out a class of correctness bug rather than merely being conservative:
//!
//! - **Mapped read-only to EL0** (`AP_RO_ALL`, i.e. `user_flags::RO`/`RX`).
//!   A writable private file mapping would need copy-on-write before sharing;
//!   ELF data segments carrying relocations stay private.
//! - **Fully covered by file data.** A page straddling `filesz` has a zero-fill
//!   tail whose length depends on the *mapping*, not the file, so two mappers can
//!   legitimately disagree about its contents. One page per segment, not worth it.
//! - **Resolved inode** (`inode != 0`). The path-only fallback has no stable
//!   identity to key on.
//!
//! # Invalidation
//!
//! Stale pages here would be a silent miscompile, not a crash: `rustc` mmaps
//! `.rlib`/`.rmeta` files that `cargo` later rewrites, and ext2 reuses inodes.
//! Every mutating VFS entry point therefore calls [`invalidate_inode`]
//! (`vfs::write_at`, `write_file`, `truncate`, `fallocate`,
//! `remove_file`, `rename`) before the mutation lands.

use akuma_exec::PhysFrame;
use alloc::collections::BTreeMap;
use core::sync::atomic::{AtomicUsize, Ordering};
use spinning_top::Spinlock;

/// One cached page.
#[derive(Clone, Copy)]
struct Entry {
    pa: usize,
    /// Whether this frame has had `dc cvau` + `ic ivau` run over it. A page first
    /// cached from a non-exec RO mapping has not, so a later `RX` mapper must do
    /// the maintenance itself and set this (see [`mark_icache_clean`]).
    icache_done: bool,
}

/// `(inode, page-aligned file offset)` → frame.
///
/// IRQs are masked around every access for the same reason `FUTEX_WAITERS` masks
/// them: this table is reachable from the BKL-free fault window, and a nested IRQ
/// that hard-spins for the BKL while a peer core holds this lock is the AB-BA
/// shape `docs/reference/subsystems/locking.md` warns about.
static PAGES: Spinlock<BTreeMap<(u32, usize), Entry>> = Spinlock::new(BTreeMap::new());

/// Rotating eviction cursor, so successive evictions sweep the keyspace
/// (clock-like) instead of always dropping the numerically lowest inode.
static EVICT_CURSOR: AtomicUsize = AtomicUsize::new(0);

/// Maximum entries **right now**: the base cap, plus the elastic inflation when
/// free RAM can spare it. 0 until [`init`] runs (cache off). Re-derived by
/// [`reassess_cap`]; every reader loads it fresh rather than caching it.
static CAP_PAGES: AtomicUsize = AtomicUsize::new(0);

/// The un-inflated cap, fixed at [`init`] from RAM size. [`reassess_cap`] never
/// takes the effective cap below this — memory pressure below the base is the
/// [`shrink`] hook's job, not the cap's.
static BASE_CAP_PAGES: AtomicUsize = AtomicUsize::new(0);

/// Inserts since the last [`reassess_cap`], so the free-RAM check runs on a
/// coarse cadence instead of per insert. Inserts happen only on a *miss*, so
/// this samples fastest exactly when the cache is under pressure and stops
/// entirely when it is serving hits.
static INSERTS_SINCE_REASSESS: AtomicUsize = AtomicUsize::new(0);

/// Inserts between reassessments. At the ~2.6 K miss/s of a thrashing mmap'd
/// workload that is ~5 checks/s; at a healthy hit rate it is silent.
const REASSESS_EVERY_INSERTS: usize = 512;

pub static HITS: AtomicUsize = AtomicUsize::new(0);
pub static MISSES: AtomicUsize = AtomicUsize::new(0);
pub static EVICTIONS: AtomicUsize = AtomicUsize::new(0);
/// Evictions that had to take a **still-mapped** entry because the bounded scan found
/// no unmapped one. Each costs the next mapper a `read_at` while freeing nothing, so
/// this is the number that matters when the cache thrashes — `evict` alone cannot tell
/// a cheap eviction from an expensive one. A high ratio against `evict` means the cache
/// is genuinely too small for the working set, not merely full.
pub static EVICTIONS_MAPPED: AtomicUsize = AtomicUsize::new(0);
pub static INVALIDATIONS: AtomicUsize = AtomicUsize::new(0);

/// Size the cache from total RAM. Called once from `fs::init`.
///
/// This cache is a *deduplicator*, not an extra consumer: an entry whose frame is
/// still mapped costs nothing beyond the map node, since that frame was going to
/// exist anyway. Only entries with zero mappers hold memory that would otherwise
/// be free, which is why the cap can be generous relative to the ext2 block cache.
pub fn init(total_ram_bytes: usize) {
    if !crate::config::SHARED_FILE_PAGES_ENABLED {
        return;
    }
    let divisor = crate::config::FPCACHE_BASE_RAM_DIVISOR.max(1);
    let base = (total_ram_bytes / divisor) / 4096;
    BASE_CAP_PAGES.store(base, Ordering::Relaxed);
    CAP_PAGES.store(base, Ordering::Relaxed);
    crate::tprint!(
        160,
        "[fpcache] shared file-page cache enabled, base cap={} pages (+{}% elastic)\n",
        base,
        crate::config::FPCACHE_INFLATE_PCT
    );
    // Boot is the cheapest moment there will ever be to grant the inflation, so
    // take the first reading now instead of waiting for 512 misses to accumulate.
    reassess_cap();
}

/// Pages the cap may grow by when RAM allows: [`config::FPCACHE_INFLATE_PCT`] of
/// the base cap.
///
/// [`config::FPCACHE_INFLATE_PCT`]: crate::config::FPCACHE_INFLATE_PCT
fn inflation_pages() -> usize {
    BASE_CAP_PAGES
        .load(Ordering::Relaxed)
        .saturating_mul(crate::config::FPCACHE_INFLATE_PCT)
        / 100
}

/// Re-derive the effective cap from current free RAM.
///
/// Grants the inflation while free RAM is at least
/// [`config::FPCACHE_INFLATE_HEADROOM_MULT`]x the inflation, and withdraws it
/// once free RAM falls below the inflation itself. Those are deliberately two
/// different thresholds: a single one would let a workload parked on the line
/// flip the cap on every call.
///
/// Withdrawing the inflation does **not** evict anything by itself — the
/// over-cap trim in [`insert`] is lazy, so an over-cap cache drains one entry
/// per subsequent insert, preferring unmapped victims exactly as it always has.
/// Acute pressure is still [`shrink`]'s job; this only stops the cache *growing*
/// into memory that someone else now needs.
///
/// Cheap enough to call from anywhere: `pmm::free_count` is two relaxed atomic
/// loads (`akuma_pmm::stats`), no lock, so this is safe inside an IRQ-masked
/// region and cannot participate in a lock cycle.
///
/// [`config::FPCACHE_INFLATE_HEADROOM_MULT`]: crate::config::FPCACHE_INFLATE_HEADROOM_MULT
pub fn reassess_cap() {
    if !crate::config::SHARED_FILE_PAGES_ENABLED {
        return;
    }
    let base = BASE_CAP_PAGES.load(Ordering::Relaxed);
    if base == 0 {
        return;
    }
    let extra = inflation_pages();
    if extra == 0 {
        CAP_PAGES.store(base, Ordering::Relaxed);
        return;
    }
    let free = crate::pmm::free_count();
    // Engaged-ness is derived, not stored: the cap already knows whether the
    // inflation is granted, and a second copy could disagree with it. Grow only
    // with the full headroom free, but hold the growth until free RAM drops
    // below the inflation itself — the band between those two is what stops a
    // workload parked on the line toggling the cap on every reassessment.
    let inflated = CAP_PAGES.load(Ordering::Relaxed) > base;
    let want_inflated = akuma_kacho::hysteresis(
        inflated,
        free as u64,
        extra.saturating_mul(crate::config::FPCACHE_INFLATE_HEADROOM_MULT.max(1)) as u64,
        extra as u64,
    );
    if want_inflated != inflated {
        CAP_PAGES.store(if want_inflated { base + extra } else { base }, Ordering::Relaxed);
    }
}

/// Effective cap in pages (diagnostics / `[FPCACHE]` PSTATS line).
pub fn cap() -> usize {
    CAP_PAGES.load(Ordering::Relaxed)
}

// The eligibility predicate (AP-field test + the `SHARED_FILE_PAGES_ENABLED`
// gate) lives in `akuma_exec::memmath`, where both halves are host-tested — the
// gate reaches it through the injectable `ExecConfig` rather than
// `crate::config`, which is what let the whole function move instead of leaving a
// wrapper behind (docs/archive/TRIM_FAT_EMBARASSING_DUPLICATIONS.md §5.11).
// Re-exported so `file_page_cache::is_shareable_mapping` call sites are unchanged.
pub use akuma_exec::memmath::is_shareable_mapping;

/// Look up a cached page and take a reference for a new mapper.
///
/// On hit the returned frame is already filled and may be mapped immediately —
/// no allocation, no `read_at`. The caller **must** either map it or hand it to
/// `pmm::free_page` (e.g. when it loses the install race), so the reference this
/// takes is always balanced.
///
/// `want_exec` upgrades the I-cache state: a frame first cached as plain RO data
/// has never been through `ic ivau`, so an `RX` mapper needs to do that before
/// executing from it. Returns `(frame, needs_icache_maintenance)`.
pub fn lookup_and_ref(inode: u32, file_off: usize, want_exec: bool) -> Option<(PhysFrame, bool)> {
    if !crate::config::SHARED_FILE_PAGES_ENABLED || inode == 0 {
        return None;
    }
    let hit = crate::irq::with_irqs_disabled(|| {
        let pages = PAGES.lock();
        let hit = pages.get(&(inode, file_off)).copied();
        if let Some(ref h) = hit {
            // Take the mapper's reference while the entry is still present.
            // Every free path (insert-eviction, invalidate_inode, shrink)
            // removes the entry under this same PAGES hold BEFORE dropping the
            // cache's reference, so "entry present" ⇒ the cache still holds a
            // reference ⇒ this inc can never land on a count that already hit
            // zero. Inc'ing after the lock drop raced those paths: dec 1->0
            // freed and poisoned the frame, then the late inc resurrected it
            // and the mapper installed poison as file content (memory.md,
            // "Frame lifecycle" W1). COW_REFCOUNTS is a leaf lock already
            // taken under this hold by insert's eviction scan, so the nesting
            // order is established.
            crate::pmm::cow_ref_inc(h.pa);
        }
        hit
    })?;
    HITS.fetch_add(1, Ordering::Relaxed);
    Some((PhysFrame::new(hit.pa), want_exec && !hit.icache_done))
}

/// Record that `frame` has had I-cache maintenance, so later `RX` mappers can skip it.
pub fn mark_icache_clean(inode: u32, file_off: usize, frame: PhysFrame) {
    if !crate::config::SHARED_FILE_PAGES_ENABLED {
        return;
    }
    crate::irq::with_irqs_disabled(|| {
        if let Some(e) = PAGES.lock().get_mut(&(inode, file_off))
            && e.pa == frame.addr
        {
            e.icache_done = true;
        }
    });
}

/// Publish a freshly filled frame. Takes the cache's own reference.
///
/// Call this only for a page that satisfies every eligibility rule in the module
/// docs; `frame` must be fully filled (and I-cache-maintained if `icache_done`)
/// *before* publishing, since a peer core can map it the instant it lands.
pub fn insert(inode: u32, file_off: usize, frame: PhysFrame, icache_done: bool) {
    if !crate::config::SHARED_FILE_PAGES_ENABLED || inode == 0 {
        return;
    }
    // Re-derive the elastic cap on a coarse cadence. Placed before the read
    // below so a freshly granted inflation takes effect on this very insert,
    // and before the `PAGES` lock so it never lengthens that hold.
    if INSERTS_SINCE_REASSESS.fetch_add(1, Ordering::Relaxed) >= REASSESS_EVERY_INSERTS {
        INSERTS_SINCE_REASSESS.store(0, Ordering::Relaxed);
        reassess_cap();
    }
    let cap = CAP_PAGES.load(Ordering::Relaxed);
    if cap == 0 {
        return;
    }
    MISSES.fetch_add(1, Ordering::Relaxed);

    let evicted = crate::irq::with_irqs_disabled(|| {
        let mut pages = PAGES.lock();
        // Lost the race to a peer filling the same page: keep the existing entry
        // so both mappers converge on one frame, and let the caller's frame stay
        // private (it is already correct, just not shared).
        if pages.contains_key(&(inode, file_off)) {
            return None;
        }
        pages.insert((inode, file_off), Entry { pa: frame.addr, icache_done });
        // The cache's own reference, taken while the entry is being published
        // and only when it actually was. Taking it after this closure left a
        // window where the entry was visible with no cache reference — every
        // mapper could unmap and dec the count to zero, freeing the frame
        // under a live cache entry — and it also ran on the lost-race early
        // return above, leaking one count per race (memory.md, "Frame
        // lifecycle" W2).
        crate::pmm::cow_ref_inc(frame.addr);

        if pages.len() <= cap {
            return None;
        }
        // Over cap: drop one entry at/after the rotating cursor, wrapping.
        //
        // **Prefer an entry nobody maps.** Evicting a *mapped* page costs a future
        // `read_at` for the next mapper while freeing nothing now — the frame survives
        // on its mappers' references — so a full cache that evicts blindly re-reads its
        // own hot set from disk. That is the thrash `shrink` already avoids with the
        // same `cow_ref_get(pa) <= 1` test ("cached with no mappers"); this path was
        // the one place that skipped it, and it is the path a *full* cache takes on
        // every insert. Measured: builds at 86-155 s with `evict=5337`, against 43-44 s
        // with `evict=147` on the same tree.
        //
        // The scan is bounded (`EVICT_SCAN`), because `cow_ref_get` takes the CoW table
        // lock per candidate and this runs inside the `PAGES` hold with IRQs masked. If
        // no unmapped entry turns up within the window, fall back to the old
        // any-entry-at-the-cursor behaviour rather than growing past cap.
        const EVICT_SCAN: usize = 64;
        let cursor = EVICT_CURSOR.load(Ordering::Relaxed);
        let mut unmapped = None;
        let mut fallback = None;
        for (k, v) in pages
            .range((0u32, cursor)..)
            .chain(pages.range(..(0u32, cursor)))
            .map(|(k, v)| (*k, *v))
            .filter(|(k, _)| *k != (inode, file_off))
            .take(EVICT_SCAN)
        {
            if fallback.is_none() {
                fallback = Some((k, v));
            }
            if crate::pmm::cow_ref_get(v.pa) <= 1 {
                unmapped = Some((k, v));
                break;
            }
        }
        let took_mapped = unmapped.is_none();
        let (vk, ve) = unmapped.or(fallback)?;
        pages.remove(&vk);
        EVICT_CURSOR.store(vk.1.wrapping_add(4096), Ordering::Relaxed);
        Some((ve.pa, took_mapped))
    });

    if let Some((pa, took_mapped)) = evicted {
        EVICTIONS.fetch_add(1, Ordering::Relaxed);
        if took_mapped {
            EVICTIONS_MAPPED.fetch_add(1, Ordering::Relaxed);
        }
        // Drop the cache's reference. Frees only if nobody still has it mapped;
        // otherwise the last unmapper frees it through the same path.
        crate::pmm::free_page_at(PhysFrame::new(pa), akuma_pmm::FreeSite::FpcacheEvict);
    }
}

/// Drop every cached page belonging to `inode`.
///
/// Called from the mutating VFS entry points. Cheap when the inode is absent,
/// which is the common case: build outputs are written far more often than the
/// toolchain images that populate this cache are mapped.
pub fn invalidate_inode(inode: u32) {
    if !crate::config::SHARED_FILE_PAGES_ENABLED || inode == 0 {
        return;
    }
    let dropped: alloc::vec::Vec<usize> = crate::irq::with_irqs_disabled(|| {
        let mut pages = PAGES.lock();
        let keys: alloc::vec::Vec<(u32, usize)> = pages
            .range((inode, 0)..=(inode, usize::MAX))
            .map(|(k, _)| *k)
            .collect();
        keys.iter().filter_map(|k| pages.remove(k).map(|e| e.pa)).collect()
    });
    if dropped.is_empty() {
        return;
    }
    INVALIDATIONS.fetch_add(dropped.len(), Ordering::Relaxed);
    for pa in dropped {
        crate::pmm::free_page_at(PhysFrame::new(pa), akuma_pmm::FreeSite::FpcacheInvalidate);
    }
}

/// Number of cached pages (diagnostics / `[FPCACHE]` PSTATS line).
pub fn len() -> usize {
    crate::irq::with_irqs_disabled(|| PAGES.lock().len())
}

/// Release up to `want` *unmapped* cached pages back to the PMM under memory
/// pressure.
///
/// Without this the cache would pin its frames against `reclaim_clean_file_pages`:
/// that sweep frees a process's mapping, but a frame the cache still references
/// survives, so reclaim would report progress and free nothing. Entries whose
/// frame is still mapped elsewhere (refcount > 2) are skipped — evicting those
/// costs a future re-read without freeing anything now.
pub fn shrink(want: usize) -> usize {
    if !crate::config::SHARED_FILE_PAGES_ENABLED || want == 0 {
        return 0;
    }
    let dropped: alloc::vec::Vec<usize> = crate::irq::with_irqs_disabled(|| {
        let mut pages = PAGES.lock();
        let victims: alloc::vec::Vec<(u32, usize)> = pages
            .iter()
            // refcount 1 == cached with no mappers: freeing it actually returns memory.
            .filter(|(_, e)| crate::pmm::cow_ref_get(e.pa) <= 1)
            .map(|(k, _)| *k)
            .take(want)
            .collect();
        victims.iter().filter_map(|k| pages.remove(k).map(|e| e.pa)).collect()
    });
    let n = dropped.len();
    EVICTIONS.fetch_add(n, Ordering::Relaxed);
    for pa in dropped {
        crate::pmm::free_page_at(PhysFrame::new(pa), akuma_pmm::FreeSite::FpcacheEvict);
    }
    n
}

/// One-line summary for the 30 s PSTATS block. Writes into the caller's
/// buffer instead of returning a `String` — this runs on the periodic
/// memory-monitor tick and shouldn't need a healthy heap to report itself.
pub fn stats_line(w: &mut dyn core::fmt::Write) {
    let _ = writeln!(
        w,
        "[FPCACHE] entries={}/{} hits={} misses={} evict={} evict_mapped={} inval={}",
        len(),
        cap(),
        HITS.load(Ordering::Relaxed),
        MISSES.load(Ordering::Relaxed),
        EVICTIONS.load(Ordering::Relaxed),
        EVICTIONS_MAPPED.load(Ordering::Relaxed),
        INVALIDATIONS.load(Ordering::Relaxed),
    );
}
