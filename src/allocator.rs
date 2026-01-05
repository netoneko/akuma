//! Kernel memory allocator with page-based and talc-based options
//!
//! The page-based allocator is ported from libakuma's mmap allocator.
//! It allocates whole pages for each allocation, fixing layout-sensitive
//! heap corruption bugs at the cost of higher memory usage.

use core::alloc::{GlobalAlloc, Layout};
use core::ptr;
use core::sync::atomic::{AtomicUsize, Ordering};
use spinning_top::Spinlock;
use talc::ErrOnOom;
use talc::{Span, Talc};

/// Set to true to use page-based allocation (like userspace mmap allocator)
/// This fixes layout-sensitive heap corruption bugs but uses more memory.
/// Deallocation properly returns pages to PMM.
pub const USE_PAGE_ALLOCATOR: bool = false; // DOES NOT ACTUALLY WORK

const PAGE_SIZE: usize = 4096;

/// Flag indicating PMM is ready for use (set after PMM init completes)
static PMM_READY: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Signal that PMM is ready for use by the page allocator
pub fn mark_pmm_ready() {
    PMM_READY.store(true, Ordering::Release);
}

/// Check if PMM is ready
fn is_pmm_ready() -> bool {
    PMM_READY.load(Ordering::Acquire)
}

#[global_allocator]
static ALLOCATOR: HybridAllocator = HybridAllocator;

static TALC: Spinlock<Talc<ErrOnOom>> = Spinlock::new(Talc::new(ErrOnOom));

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
// Hybrid Allocator (switches between page-based and talc-based)
// ============================================================================

struct HybridAllocator;

unsafe impl GlobalAlloc for HybridAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        unsafe {
            // Use page allocator only if enabled AND PMM is ready
            if USE_PAGE_ALLOCATOR && is_pmm_ready() {
                page_alloc(layout)
            } else {
                talc_alloc(layout)
            }
        }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        unsafe {
            let ptr = self.alloc(layout);
            if !ptr.is_null() {
                // Page allocator already returns zeroed pages
                if !(USE_PAGE_ALLOCATOR && is_pmm_ready()) {
                    ptr::write_bytes(ptr, 0, layout.size());
                }
            }
            ptr
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe {
            // For dealloc, we need to handle both cases since memory might
            // have been allocated with either allocator
            if USE_PAGE_ALLOCATOR && is_pmm_ready() {
                page_dealloc(ptr, layout);
            } else {
                talc_dealloc(ptr, layout);
            }
        }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        unsafe {
            if USE_PAGE_ALLOCATOR && is_pmm_ready() {
                page_realloc(ptr, layout, new_size)
            } else {
                talc_realloc(ptr, layout, new_size)
            }
        }
    }
}

// ============================================================================
// Page-based allocator (ported from libakuma mmap allocator)
// ============================================================================

/// Allocate using PMM pages directly
unsafe fn page_alloc(layout: Layout) -> *mut u8 {
    with_irqs_disabled(|| {
        let size = layout.size().max(layout.align());
        let alloc_size = (size + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
        let pages = alloc_size / PAGE_SIZE;

        // Allocate contiguous pages from PMM
        // For simplicity, allocate pages one at a time and use the first one's address
        // This works because PMM allocates from a contiguous region
        let mut first_addr: Option<*mut u8> = None;
        
        for i in 0..pages {
            if let Some(frame) = crate::pmm::alloc_page_zeroed() {
                // Track as kernel allocation (PID=0 for kernel)
                crate::pmm::track_frame(frame, crate::pmm::FrameSource::Kernel, 0);
                if i == 0 {
                    // Convert physical address to kernel virtual address
                    first_addr = Some(crate::mmu::phys_to_virt(frame.addr));
                }
                // Track allocation
                ALLOCATED_BYTES.fetch_add(PAGE_SIZE, Ordering::Relaxed);
            } else {
                // Allocation failed
                return ptr::null_mut();
            }
        }

        ALLOCATION_COUNT.fetch_add(1, Ordering::Relaxed);
        
        // Update peak
        let new_allocated = ALLOCATED_BYTES.load(Ordering::Relaxed);
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

        first_addr.unwrap_or(ptr::null_mut())
    })
}

/// Deallocate pages - returns pages to PMM
unsafe fn page_dealloc(ptr: *mut u8, layout: Layout) {
    with_irqs_disabled(|| {
        let size = layout.size().max(layout.align());
        let alloc_size = (size + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
        let pages = alloc_size / PAGE_SIZE;

        // Convert virtual address back to physical and free each page
        let phys_addr = crate::mmu::virt_to_phys(ptr as usize);
        for i in 0..pages {
            let frame = crate::pmm::PhysFrame::new(phys_addr + i * PAGE_SIZE);
            crate::pmm::free_page(frame);
        }

        ALLOCATED_BYTES.fetch_sub(alloc_size, Ordering::Relaxed);
    })
}

/// Realloc using page allocation
unsafe fn page_realloc(ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
    unsafe {
        if new_size == 0 {
            page_dealloc(ptr, layout);
            return ptr::null_mut();
        }

        let new_layout = match Layout::from_size_align(new_size, layout.align()) {
            Ok(l) => l,
            Err(_) => return ptr::null_mut(),
        };

        let new_ptr = page_alloc(new_layout);
        if new_ptr.is_null() {
            return ptr::null_mut();
        }

        // Copy old data and free old allocation
        if !ptr.is_null() {
            let copy_size = layout.size().min(new_size);
            ptr::copy_nonoverlapping(ptr, new_ptr, copy_size);
            page_dealloc(ptr, layout);
        }

        new_ptr
    }
}

// ============================================================================
// Talc-based allocator (original implementation)
// ============================================================================

unsafe fn talc_alloc(layout: Layout) -> *mut u8 {
    with_irqs_disabled(|| {
        let result = TALC
            .lock()
            .malloc(layout)
            .map(|ptr| ptr.as_ptr())
            .unwrap_or(ptr::null_mut());

        if result.is_null() {
            crate::console::print("[ALLOC FAIL]");
        } else {
            let new_allocated =
                ALLOCATED_BYTES.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
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
        }

        result
    })
}

unsafe fn talc_dealloc(ptr: *mut u8, layout: Layout) {
    with_irqs_disabled(|| {
        TALC.lock()
            .free(core::ptr::NonNull::new_unchecked(ptr), layout);
        ALLOCATED_BYTES.fetch_sub(layout.size(), Ordering::Relaxed);
    })
}

unsafe fn talc_realloc(ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
    unsafe {
        if new_size == 0 {
            talc_dealloc(ptr, layout);
            return ptr::null_mut();
        }

        let new_layout = match Layout::from_size_align(new_size, layout.align()) {
            Ok(layout) => layout,
            Err(_) => return ptr::null_mut(),
        };

        let new_ptr = talc_alloc(new_layout);
        if new_ptr.is_null() {
            return ptr::null_mut();
        }

        if !ptr.is_null() && layout.size() > 0 {
            let copy_size = core::cmp::min(layout.size(), new_size);
            if copy_size > 0 {
                ptr::copy_nonoverlapping(ptr, new_ptr, copy_size);
            }
            talc_dealloc(ptr, layout);
        }

        new_ptr
    }
}
