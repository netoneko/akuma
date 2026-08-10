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

/// Maximum entries. Set once at init from RAM size; 0 until then (cache off).
static CAP_PAGES: AtomicUsize = AtomicUsize::new(0);

pub static HITS: AtomicUsize = AtomicUsize::new(0);
pub static MISSES: AtomicUsize = AtomicUsize::new(0);
pub static EVICTIONS: AtomicUsize = AtomicUsize::new(0);
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
    let cap = (total_ram_bytes / 8) / 4096;
    CAP_PAGES.store(cap, Ordering::Relaxed);
    crate::tprint!(128, "[fpcache] shared file-page cache enabled, cap={} pages\n", cap);
}

/// Is a page mapped with `flags` shareable? True only for mappings that give EL0
/// no write access (`AP_RO_ALL`), i.e. `user_flags::RO` and `user_flags::RX`.
#[inline]
pub fn is_shareable_mapping(map_flags: u64) -> bool {
    const AP_MASK: u64 = 3 << 6;
    crate::config::SHARED_FILE_PAGES_ENABLED
        && (map_flags & AP_MASK) == akuma_exec::mmu::flags::AP_RO_ALL
}

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
        PAGES.lock().get(&(inode, file_off)).copied()
    })?;
    // Take the mapper's reference before returning: the frame must not be freed
    // out from under the caller between here and the install pass.
    crate::pmm::cow_ref_inc(hit.pa);
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

        if pages.len() <= cap {
            return None;
        }
        // Over cap: drop one entry at/after the rotating cursor, wrapping.
        let cursor = EVICT_CURSOR.load(Ordering::Relaxed);
        let victim = pages
            .range((0u32, cursor)..)
            .map(|(k, v)| (*k, *v))
            .find(|(k, _)| *k != (inode, file_off))
            .or_else(|| {
                pages
                    .iter()
                    .map(|(k, v)| (*k, *v))
                    .find(|(k, _)| *k != (inode, file_off))
            });
        let (vk, ve) = victim?;
        pages.remove(&vk);
        EVICT_CURSOR.store(vk.1.wrapping_add(4096), Ordering::Relaxed);
        Some(ve.pa)
    });

    if let Some(pa) = evicted {
        EVICTIONS.fetch_add(1, Ordering::Relaxed);
        // Drop the cache's reference. Frees only if nobody still has it mapped;
        // otherwise the last unmapper frees it through the same path.
        crate::pmm::free_page(PhysFrame::new(pa));
    }
    // The cache's own reference for the entry we just inserted. Combined with the
    // caller's mapping this makes the refcount 2 — see the module invariant.
    crate::pmm::cow_ref_inc(frame.addr);
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
        crate::pmm::free_page(PhysFrame::new(pa));
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
        crate::pmm::free_page(PhysFrame::new(pa));
    }
    n
}

/// One-line summary for the 30 s PSTATS block. Writes into the caller's
/// buffer instead of returning a `String` — this runs on the periodic
/// memory-monitor tick and shouldn't need a healthy heap to report itself.
pub fn stats_line(w: &mut dyn core::fmt::Write) {
    let _ = writeln!(
        w,
        "[FPCACHE] entries={} hits={} misses={} evict={} inval={}",
        len(),
        HITS.load(Ordering::Relaxed),
        MISSES.load(Ordering::Relaxed),
        EVICTIONS.load(Ordering::Relaxed),
        INVALIDATIONS.load(Ordering::Relaxed),
    );
}
