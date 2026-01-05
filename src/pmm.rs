//! Physical Memory Manager (PMM)
//!
//! Manages physical page allocation using a bitmap allocator.
//! Each bit in the bitmap represents a 4KB page.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};
use spinning_top::Spinlock;

use crate::mmu::PAGE_SIZE;

// ============================================================================
// Debug Frame Tracking
// ============================================================================

/// Enable debug frame tracking (adds overhead but helps find leaks)
/// Set to true to track all frame allocations with metadata
pub const DEBUG_FRAME_TRACKING: bool = true;

/// Allocation source for debug tracking
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameSource {
    /// Kernel heap allocation
    Kernel,
    /// User page table
    UserPageTable,
    /// User data page (mmap/brk)
    UserData,
    /// ELF loader (code/data segments)
    ElfLoader,
    /// Unknown/unspecified
    Unknown,
}

/// Information about a tracked frame allocation
#[derive(Debug, Clone)]
pub struct FrameInfo {
    /// Source of the allocation
    pub source: FrameSource,
    /// Process ID (0 for kernel)
    pub pid: u32,
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

    fn track(&mut self, addr: usize, source: FrameSource, pid: u32) {
        if let Some(old) = self.allocations.insert(addr, FrameInfo { source, pid }) {
            // Double allocation detected!
            crate::console::print(&alloc::format!(
                "[PMM WARN] Double allocation at 0x{:x}! Old: {:?}, New: {:?}\n",
                addr, old.source, source
            ));
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
                FrameSource::UserPageTable => self.user_page_table_count = self.user_page_table_count.saturating_sub(1),
                FrameSource::UserData => self.user_data_count = self.user_data_count.saturating_sub(1),
                FrameSource::ElfLoader => self.elf_loader_count = self.elf_loader_count.saturating_sub(1),
                FrameSource::Unknown => self.unknown_count = self.unknown_count.saturating_sub(1),
            }
            self.total_untracked += 1;
            Some(info)
        } else {
            crate::console::print(&alloc::format!(
                "[PMM WARN] Freeing untracked frame at 0x{:x}\n", addr
            ));
            None
        }
    }
    
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
pub fn track_frame(frame: PhysFrame, source: FrameSource, pid: u32) {
    if DEBUG_FRAME_TRACKING {
        FRAME_TRACKER.lock().track(frame.addr, source, pid);
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
pub fn leak_count() -> usize {
    if DEBUG_FRAME_TRACKING {
        FRAME_TRACKER.lock().leak_count()
    } else {
        0
    }
}

/// Physical page frame
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PhysFrame {
    pub addr: usize,
}

impl PhysFrame {
    pub const fn new(addr: usize) -> Self {
        Self {
            addr: addr & !(PAGE_SIZE - 1),
        }
    }

    pub fn containing_address(addr: usize) -> Self {
        Self::new(addr)
    }

    pub fn start_address(&self) -> usize {
        self.addr
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
        let bitmap_size = (self.total_pages + 63) / 64;
        self.bitmap = alloc::vec![0u64; bitmap_size];

        // Mark all pages as free initially
        for i in 0..bitmap_size {
            self.bitmap[i] = !0u64; // All bits set = all free
        }

        // Mark pages below kernel_end as used (kernel code/data/heap)
        let kernel_pages = (kernel_end.saturating_sub(base) + PAGE_SIZE - 1) / PAGE_SIZE;
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

    /// Allocate contiguous pages
    fn alloc_pages(&mut self, count: usize) -> Option<PhysFrame> {
        if count == 0 {
            return None;
        }
        if count == 1 {
            return self.alloc_page();
        }

        // Search for contiguous free pages
        let mut start = 0;
        let mut found = 0;

        for page_idx in 0..self.total_pages {
            if self.is_free(page_idx) {
                if found == 0 {
                    start = page_idx;
                }
                found += 1;
                if found == count {
                    // Found enough contiguous pages
                    for i in start..start + count {
                        self.mark_used(i);
                    }
                    self.free_pages -= count;
                    self.next_free_hint = start + count;
                    let addr = self.base_addr + start * PAGE_SIZE;
                    return Some(PhysFrame::new(addr));
                }
            } else {
                found = 0;
            }
        }

        None
    }

    /// Free a single page
    fn free_page(&mut self, frame: PhysFrame) {
        if frame.addr < self.base_addr {
            return;
        }

        let page_idx = (frame.addr - self.base_addr) / PAGE_SIZE;
        if page_idx < self.total_pages && !self.is_free(page_idx) {
            self.mark_free(page_idx);
            self.free_pages += 1;

            // Update hint if this is before current hint
            if page_idx < self.next_free_hint {
                self.next_free_hint = page_idx;
            }
        }
    }

    /// Free contiguous pages
    fn free_pages(&mut self, frame: PhysFrame, count: usize) {
        for i in 0..count {
            self.free_page(PhysFrame::new(frame.addr + i * PAGE_SIZE));
        }
    }
}

/// Global physical memory allocator
static PMM: Spinlock<BitmapAllocator> = Spinlock::new(BitmapAllocator::new());

/// Statistics
static TOTAL_PAGES: AtomicUsize = AtomicUsize::new(0);
static ALLOCATED_PAGES: AtomicUsize = AtomicUsize::new(0);

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
}

/// Allocate a single physical page
pub fn alloc_page() -> Option<PhysFrame> {
    let mut pmm = PMM.lock();
    let result = pmm.alloc_page();
    if result.is_some() {
        ALLOCATED_PAGES.fetch_add(1, Ordering::Relaxed);
    }
    result
}

/// Allocate contiguous physical pages
pub fn alloc_pages(count: usize) -> Option<PhysFrame> {
    let mut pmm = PMM.lock();
    let result = pmm.alloc_pages(count);
    if result.is_some() {
        ALLOCATED_PAGES.fetch_add(count, Ordering::Relaxed);
    }
    result
}

/// Free a single physical page
pub fn free_page(frame: PhysFrame) {
    // Untrack BEFORE freeing to prevent race condition:
    // If we free first then untrack, another CPU could reallocate the same
    // frame and track it before we untrack, causing us to remove their tracking.
    untrack_frame(frame);
    
    let mut pmm = PMM.lock();
    pmm.free_page(frame);
    ALLOCATED_PAGES.fetch_sub(1, Ordering::Relaxed);
}

/// Free contiguous physical pages
pub fn free_pages(frame: PhysFrame, count: usize) {
    let mut pmm = PMM.lock();
    pmm.free_pages(frame, count);
    ALLOCATED_PAGES.fetch_sub(count, Ordering::Relaxed);
}

/// Get physical memory statistics
pub fn stats() -> (usize, usize, usize) {
    let total = TOTAL_PAGES.load(Ordering::Relaxed);
    let allocated = ALLOCATED_PAGES.load(Ordering::Relaxed);
    let free = total.saturating_sub(allocated);
    (total, allocated, free)
}

/// Allocate a zeroed page
pub fn alloc_page_zeroed() -> Option<PhysFrame> {
    use crate::mmu::phys_to_virt;
    
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
        core::arch::asm!("dsb ish");  // Data synchronization barrier
    }
    Some(frame)
}

/// Allocate zeroed contiguous pages
pub fn alloc_pages_zeroed(count: usize) -> Option<PhysFrame> {
    use crate::mmu::phys_to_virt;
    
    let frame = alloc_pages(count)?;
    let total_size = PAGE_SIZE * count;
    unsafe {
        // Use phys_to_virt to get a valid kernel VA for the physical address
        let virt_addr = phys_to_virt(frame.addr);
        core::ptr::write_bytes(virt_addr, 0, total_size);
        
        // Clean data cache for all pages
        const CACHE_LINE_SIZE: usize = 64;
        let mut addr = virt_addr as usize;
        let end = addr + total_size;
        while addr < end {
            core::arch::asm!(
                "dc cvac, {addr}",
                addr = in(reg) addr,
            );
            addr += CACHE_LINE_SIZE;
        }
        core::arch::asm!("dsb ish");
    }
    Some(frame)
}

