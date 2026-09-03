#![no_std]
// The canary reads/writes place a `u64` at an 8-byte-aligned offset either side
// of the user pointer, which clippy sees only as `*mut u8 -> *mut u64`. The bin
// crate carried the same allow, with the same justification, before this file
// moved; the casts are unchanged.
#![allow(clippy::cast_ptr_alignment)]
//! Kernel memory allocator — Talc with on-demand PMM growth.
//!
//! The heap is seeded with a small bootstrap arena (~1 MB) and grows on
//! demand by claiming contiguous pages from the PMM once it is ready.
//!
//! Debug features:
//! - ENABLE_ALLOCATION_REGISTRY: Track all allocations to detect overlaps, double frees
//! - ENABLE_CANARIES: Add guard bytes around allocations to detect overflows
//!
//! # Why this is a crate
//!
//! Moved out of `src/allocator.rs` on 2026-08-31. **This crate cannot
//! `forbid(unsafe_code)` and is not meant to** — it holds 18 `unsafe` sites, and
//! that is the point: they are trusted-but-difficult operations (raw span
//! claiming, canary reads/writes either side of a user pointer, the `GlobalAlloc`
//! impl itself) and concentrating them in a named crate is what lets the rest of
//! the kernel trend safe. Quarantine, not elimination.
//!
//! # What it deliberately does NOT depend on
//!
//! `akuma-primitives`, `akuma-pmm`, `talc`, `spinning_top` — and nothing else.
//! In particular **not `akuma-exec`** and **not the syscall layer**, even though
//! the pre-move file reached into both. An allocator that depends on the syscall
//! layer is upside down: the syscall layer allocates.
//!
//! None of those five call sites was part of allocating. They were OOM *policy*
//! and *diagnostics*, and each went where it belonged rather than being routed
//! back through a hook:
//!
//! | was | now |
//! |---|---|
//! | `#[alloc_error_handler]` calling `current_process_shared` + `return_to_kernel` | the handler lives in `src/main.rs`. It is a **binary-level declaration** and its body is OOM policy — "kill the process, not the kernel" — which is the bin's business, not the heap's |
//! | `#[global_allocator]` | likewise `src/main.rs`. This crate exports [`KernelAllocator`]; the bin installs it |
//! | `syscall_counters::dump()` on allocation failure | moved into that handler. Returning null from `alloc` reaches it immediately anyway, so the dump lost nothing by moving to where whole-kernel diagnostics belong |
//! | `current_syscall_nr()` + `current_thread_id()` on the `[HEAP]` line | **deleted.** Attribution on a 5 MB-boundary progress print did not justify an allocator knowing about syscalls or threads; the line still reports the size that drove the growth |
//!
//! An earlier cut of this crate kept all four as `OnceCopy` hooks. That was
//! backwards — it preserved the inverted dependency and added a registration
//! step to do it. Three of the four simply belong in the bin, and the fourth was
//! not worth keeping at all.
//!
//! `phys_to_virt` came from `akuma_exec::mmu` and now comes from
//! `akuma_primitives::addr`, which is where it actually lives (`akuma_exec`
//! re-exports it).
//!
//! # No `build.rs`, on purpose
//!
//! The pre-move file had exactly one cfg, `#[cfg(kernel_tests)]` on
//! `allocated_bytes()`. A crate only sees a cfg its **own** build script emits —
//! `akuma-exec` once shipped a whole family of dormant `kernel_profile_extreme`
//! gates for want of one. Rather than add a build script and a forwarded
//! `no-tests` feature for a single three-line atomic read, the gate is gone: both
//! callers are themselves in `kernel_tests`-gated modules, so the cfg guarded
//! nothing, and LTO drops the function when nothing calls it. Adding a cfg to
//! this crate later means adding a `build.rs` first.

extern crate alloc;

use core::alloc::{GlobalAlloc, Layout};
use core::ptr;
use core::sync::atomic::{AtomicUsize, AtomicBool, Ordering};
use spinning_top::Spinlock;
use talc::{Span, Talc};

use akuma_primitives::addr::phys_to_virt;
use akuma_primitives::irq::with_irqs_disabled;
use akuma_primitives::safe_print;


/// Enable allocation registry for debugging heap corruption.
///
/// This tracks all allocations and detects overlaps, double frees, and invalid frees
/// WARNING: Canaries break virtio-drivers which does address comparisons on DMA buffers
/// WARNING: Registry causes performance issues - iterates 4096 entries per alloc
pub const ENABLE_ALLOCATION_REGISTRY: bool = false;

/// Enable canary bytes around allocations (requires `ENABLE_ALLOCATION_REGISTRY`).
///
/// Adds 8 bytes before and after each allocation with magic values
/// WARNING: This breaks virtio-drivers! Only enable for targeted debugging.
pub const ENABLE_CANARIES: bool = false;

/// Canary magic values
const CANARY_BEFORE: u64 = 0xDEAD_BEEF_CAFE_BABE;
const CANARY_AFTER: u64 = 0xFEED_FACE_DEAD_C0DE;
const CANARY_SIZE: usize = 8;

const PAGE_SIZE: usize = 4096;

/// Flag indicating PMM is ready — the OOM handler checks this before growing.
static PMM_READY: AtomicBool = AtomicBool::new(false);

pub fn mark_pmm_ready() {
    PMM_READY.store(true, Ordering::Release);
}

fn is_pmm_ready() -> bool {
    PMM_READY.load(Ordering::Acquire)
}

// ============================================================================
// PMM-backed OOM handler — grows the Talc arena on demand
// ============================================================================

struct PmmOomHandler;

impl talc::OomHandler for PmmOomHandler {
    fn handle_oom(talc: &mut Talc<Self>, layout: Layout) -> Result<(), ()> {
        if !is_pmm_ready() {
            return Err(());
        }
        // Grow by at least 256 KB (64 pages) to amortise per-OOM overhead —
        // EXCEPT when the PMM is critically low (a process is exhausting RAM).
        // Then grow by just what's needed, so the kernel heap can still satisfy
        // small allocations from the thin `USER_PAGE_RESERVE` pool. This is what
        // keeps the OOM process-kill path able to allocate instead of the kernel
        // itself failing to grow the heap and aborting.
        // talc keeps a little per-span metadata at each claimed span, so a span
        // of exactly `pages_for_layout` pages can NOT hold a `pages_for_layout`-page
        // allocation — the request falls a few bytes short, talc re-invokes
        // handle_oom, and we claim another just-too-small span … forever. That is
        // the 4 GB heap runaway seen under llama's recurring 256 KB reads
        // (`[HEAP-GROW] this_req=262144 claimed=64 pages`, used stuck at 1 MB).
        // Claim `HEAP_GROW_HEADROOM_PAGES` extra so the allocation fits and the
        // freed span is reusable for the next same-size request.
        let pages_for_layout = layout.size().div_ceil(PAGE_SIZE);
        let needed = pages_for_layout + HEAP_GROW_HEADROOM_PAGES;
        let mut n = heap_grow_initial_pages(needed, akuma_pmm::free_count());

        // The kernel heap lives in the linear (`phys_to_virt`) map, so a heap
        // span must be *physically* contiguous. On a fragmented small-RAM pool
        // the amortised `n`-page run can fail to exist even though plenty of
        // single pages are free — e.g. 2.6M tiny churning network-buffer allocs
        // leave the PMM bitmap a checkerboard with 100+ free pages but no long
        // run. Historically that made `handle_oom` return `Err`, which turns a
        // *satisfiable* allocation into a whole-kernel `brk #1` abort (the
        // EC=0x3c crash seen at the 4 MB meow+tcc floor). Instead, back off the
        // run length toward `needed`: any layout that fits in one page
        // (`needed == 1`, the dominant case) is then guaranteed to grow as long
        // as *one* page is free; larger layouts get the largest run we can still
        // form. Only a genuine multi-page-contiguous shortfall (true
        // fragmentation OOM) falls through to `Err` — that case is the OOM
        // killer's job (see docs/LOW_MEMORY_ENVIRONMENT.md).
        //
        // NB: `alloc_pages_contiguous_zeroed` may try `reclaim_to_pmm()` on
        // failure, which `TALC.try_lock()`s — that lock is held by us right now
        // (we were called from inside `malloc`), so the try_lock fails and the
        // reclaim is a no-op. No deadlock, no benefit; just don't rely on it here.
        loop {
            if let Some(pa) = akuma_pmm::alloc_pages_contiguous_zeroed(n) {
                let ptr = phys_to_virt(pa).cast::<u8>();
                let span = Span::from_base_size(ptr, n * PAGE_SIZE);
                return if let Ok(_heap) = unsafe { talc.claim(span) } {
                    // Record the PMM-backed span so `reclaim_to_pmm()` can
                    // return it later once it is fully free. If the registry
                    // is full the span is still used as heap — it just
                    // becomes non-reclaimable (the pre-reclaim one-way
                    // behaviour).
                    register_claimed_span(pa, n);
                    let prev = HEAP_SIZE.fetch_add(n * PAGE_SIZE, Ordering::Relaxed);
                    let now = prev + n * PAGE_SIZE;
                    // Leak-debug: log the request driving growth each time the
                    // heap crosses a 256 MB boundary, so a runaway grow is
                    // attributable to a specific allocation size. safe_print
                    // is alloc-free (used by the alloc error handler too).
                    const STEP: usize = 256 * 1024 * 1024;
                    if prev / STEP != now / STEP {
                        safe_print!(160,
                            "[HEAP-GROW] total={}MB used={}MB this_req={} bytes claimed={} pages\n",
                            now / 1024 / 1024,
                            ALLOCATED_BYTES.load(Ordering::Relaxed) / 1024 / 1024,
                            layout.size(), n);
                    }
                    Ok(())
                } else {
                    // Couldn't establish a heap in the pages — return them to
                    // PMM rather than leaking (old code dropped them).
                    akuma_pmm::free_pages_contiguous(pa, n);
                    Err(())
                };
            }
            match heap_grow_backoff(n, needed) {
                Some(next) => n = next,
                // Can't even form the minimum contiguous span the layout needs:
                // genuine fragmentation/exhaustion OOM. Returning Err here aborts
                // the kernel today; the OOM killer will hook in at this point.
                None => return Err(()),
            }
        }
    }
}

/// Amortisation granularity for kernel-heap growth: claim 256 KB (64 pages) per
/// OOM event when memory is ample, to spread the per-claim cost over many small
/// allocations.
pub const HEAP_GROW_PAGES: usize = 64;

/// Extra pages claimed above what a layout strictly needs, to cover talc's
/// per-claimed-span metadata.
///
/// Without this, an allocation whose size is an exact
/// multiple of the page size (e.g. a recurring 256 KB / 64-page request) never
/// fits in a span of exactly that many pages: handle_oom claims a just-too-small
/// span, talc re-fails, and the heap grows without bound until the PMM is drained
/// and the kernel aborts (`brk #1`). talc's overhead is a handful of tag words —
/// well under one page — so 2 pages is ample headroom. See docs/LLAMA_MMAP_OOM_KERNEL_ABORT.md.
pub const HEAP_GROW_HEADROOM_PAGES: usize = 2;

/// Initial contiguous-page request for a heap growth.
///
/// Must satisfy a layout needing `needed` pages, given `free` PMM pages remain.
/// Amortise to
/// [`HEAP_GROW_PAGES`] when memory is ample; shrink to exactly `needed` under
/// pressure (`free <= 2 * HEAP_GROW_PAGES`) so the thin `USER_PAGE_RESERVE` pool
/// is preserved for the OOM-kill bookkeeping path. Pure fn over its inputs so the
/// boundary is unit-testable without draining real RAM.
#[inline]
#[must_use]
pub fn heap_grow_initial_pages(needed: usize, free: usize) -> usize {
    if free <= 2 * HEAP_GROW_PAGES {
        needed
    } else {
        needed.max(HEAP_GROW_PAGES)
    }
}

/// Next contiguous-page request after a run of `n` pages failed to allocate.
///
/// For a layout needing at least `needed` pages. Halves toward `needed` so a
/// fragmented pool that can't yield the amortised run can still back off to the
/// minimum the layout requires (and, when `needed == 1`, to a single page —
/// satisfiable whenever any page is free). Returns `None` once `needed` itself
/// has been tried, i.e. genuine multi-page-contiguous OOM. Pure + monotonically
/// decreasing, so the `handle_oom` loop is guaranteed to terminate.
#[inline]
#[must_use]
pub fn heap_grow_backoff(n: usize, needed: usize) -> Option<usize> {
    if n <= needed {
        None
    } else {
        Some((n / 2).max(needed))
    }
}

// ============================================================================
// Heap → PMM reclaim
// ============================================================================
//
// `handle_oom` grows the kernel heap by claiming contiguous pages from the PMM.
// Talc never returns those pages on its own, so on a small machine the heap's
// high-water mark is permanent: after a memory-hungry process (tcc, meow) exits,
// its kernel-side allocations are freed back into Talc's free list, but the
// underlying PMM pages stay committed to the heap. The free PMM pool ratchets
// down until the next spawn / demand page-fault gets "0 free pages".
//
// `reclaim_to_pmm()` walks the recorded PMM-backed spans and, for each one that
// is now *entirely* free inside Talc, truncates it out of the heap and returns
// the pages to the PMM. It is called:
//   * from `pmm::alloc_*` on allocation failure (reclaim-under-pressure, the
//     path that lets a single tcc compile fit at 8 MB), and
//   * periodically from the memory monitor + on process reap (so back-to-back
//     runs start from a clean pool).

/// Max number of PMM-backed heap spans we track for reclaim. At the 256 KB grow
/// granularity this covers 128 MB of heap growth, far beyond any small-RAM
/// target. Overflow degrades to non-reclaimable (safe). Kept small because the
/// array is static BSS and the size-profile kernel has a tight image reserve.
const MAX_CLAIMED_SPANS: usize = 512;

/// A heap region claimed from the PMM by `handle_oom`. We always claim
/// page-aligned, page-multiple spans, and Talc's `claim()` word-aligns inward
/// (a no-op for page alignment), so the Talc heap extent is exactly
/// `[phys_to_virt(pmm_addr), +pages*PAGE_SIZE)` — no need to store it. `pages
/// == 0` marks a free slot.
#[derive(Clone, Copy)]
struct ClaimedSpan {
    pmm_addr: usize,
    pages: usize,
}

impl ClaimedSpan {
    const fn empty() -> Self {
        Self { pmm_addr: 0, pages: 0 }
    }
    fn heap_span(&self) -> Span {
        let base = phys_to_virt(self.pmm_addr).cast::<u8>();
        Span::from_base_size(base, self.pages * PAGE_SIZE)
    }
}

static CLAIMED_SPANS: Spinlock<[ClaimedSpan; MAX_CLAIMED_SPANS]> =
    Spinlock::new([ClaimedSpan::empty(); MAX_CLAIMED_SPANS]);
/// Running total of pages handed back to the PMM (for the `[Mem]` stats line).
static RECLAIMED_PAGES_TOTAL: AtomicUsize = AtomicUsize::new(0);

/// Record a heap span claimed from the PMM. Called from `handle_oom` with the
/// `TALC` lock held → lock order is always TALC → CLAIMED_SPANS, matching
/// `reclaim_to_pmm()`, so the two never deadlock.
fn register_claimed_span(pmm_addr: usize, pages: usize) {
    let mut spans = CLAIMED_SPANS.lock();
    for s in spans.iter_mut() {
        if s.pages == 0 {
            *s = ClaimedSpan { pmm_addr, pages };
            return;
        }
    }
    // Registry full: leave the span claimed but untracked (non-reclaimable).
}

/// Return fully-free PMM-backed heap spans to the physical allocator.
///
/// Returns the number of pages reclaimed. Safe to call from any non-allocator
/// context; if the `TALC` lock is held (e.g. we are reentered from inside
/// `handle_oom`) it bails immediately via `try_lock`.
pub fn reclaim_to_pmm() -> usize {
    if !is_pmm_ready() {
        return 0;
    }
    let mut reclaimed_pages = 0usize;
    // Free one span per lock cycle: keep the TALC/CLAIMED critical section tiny
    // and release both locks before touching the PMM (lock order TALC → PMM is
    // what `handle_oom` uses; we never invert it).
    for _ in 0..MAX_CLAIMED_SPANS {
        let to_free = with_irqs_disabled(|| {
            // try_lock, not lock: if TALC is held we were reentered from the
            // allocator itself — bail rather than self-deadlock on the spinlock.
            let mut talc = match TALC.try_lock() {
                Some(t) => t,
                None => return None,
            };
            let mut spans = CLAIMED_SPANS.lock();
            for s in spans.iter_mut() {
                if s.pages == 0 {
                    continue;
                }
                let heap = s.heap_span();
                // get_allocated_span + truncate must be atomic w.r.t. other
                // allocations; we hold TALC across both, so they are.
                let allocated = unsafe { talc.get_allocated_span(heap) };
                if allocated.is_empty() {
                    unsafe { talc.truncate(heap, Span::empty()); }
                    let result = (s.pmm_addr, s.pages);
                    *s = ClaimedSpan::empty();
                    return Some(result);
                }
            }
            None
        });
        match to_free {
            Some((addr, pages)) => {
                akuma_pmm::free_pages_contiguous(addr, pages);
                HEAP_SIZE.fetch_sub(pages * PAGE_SIZE, Ordering::Relaxed);
                reclaimed_pages += pages;
            }
            None => break,
        }
    }
    if reclaimed_pages > 0 {
        RECLAIMED_PAGES_TOTAL.fetch_add(reclaimed_pages, Ordering::Relaxed);
    }
    reclaimed_pages
}

/// Total pages returned to the PMM since boot (for stats / tests).
pub fn reclaimed_pages_total() -> usize {
    RECLAIMED_PAGES_TOTAL.load(Ordering::Relaxed)
}

/// Occupancy snapshot of the PMM-backed heap spans — the data needed to tell a
/// genuine frame leak apart from the kernel-heap high-water mark.
///
/// `reclaim_to_pmm()` can only return a claimed span once it is *entirely* free
/// inside Talc; a single surviving allocation pins the whole 256 KB span. After
/// a workload exits, the leak you observe as "free PMM never recovered" is
/// almost always this: many spans pinned by a few bytes each. This report makes
/// that visible — `pinned_spans` * span size is the committed-but-stuck pool,
/// and `pinned_used_bytes` is how little is actually keeping it hostage.
#[derive(Debug, Clone, Copy, Default)]
pub struct SpanReport {
    /// Claimed spans currently tracked in the registry.
    pub live_spans: usize,
    /// Total PMM pages committed to the heap via claims (== current heap growth).
    pub committed_pages: usize,
    /// Spans that are NOT fully free in Talc → cannot be reclaimed right now.
    pub pinned_spans: usize,
    /// Pages locked up in pinned spans (the recoverable-once-drained pool).
    pub pinned_pages: usize,
    /// Bounding extent of live allocations inside pinned spans, in bytes — the
    /// "fragmentation tax": how few live bytes are holding `pinned_pages` hostage.
    pub pinned_used_bytes: usize,
    /// Spans fully free right now (reclaim_to_pmm would return these immediately).
    pub free_spans: usize,
    /// True if the report could not be taken because Talc was locked (reentrant
    /// from the allocator) — all other fields are then meaningless.
    pub busy: bool,
}

/// Take a [`SpanReport`].
///
/// Safe from any non-allocator context; if Talc is held
/// (we were reentered from `handle_oom`) it returns `busy = true` rather than
/// deadlocking, matching `reclaim_to_pmm`'s `try_lock` discipline.
#[must_use]
pub fn claimed_span_report() -> SpanReport {
    if !is_pmm_ready() {
        return SpanReport::default();
    }
    with_irqs_disabled(|| {
        let talc = match TALC.try_lock() {
            Some(t) => t,
            None => return SpanReport { busy: true, ..SpanReport::default() },
        };
        let spans = CLAIMED_SPANS.lock();
        let mut r = SpanReport::default();
        for s in spans.iter() {
            if s.pages == 0 {
                continue;
            }
            r.live_spans += 1;
            r.committed_pages += s.pages;
            let heap = s.heap_span();
            // Same primitive reclaim_to_pmm uses to decide reclaimability.
            let allocated = unsafe { talc.get_allocated_span(heap) };
            if allocated.is_empty() {
                r.free_spans += 1;
            } else {
                r.pinned_spans += 1;
                r.pinned_pages += s.pages;
                r.pinned_used_bytes += allocated.size();
            }
        }
        r
    })
}

// ============================================================================
// Allocation Registry - tracks all allocations to detect corruption
// ============================================================================

/// Maximum number of allocations to track
const REGISTRY_SIZE: usize = 4096;

/// Record of a single allocation
#[derive(Clone, Copy)]
struct AllocationRecord {
    /// Start address (user-visible, after canary if enabled)
    addr: usize,
    /// Size of allocation (user-visible, without canaries)
    size: usize,
    /// True if this slot is in use
    active: bool,
}

impl AllocationRecord {
    const fn empty() -> Self {
        Self {
            addr: 0,
            size: 0,
            active: false,
        }
    }
}

/// The allocation registry
static ALLOCATION_REGISTRY: Spinlock<[AllocationRecord; REGISTRY_SIZE]> = 
    Spinlock::new([AllocationRecord::empty(); REGISTRY_SIZE]);

/// Count of registry slots in use
static REGISTRY_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Count of detected issues
static OVERLAP_COUNT: AtomicUsize = AtomicUsize::new(0);
static DOUBLE_FREE_COUNT: AtomicUsize = AtomicUsize::new(0);
static INVALID_FREE_COUNT: AtomicUsize = AtomicUsize::new(0);
static CANARY_CORRUPTION_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Check if two ranges overlap
fn ranges_overlap(start1: usize, size1: usize, start2: usize, size2: usize) -> bool {
    if size1 == 0 || size2 == 0 {
        return false;
    }
    let end1 = start1.saturating_add(size1);
    let end2 = start2.saturating_add(size2);
    start1 < end2 && start2 < end1
}

/// Register a new allocation, checking for overlaps
/// Returns true if OK, false if overlap detected (allocation still registered)
fn registry_add(addr: usize, size: usize) -> bool {
    if !ENABLE_ALLOCATION_REGISTRY || size == 0 {
        return true;
    }

    let mut registry = ALLOCATION_REGISTRY.lock();
    let mut overlap_found = false;

    // Check for overlaps with existing allocations
    for record in registry.iter() {
        if record.active && ranges_overlap(addr, size, record.addr, record.size) {
            // Found an overlap!
            OVERLAP_COUNT.fetch_add(1, Ordering::Relaxed);
            akuma_primitives::console::print_str("[ALLOC] OVERLAP DETECTED!\n");
            safe_print!(
                80,
                "  New: 0x{:x}-0x{:x} (size={})\n",
                addr,
                addr + size,
                size
            );
            safe_print!(
                80,
                "  Existing: 0x{:x}-0x{:x} (size={})\n",
                record.addr,
                record.addr + record.size,
                record.size
            );
            overlap_found = true;
        }
    }

    // Find empty slot and register
    for record in registry.iter_mut() {
        if !record.active {
            record.addr = addr;
            record.size = size;
            record.active = true;
            REGISTRY_COUNT.fetch_add(1, Ordering::Relaxed);
            return !overlap_found;
        }
    }

    // Registry full - just warn, don't fail allocation
    akuma_primitives::console::print_str("[ALLOC] Registry full, cannot track allocation\n");
    !overlap_found
}

/// Remove an allocation from the registry
/// Returns true if found and removed, false if not found (invalid free)
fn registry_remove(addr: usize) -> bool {
    if !ENABLE_ALLOCATION_REGISTRY {
        return true;
    }

    let mut registry = ALLOCATION_REGISTRY.lock();

    for record in registry.iter_mut() {
        if record.active && record.addr == addr {
            record.active = false;
            REGISTRY_COUNT.fetch_sub(1, Ordering::Relaxed);
            return true;
        }
    }

    // Not found - this is an invalid free (could be double free or wild pointer)
    INVALID_FREE_COUNT.fetch_add(1, Ordering::Relaxed);
    safe_print!(64, "[ALLOC] INVALID FREE at 0x{:x}\n", addr);
    false
}


static TALC: Spinlock<Talc<PmmOomHandler>> = Spinlock::new(Talc::new(PmmOomHandler));

// Memory tracking
static HEAP_SIZE: AtomicUsize = AtomicUsize::new(0);
static ALLOCATED_BYTES: AtomicUsize = AtomicUsize::new(0);
static ALLOCATION_COUNT: AtomicUsize = AtomicUsize::new(0);
static PEAK_ALLOCATED: AtomicUsize = AtomicUsize::new(0);

/// Temporary leak-attribution instrumentation, behind `leak-instr` (off by
/// default). Everything in here sits on an allocator hot path or depends on
/// `-C force-frame-pointers=yes`; see
/// `docs/archive/SELFHOST_KERNEL_HEAP_LEAK.md` for what it measured and how.
#[cfg(feature = "leak-instr")]
#[allow(clippy::redundant_pub_crate)]
mod leak_instr {
    use super::*;
    use core::sync::atomic::{AtomicU16, AtomicU32, AtomicU64};

    /// Live bytes and live object count per log2 size class (leak attribution).
    /// `LIVE_COUNT[class]` tells multiplicity: bytes/class_size = how many objects
    /// of that size survive, which is what distinguishes per-unit leaks from
    /// per-page drips. Temporary instrumentation for the self-host heap hunt.
    pub(super) static LIVE_BYTES: [AtomicUsize; 32] = [const { AtomicUsize::new(0) }; 32];
    pub(super) static LIVE_COUNT: [AtomicUsize; 32] = [const { AtomicUsize::new(0) }; 32];

    /// Exact-size live counts for class 2^8 (sizes 240 and 256 exactly), further
    /// attributed by current syscall number (index 0 = no-syscall context).
    /// Registered from kernel-glue at init. Temporary instrumentation.
    pub(super) static LIVE_BY_NR: [[AtomicU32; 512]; 2] = [const { [const { AtomicU32::new(0) }; 512] }; 2];

    /// Cumulative allocation counts by syscall nr, per leak size. Add-side only:
    /// no underflow, and races merely misattribute a few counts — the dominant nr
    /// for each size still identifies the syscall family that allocates the
    /// leaking objects. Temporary instrumentation.
    #[allow(dead_code)]
    pub(super) static ALLOCS_BY_NR: [[AtomicU64; 512]; 4] =
        [const { [const { AtomicU64::new(0) }; 512] }; 4]; // superseded by ALLOCS_BY_PID; kept for the [ALLOCNR] dump shape

    /// Cumulative allocation counts and live counts, by CURRENT PID, for the leak
    /// sizes. Live counts wrap (u32) when a free lands under a different pid than
    /// the alloc — a large-wrapped value at a dead pid means "allocated by that
    /// pid, freed by someone else" — exactly the teardown-path fingerprint this
    /// hunt wants. Temporary instrumentation.
    pub(super) static ALLOCS_BY_PID: [[AtomicU64; 512]; 4] =
        [const { [const { AtomicU64::new(0) }; 512] }; 4];
    pub(super) static LIVE_BY_PID: [[AtomicU32; 512]; 4] =
        [const { [const { AtomicU32::new(0) }; 512] }; 4];

    /// The four observed leak sizes.
    pub(super) const NR_SIZES: [usize; 4] = [144, 224, 240, 256];

    /// Exact-size live counts for class 2^8 (sizes 129..=256), where the build-
    /// churn leak lives. Index `size - 128`. Temporary instrumentation.
    pub(super) static LIVE_COUNT_8: [AtomicU32; 128] = [const { AtomicU32::new(0) }; 128];
    pub(super) static SYSCALL_NR_HOOK: AtomicUsize = AtomicUsize::new(0);

    // ── Temporary leak attribution: live objects by allocating call chain ──────
    //
    // The self-host heap leak is ~261 000 live 144-byte objects per clean build
    // (`docs/archive/SELFHOST_KERNEL_HEAP_LEAK.md`). Size alone names the *type*
    // (a `BTreeMap<usize, u32>` leaf node) but not the *owner*, and cumulative
    // alloc counts cannot separate the leaking site from the churning one — 86% of
    // 144-byte allocations are freed normally. So this tracks LIVE count per call
    // chain: capture the frame-pointer chain at allocation, intern it, and remember
    // which slot each live pointer belongs to so the free can decrement the right
    // one.
    //
    // Requires `-C force-frame-pointers=yes` (aarch64-unknown-none omits x29 chains
    // otherwise). Delete this whole block, its call sites and that flag once the
    // leak is named.
    pub(super) const PCTRACK_SIZE: usize = 144;
    pub(super) const PC_DEPTH: usize = 6;
    pub(super) const PC_SLOTS: usize = 512;
    /// Interned chain key (FNV of the chain); 0 = free slot.
    pub(super) static PC_KEY: [AtomicU64; PC_SLOTS] = [const { AtomicU64::new(0) }; PC_SLOTS];
    pub(super) static PC_CHAIN: [[AtomicUsize; PC_DEPTH]; PC_SLOTS] =
        [const { [const { AtomicUsize::new(0) }; PC_DEPTH] }; PC_SLOTS];
    pub(super) static PC_ALLOCS: [AtomicU64; PC_SLOTS] = [const { AtomicU64::new(0) }; PC_SLOTS];
    /// Live count, wrapping: a free whose alloc slot was evicted by an address
    /// collision decrements a neighbour. Noise, not corruption — the ranking holds.
    pub(super) static PC_LIVE: [AtomicU64; PC_SLOTS] = [const { AtomicU64::new(0) }; PC_SLOTS];

    /// Direct-mapped pointer -> slot+1, so a free can find its allocation's chain
    /// without a header (changing the layout would have to stay symmetric across
    /// alloc/dealloc/realloc; this does not touch the layout at all). 4 M entries
    /// -> ~13% occupancy at the observed live count; collisions misattribute a few
    /// percent of frees.
    pub(super) const PTR_SLOT_BITS: usize = 21;
    pub(super) static PTR_SLOT: [AtomicU16; 1 << PTR_SLOT_BITS] =
        [const { AtomicU16::new(0) }; 1 << PTR_SLOT_BITS];

    #[inline]
    pub(super) fn ptr_slot_index(ptr: usize) -> usize {
        (ptr >> 4) & ((1 << PTR_SLOT_BITS) - 1)
    }

    /// Walk the x29 frame chain, newest first. Every step is validated against the
    /// previous frame — stacks grow down, so a caller's frame is always at a higher
    /// address, and a real frame is within a page or so of its callee's. A chain
    /// that fails either check stops the walk rather than dereferencing a guess.
    #[cfg(target_os = "none")]
    #[inline(never)]
    pub(super) fn capture_chain() -> [usize; PC_DEPTH] {
        let mut out = [0usize; PC_DEPTH];
        let mut fp: usize;
        // SAFETY: reads a register. `x29` is the frame pointer under
        // `-C force-frame-pointers=yes`; without it the validation below rejects
        // whatever it holds and the walk yields zeros.
        unsafe { core::arch::asm!("mov {}, x29", out(reg) fp, options(nomem, nostack, preserves_flags)) };
        for slot in &mut out {
            if fp == 0 || fp & 0xf != 0 || fp < 0x4000_0000 {
                break;
            }
            // SAFETY: `fp` passed the alignment/range check and, after the first
            // iteration, the monotonic-and-nearby check below, so it points into
            // the current (mapped) kernel stack.
            let (next, ra) = unsafe { (*(fp as *const usize), *((fp + 8) as *const usize)) };
            *slot = ra;
            if next <= fp || next - fp > 0x1_0000 {
                break;
            }
            fp = next;
        }
        out
    }

    #[cfg(not(target_os = "none"))]
    pub(super) fn capture_chain() -> [usize; PC_DEPTH] { [0; PC_DEPTH] }

    /// Intern `chain`, returning its slot. `None` when the table is full.
    pub(super) fn intern_chain(chain: &[usize; PC_DEPTH]) -> Option<usize> {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for &pc in chain {
            h ^= pc as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        if h == 0 {
            h = 1;
        }
        let start = (h as usize) % PC_SLOTS;
        for probe in 0..32 {
            let i = (start + probe) % PC_SLOTS;
            let cur = PC_KEY[i].load(Ordering::Relaxed);
            if cur == h {
                return Some(i);
            }
            if cur == 0
                && PC_KEY[i]
                    .compare_exchange(0, h, Ordering::AcqRel, Ordering::Relaxed)
                    .is_ok()
            {
                for (d, pc) in chain.iter().enumerate() {
                    PC_CHAIN[i][d].store(*pc, Ordering::Relaxed);
                }
                return Some(i);
            }
        }
        None
    }

    #[inline]
    pub(super) fn pc_track_alloc(ptr: usize, size: usize) {
        if size != PCTRACK_SIZE {
            return;
        }
        let chain = capture_chain();
        if let Some(slot) = intern_chain(&chain) {
            PC_ALLOCS[slot].fetch_add(1, Ordering::Relaxed);
            PC_LIVE[slot].fetch_add(1, Ordering::Relaxed);
            PTR_SLOT[ptr_slot_index(ptr)].store(slot as u16 + 1, Ordering::Relaxed);
        }
    }

    #[inline]
    pub(super) fn pc_track_free(ptr: usize, size: usize) {
        if size != PCTRACK_SIZE {
            return;
        }
        let i = ptr_slot_index(ptr);
        let s = PTR_SLOT[i].swap(0, Ordering::Relaxed);
        if s != 0 {
            PC_LIVE[s as usize - 1].fetch_sub(1, Ordering::Relaxed);
        }
    }

    /// Print the call chains holding the most live `PCTRACK_SIZE` objects.
    /// Symbolize the addresses against the kernel ELF, e.g.
    /// `llvm-symbolizer --obj=target/aarch64-unknown-none/release/akuma <pc>`.
    pub fn dump_pc_attribution() {
        for i in 0..PC_SLOTS {
            let live = PC_LIVE[i].load(Ordering::Relaxed).cast_signed();
            if live < 500 {
                continue;
            }
            let allocs = PC_ALLOCS[i].load(Ordering::Relaxed);
            safe_print!(96, "[PCLIVE] slot={} live={} allocs={}\n", i, live, allocs);
            for (d, cell) in PC_CHAIN[i].iter().enumerate() {
                let pc = cell.load(Ordering::Relaxed);
                if pc != 0 {
                    safe_print!(64, "[PCLIVE]   #{} 0x{:x}\n", d, pc);
                }
            }
        }
    }


    /// Register the `current_syscall_nr` hook so leak attribution can name the
    /// syscall family a surviving allocation came from. Called once at kernel init.
    pub fn register_syscall_nr_hook(f: fn() -> u64) {
        SYSCALL_NR_HOOK.store(f as usize, Ordering::Release);
    }

    /// Register the `current_pid` hook for leak attribution.
    ///
    /// Lets the histogram name the process whose syscall path allocated (and later
    /// freed) surviving objects. Called once at kernel init; the hook returns 0
    /// when no process context exists.
    pub fn register_current_pid_hook(f: fn() -> u64) {
        CURRENT_PID_HOOK.store(f as usize, Ordering::Release);
    }

    pub(super) static CURRENT_PID_HOOK: AtomicUsize = AtomicUsize::new(0);

    #[inline]
    pub(super) fn hook_current_pid() -> u64 {
        let f = CURRENT_PID_HOOK.load(Ordering::Acquire);
        if f == 0 {
            0
        } else {
            (unsafe { core::mem::transmute::<usize, fn() -> u64>(f) })()
        }
    }

    #[inline]
    pub(super) fn live_nr_slot(size: usize) -> Option<usize> {
        match size {
            240 => Some(0),
            256 => Some(1),
            _ => None,
        }
    }

    #[inline]
    pub(super) fn alloc_nr_slot(size: usize) -> Option<usize> {
        NR_SIZES.iter().position(|&s| s == size)
    }

    #[inline]
    pub(super) fn size_class(size: usize) -> usize {
        if size == 0 {
            0
        } else {
            (size.next_power_of_two().trailing_zeros() as usize).min(31)
        }
    }

    #[inline]
    pub(super) fn livehist_add(size: usize) {
        let sc = size_class(size);
        LIVE_BYTES[sc].fetch_add(size, Ordering::Relaxed);
        LIVE_COUNT[sc].fetch_add(1, Ordering::Relaxed);
        if let Some(table) = live_nr_slot(size) {
            let nr = hook_syscall_nr() as usize;
            if nr < 512 {
                LIVE_BY_NR[table][nr].fetch_add(1, Ordering::Relaxed);
            }
        }
        if let Some(table) = alloc_nr_slot(size) {
            let pid = hook_current_pid() as usize;
            if pid < 512 {
                ALLOCS_BY_PID[table][pid].fetch_add(1, Ordering::Relaxed);
                LIVE_BY_PID[table][pid].fetch_add(1, Ordering::Relaxed);
            }
            let nr = hook_syscall_nr() as usize;
            if nr < 512 {
                ALLOCS_BY_NR[table][nr].fetch_add(1, Ordering::Relaxed);
            }
        }
        if sc == 8 && size > 128 && size < 256 {
            LIVE_COUNT_8[size - 128].fetch_add(1, Ordering::Relaxed);
        }
    }

    #[inline]
    pub(super) fn livehist_sub(size: usize) {
        let sc = size_class(size);
        LIVE_BYTES[sc].fetch_sub(size, Ordering::Relaxed);
        LIVE_COUNT[sc].fetch_sub(1, Ordering::Relaxed);
        if let Some(table) = live_nr_slot(size) {
            let nr = hook_syscall_nr() as usize;
            if nr < 512 {
                LIVE_BY_NR[table][nr].fetch_sub(1, Ordering::Relaxed);
            }
        }
        if let Some(table) = alloc_nr_slot(size) {
            let pid = hook_current_pid() as usize;
            if pid < 512 {
                LIVE_BY_PID[table][pid].fetch_sub(1, Ordering::Relaxed);
            }
        }
        if sc == 8 && size > 128 && size < 256 {
            LIVE_COUNT_8[size - 128].fetch_sub(1, Ordering::Relaxed);
        }
    }

    #[inline]
    pub(super) fn hook_syscall_nr() -> u64 {
        let f = SYSCALL_NR_HOOK.load(Ordering::Acquire);
        if f == 0 {
            0
        } else {
            (unsafe { core::mem::transmute::<usize, fn() -> u64>(f) })()
        }
    }

    /// Print one line per nonzero size class. Alloc-free (safe_print), safe from
    /// any context. Sum of LIVE_BYTES tracks ALLOCATED_BYTES within class-granularity.
    pub fn dump_live_histogram() {
        for i in 0..32 {
            let b = LIVE_BYTES[i].load(Ordering::Relaxed);
            let c = LIVE_COUNT[i].load(Ordering::Relaxed);
            if b > 0 {
                safe_print!(128, "[LIVEHIST] 2^{}: {} objs, {}KB live\n", i, c, b / 1024);
            }
        }
        for (i, c) in LIVE_COUNT_8.iter().enumerate() {
            let c = c.load(Ordering::Relaxed);
            if c > 0 {
                safe_print!(96, "[LIVE8] size={}: {} objs\n", 128 + i, c);
            }
        }
        for (t, size) in [(0usize, 240usize), (1, 256)] {
            for (nr, c) in LIVE_BY_NR[t].iter().enumerate() {
                let c = c.load(Ordering::Relaxed);
                if c > 0 {
                    safe_print!(96, "[LIVENR] size={} nr={}: {} objs live\n", size, nr, c);
                }
            }
        }
        for (t, size) in NR_SIZES.iter().enumerate() {
            for (nr, c) in ALLOCS_BY_NR[t].iter().enumerate() {
                let c = c.load(Ordering::Relaxed);
                if c > 30 {
                    safe_print!(96, "[NRALLOC] size={} nr={}: {} allocs\n", size, nr, c);
                }
            }
            for (pid, c) in ALLOCS_BY_PID[t].iter().enumerate() {
                let c = c.load(Ordering::Relaxed);
                if c > 20 {
                    let live = LIVE_BY_PID[t][pid].load(Ordering::Relaxed);
                    safe_print!(128, "[PIDNR] size={} pid={}: {} allocs, {} live (wrap=dead-pid-freed)\n",
                        size, pid, c, live.cast_signed());
                }
            }
        }
    }

}
#[cfg(feature = "leak-instr")]
use leak_instr::{livehist_add, livehist_sub, pc_track_alloc, pc_track_free};
#[cfg(feature = "leak-instr")]
pub use leak_instr::{dump_live_histogram, dump_pc_attribution, register_current_pid_hook,
    register_syscall_nr_hook};

// ── `leak-instr` off: no-op stubs so the allocator's hot paths keep their
// call sites and compile to nothing. ────────────────────────────────────────
#[cfg(not(feature = "leak-instr"))]
#[inline(always)]
fn livehist_add(_size: usize) {}
#[cfg(not(feature = "leak-instr"))]
#[inline(always)]
fn livehist_sub(_size: usize) {}
#[cfg(not(feature = "leak-instr"))]
#[inline(always)]
fn pc_track_alloc(_ptr: usize, _size: usize) {}
#[cfg(not(feature = "leak-instr"))]
#[inline(always)]
fn pc_track_free(_ptr: usize, _size: usize) {}

/// Live-bytes histogram dump. No-op unless `leak-instr` is enabled.
#[cfg(not(feature = "leak-instr"))]
pub fn dump_live_histogram() {}
/// Call-chain attribution dump. No-op unless `leak-instr` is enabled.
#[cfg(not(feature = "leak-instr"))]
pub fn dump_pc_attribution() {}
/// Registration is a no-op unless `leak-instr` is enabled.
#[cfg(not(feature = "leak-instr"))]
pub fn register_syscall_nr_hook(_f: fn() -> u64) {}
#[cfg(not(feature = "leak-instr"))]
pub fn register_current_pid_hook(_f: fn() -> u64) {}

/// Memory statistics
#[derive(Debug, Clone, Copy)]
pub struct MemoryStats {
    pub heap_size: usize,
    pub allocated: usize,
    pub free: usize,
    pub allocation_count: usize,
    pub peak_allocated: usize,
}

/// Get current allocated bytes (live allocations).
///
/// Was `#[cfg(kernel_tests)]`; see the module header's "No `build.rs`" note.
pub fn allocated_bytes() -> usize {
    ALLOCATED_BYTES.load(Ordering::Relaxed)
}

/// Get current memory statistics
pub fn stats() -> MemoryStats {
    let heap_size = HEAP_SIZE.load(Ordering::Relaxed);
    let allocated = ALLOCATED_BYTES.load(Ordering::Relaxed);
    MemoryStats {
        heap_size,
        allocated,
        free: heap_size.saturating_sub(allocated),
        allocation_count: ALLOCATION_COUNT.load(Ordering::Relaxed),
        peak_allocated: PEAK_ALLOCATED.load(Ordering::Relaxed),
    }
}

/// Returns true if the system is running low on physical memory.
///
/// Pre-PMM: checks heap slab free space. Post-PMM: checks PMM free pages,
/// since the heap now grows on demand and the seeded slab size is irrelevant.
pub fn is_memory_low() -> bool {
    const LOW_PAGES: usize = 128; // 512 KB threshold
    if is_pmm_ready() {
        akuma_pmm::free_count() < LOW_PAGES
    } else {
        let heap_size = HEAP_SIZE.load(Ordering::Relaxed);
        let allocated = ALLOCATED_BYTES.load(Ordering::Relaxed);
        heap_size.saturating_sub(allocated) < 256 * 1024
    }
}

/// No-op for backwards compatibility - IRQs are now always disabled during allocation
pub fn enable_preemption_safe_alloc() {}

pub fn init(heap_start: usize, heap_size: usize) -> Result<(), &'static str> {
    if heap_size == 0 {
        return Err("Heap size cannot be zero");
    }

    if heap_start == 0 {
        return Err("Invalid heap start address");
    }

    // Store heap size for stats
    HEAP_SIZE.store(heap_size, Ordering::Relaxed);

    // Initialize talc allocator (used as fallback or when USE_PAGE_ALLOCATOR is false)
    unsafe {
        let heap_ptr = heap_start as *mut u8;
        let span = Span::from_base_size(heap_ptr, heap_size);
        TALC.lock()
            .claim(span)
            .map_err(|()| "Failed to claim heap memory")?;
    }

    Ok(())
}

// ============================================================================
// Global allocator — delegates directly to Talc
// ============================================================================

/// The kernel heap, as a `GlobalAlloc`.
///
/// **The bin crate installs this**, with `#[global_allocator]` in `main.rs`.
/// Deliberately not installed here: `#[global_allocator]` and
/// `#[alloc_error_handler]` are *binary-level* declarations, and a library that
/// makes them silently decides the allocator for anything that links it —
/// including a host test binary, where it fights std. Keeping them in the bin is
/// also what lets this crate build for the host at all.
pub struct KernelAllocator;

unsafe impl GlobalAlloc for KernelAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        unsafe { talc_alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        unsafe {
            let ptr = talc_alloc(layout);
            if !ptr.is_null() {
                ptr::write_bytes(ptr, 0, layout.size());
            }
            ptr
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { talc_dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        unsafe { talc_realloc(ptr, layout, new_size) }
    }
}

// ============================================================================
// Talc-based allocator (original implementation)
// ============================================================================

unsafe fn talc_alloc(layout: Layout) -> *mut u8 { unsafe {
    with_irqs_disabled(|| {
        // Calculate actual allocation size with canaries
        let user_size = layout.size();
        let (actual_layout, _user_offset) = if ENABLE_ALLOCATION_REGISTRY && ENABLE_CANARIES {
            // Add space for canaries: [canary_before(8)] [user_data] [canary_after(8)]
            let total_size = CANARY_SIZE + user_size + CANARY_SIZE;
            let actual_align = layout.align().max(8); // Ensure 8-byte alignment for canaries
            match Layout::from_size_align(total_size, actual_align) {
                Ok(l) => (l, CANARY_SIZE),
                Err(_) => return ptr::null_mut(),
            }
        } else {
            (layout, 0)
        };

        let result = TALC
            .lock()
            .malloc(actual_layout)
            .map_or(ptr::null_mut(), core::ptr::NonNull::as_ptr);

        if result.is_null() {
            let heap_total = HEAP_SIZE.load(Ordering::Relaxed);
            let heap_used = ALLOCATED_BYTES.load(Ordering::Relaxed);
            let heap_peak = PEAK_ALLOCATED.load(Ordering::Relaxed);
            let heap_count = ALLOCATION_COUNT.load(Ordering::Relaxed);
            safe_print!(256,
                "\n[ALLOC FAIL] requested={} heap_total={}MB heap_used={}MB ({}%) peak={}MB allocs={}\n",
                user_size,
                heap_total / 1024 / 1024,
                heap_used / 1024 / 1024,
                (heap_used * 100).checked_div(heap_total).unwrap_or(0),
                heap_peak / 1024 / 1024,
                heap_count);
            // Returning null sends Rust straight to `handle_alloc_error`, i.e.
            // the bin's `#[alloc_error_handler]` — which is where any further
            // whole-kernel diagnostics (syscall counters, and so on) belong.
            return ptr::null_mut();
        }

        // Set up canaries and calculate user pointer
        let user_ptr = if ENABLE_ALLOCATION_REGISTRY && ENABLE_CANARIES {
            // Write canary before
            let canary_before_ptr = result.cast::<u64>();
            core::ptr::write_volatile(canary_before_ptr, CANARY_BEFORE);

            // Calculate user pointer (after the before-canary)
            let user = result.add(CANARY_SIZE);

            // Write canary after
            let canary_after_ptr = user.add(user_size).cast::<u64>();
            core::ptr::write_volatile(canary_after_ptr, CANARY_AFTER);

            user
        } else {
            result
        };

        // Register allocation
        if ENABLE_ALLOCATION_REGISTRY {
            registry_add(user_ptr as usize, user_size);
        }

        // Update stats
        let new_allocated =
            ALLOCATED_BYTES.fetch_add(user_size, Ordering::Relaxed) + user_size;
        ALLOCATION_COUNT.fetch_add(1, Ordering::Relaxed);
        livehist_add(user_size);
        pc_track_alloc(user_ptr as usize, user_size);
        let mut peak = PEAK_ALLOCATED.load(Ordering::Relaxed);
        while new_allocated > peak {
            match PEAK_ALLOCATED.compare_exchange_weak(
                peak,
                new_allocated,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(p) => peak = p,
            }
        }

        // Heap growth monitor: print at each 5MB boundary crossing
        static NEXT_REPORT_MB: AtomicUsize = AtomicUsize::new(13);
        let mb = new_allocated / (1024 * 1024);
        let next = NEXT_REPORT_MB.load(Ordering::Relaxed);
        if mb >= next {
            NEXT_REPORT_MB.store(mb + 5, Ordering::Relaxed);
            safe_print!(96, "[HEAP] {}MB used (alloc={} bytes)\n", mb, user_size);
        }

        user_ptr
    })
}}

unsafe fn talc_dealloc(ptr: *mut u8, layout: Layout) { unsafe {
    with_irqs_disabled(|| {
        let user_size = layout.size();

        // Check registry and canaries
        if ENABLE_ALLOCATION_REGISTRY {
            // Check if this allocation exists
            if !registry_remove(ptr as usize) {
                // Could be double free - check if we've seen this address before
                DOUBLE_FREE_COUNT.fetch_add(1, Ordering::Relaxed);
                safe_print!(64, "[ALLOC] Possible DOUBLE FREE at 0x{:x}\n", ptr as usize);
                // Don't actually free - could cause more corruption
                return;
            }

            // Check canaries if enabled
            if ENABLE_CANARIES {
                // Check canary before
                let canary_before_ptr = ptr.sub(CANARY_SIZE) as *const u64;
                let canary_before = core::ptr::read_volatile(canary_before_ptr);
                if canary_before != CANARY_BEFORE {
                    CANARY_CORRUPTION_COUNT.fetch_add(1, Ordering::Relaxed);
                    safe_print!(
                        128,
                        "[ALLOC] CANARY CORRUPTION (before) at dealloc 0x{:x}: expected 0x{:x}, got 0x{:x}\n",
                        ptr as usize,
                        CANARY_BEFORE,
                        canary_before
                    );
                }

                // Check canary after
                let canary_after_ptr = ptr.add(user_size) as *const u64;
                let canary_after = core::ptr::read_volatile(canary_after_ptr);
                if canary_after != CANARY_AFTER {
                    CANARY_CORRUPTION_COUNT.fetch_add(1, Ordering::Relaxed);
                    safe_print!(
                        128,
                        "[ALLOC] CANARY CORRUPTION (after) at dealloc 0x{:x}+{}: expected 0x{:x}, got 0x{:x}\n",
                        ptr as usize,
                        user_size,
                        CANARY_AFTER,
                        canary_after
                    );
                }
            }
        }

        // Calculate actual allocation to free
        let (actual_ptr, actual_layout) = if ENABLE_ALLOCATION_REGISTRY && ENABLE_CANARIES {
            let actual_ptr = ptr.sub(CANARY_SIZE);
            let total_size = CANARY_SIZE + user_size + CANARY_SIZE;
            let actual_align = layout.align().max(8);
            let actual_layout = Layout::from_size_align_unchecked(total_size, actual_align);
            (actual_ptr, actual_layout)
        } else {
            (ptr, layout)
        };

        // Before the free: once the span is back in Talc another core can hand
        // the same address out and claim its `PTR_SLOT` entry, which this would
        // then clear.
        pc_track_free(ptr as usize, user_size);
        TALC.lock()
            .free(core::ptr::NonNull::new_unchecked(actual_ptr), actual_layout);
        ALLOCATED_BYTES.fetch_sub(user_size, Ordering::Relaxed);
        livehist_sub(user_size);
    });
}}

unsafe fn talc_realloc(ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
    // CRITICAL: Wrap entire realloc operation in IRQ protection!
    //
    // Previously, only talc_alloc and talc_dealloc were individually protected,
    // but the memory copy between them was not. If a timer fired during the copy:
    // 1. Thread A starts copying from old to new allocation
    // 2. Timer fires, scheduler switches to Thread B
    // 3. Thread B allocates/deallocates, modifying heap metadata
    // 4. Thread A resumes, continues copying, then frees old allocation
    //
    // While the heap metadata stays consistent (alloc/dealloc are atomic),
    // the timing window could cause subtle issues. Wrapping the entire operation
    // ensures atomicity of the full realloc sequence.
    with_irqs_disabled(|| {
        unsafe {
            let old_user_size = layout.size();

            if new_size == 0 {
                // Handle as dealloc
                if ENABLE_ALLOCATION_REGISTRY {
                    registry_remove(ptr as usize);
                    
                    // Check canaries before freeing
                    if ENABLE_CANARIES && !ptr.is_null() {
                        let canary_before = core::ptr::read_volatile(ptr.sub(CANARY_SIZE) as *const u64);
                        let canary_after = core::ptr::read_volatile(ptr.add(old_user_size) as *const u64);
                        if canary_before != CANARY_BEFORE || canary_after != CANARY_AFTER {
                            CANARY_CORRUPTION_COUNT.fetch_add(1, Ordering::Relaxed);
                            akuma_primitives::console::print_str("[ALLOC] CANARY CORRUPTION in realloc(0)\n");
                        }
                    }
                }

                let (actual_ptr, actual_layout) = if ENABLE_ALLOCATION_REGISTRY && ENABLE_CANARIES {
                    let actual_ptr = ptr.sub(CANARY_SIZE);
                    let total_size = CANARY_SIZE + old_user_size + CANARY_SIZE;
                    let actual_align = layout.align().max(8);
                    (actual_ptr, Layout::from_size_align_unchecked(total_size, actual_align))
                } else {
                    (ptr, layout)
                };

                TALC.lock()
                    .free(core::ptr::NonNull::new_unchecked(actual_ptr), actual_layout);
                ALLOCATED_BYTES.fetch_sub(old_user_size, Ordering::Relaxed);
                livehist_sub(old_user_size);
                return ptr::null_mut();
            }

            // Calculate new layout with canaries
            let (new_actual_layout, _new_user_offset) = if ENABLE_ALLOCATION_REGISTRY && ENABLE_CANARIES {
                let total_size = CANARY_SIZE + new_size + CANARY_SIZE;
                let actual_align = layout.align().max(8);
                match Layout::from_size_align(total_size, actual_align) {
                    Ok(l) => (l, CANARY_SIZE),
                    Err(_) => return ptr::null_mut(),
                }
            } else {
                match Layout::from_size_align(new_size, layout.align()) {
                    Ok(l) => (l, 0),
                    Err(_) => return ptr::null_mut(),
                }
            };

            // Allocate new memory
            let new_actual_ptr = TALC
                .lock()
                .malloc(new_actual_layout)
                .map_or(ptr::null_mut(), core::ptr::NonNull::as_ptr);
            
            if new_actual_ptr.is_null() {
                return ptr::null_mut();
            }

            // Set up canaries and get user pointer
            let new_user_ptr = if ENABLE_ALLOCATION_REGISTRY && ENABLE_CANARIES {
                core::ptr::write_volatile(new_actual_ptr.cast::<u64>(), CANARY_BEFORE);
                let user = new_actual_ptr.add(CANARY_SIZE);
                core::ptr::write_volatile(user.add(new_size).cast::<u64>(), CANARY_AFTER);
                user
            } else {
                new_actual_ptr
            };

            // Register new allocation
            if ENABLE_ALLOCATION_REGISTRY {
                registry_add(new_user_ptr as usize, new_size);
            }

            // Update allocation stats for new allocation
            let new_allocated = ALLOCATED_BYTES.fetch_add(new_size, Ordering::Relaxed) + new_size;
            ALLOCATION_COUNT.fetch_add(1, Ordering::Relaxed);
            livehist_add(new_size);
            let mut peak = PEAK_ALLOCATED.load(Ordering::Relaxed);
            while new_allocated > peak {
                match PEAK_ALLOCATED.compare_exchange_weak(
                    peak,
                    new_allocated,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => break,
                    Err(p) => peak = p,
                }
            }

            // Copy old data to new allocation
            if !ptr.is_null() && old_user_size > 0 {
                let copy_size = core::cmp::min(old_user_size, new_size);
                if copy_size > 0 {
                    ptr::copy_nonoverlapping(ptr, new_user_ptr, copy_size);
                }

                // Remove old from registry
                if ENABLE_ALLOCATION_REGISTRY {
                    registry_remove(ptr as usize);
                    
                    // Check old canaries
                    if ENABLE_CANARIES {
                        let canary_before = core::ptr::read_volatile(ptr.sub(CANARY_SIZE) as *const u64);
                        let canary_after = core::ptr::read_volatile(ptr.add(old_user_size) as *const u64);
                        if canary_before != CANARY_BEFORE || canary_after != CANARY_AFTER {
                            CANARY_CORRUPTION_COUNT.fetch_add(1, Ordering::Relaxed);
                            akuma_primitives::console::print_str("[ALLOC] CANARY CORRUPTION in realloc\n");
                        }
                    }
                }

                // Free old allocation
                let (old_actual_ptr, old_actual_layout) = if ENABLE_ALLOCATION_REGISTRY && ENABLE_CANARIES {
                    let old_actual_ptr = ptr.sub(CANARY_SIZE);
                    let total_size = CANARY_SIZE + old_user_size + CANARY_SIZE;
                    let actual_align = layout.align().max(8);
                    (old_actual_ptr, Layout::from_size_align_unchecked(total_size, actual_align))
                } else {
                    (ptr, layout)
                };

                TALC.lock()
                    .free(core::ptr::NonNull::new_unchecked(old_actual_ptr), old_actual_layout);
                ALLOCATED_BYTES.fetch_sub(old_user_size, Ordering::Relaxed);
                livehist_sub(old_user_size);
            }

            // Heap growth monitor for realloc (net growth = new_size - old_user_size)
            {
                static NEXT_REALLOC_REPORT_MB: AtomicUsize = AtomicUsize::new(15);
                let current = ALLOCATED_BYTES.load(Ordering::Relaxed);
                let mb = current / (1024 * 1024);
                let next = NEXT_REALLOC_REPORT_MB.load(Ordering::Relaxed);
                if mb >= next {
                    NEXT_REALLOC_REPORT_MB.store(mb + 5, Ordering::Relaxed);
                    safe_print!(128, "[HEAP-R] {}MB used (realloc {}->{})\n", mb, old_user_size, new_size);
                }
            }

            new_user_ptr
        }
    })
}
