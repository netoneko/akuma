//! Physical Memory Manager (PMM)
//!
//! Manages physical page allocation using a bitmap allocator.
//! Each bit in the bitmap represents a 4KB page.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};
use spinning_top::Spinlock;

use akuma_exec::mmu::PAGE_SIZE;
pub use akuma_exec::{PhysFrame, FrameSource};

// ============================================================================
// Debug Frame Tracking
// ============================================================================

/// Enable debug frame tracking (adds overhead but helps find leaks)
/// Set to true to track all frame allocations with metadata
pub const DEBUG_FRAME_TRACKING: bool = false;

/// Information about a tracked frame allocation
#[derive(Debug, Clone)]
pub struct FrameInfo {
    /// Source of the allocation
    pub source: FrameSource,
}

/// Debug tracker for frame allocations
struct FrameTracker {
    /// Map of physical address to allocation info
    allocations: BTreeMap<usize, FrameInfo>,
    /// Count of current allocations by source
    kernel_count: usize,
    user_page_table_count: usize,
    user_data_count: usize,
    elf_loader_count: usize,
    unknown_count: usize,
    /// Cumulative stats
    total_tracked: usize,
    total_untracked: usize,
}

impl FrameTracker {
    const fn new() -> Self {
        Self {
            allocations: BTreeMap::new(),
            kernel_count: 0,
            user_page_table_count: 0,
            user_data_count: 0,
            elf_loader_count: 0,
            unknown_count: 0,
            total_tracked: 0,
            total_untracked: 0,
        }
    }

    fn track(&mut self, addr: usize, source: FrameSource) {
        if let Some(_old) = self.allocations.insert(addr, FrameInfo { source }) {
            // Double allocation detected! Use stack-only print to avoid heap in PMM
            crate::console::print("[PMM WARN] Double allocation detected!\n");
        }
        match source {
            FrameSource::Kernel => self.kernel_count += 1,
            FrameSource::UserPageTable => self.user_page_table_count += 1,
            FrameSource::UserData => self.user_data_count += 1,
            FrameSource::ElfLoader => self.elf_loader_count += 1,
            FrameSource::Unknown => self.unknown_count += 1,
        }
        self.total_tracked += 1;
    }

    fn untrack(&mut self, addr: usize) -> Option<FrameInfo> {
        if let Some(info) = self.allocations.remove(&addr) {
            match info.source {
                FrameSource::Kernel => self.kernel_count = self.kernel_count.saturating_sub(1),
                FrameSource::UserPageTable => {
                    self.user_page_table_count = self.user_page_table_count.saturating_sub(1);
                }
                FrameSource::UserData => {
                    self.user_data_count = self.user_data_count.saturating_sub(1);
                }
                FrameSource::ElfLoader => {
                    self.elf_loader_count = self.elf_loader_count.saturating_sub(1);
                }
                FrameSource::Unknown => self.unknown_count = self.unknown_count.saturating_sub(1),
            }
            self.total_untracked += 1;
            Some(info)
        } else {
            // Use stack-only print to avoid heap in PMM
            crate::console::print("[PMM WARN] Freeing untracked frame\n");
            None
        }
    }

    // Frame-leak diagnostic. Its only in-tree caller was the built-in shell's
    // memory command; kept because it pairs with DEBUG_FRAME_TRACKING and is what
    // you reach for from the debugger when frames go missing.
    #[allow(dead_code)]
    fn leak_count(&self) -> usize {
        self.allocations.len()
    }

    fn stats(&self) -> FrameTrackingStats {
        FrameTrackingStats {
            current_tracked: self.allocations.len(),
            kernel_count: self.kernel_count,
            user_page_table_count: self.user_page_table_count,
            user_data_count: self.user_data_count,
            elf_loader_count: self.elf_loader_count,
            unknown_count: self.unknown_count,
            total_tracked: self.total_tracked,
            total_untracked: self.total_untracked,
        }
    }
}

/// Statistics from frame tracking
#[derive(Debug, Clone)]
// A full diagnostic snapshot: the periodic report prints some of these, the rest
// are read from the debugger / by whoever is chasing a frame leak.
#[allow(dead_code)]
pub struct FrameTrackingStats {
    pub current_tracked: usize,
    pub kernel_count: usize,
    pub user_page_table_count: usize,
    pub user_data_count: usize,
    pub elf_loader_count: usize,
    pub unknown_count: usize,
    /// Cumulative totals
    pub total_tracked: usize,
    pub total_untracked: usize,
}

static FRAME_TRACKER: Spinlock<FrameTracker> = Spinlock::new(FrameTracker::new());

/// Track a frame allocation (only if DEBUG_FRAME_TRACKING is enabled)
pub fn track_frame(frame: PhysFrame, source: FrameSource) {
    if DEBUG_FRAME_TRACKING {
        FRAME_TRACKER.lock().track(frame.addr, source);
    }
}

/// Untrack a frame (only if DEBUG_FRAME_TRACKING is enabled)
pub fn untrack_frame(frame: PhysFrame) {
    if DEBUG_FRAME_TRACKING {
        FRAME_TRACKER.lock().untrack(frame.addr);
    }
}

/// Get frame tracking statistics
pub fn tracking_stats() -> Option<FrameTrackingStats> {
    if DEBUG_FRAME_TRACKING {
        Some(FRAME_TRACKER.lock().stats())
    } else {
        None
    }
}

/// Get number of potentially leaked frames (only meaningful if DEBUG_FRAME_TRACKING is enabled)
#[allow(dead_code)]
pub fn leak_count() -> usize {
    if DEBUG_FRAME_TRACKING {
        FRAME_TRACKER.lock().leak_count()
    } else {
        0
    }
}


/// Bitmap-based physical memory allocator
struct BitmapAllocator {
    /// Bitmap where each bit represents a page (1 = free, 0 = used)
    bitmap: Vec<u64>,
    /// Base physical address of managed memory
    base_addr: usize,
    /// Total number of pages
    total_pages: usize,
    /// Number of free pages
    free_pages: usize,
    /// First page index to start searching from (optimization)
    next_free_hint: usize,
}

impl BitmapAllocator {
    const fn new() -> Self {
        Self {
            bitmap: Vec::new(),
            base_addr: 0,
            total_pages: 0,
            free_pages: 0,
            next_free_hint: 0,
        }
    }

    /// Initialize the allocator for a memory region
    fn init(&mut self, base: usize, size: usize, kernel_end: usize) {
        self.base_addr = base;
        self.total_pages = size / PAGE_SIZE;

        // Calculate bitmap size (64 pages per u64)
        let bitmap_size = self.total_pages.div_ceil(64);
        self.bitmap = alloc::vec![0u64; bitmap_size];

        // Mark all pages as free initially
        for i in 0..bitmap_size {
            self.bitmap[i] = !0u64; // All bits set = all free
        }

        // Mark pages below kernel_end as used (kernel code/data/heap)
        let kernel_pages = kernel_end.saturating_sub(base).div_ceil(PAGE_SIZE);
        for i in 0..kernel_pages {
            self.mark_used(i);
        }

        self.free_pages = self.total_pages - kernel_pages;
        self.next_free_hint = kernel_pages;

        // Handle partial last u64
        let remaining = self.total_pages % 64;
        if remaining != 0 {
            let last_idx = bitmap_size - 1;
            // Mask off bits beyond total_pages
            let mask = (1u64 << remaining) - 1;
            self.bitmap[last_idx] &= mask;
        }
    }

    /// Mark a page as used
    fn mark_used(&mut self, page_idx: usize) {
        let word_idx = page_idx / 64;
        let bit_idx = page_idx % 64;
        if word_idx < self.bitmap.len() {
            self.bitmap[word_idx] &= !(1u64 << bit_idx);
        }
    }

    /// Mark a page as free
    fn mark_free(&mut self, page_idx: usize) {
        let word_idx = page_idx / 64;
        let bit_idx = page_idx % 64;
        if word_idx < self.bitmap.len() {
            self.bitmap[word_idx] |= 1u64 << bit_idx;
        }
    }

    /// Check if a page is free
    fn is_free(&self, page_idx: usize) -> bool {
        let word_idx = page_idx / 64;
        let bit_idx = page_idx % 64;
        if word_idx < self.bitmap.len() {
            (self.bitmap[word_idx] & (1u64 << bit_idx)) != 0
        } else {
            false
        }
    }

    /// Is the page containing physical address `pa` marked free?
    ///
    /// Address-taking counterpart of [`is_free`], for callers holding a PA out of
    /// a page-table entry rather than a page index. Out-of-range addresses (below
    /// `base_addr`, or past the managed area) report `false`: this allocator has
    /// no claim on them, so "free" is not a statement it can make.
    fn is_page_free_pa(&self, pa: usize) -> bool {
        if pa < self.base_addr {
            return false;
        }
        let page_idx = (pa - self.base_addr) / PAGE_SIZE;
        page_idx < self.total_pages && self.is_free(page_idx)
    }

    /// Allocate a single page
    fn alloc_page(&mut self) -> Option<PhysFrame> {
        // Start searching from hint
        let start_word = self.next_free_hint / 64;

        for word_idx in start_word..self.bitmap.len() {
            if self.bitmap[word_idx] != 0 {
                // Found a word with at least one free bit
                let bit_idx = self.bitmap[word_idx].trailing_zeros() as usize;
                let page_idx = word_idx * 64 + bit_idx;

                if page_idx < self.total_pages {
                    self.mark_used(page_idx);
                    self.free_pages -= 1;
                    self.next_free_hint = page_idx + 1;

                    let addr = self.base_addr + page_idx * PAGE_SIZE;
                    return Some(PhysFrame::new(addr));
                }
            }
        }

        // Wrap around and search from beginning
        for word_idx in 0..start_word {
            if self.bitmap[word_idx] != 0 {
                let bit_idx = self.bitmap[word_idx].trailing_zeros() as usize;
                let page_idx = word_idx * 64 + bit_idx;

                if page_idx < self.total_pages {
                    self.mark_used(page_idx);
                    self.free_pages -= 1;
                    self.next_free_hint = page_idx + 1;

                    let addr = self.base_addr + page_idx * PAGE_SIZE;
                    return Some(PhysFrame::new(addr));
                }
            }
        }

        None
    }

    /// Allocate multiple pages in a single bitmap scan, appending them to `result`.
    /// Pages are not necessarily contiguous. Returns `false` (having rolled back)
    /// if fewer than `count` pages are available.
    ///
    /// **`result` must already have capacity for `count` frames, reserved by the
    /// caller BEFORE it took the PMM lock.** This function runs with `PMM` held,
    /// and a `Vec` growth here would call the kernel heap, whose own growth path
    /// (`PmmOomHandler::handle_oom`) takes `PMM` — the inversion that deadlocked
    /// `-j4` self-host builds. `push` beyond capacity would reallocate, so the
    /// loops below stop at exactly `count`. See
    /// `docs/reference/subsystems/memory.md` -> "PMM ↔ heap lock flow".
    fn alloc_pages_into(&mut self, count: usize, result: &mut alloc::vec::Vec<PhysFrame>) -> bool {
        debug_assert!(result.capacity() >= count, "caller must reserve before locking PMM");
        if count == 0 { return true; }
        if self.free_pages < count { return false; }

        let start_word = self.next_free_hint / 64;

        // First pass: from hint to end
        for word_idx in start_word..self.bitmap.len() {
            while self.bitmap[word_idx] != 0 {
                let bit_idx = self.bitmap[word_idx].trailing_zeros() as usize;
                let page_idx = word_idx * 64 + bit_idx;
                if page_idx >= self.total_pages { break; }
                self.mark_used(page_idx);
                self.free_pages -= 1;
                result.push(PhysFrame::new(self.base_addr + page_idx * PAGE_SIZE));
                if result.len() == count {
                    self.next_free_hint = page_idx + 1;
                    return true;
                }
            }
        }

        // Second pass: wrap around from beginning
        for word_idx in 0..start_word {
            while self.bitmap[word_idx] != 0 {
                let bit_idx = self.bitmap[word_idx].trailing_zeros() as usize;
                let page_idx = word_idx * 64 + bit_idx;
                if page_idx >= self.total_pages { break; }
                self.mark_used(page_idx);
                self.free_pages -= 1;
                result.push(PhysFrame::new(self.base_addr + page_idx * PAGE_SIZE));
                if result.len() == count {
                    self.next_free_hint = page_idx + 1;
                    return true;
                }
            }
        }

        // Not enough pages — roll back
        for frame in result.iter() {
            let page_idx = (frame.addr - self.base_addr) / PAGE_SIZE;
            self.mark_free(page_idx);
            self.free_pages += 1;
        }
        result.clear();
        false
    }

    /// Allocate `count` contiguous pages. Returns the first frame's address.
    /// Scans the bitmap for a run of `count` consecutive free pages.
    ///
    /// Starts the scan from `next_free_hint` to avoid re-scanning already-used
    /// kernel pages. Falls back to scanning from 0 if no run found after hint.
    fn alloc_pages_contiguous(&mut self, count: usize) -> Option<PhysFrame> {
        if count == 0 { return None; }
        if count == 1 { return self.alloc_page(); }
        if self.free_pages < count { return None; }

        let hint = self.next_free_hint;

        // Try two passes: start from hint, then wrap around from 0.
        // In the common boot case (32 thread stacks, large free block after kernel)
        // the run is found in the first pass near the hint position.
        for &start in &[hint, 0usize] {
            let mut run_start = 0;
            let mut run_len = 0;

            for page_idx in start..self.total_pages {
                if self.is_free(page_idx) {
                    if run_len == 0 {
                        run_start = page_idx;
                    }
                    run_len += 1;
                    if run_len == count {
                        // Found a run — mark all pages as used
                        for i in run_start..run_start + count {
                            self.mark_used(i);
                        }
                        self.free_pages -= count;
                        self.next_free_hint = run_start + count;
                        let addr = self.base_addr + run_start * PAGE_SIZE;
                        return Some(PhysFrame::new(addr));
                    }
                } else {
                    run_len = 0;
                }
            }

            if start == 0 { break; } // second pass already started from 0
        }

        None
    }

    /// Free `count` contiguous pages starting from `frame`.
    fn free_pages_contiguous(&mut self, frame: PhysFrame, count: usize) {
        if frame.addr < self.base_addr { return; }
        let start_page = (frame.addr - self.base_addr) / PAGE_SIZE;
        for i in 0..count {
            let page_idx = start_page + i;
            if page_idx < self.total_pages && !self.is_free(page_idx) {
                self.mark_free(page_idx);
                self.free_pages += 1;
            }
        }
        if start_page < self.next_free_hint {
            self.next_free_hint = start_page;
        }
    }

    /// Free a single page. Returns the outcome so the caller can keep
    /// `ALLOCATED_PAGES` exact (decrement only on a real allocated→free
    /// transition) and observe double-frees instead of corrupting the counter.
    fn free_page(&mut self, frame: PhysFrame) -> FreeOutcome {
        if frame.addr < self.base_addr {
            return FreeOutcome::OutOfRange;
        }
        let page_idx = (frame.addr - self.base_addr) / PAGE_SIZE;
        if page_idx >= self.total_pages {
            return FreeOutcome::OutOfRange;
        }
        if self.is_free(page_idx) {
            // Already free: re-marking it would double the page on the free
            // list and, after a reallocation, hand a live page to a second
            // owner — the heap corruption behind the Thread0 EL1 fault. Refuse
            // the re-mark and report it to the caller.
            return FreeOutcome::DoubleFree;
        }
        self.mark_free(page_idx);
        self.free_pages += 1;
        // Update hint if this is before current hint
        if page_idx < self.next_free_hint {
            self.next_free_hint = page_idx;
        }
        FreeOutcome::Freed
    }

}

/// Result of returning a page to the bitmap allocator.
#[derive(PartialEq, Eq, Clone, Copy)]
enum FreeOutcome {
    /// Page transitioned allocated→free (the normal case).
    Freed,
    /// Page was already free — a double-free, refused (see `free_page`).
    DoubleFree,
    /// Address is outside managed RAM (e.g. below base) — ignored, as before.
    OutOfRange,
}

/// Global physical memory allocator
static PMM: Spinlock<BitmapAllocator> = Spinlock::new(BitmapAllocator::new());

/// Statistics
static TOTAL_PAGES: AtomicUsize = AtomicUsize::new(0);
static ALLOCATED_PAGES: AtomicUsize = AtomicUsize::new(0);

/// Count of detected double-frees: a page returned to the PMM while already
/// free. The bitmap guard in `BitmapAllocator::free_page` refuses the re-mark,
/// so this is contained, but any non-zero value means some caller's free
/// obligations are out of sync with its allocations (a `track_user_frame` /
/// `cow_ref` desync) — the latent cause of heap corruption and the Thread0
/// EL1 fault. Surfaced in the periodic `[Mem]` stats line so it is visible
/// under load instead of silently corrupting the heap and faulting later.
static DOUBLE_FREE_COUNT: AtomicUsize = AtomicUsize::new(0);

// ============================================================================
// UAF hunt: "is this frame on the free list?" + a ring of recent frees
// ============================================================================
//
// The cargo null-`Rc` defect (proposals/CARGO_HEAP_NULL_RC.md) has the shape of a
// frame handed back to the PMM while a process still maps it: the next
// `alloc_page_zeroed` wipes the page under its live owner, so a qword holding a
// pointer reads back as 0 with no fault at the moment of corruption. Both probes
// below exist to make that state *observable at the anomaly* rather than
// inferred from the crash 20 ms downstream:
//
// - [`is_page_free`] answers "is the PA behind this live PTE simultaneously on
//   the free list?" in O(1). A `true` is proof of the premature free.
// - [`record_free`]/[`last_free_record`] name the pid that freed it last, so the
//   report points at a caller instead of a class.
//
// Both are cheap enough to leave on: one bitmap probe, and one lock-free store
// per `free_page`.

/// Slots in the recent-free ring. Power of two so the index wrap is a mask.
/// 4096 frees is ~2 s of build traffic at the observed rate — long enough that a
/// frame faulted on right after being freed is still in the window.
const FREE_LEDGER_SLOTS: usize = 4096;

static FREE_LEDGER_PA: [AtomicUsize; FREE_LEDGER_SLOTS] =
    [const { AtomicUsize::new(0) }; FREE_LEDGER_SLOTS];
/// `tid << 32 | seq` for the matching `FREE_LEDGER_PA` slot.
static FREE_LEDGER_META: [AtomicUsize; FREE_LEDGER_SLOTS] =
    [const { AtomicUsize::new(0) }; FREE_LEDGER_SLOTS];
static FREE_LEDGER_NEXT: AtomicUsize = AtomicUsize::new(0);

/// Note that `pa` was returned to the PMM by thread `tid`. Lock-free and
/// IRQ-safe: two relaxed stores and a fetch_add, so it is callable from inside
/// the PMM's own paths and from a `Process` drop.
fn record_free(pa: usize, tid: u32) {
    let seq = FREE_LEDGER_NEXT.fetch_add(1, Ordering::Relaxed);
    let idx = seq & (FREE_LEDGER_SLOTS - 1);
    // Meta first, PA last with Release: a reader that observes the PA sees the
    // meta that belongs to it (a torn pair would misattribute the free).
    FREE_LEDGER_META[idx].store(((tid as usize) << 32) | (seq & 0xFFFF_FFFF), Ordering::Relaxed);
    FREE_LEDGER_PA[idx].store(pa, Ordering::Release);
}

/// Most recent ledger entry for `pa`, as `(tid, seq)` — `None` if this frame has
/// not been freed inside the ring's window. Linear scan of the ring; only ever
/// called from an anomaly report, never on a hot path.
pub fn last_free_record(pa: usize) -> Option<(u32, u32)> {
    let mut best: Option<(u32, u32)> = None;
    for i in 0..FREE_LEDGER_SLOTS {
        if FREE_LEDGER_PA[i].load(Ordering::Acquire) != pa {
            continue;
        }
        let meta = FREE_LEDGER_META[i].load(Ordering::Relaxed);
        let (tid, seq) = ((meta >> 32) as u32, (meta & 0xFFFF_FFFF) as u32);
        if best.is_none_or(|(_, prev)| seq.wrapping_sub(prev) < u32::MAX / 2) {
            best = Some((tid, seq));
        }
    }
    best
}

/// Sequence number the next free will be stamped with. Paired with the `seq` in a
/// [`last_free_record`] result it gives the *distance* to that free — "freed 40
/// frees ago" and "freed 300 000 frees ago" are very different findings, and the
/// raw seq alone cannot tell them apart.
pub fn free_ledger_seq() -> u32 {
    (FREE_LEDGER_NEXT.load(Ordering::Relaxed) & 0xFFFF_FFFF) as u32
}

// ── CoW/share refcount event ledger ─────────────────────────────────────────
//
// `COW_REFCOUNTS` is the one counter that decides whether a frame is still owned:
// `free_page` frees only when a decrement reaches 0. The `EAGER-UPGRADE` anomaly
// is a page sitting read-only with that count at **0** while the process still
// maps and owns it — which means the count was driven to zero by more decrements
// than there were shares. This records every inc/dec so the anomaly report can
// print the frame's whole reference history and show that imbalance directly,
// with the thread behind each event.
//
// Same shape as the free ledger: fixed arrays, relaxed atomics, no allocation, so
// it is callable from inside `cow_ref_inc`/`cow_ref_dec`'s IRQ-masked sections.

/// Exact, durable record of "this frame has had at least one CoW/share reference
/// event since boot" — one bit per physical frame, sized from RAM at [`init`].
///
/// The ring below is a *recent window*: `cow_share_and_demote_range` emits one
/// event per shared page, so a single fork of a large process can evict the whole
/// ring. That makes "no events in the ring" ambiguous — aged out, or never
/// happened? — and the difference decides whether a page's history is even
/// relevant. This bitset answers it exactly: a clear bit means the frame has never
/// been through `cow_ref_inc`/`cow_ref_dec`, full stop.
///
/// Empty (and therefore a no-op in both directions) until `init` fills it, which
/// it skips when [`crate::config::COW_REF_LEDGER`] is off — so the low-RAM
/// profiles pay nothing, with no `cfg` on the call sites.
static COW_EVER: Spinlock<Vec<u64>> = Spinlock::new(Vec::new());

/// Base physical address the bitset is indexed from — the PMM's own `base_addr`.
/// The bitset has one bit per *managed* frame, so an absolute PA has to be
/// rebased before use, exactly like `BitmapAllocator::is_page_free_pa` does.
static COW_EVER_BASE: AtomicUsize = AtomicUsize::new(0);

/// Bit index for `pa`, or `None` when it falls outside managed RAM.
fn cow_ever_index(pa: usize, len_words: usize) -> Option<usize> {
    let base = COW_EVER_BASE.load(Ordering::Relaxed);
    let idx = pa.checked_sub(base)? / PAGE_SIZE;
    (idx / 64 < len_words).then_some(idx)
}

/// Note that `pa` has taken part in a reference event.
fn cow_ever_mark(pa: usize) {
    crate::irq::with_irqs_disabled(|| {
        let mut bits = COW_EVER.lock();
        let Some(idx) = cow_ever_index(pa, bits.len()) else { return };
        bits[idx / 64] |= 1u64 << (idx % 64);
    });
}

/// Has `pa` ever taken part in a reference event? `None` when the instrument is
/// off (which says nothing at all — not the same as "no").
///
/// The record is **per frame, since boot**, so it survives the frame being freed
/// and handed to a new owner. A set bit therefore means "this *frame* was shared
/// at some point", not "this frame's current owner shared it"; a clear bit is the
/// strong direction — proof the frame has never been through
/// `cow_ref_inc`/`cow_ref_dec` at all.
pub fn cow_ever_touched(pa: usize) -> Option<bool> {
    crate::irq::with_irqs_disabled(|| {
        let bits = COW_EVER.lock();
        if bits.is_empty() {
            return None;
        }
        // Out of managed range reads as "never" rather than `None`: the bitset is
        // initialised, it simply has nothing to say about non-RAM addresses.
        let Some(idx) = cow_ever_index(pa, bits.len()) else { return Some(false) };
        Some(bits[idx / 64] & (1u64 << (idx % 64)) != 0)
    })
}

/// Recent-window ring of reference events. Deliberately small: it carries the
/// *detail* (order, thread, before→after) for the last few thousand events, while
/// [`COW_EVER`] carries the durable yes/no for all of RAM.
const COW_LEDGER_SLOTS: usize = 4096;
static COW_LEDGER_PA: [AtomicUsize; COW_LEDGER_SLOTS] =
    [const { AtomicUsize::new(0) }; COW_LEDGER_SLOTS];
/// `tid << 32 | op << 24 | before << 12 | after` (op: 0 = inc, 1 = dec).
static COW_LEDGER_META: [AtomicUsize; COW_LEDGER_SLOTS] =
    [const { AtomicUsize::new(0) }; COW_LEDGER_SLOTS];
static COW_LEDGER_SEQ: [AtomicUsize; COW_LEDGER_SLOTS] =
    [const { AtomicUsize::new(0) }; COW_LEDGER_SLOTS];
static COW_LEDGER_NEXT: AtomicUsize = AtomicUsize::new(0);

fn cow_ledger_record(pa: usize, is_dec: bool, before: u16, after: u16) {
    if !crate::config::COW_REF_LEDGER {
        return;
    }
    cow_ever_mark(pa);
    let seq = COW_LEDGER_NEXT.fetch_add(1, Ordering::Relaxed);
    let idx = seq & (COW_LEDGER_SLOTS - 1);
    let tid = akuma_exec::threading::current_thread_id();
    let meta = (tid << 32)
        | (usize::from(is_dec) << 24)
        | (((before as usize) & 0xFFF) << 12)
        | ((after as usize) & 0xFFF);
    COW_LEDGER_SEQ[idx].store(seq, Ordering::Relaxed);
    COW_LEDGER_META[idx].store(meta, Ordering::Relaxed);
    COW_LEDGER_PA[idx].store(pa, Ordering::Release);
}

/// Number of reference events currently recorded for `pa`. Lets a boot self-test
/// assert the ledger is actually recording without parsing console output — its
/// only caller, so it compiles out wherever that suite does.
#[cfg(kernel_tests)]
pub fn cow_event_count(pa: usize) -> usize {
    if !crate::config::COW_REF_LEDGER {
        return 0;
    }
    (0..COW_LEDGER_SLOTS)
        .filter(|&i| COW_LEDGER_PA[i].load(Ordering::Acquire) == pa)
        .count()
}

/// Print every recorded reference event for `pa`, oldest first (up to 12).
///
/// Read the `before->after` column: a healthy frame's history alternates around
/// its share count and only reaches 0 on the last unmapper. A history whose
/// decrements outnumber its increments is the accounting bug the null-`Rc` hunt
/// is looking for, and the `tid` on the offending decrement names the caller.
pub fn print_cow_history(pa: usize) {
    if !crate::config::COW_REF_LEDGER {
        return;
    }
    // Selection-sort the matches by seq as we print: at most 12 entries, and this
    // runs once on an anomaly path, so an O(n²) pass over the hits is free.
    let mut printed = 0usize;
    let mut last_seq: Option<usize> = None;
    while printed < 12 {
        let mut best: Option<(usize, usize)> = None; // (seq, idx)
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
        crate::safe_print!(160, "  [COW-HIST] pa={:#x} seq={} tid={} {} {}->{}\n",
            pa, seq, meta >> 32,
            if (meta >> 24) & 1 == 1 { "dec" } else { "inc" },
            (meta >> 12) & 0xFFF, meta & 0xFFF);
        last_seq = Some(seq);
        printed += 1;
    }
    if printed == 0 {
        // Say which of the two possible meanings this is. "No events in the ring"
        // is ambiguous on its own — one fork can evict the whole window — so the
        // durable bitset is what makes the negative worth anything.
        let verdict = match cow_ever_touched(pa) {
            Some(false) => "NEVER shared (durable bitset clear)",
            // Per-frame and since boot, so this can be a *previous* owner of the
            // frame — enough to keep the frame in scope, not enough to convict.
            Some(true) => "shared at some point (frame, not necessarily this owner)",
            None => "instrument off — says nothing",
        };
        crate::safe_print!(160, "  [COW-HIST] pa={:#x} no events in window: {}\n", pa, verdict);
    }
}

/// Is the frame containing `pa` currently marked **free** in the PMM bitmap?
///
/// The invariant every caller of this cares about: a physical address reachable
/// through a live user PTE must never be free. When it is, the frame has two
/// owners and one of them is about to have its data zeroed by the next
/// `alloc_page_zeroed`.
///
/// Returns `false` for addresses outside managed RAM (nothing to say about them).
pub fn is_page_free(pa: usize) -> bool {
    crate::irq::with_irqs_disabled(|| {
        let pmm = PMM.lock();
        pmm.is_page_free_pa(pa)
    })
}

// ============================================================================
// UAF hunt: poison quarantine
// ============================================================================
//
// [`is_page_free`] proves a premature free only if something happens to *look* at
// the right frame at the right moment. This catches it unconditionally.
//
// A freed frame is filled with a PA-derived poison word and parked in a FIFO
// instead of going straight back to the bitmap. It is released only after
// `QUARANTINE_SLOTS` further frees, and the poison is verified on the way out. A
// frame that some process still maps will have been *written* during that window
// — heap traffic, a CoW break, anything — and the broken poison names the frame,
// the offset, and (via the ledger) the pid that gave it up. That turns a silent
// use-after-free into a deterministic log line at a bounded distance from the
// cause, instead of a null pointer read minutes later in another process.
//
// Costs: one 4 KiB store per free, and `QUARANTINE_SLOTS` pages (2 MiB) held back.
// The hold-back is given up the moment allocation actually fails
// ([`quarantine_drain_all`] sits on the pressure ladder), so it cannot turn into
// an OOM of its own.

/// Frames held back before release. 512 frees of lag is far more than the
/// window between a premature free and the first write through the stale mapping.
const QUARANTINE_SLOTS: usize = 512;

// The poison codec (`POISON_MAGIC`, `poison_word`, `poison_word_frame`) lives in
// `akuma_exec::memmath`, where the XOR/alignment/RAM-window decode is host-tested
// against the observed crash values instead of only from a booted kernel
// (docs/archive/TRIMMING_FAT_EMBARASSING_DUPLICATIONS.md §5.11).
use akuma_exec::memmath::poison_word;
pub use akuma_exec::memmath::poison_word_frame;

struct Quarantine {
    pa: [usize; QUARANTINE_SLOTS],
    head: usize,
    len: usize,
}

static QUARANTINE: Spinlock<Quarantine> =
    Spinlock::new(Quarantine { pa: [0; QUARANTINE_SLOTS], head: 0, len: 0 });

/// Frames written after being freed — every one is a use-after-free with a live
/// second owner. Surfaced in the `[Mem]` line; must be 0.
static UAF_DETECTED: AtomicUsize = AtomicUsize::new(0);

/// Direct-mapped "is this PA in the quarantine" table. Approximate (collisions
/// yield false negatives, never false positives), which is all a *detector* needs:
/// it upgrades double-frees that today slip through because the page was
/// re-allocated between the two frees.
const QUAR_PRESENT_SLOTS: usize = 2048;
static QUAR_PRESENT: [AtomicUsize; QUAR_PRESENT_SLOTS] =
    [const { AtomicUsize::new(0) }; QUAR_PRESENT_SLOTS];

#[inline]
fn quar_slot(pa: usize) -> usize {
    (pa >> 12) & (QUAR_PRESENT_SLOTS - 1)
}

/// Fill `pa`'s page with its poison word.
///
/// No cache maintenance: this VA and the user VA that may still map the frame are
/// both Normal Inner-Shareable cacheable, so they are hardware-coherent, and the
/// verify below reads back through this same identity mapping either way.
fn poison_page(pa: usize) {
    let p = akuma_exec::mmu::phys_to_virt(pa).cast::<u64>();
    let word = poison_word(pa);
    for i in 0..(PAGE_SIZE / 8) {
        unsafe { p.add(i).write_volatile(word) };
    }
}

/// First word of `pa`'s page that is no longer its poison, as `(byte_offset, got)`.
fn verify_poison(pa: usize) -> Option<(usize, u64)> {
    let p = akuma_exec::mmu::phys_to_virt(pa).cast::<u64>();
    let want = poison_word(pa);
    for i in 0..(PAGE_SIZE / 8) {
        let got = unsafe { p.add(i).read_volatile() };
        if got != want {
            return Some((i * 8, got));
        }
    }
    None
}

/// Report a value that decoded as quarantine poison, naming the frame it belonged
/// to, who freed it and how its reference count got to zero.
///
/// Called from the fault path with whatever registers the faulting instruction
/// used, so it costs nothing until something actually goes wrong — the opposite
/// trade from [`report_premature_free`], whose per-`free_page` scan is heavy
/// enough to perturb the very race it hunts (measured: 10 consecutive clean cold
/// builds with it armed, against a 25 % baseline).
pub fn report_poison_value(tag: &str, word: u64) {
    let Some(pa) = poison_word_frame(word) else { return };
    let (tid_freed, seq_freed) = last_free_record(pa).unwrap_or((u32::MAX, 0));
    crate::safe_print!(255,
        "[PMM-POISON] {}={:#x} is quarantine poison for pa={:#x} — the kernel FREED \
         this frame while the process still had it. freed_by=(tid={} seq={}) now_seq={} cow_ref={}\n",
        tag, word, pa, tid_freed, seq_freed, free_ledger_seq(), cow_ref_get(pa));
    if let Some((pid, tgid)) = surviving_mapper(pa) {
        crate::safe_print!(128,
            "  [PMM-POISON] pa={:#x} still tracked by pid={} tgid={}\n", pa, pid, tgid);
    }
    print_cow_history(pa);
}

/// Frames released while a live address space still tracked them — the premature
/// free itself, counted at the moment it happens rather than inferred from a
/// crash later. Surfaced in the `[Mem]` line; must be 0.
static PREMATURE_FREES: AtomicUsize = AtomicUsize::new(0);

/// Number of premature frees detected since boot (see [`PREMATURE_FREES`]).
pub fn premature_free_count() -> usize {
    PREMATURE_FREES.load(Ordering::Relaxed)
}

/// The first live address space, other than any this thread is tearing down, that
/// still tracks `pa` as one of its user frames — as `(pid, tgid)`.
///
/// # Why this is the instrument the poison check could not be
///
/// `verify_poison` only catches a **write** through a mapping that outlived its
/// free. The fatal access in the null-`Rc` defect is a **read** — a poisoned qword
/// loaded as a pointer and dereferenced — which leaves the frame's contents intact
/// and is therefore structurally invisible to it
/// (`docs/archive/CARGO_NULL_RC_MEMORY_REFERENCE_AUDIT.md` §13.8.2). This asks the
/// question directly instead: at the instant the last reference is dropped, does
/// anyone still hold the frame?
///
/// A hit is unambiguous. `free_page` only gets here when `cow_ref_dec` reported
/// the *last* reference, and `remove_user_frame` removes its entry **before**
/// handing the free obligation to its caller — so the releasing address space has
/// already stopped tracking it. Any other address space that still does means the
/// reference count reached zero while the frame was genuinely still owned, which
/// is §2.1's "one decrement too many frees a live page".
///
/// # Safety of calling this from inside `free_page`
///
/// `find_process` is lock-free (slot-state atomics + raw pointers under an IRQ
/// guard) — it is **not** the §10 re-entrancy hazard that bans `read_current_pid`
/// here, which deadlocks on `THREAD_PID_MAP`'s `Spinlock`. `tracks_user_frame`
/// does take a `Spinlock`, but no path holds a `user_frames` lock across a
/// `free_page` call: `remove_user_frame` drops it before returning `true`, and
/// address-space teardown moves the whole map out of the lock first
/// (`free_as_frames_now`). RETIRED slots are skipped by `find_process`, so a
/// process being reaped cannot report itself.
fn surviving_mapper(pa: usize) -> Option<(u32, u32)> {
    akuma_exec::process::table::find_process(|p| {
        if p.address_space.tracks_user_frame(pa) {
            Some((p.pid, p.tgid))
        } else {
            None
        }
    })
}

/// Report a frame that was freed while a live address space still mapped it.
///
/// Prints the victim as well as the culprit, because the two halves are what the
/// investigation was missing: `freed_by` names the thread that released it, and
/// `still_mapped_by` names the process that is about to read poison out of it. The
/// CoW history follows, since a premature free is by construction an accounting
/// bug and that ledger shows the inc/dec sequence that drove the count to zero.
fn report_premature_free(pa: usize) {
    let Some((pid, tgid)) = surviving_mapper(pa) else { return };
    let n = PREMATURE_FREES.fetch_add(1, Ordering::Relaxed);
    // Rate-limited: one bad decrement under load can repeat thousands of times,
    // and a console flood is its own defect (SERIAL_TRACE_TRAFFIC_AUDIT.md).
    if n >= 64 {
        return;
    }
    crate::safe_print!(255,
        "[PMM-PREMATURE] pa={:#x} freed by tid={} while still mapped by pid={} tgid={} \
         cow_ref={} seq={}\n",
        pa, akuma_exec::threading::current_thread_id(), pid, tgid,
        cow_ref_get(pa), free_ledger_seq());
    print_cow_history(pa);
}

/// Verify a frame leaving quarantine and hand it back to the bitmap.
fn release_from_quarantine(pa: usize) {
    if let Some((off, got)) = verify_poison(pa) {
        UAF_DETECTED.fetch_add(1, Ordering::Relaxed);
        let (tid_freed, seq_freed) = last_free_record(pa).unwrap_or((u32::MAX, 0));
        crate::safe_print!(255,
            "[PMM-UAF] pa={:#x} WRITTEN AFTER FREE: off={:#x} got={:#x} want={:#x} \
             freed_by=(tid={} seq={}) cow_ref={}\n",
            pa, off, got, poison_word(pa), tid_freed, seq_freed, cow_ref_get(pa));
        // The write proves someone held it; name them and show the accounting that
        // let go too early. Cheap — this path already runs only on an anomaly.
        if let Some((pid, tgid)) = surviving_mapper(pa) {
            crate::safe_print!(160,
                "  [PMM-UAF] pa={:#x} STILL MAPPED BY pid={} tgid={}\n", pa, pid, tgid);
        }
        print_cow_history(pa);
    }
    QUAR_PRESENT[quar_slot(pa)].compare_exchange(pa, 0, Ordering::AcqRel, Ordering::Relaxed).ok();

    let frame = PhysFrame::new(pa);
    let outcome = crate::irq::with_irqs_disabled(|| {
        let mut pmm = PMM.lock();
        // The bitmap must still read USED: quarantined frames are not free, and
        // nothing else may have marked this one free behind our back. If it did,
        // re-marking would double it on the free list — refuse and report instead.
        if pmm.is_page_free_pa(pa) {
            return FreeOutcome::DoubleFree;
        }
        pmm.free_page(frame)
    });
    match outcome {
        FreeOutcome::Freed => {
            ALLOCATED_PAGES.fetch_sub(1, Ordering::Relaxed);
            USER_PAGES_FREED.fetch_add(1, Ordering::Relaxed);
        }
        FreeOutcome::DoubleFree => { DOUBLE_FREE_COUNT.fetch_add(1, Ordering::Relaxed); }
        FreeOutcome::OutOfRange => {}
    }
}

/// Poison `pa` and park it. Returns the frame displaced from the ring's tail, if
/// the ring was full, for the caller to verify and release **outside** the lock.
/// `None` also covers the "already parked" case — a double free, reported here.
fn quarantine_push(pa: usize) -> Option<usize> {
    if QUAR_PRESENT[quar_slot(pa)].load(Ordering::Acquire) == pa {
        DOUBLE_FREE_COUNT.fetch_add(1, Ordering::Relaxed);
        let (tid_freed, seq_freed) = last_free_record(pa).unwrap_or((u32::MAX, 0));
        crate::safe_print!(192,
            "[PMM-QUAR-DF] pa={:#x} freed twice while quarantined, prev freed_by=(tid={} seq={})\n",
            pa, tid_freed, seq_freed);
        return None;
    }
    poison_page(pa);
    QUAR_PRESENT[quar_slot(pa)].store(pa, Ordering::Release);
    crate::irq::with_irqs_disabled(|| {
        let mut q = QUARANTINE.lock();
        if q.len < QUARANTINE_SLOTS {
            let idx = (q.head + q.len) % QUARANTINE_SLOTS;
            q.pa[idx] = pa;
            q.len += 1;
            None
        } else {
            let head = q.head;
            let evicted = q.pa[head];
            q.pa[head] = pa;
            q.head = (head + 1) % QUARANTINE_SLOTS;
            Some(evicted)
        }
    })
}

/// Empty the quarantine, verifying every frame on the way out. Sits on the
/// allocator's pressure ladder so held-back frames are always given up before an
/// allocation is allowed to fail: the instrument must never be the reason a build
/// OOMs. Returns the number of frames released.
pub fn quarantine_drain_all() -> usize {
    let mut released = 0usize;
    loop {
        let pa = crate::irq::with_irqs_disabled(|| {
            let mut q = QUARANTINE.lock();
            if q.len == 0 {
                return None;
            }
            let pa = q.pa[q.head];
            q.head = (q.head + 1) % QUARANTINE_SLOTS;
            q.len -= 1;
            Some(pa)
        });
        // Verify/release outside the quarantine lock — it takes the PMM lock.
        match pa {
            Some(pa) => { release_from_quarantine(pa); released += 1; }
            None => break,
        }
    }
    released
}

/// Discount `n` detections from the running total. Only for boot self-tests that
/// deliberately write through a freed frame to exercise the detector, so the
/// `[Mem] UAF=` signal keeps reflecting only *real* use-after-free writes.
/// Mirrors [`discount_double_frees`], including its build gating: the boot suite
/// is its only caller, so it compiles out wherever that suite does.
#[doc(hidden)]
#[cfg(kernel_tests)]
pub fn discount_uaf_detections(n: usize) {
    UAF_DETECTED.fetch_sub(n, Ordering::Relaxed);
}

/// Frames currently held in the quarantine, and the number of use-after-free
/// writes it has caught. Diagnostics (`[Mem]`).
pub fn quarantine_stats() -> (usize, usize) {
    let len = crate::irq::with_irqs_disabled(|| QUARANTINE.lock().len);
    (len, UAF_DETECTED.load(Ordering::Relaxed))
}

// ============================================================================
// Leak-debugging: per-site demand-paging frame counters (temporary instrument)
// ============================================================================
// Each demand-paging map site bumps the matching counter once per page it maps,
// and the page-free path bumps PAGES_FREED. Dumped in the crash handler and the
// periodic [Mem] line so a memory spike can be attributed to a specific path.
pub static DP_FILE_PAGES: AtomicUsize = AtomicUsize::new(0);
pub static DP_ANON_PAGES: AtomicUsize = AtomicUsize::new(0);
pub static DP_COW_PAGES: AtomicUsize = AtomicUsize::new(0);
pub static DP_PROTNONE_PAGES: AtomicUsize = AtomicUsize::new(0);
pub static EAGER_MMAP_PAGES: AtomicUsize = AtomicUsize::new(0);
pub static USER_PAGES_FREED: AtomicUsize = AtomicUsize::new(0);

#[inline]
pub fn dp_count(counter: &AtomicUsize, n: usize) {
    counter.fetch_add(n, Ordering::Relaxed);
}

/// One-line dump of the demand-paging frame attribution counters, written into
/// the caller's buffer. Takes a `&mut dyn Write` (instead of returning a
/// `String`) so it stays heap-free — this is reached from the sync-EL1 crash
/// handler, which must not touch the allocator (see ALLOC_PRINT_AUDIT.md §6.3).
pub fn dp_counters_line(w: &mut dyn core::fmt::Write) {
    let _ = write!(
        w,
        "file={} anon={} cow={} protnone={} eager={} freed={}",
        DP_FILE_PAGES.load(Ordering::Relaxed),
        DP_ANON_PAGES.load(Ordering::Relaxed),
        DP_COW_PAGES.load(Ordering::Relaxed),
        DP_PROTNONE_PAGES.load(Ordering::Relaxed),
        EAGER_MMAP_PAGES.load(Ordering::Relaxed),
        USER_PAGES_FREED.load(Ordering::Relaxed),
    );
}

/// Initialize the physical memory manager
///
/// # Arguments
/// * `ram_base` - Physical base address of RAM
/// * `ram_size` - Total RAM size in bytes
/// * `kernel_end` - End address of kernel (code + data + heap)
pub fn init(ram_base: usize, ram_size: usize, kernel_end: usize) {
    let mut pmm = PMM.lock();
    pmm.init(ram_base, ram_size, kernel_end);

    TOTAL_PAGES.store(pmm.total_pages, Ordering::Release);
    ALLOCATED_PAGES.store(pmm.total_pages - pmm.free_pages, Ordering::Release);

    // One bit per frame for the durable "was this ever CoW-shared" record — 1 KiB
    // per 32 MiB of RAM. Left empty (and so inert) when the instrument is off,
    // which is what keeps the low-RAM profiles free of it.
    if crate::config::COW_REF_LEDGER {
        let words = pmm.total_pages.div_ceil(64);
        COW_EVER_BASE.store(pmm.base_addr, Ordering::Release);
        *COW_EVER.lock() = alloc::vec![0u64; words];
    }
}

/// Allocate a single physical page
pub fn alloc_page() -> Option<PhysFrame> {
    if let Some(frame) = alloc_page_once() {
        return Some(frame);
    }
    // Out of free pages: give back whatever the UAF quarantine is holding first.
    // It parks up to `QUARANTINE_SLOTS` frames purely to catch writes through
    // stale mappings, and that debt must never be the reason an allocation fails.
    if crate::config::PMM_UAF_QUARANTINE
        && quarantine_drain_all() > 0
        && let Some(frame) = alloc_page_once()
    {
        return Some(frame);
    }
    // Still short: try clawing back fully-free kernel-heap spans (the
    // heap grows one-way via the OOM handler; this returns the watermark), then
    // retry once. `reclaim_to_pmm` is a no-op if the heap lock is already held
    // (i.e. we were reentered from the allocator), so this can't deadlock.
    if crate::allocator::reclaim_to_pmm() > 0 {
        alloc_page_once()
    } else {
        None
    }
}

fn alloc_page_once() -> Option<PhysFrame> {
    // CRITICAL: Disable IRQs to prevent deadlock!
    // If a timer fires while holding PMM lock, scheduler switches to another
    // thread which tries to allocate -> spins forever waiting for lock.
    crate::irq::with_irqs_disabled(|| {
        let mut pmm = PMM.lock();
        let result = pmm.alloc_page();
        if result.is_some() {
            ALLOCATED_PAGES.fetch_add(1, Ordering::Relaxed);
        }
        result
    })
}

/// Free a single physical page.
/// If the frame is CoW-shared (refcount > 0), only decrements the refcount
/// instead of actually freeing.  The physical frame is freed when the last
/// reference is dropped.
pub fn free_page(frame: PhysFrame) {
    // Check CoW refcount: if shared, just decrement instead of freeing.
    if !cow_ref_dec(frame.addr) {
        // Still shared by other processes — don't free the physical page.
        return;
    }

    // Untrack BEFORE freeing to prevent race condition:
    // If we free first then untrack, another CPU could reallocate the same
    // frame and track it before we untrack, causing us to remove their tracking.
    untrack_frame(frame);

    // Name the thread that gave this frame up, so an anomaly report on a
    // still-mapped frame can point at the caller that released it (see
    // `last_free_record`). Deliberately the *thread id* and not
    // `read_current_pid()`: that resolves through `THREAD_PID_MAP` and the process
    // table, and `free_page` is reachable from inside both (a `Process` drop frees
    // every frame of its address space), so calling it here deadlocks on a
    // non-reentrant `Spinlock`. `current_thread_id` is a register read.
    record_free(frame.addr, akuma_exec::threading::current_thread_id() as u32);

    // Ask, at the instant the last reference is dropped, whether anyone still
    // holds this frame. A hit is the premature free itself — caught at its own
    // call site with the freeing thread live, instead of inferred from a poisoned
    // pointer minutes later in another process. Unlike the poison check below it
    // sees read-only survivors too, which is the class that kills cargo
    // (docs/archive/CARGO_NULL_RC_MEMORY_REFERENCE_AUDIT.md §13.8).
    if crate::config::PMM_PREMATURE_FREE_CHECK {
        report_premature_free(frame.addr);
    }

    // Poison and park instead of releasing immediately, so a write through a
    // mapping that outlived this free is caught with the frame still named.
    if crate::config::PMM_UAF_QUARANTINE {
        if let Some(evicted) = quarantine_push(frame.addr) {
            release_from_quarantine(evicted);
        }
        return;
    }

    // CRITICAL: Disable IRQs to prevent deadlock!
    let outcome = crate::irq::with_irqs_disabled(|| {
        let mut pmm = PMM.lock();
        pmm.free_page(frame)
    });
    match outcome {
        // Only a real allocated→free transition adjusts the counter. The old
        // code decremented unconditionally, so a double-free silently drifted
        // ALLOCATED_PAGES even when the bitmap guard made the free a no-op.
        FreeOutcome::Freed => {
            ALLOCATED_PAGES.fetch_sub(1, Ordering::Relaxed);
            USER_PAGES_FREED.fetch_add(1, Ordering::Relaxed);
        }
        FreeOutcome::DoubleFree => { DOUBLE_FREE_COUNT.fetch_add(1, Ordering::Relaxed); }
        FreeOutcome::OutOfRange => {}
    }
}

/// Number of double-frees detected since boot (see `DOUBLE_FREE_COUNT`).
/// Non-zero indicates a `track_user_frame`/`cow_ref` desync in some caller.
pub fn double_free_count() -> usize {
    DOUBLE_FREE_COUNT.load(Ordering::Relaxed)
}

/// Discount `n` double-frees from the running total. Only for boot self-tests
/// that deliberately trigger a double-free to exercise the guard, so the
/// `[Mem]` signal keeps reflecting only *real* desyncs and operators aren't
/// misled by a test artifact.
#[doc(hidden)]
#[cfg(kernel_tests)]
pub fn discount_double_frees(n: usize) {
    DOUBLE_FREE_COUNT.fetch_sub(n, Ordering::Relaxed);
}

/// Allocate `count` contiguous zeroed physical pages.
/// Returns the first frame if successful. Pages are physically contiguous.
pub fn alloc_pages_contiguous_zeroed(count: usize) -> Option<PhysFrame> {
    use akuma_exec::mmu::phys_to_virt;

    let alloc_once = || crate::irq::with_irqs_disabled(|| {
        let mut pmm = PMM.lock();
        let result = pmm.alloc_pages_contiguous(count)?;
        ALLOCATED_PAGES.fetch_add(count, Ordering::Relaxed);
        Some(result)
    });

    let frame = match alloc_once() {
        Some(f) => f,
        None => {
            // No contiguous run available: reclaim fully-free heap spans (which
            // returns whole 256 KB-aligned regions, helping contiguous demand)
            // and retry once. No-op + no deadlock if the heap lock is held.
            if crate::allocator::reclaim_to_pmm() > 0 {
                alloc_once()?
            } else {
                return None;
            }
        }
    };

    unsafe {
        let virt_addr = phys_to_virt(frame.addr);
        core::ptr::write_bytes(virt_addr, 0, count * PAGE_SIZE);

        const CACHE_LINE_SIZE: usize = 64;
        let mut addr = virt_addr as usize;
        let end = addr + count * PAGE_SIZE;
        while addr < end {
            core::arch::asm!("dc cvac, {addr}", addr = in(reg) addr);
            addr += CACHE_LINE_SIZE;
        }
        core::arch::asm!("dsb ish");
    }
    Some(frame)
}

/// Free `count` contiguous physical pages starting from `frame`.
pub fn free_pages_contiguous(frame: PhysFrame, count: usize) {
    crate::irq::with_irqs_disabled(|| {
        let mut pmm = PMM.lock();
        pmm.free_pages_contiguous(frame, count);
        ALLOCATED_PAGES.fetch_sub(count, Ordering::Relaxed);
    });
}

/// Get physical memory statistics
pub fn stats() -> (usize, usize, usize) {
    let total = TOTAL_PAGES.load(Ordering::Relaxed);
    let allocated = ALLOCATED_PAGES.load(Ordering::Relaxed);
    let free = total.saturating_sub(allocated);
    (total, allocated, free)
}

/// Get number of free pages
pub fn free_count() -> usize {
    let (total, allocated, _) = stats();
    total.saturating_sub(allocated)
}

/// Get total number of pages
pub fn total_count() -> usize {
    TOTAL_PAGES.load(Ordering::Relaxed)
}

/// Allocate a zeroed page
pub fn alloc_page_zeroed() -> Option<PhysFrame> {
    use akuma_exec::mmu::phys_to_virt;

    let frame = alloc_page()?;
    unsafe {
        // Use phys_to_virt to get a valid kernel VA for the physical address
        // This ensures the write works regardless of current TTBR0 state
        let virt_addr = phys_to_virt(frame.addr);
        core::ptr::write_bytes(virt_addr, 0, PAGE_SIZE);

        // Clean data cache for entire page to ensure zeros are visible through
        // other VA mappings (e.g., user VA vs kernel identity mapping)
        // ARM64 cache line is typically 64 bytes
        const CACHE_LINE_SIZE: usize = 64;
        let mut addr = virt_addr as usize;
        let end = addr + PAGE_SIZE;
        while addr < end {
            core::arch::asm!(
                "dc cvac, {addr}",  // Clean data cache by VA to PoC
                addr = in(reg) addr,
            );
            addr += CACHE_LINE_SIZE;
        }
        core::arch::asm!("dsb ish"); // Data synchronization barrier
    }
    Some(frame)
}

// The reserve, its predicate and the readahead budget now live in
// `akuma_exec::memmath` — pure arithmetic, host-tested there instead of from the
// boot suite (docs/archive/TRIMMING_FAT_EMBARASSING_DUPLICATIONS.md §5.11).
// Re-exported so every existing `pmm::USER_PAGE_RESERVE` /
// `pmm::user_readahead_budget` caller is unchanged and there is still exactly one
// definition.
//
// `user_alloc_would_starve` is deliberately NOT re-exported: since the reclaim
// escalation became `memmath::next_reclaim_step`, the predicate has exactly one
// consumer — that decision function — and nothing in the kernel asks "would this
// starve?" without also needing the answer to "so what do I do about it?". Import it
// from `memmath` directly if that ever stops being true.
pub use akuma_exec::memmath::{USER_PAGE_RESERVE, user_readahead_budget};

/// Allocate a zeroed page for a **user** demand-paging fault (anonymous fill,
/// ELF demand-load, reserved-region commit). Returns `None` once free PMM has
/// fallen to [`USER_PAGE_RESERVE`], so the caller treats it as OOM and SIGSEGVs
/// the faulting process — leaving the reserve for page-table completion and the
/// kill path. Kernel-internal callers (page tables, heap growth) keep calling
/// [`alloc_page_zeroed`], which is allowed to dip into the reserve.
pub fn alloc_page_zeroed_user() -> Option<PhysFrame> {
    use akuma_exec::memmath::{ReclaimStep, next_reclaim_step};

    // The ORDER and the give-up decision live in `akuma_exec::memmath`
    // (`next_reclaim_step`, host-tested); this loop is only the effects. Progress is
    // judged by re-reading `free_count()` on every turn, never by a rung's return
    // value — `drain_retired_under_pressure` declines silently inside its cooldown.
    //
    // Lock context, common to all four rungs: every caller of this function is the
    // EL0 fault handler, which holds neither `as_lock` nor the PMM lock here — the
    // invariant `reclaim_clean_file_pages` already relies on (it takes `as_lock` per
    // page). `drain_retired_under_pressure` additionally declines when nothing is
    // parked and guards against reentry. See `process::reclaim`.
    let mut done = None;
    loop {
        let step = next_reclaim_step(free_count(), done);
        match step {
            ReclaimStep::Allocate => return alloc_page_zeroed(),
            // Out of rungs. The caller treats this as OOM and SIGSEGVs the faulting
            // process, leaving the reserve for page-table completion and the kill
            // path. Note the known gap `next_reclaim_step` documents: memory parked
            // more recently than `PROCESS_RECLAIM_COOLDOWN_US` is not collectable by
            // any rung, so this can be an invented OOM.
            ReclaimStep::GiveUp => return None,
            // Return any fully-free heap watermark to the PMM — mirrors
            // `alloc_page`'s reclaim-under-pressure. Costs nothing anyone misses.
            ReclaimStep::ReclaimHeap => {
                crate::allocator::reclaim_to_pmm();
            }
            // Hand back the memory of processes that are already DEAD before
            // evicting anything from a process that is still alive. Since Phase 7e's
            // "Free" half a reaped process's whole address space sits in a RETIRED
            // slot until a collector drops it, and the only steady-state collector
            // (netpoll_maint, 100 ms) is exactly what this kind of pressure starves
            // — measured: ~35 K pages parked while the PMM sat pinned at the reserve
            // (docs/archive/OOM_KILL_DEFERRED_RECLAIM_GAP.md). Honors the RETIRE
            // cooldown (never the `_force` variant), so a process killed microseconds
            // ago is deliberately NOT collected here and we fall through to eviction.
            ReclaimStep::DrainRetired => {
                akuma_exec::process::reclaim::drain_retired_under_pressure();
            }
            // Page out clean, read-only file-backed pages (e.g. model weights mmap'd
            // larger than RAM) and let them re-fault from the file. This is the
            // page-reclaim half of demand paging — what lets a file mmap bigger than
            // physical RAM make progress instead of the process OOM-ing here. A batch,
            // to amortise the sweep over many subsequent faults.
            ReclaimStep::EvictCleanFilePages => {
                akuma_exec::process::reclaim_clean_file_pages(USER_RECLAIM_BATCH);
            }
            // The sweep above unmaps pages but cannot free a frame the shared
            // file-page cache still references, so a cache holding unmapped pages
            // would let reclaim report progress while freeing nothing. Drop the
            // entries that actually own memory (no remaining mappers).
            ReclaimStep::ShrinkPageCache => {
                crate::file_page_cache::shrink(USER_RECLAIM_BATCH);
            }
        }
        done = Some(step);
    }
}

/// Pages to reclaim per memory-pressure event in [`alloc_page_zeroed_user`].
/// Sized to free a readahead batch (256) plus headroom, so a faulting process
/// doesn't re-enter reclaim on every single page.
const USER_RECLAIM_BATCH: usize = 512;

/// Allocate multiple zeroed pages in a single lock acquisition.
/// All pages are zeroed and cache-cleaned with a single DSB at the end.
/// Returns None (without partial allocation) if `count` pages aren't available.
pub fn alloc_pages_zeroed(count: usize) -> Option<alloc::vec::Vec<PhysFrame>> {
    use akuma_exec::mmu::phys_to_virt;

    // Reserve the result buffer BEFORE taking PMM. Growing a `Vec` calls the kernel
    // heap, and the heap's growth path (`PmmOomHandler::handle_oom`) takes PMM — so
    // allocating under the lock closes a TALC<->PMM cycle. That deadlocked `-j4`
    // self-host builds (5 of 6 deaths in a 22-round campaign, 2026-08-08) and can
    // even self-deadlock one core, since these spinlocks are not reentrant.
    // See docs/reference/subsystems/memory.md -> "PMM ↔ heap lock flow".
    let mut frames: alloc::vec::Vec<PhysFrame> = alloc::vec::Vec::new();
    if frames.try_reserve_exact(count).is_err() {
        return None;
    }

    let ok = crate::irq::with_irqs_disabled(|| {
        let mut pmm = PMM.lock();
        if !pmm.alloc_pages_into(count, &mut frames) {
            return false;
        }
        ALLOCATED_PAGES.fetch_add(count, Ordering::Relaxed);
        true
    });
    if !ok {
        return None;
    }

    unsafe {
        const CACHE_LINE_SIZE: usize = 64;
        for frame in &frames {
            let virt_addr = phys_to_virt(frame.addr);
            core::ptr::write_bytes(virt_addr, 0, PAGE_SIZE);

            let mut addr = virt_addr as usize;
            let end = addr + PAGE_SIZE;
            while addr < end {
                core::arch::asm!("dc cvac, {addr}", addr = in(reg) addr);
                addr += CACHE_LINE_SIZE;
            }
        }
        core::arch::asm!("dsb ish");
    }
    Some(frames)
}

// ============================================================================
// Copy-on-Write Reference Counting
// ============================================================================

/// Tracks CoW-shared physical frames.  Only pages that are actually shared
/// between parent and child after fork get entries here.  Non-shared pages
/// have no overhead.
static COW_REFCOUNTS: Spinlock<BTreeMap<usize, u16>> = Spinlock::new(BTreeMap::new());

#[allow(dead_code)]
/// Increment the CoW reference count for a physical address.
/// First call for a new address inserts it with count=2 (parent + child).
/// Subsequent calls increment by 1 (additional fork children).
pub fn cow_ref_inc(pa: usize) {
    let (before, after) = crate::irq::with_irqs_disabled(|| {
        let mut table = COW_REFCOUNTS.lock();
        let entry = table.entry(pa).or_insert(1);
        let before = *entry;
        *entry = entry.saturating_add(1);
        (before, *entry)
    });
    // Recorded outside the `COW_REFCOUNTS` hold: the ledger takes no lock, but
    // keeping it out keeps the hot path's critical section exactly as short as it
    // was before the instrument existed.
    cow_ledger_record(pa, false, before, after);
}

/// Decrement the CoW reference count.  Returns true if the count reached 0
/// (meaning the caller should free the physical frame).  Removes the entry
/// from the table when count reaches 0 to avoid unbounded growth.
pub fn cow_ref_dec(pa: usize) -> bool {
    let (before, after, last) = crate::irq::with_irqs_disabled(|| {
        let mut table = COW_REFCOUNTS.lock();
        match table.get_mut(&pa) {
            Some(count) => {
                let before = *count;
                *count = count.saturating_sub(1);
                if *count == 0 {
                    table.remove(&pa);
                    (before, 0, true)
                } else {
                    (before, *count, false)
                }
            }
            // Not tracked → single owner → safe to free. Recorded as `0->0` so an
            // untracked decrement is still visible in the history: a run of these
            // on a frame that *should* be shared is itself the desync.
            None => (0, 0, true),
        }
    });
    cow_ledger_record(pa, true, before, after);
    last
}

#[allow(dead_code)]
/// Get the current CoW reference count for a physical address.
/// Returns 0 if the address is not in the CoW table (not shared).
pub fn cow_ref_get(pa: usize) -> u16 {
    crate::irq::with_irqs_disabled(|| {
        COW_REFCOUNTS.lock().get(&pa).copied().unwrap_or(0)
    })
}

#[allow(dead_code)]
/// Number of entries in the CoW refcount table (for diagnostics).
pub fn cow_ref_count() -> usize {
    crate::irq::with_irqs_disabled(|| COW_REFCOUNTS.lock().len())
}

