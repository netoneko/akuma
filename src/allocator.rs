//! Kernel memory allocator — Talc with on-demand PMM growth.
//!
//! The heap is seeded with a small bootstrap arena (~1 MB) and grows on
//! demand by claiming contiguous pages from the PMM once it is ready.
//!
//! Debug features:
//! - ENABLE_ALLOCATION_REGISTRY: Track all allocations to detect overlaps, double frees
//! - ENABLE_CANARIES: Add guard bytes around allocations to detect overflows

use core::alloc::{GlobalAlloc, Layout};
use core::ptr;
use core::sync::atomic::{AtomicUsize, AtomicBool, Ordering};
use spinning_top::Spinlock;
use talc::{Span, Talc};

/// Enable allocation registry for debugging heap corruption
/// This tracks all allocations and detects overlaps, double frees, and invalid frees
/// WARNING: Canaries break virtio-drivers which does address comparisons on DMA buffers
/// WARNING: Registry causes performance issues - iterates 4096 entries per alloc
pub const ENABLE_ALLOCATION_REGISTRY: bool = false;

/// Enable canary bytes around allocations (requires ENABLE_ALLOCATION_REGISTRY)
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
        let pages_for_layout = (layout.size() + PAGE_SIZE - 1) / PAGE_SIZE;
        let needed = pages_for_layout + HEAP_GROW_HEADROOM_PAGES;
        let mut n = heap_grow_initial_pages(needed, crate::pmm::free_count());

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
            if let Some(frame) = crate::pmm::alloc_pages_contiguous_zeroed(n) {
                let ptr = akuma_exec::mmu::phys_to_virt(frame.addr) as *mut u8;
                let span = Span::from_base_size(ptr, n * PAGE_SIZE);
                return match unsafe { talc.claim(span) } {
                    Ok(_heap) => {
                        // Record the PMM-backed span so `reclaim_to_pmm()` can
                        // return it later once it is fully free. If the registry
                        // is full the span is still used as heap — it just
                        // becomes non-reclaimable (the pre-reclaim one-way
                        // behaviour).
                        register_claimed_span(frame.addr, n);
                        let prev = HEAP_SIZE.fetch_add(n * PAGE_SIZE, Ordering::Relaxed);
                        let now = prev + n * PAGE_SIZE;
                        // Leak-debug: log the request driving growth each time the
                        // heap crosses a 256 MB boundary, so a runaway grow is
                        // attributable to a specific allocation size. safe_print
                        // is alloc-free (used by the alloc error handler too).
                        const STEP: usize = 256 * 1024 * 1024;
                        if prev / STEP != now / STEP {
                            crate::safe_print!(160,
                                "[HEAP-GROW] total={}MB used={}MB this_req={} bytes claimed={} pages\n",
                                now / 1024 / 1024,
                                ALLOCATED_BYTES.load(Ordering::Relaxed) / 1024 / 1024,
                                layout.size(), n);
                        }
                        Ok(())
                    }
                    Err(()) => {
                        // Couldn't establish a heap in the pages — return them to
                        // PMM rather than leaking (old code dropped them).
                        crate::pmm::free_pages_contiguous(
                            akuma_exec::PhysFrame::new(frame.addr), n);
                        Err(())
                    }
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
/// per-claimed-span metadata. Without this, an allocation whose size is an exact
/// multiple of the page size (e.g. a recurring 256 KB / 64-page request) never
/// fits in a span of exactly that many pages: handle_oom claims a just-too-small
/// span, talc re-fails, and the heap grows without bound until the PMM is drained
/// and the kernel aborts (`brk #1`). talc's overhead is a handful of tag words —
/// well under one page — so 2 pages is ample headroom. See docs/LLAMA_MMAP_OOM_KERNEL_ABORT.md.
pub const HEAP_GROW_HEADROOM_PAGES: usize = 2;

/// Initial contiguous-page request for a heap-growth that must satisfy a layout
/// needing `needed` pages, given `free` PMM pages remain. Amortise to
/// [`HEAP_GROW_PAGES`] when memory is ample; shrink to exactly `needed` under
/// pressure (`free <= 2 * HEAP_GROW_PAGES`) so the thin `USER_PAGE_RESERVE` pool
/// is preserved for the OOM-kill bookkeeping path. Pure fn over its inputs so the
/// boundary is unit-testable without draining real RAM.
#[inline]
pub fn heap_grow_initial_pages(needed: usize, free: usize) -> usize {
    if free <= 2 * HEAP_GROW_PAGES {
        needed
    } else {
        needed.max(HEAP_GROW_PAGES)
    }
}

/// Next contiguous-page request after a run of `n` pages failed to allocate, for
/// a layout needing at least `needed` pages. Halves toward `needed` so a
/// fragmented pool that can't yield the amortised run can still back off to the
/// minimum the layout requires (and, when `needed == 1`, to a single page —
/// satisfiable whenever any page is free). Returns `None` once `needed` itself
/// has been tried, i.e. genuine multi-page-contiguous OOM. Pure + monotonically
/// decreasing, so the `handle_oom` loop is guaranteed to terminate.
#[inline]
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
        let base = akuma_exec::mmu::phys_to_virt(self.pmm_addr) as *mut u8;
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
                crate::pmm::free_pages_contiguous(akuma_exec::PhysFrame::new(addr), pages);
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

/// Take a [`SpanReport`]. Safe from any non-allocator context; if Talc is held
/// (we were reentered from `handle_oom`) it returns `busy = true` rather than
/// deadlocking, matching `reclaim_to_pmm`'s `try_lock` discipline.
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
            crate::console::print("[ALLOC] OVERLAP DETECTED!\n");
            crate::safe_print!(
                80,
                "  New: 0x{:x}-0x{:x} (size={})\n",
                addr,
                addr + size,
                size
            );
            crate::safe_print!(
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
    crate::console::print("[ALLOC] Registry full, cannot track allocation\n");
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
    crate::safe_print!(64, "[ALLOC] INVALID FREE at 0x{:x}\n", addr);
    false
}

#[global_allocator]
static ALLOCATOR: KernelAllocator = KernelAllocator;

/// OOM handler: kill the current userspace process instead of panicking the kernel.
/// If there is no current process (pure kernel context), fall through to panic.
#[alloc_error_handler]
fn alloc_error_handler(layout: core::alloc::Layout) -> ! {
    let heap_total = HEAP_SIZE.load(Ordering::Relaxed);
    let heap_used = ALLOCATED_BYTES.load(Ordering::Relaxed);
    crate::safe_print!(256,
        "\n[OOM] allocation of {} bytes failed (heap {}MB / {}MB used) — killing process\n",
        layout.size(),
        heap_used / 1024 / 1024,
        heap_total / 1024 / 1024,
    );
    // Kill the current process if there is one; otherwise panic the kernel.
    if akuma_exec::process::current_process().is_some() {
        akuma_exec::process::return_to_kernel(-12); // ENOMEM
    }
    panic!("kernel OOM: allocation of {} bytes failed", layout.size());
}

static TALC: Spinlock<Talc<PmmOomHandler>> = Spinlock::new(Talc::new(PmmOomHandler));

// Memory tracking
static HEAP_SIZE: AtomicUsize = AtomicUsize::new(0);
static ALLOCATED_BYTES: AtomicUsize = AtomicUsize::new(0);
static ALLOCATION_COUNT: AtomicUsize = AtomicUsize::new(0);
static PEAK_ALLOCATED: AtomicUsize = AtomicUsize::new(0);

/// Memory statistics
#[derive(Debug, Clone, Copy)]
pub struct MemoryStats {
    pub heap_size: usize,
    pub allocated: usize,
    pub free: usize,
    pub allocation_count: usize,
    pub peak_allocated: usize,
}

/// Get current allocated bytes (live allocations)
#[cfg(not(any(feature = "no-tests", kernel_profile_size)))]
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
/// Pre-PMM: checks heap slab free space. Post-PMM: checks PMM free pages,
/// since the heap now grows on demand and the seeded slab size is irrelevant.
pub fn is_memory_low() -> bool {
    const LOW_PAGES: usize = 128; // 512 KB threshold
    if is_pmm_ready() {
        crate::pmm::free_count() < LOW_PAGES
    } else {
        let heap_size = HEAP_SIZE.load(Ordering::Relaxed);
        let allocated = ALLOCATED_BYTES.load(Ordering::Relaxed);
        heap_size.saturating_sub(allocated) < 256 * 1024
    }
}

/// No-op for backwards compatibility - IRQs are now always disabled during allocation
pub fn enable_preemption_safe_alloc() {}

// Use the shared IRQ guard from the irq module
use crate::irq::with_irqs_disabled;

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
            .map_err(|_| "Failed to claim heap memory")?;
    }

    Ok(())
}

// ============================================================================
// Global allocator — delegates directly to Talc
// ============================================================================

struct KernelAllocator;

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
            .map(|ptr| ptr.as_ptr())
            .unwrap_or(ptr::null_mut());

        if result.is_null() {
            let heap_total = HEAP_SIZE.load(Ordering::Relaxed);
            let heap_used = ALLOCATED_BYTES.load(Ordering::Relaxed);
            let heap_peak = PEAK_ALLOCATED.load(Ordering::Relaxed);
            let heap_count = ALLOCATION_COUNT.load(Ordering::Relaxed);
            crate::safe_print!(256,
                "\n[ALLOC FAIL] requested={} heap_total={}MB heap_used={}MB ({}%) peak={}MB allocs={}\n",
                user_size,
                heap_total / 1024 / 1024,
                heap_used / 1024 / 1024,
                if heap_total > 0 { heap_used * 100 / heap_total } else { 0 },
                heap_peak / 1024 / 1024,
                heap_count);
            crate::syscall::syscall_counters::dump();
            return ptr::null_mut();
        }

        // Set up canaries and calculate user pointer
        let user_ptr = if ENABLE_ALLOCATION_REGISTRY && ENABLE_CANARIES {
            // Write canary before
            let canary_before_ptr = result as *mut u64;
            core::ptr::write_volatile(canary_before_ptr, CANARY_BEFORE);

            // Calculate user pointer (after the before-canary)
            let user = result.add(CANARY_SIZE);

            // Write canary after
            let canary_after_ptr = user.add(user_size) as *mut u64;
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
            let sc_nr = crate::syscall::current_syscall_nr();
            let tid = akuma_exec::threading::current_thread_id();
            crate::safe_print!(192, "[HEAP] {}MB used (alloc={} bytes, sc_nr={}, tid={})\n", mb, user_size, sc_nr, tid);
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
                crate::safe_print!(64, "[ALLOC] Possible DOUBLE FREE at 0x{:x}\n", ptr as usize);
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
                    crate::safe_print!(
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
                    crate::safe_print!(
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

        TALC.lock()
            .free(core::ptr::NonNull::new_unchecked(actual_ptr), actual_layout);
        ALLOCATED_BYTES.fetch_sub(user_size, Ordering::Relaxed);
    })
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
                            crate::console::print("[ALLOC] CANARY CORRUPTION in realloc(0)\n");
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
                .map(|p| p.as_ptr())
                .unwrap_or(ptr::null_mut());
            
            if new_actual_ptr.is_null() {
                return ptr::null_mut();
            }

            // Set up canaries and get user pointer
            let new_user_ptr = if ENABLE_ALLOCATION_REGISTRY && ENABLE_CANARIES {
                core::ptr::write_volatile(new_actual_ptr as *mut u64, CANARY_BEFORE);
                let user = new_actual_ptr.add(CANARY_SIZE);
                core::ptr::write_volatile(user.add(new_size) as *mut u64, CANARY_AFTER);
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
                            crate::console::print("[ALLOC] CANARY CORRUPTION in realloc\n");
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
            }

            // Heap growth monitor for realloc (net growth = new_size - old_user_size)
            {
                static NEXT_REALLOC_REPORT_MB: AtomicUsize = AtomicUsize::new(15);
                let current = ALLOCATED_BYTES.load(Ordering::Relaxed);
                let mb = current / (1024 * 1024);
                let next = NEXT_REALLOC_REPORT_MB.load(Ordering::Relaxed);
                if mb >= next {
                    NEXT_REALLOC_REPORT_MB.store(mb + 5, Ordering::Relaxed);
                    crate::safe_print!(128, "[HEAP-R] {}MB used (realloc {}->{})\n", mb, old_user_size, new_size);
                }
            }

            new_user_ptr
        }
    })
}
