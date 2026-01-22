//! Kernel memory allocator with page-based and talc-based options
//!
//! The page-based allocator is ported from libakuma's mmap allocator.
//! It allocates whole pages for each allocation, fixing layout-sensitive
//! heap corruption bugs at the cost of higher memory usage.
//!
//! Debug features:
//! - ENABLE_ALLOCATION_REGISTRY: Track all allocations to detect overlaps, double frees
//! - ENABLE_CANARIES: Add guard bytes around allocations to detect overflows

use core::alloc::{GlobalAlloc, Layout};
use core::ptr;
use core::sync::atomic::{AtomicUsize, AtomicBool, Ordering};
use spinning_top::Spinlock;
use talc::ErrOnOom;
use talc::{Span, Talc};

/// Set to true to use page-based allocation (like userspace mmap allocator)
/// This fixes layout-sensitive heap corruption bugs but uses more memory.
/// Deallocation properly returns pages to PMM.
pub const USE_PAGE_ALLOCATOR: bool = false; // DOES NOT ACTUALLY WORK

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

/// Flag indicating PMM is ready for use (set after PMM init completes)
static PMM_READY: AtomicBool = AtomicBool::new(false);

/// Signal that PMM is ready for use by the page allocator
pub fn mark_pmm_ready() {
    PMM_READY.store(true, Ordering::Release);
}

/// Check if PMM is ready
fn is_pmm_ready() -> bool {
    PMM_READY.load(Ordering::Acquire)
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

/// Registry statistics
#[derive(Debug, Clone, Copy)]
pub struct RegistryStats {
    pub active_allocations: usize,
    pub overlaps_detected: usize,
    pub double_frees_detected: usize,
    pub invalid_frees_detected: usize,
    pub canary_corruptions: usize,
}

/// Get registry statistics
pub fn registry_stats() -> RegistryStats {
    RegistryStats {
        active_allocations: REGISTRY_COUNT.load(Ordering::Relaxed),
        overlaps_detected: OVERLAP_COUNT.load(Ordering::Relaxed),
        double_frees_detected: DOUBLE_FREE_COUNT.load(Ordering::Relaxed),
        invalid_frees_detected: INVALID_FREE_COUNT.load(Ordering::Relaxed),
        canary_corruptions: CANARY_CORRUPTION_COUNT.load(Ordering::Relaxed),
    }
}

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

/// Check if an address is in the registry (for double-free detection)
fn registry_contains(addr: usize) -> bool {
    if !ENABLE_ALLOCATION_REGISTRY {
        return true;
    }

    let registry = ALLOCATION_REGISTRY.lock();
    for record in registry.iter() {
        if record.active && record.addr == addr {
            return true;
        }
    }
    false
}

/// Scan all allocations for canary corruption
/// Returns number of corrupted allocations found
pub fn scan_for_corruption() -> usize {
    if !ENABLE_ALLOCATION_REGISTRY || !ENABLE_CANARIES {
        return 0;
    }

    let registry = ALLOCATION_REGISTRY.lock();
    let mut corrupted = 0;

    for record in registry.iter() {
        if !record.active {
            continue;
        }

        let user_ptr = record.addr;
        let user_size = record.size;

        // Check canary before
        let canary_before_ptr = (user_ptr - CANARY_SIZE) as *const u64;
        let canary_before = unsafe { core::ptr::read_volatile(canary_before_ptr) };
        if canary_before != CANARY_BEFORE {
            corrupted += 1;
            crate::safe_print!(
                128,
                "[ALLOC] CANARY CORRUPTION (before) at 0x{:x}: expected 0x{:x}, got 0x{:x}\n",
                user_ptr,
                CANARY_BEFORE,
                canary_before
            );
        }

        // Check canary after
        let canary_after_ptr = (user_ptr + user_size) as *const u64;
        let canary_after = unsafe { core::ptr::read_volatile(canary_after_ptr) };
        if canary_after != CANARY_AFTER {
            corrupted += 1;
            crate::safe_print!(
                128,
                "[ALLOC] CANARY CORRUPTION (after) at 0x{:x}+{}: expected 0x{:x}, got 0x{:x}\n",
                user_ptr,
                user_size,
                CANARY_AFTER,
                canary_after
            );
        }
    }

    if corrupted > 0 {
        CANARY_CORRUPTION_COUNT.fetch_add(corrupted, Ordering::Relaxed);
    }

    corrupted
}

/// Dump all active allocations (for debugging)
pub fn dump_allocations() {
    if !ENABLE_ALLOCATION_REGISTRY {
        crate::console::print("[ALLOC] Registry disabled\n");
        return;
    }

    let registry = ALLOCATION_REGISTRY.lock();
    let mut count = 0;

    crate::console::print("=== Active Allocations ===\n");
    for record in registry.iter() {
        if record.active {
            crate::safe_print!(
                64,
                "  0x{:x}-0x{:x} (size={})\n",
                record.addr,
                record.addr + record.size,
                record.size
            );
            count += 1;
            if count >= 50 {
                crate::console::print("  ... (truncated)\n");
                break;
            }
        }
    }
    crate::safe_print!(48, "Total: {} allocations\n", REGISTRY_COUNT.load(Ordering::Relaxed));
}

/// Print registry stats
pub fn print_registry_stats() {
    let stats = registry_stats();
    crate::console::print("=== Allocation Registry Stats ===\n");
    crate::safe_print!(48, "  Active allocations: {}\n", stats.active_allocations);
    crate::safe_print!(48, "  Overlaps detected: {}\n", stats.overlaps_detected);
    crate::safe_print!(48, "  Double frees: {}\n", stats.double_frees_detected);
    crate::safe_print!(48, "  Invalid frees: {}\n", stats.invalid_frees_detected);
    crate::safe_print!(48, "  Canary corruptions: {}\n", stats.canary_corruptions);
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
            crate::console::print("[ALLOC FAIL]");
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

            new_user_ptr
        }
    })
}
