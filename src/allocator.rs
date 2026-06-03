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
        // Grow by at least 256 KB (64 pages) to amortise per-OOM overhead.
        const GROW_PAGES: usize = 64;
        let needed = (layout.size() + PAGE_SIZE - 1) / PAGE_SIZE;
        let n = needed.max(GROW_PAGES);
        let frame = crate::pmm::alloc_pages_contiguous_zeroed(n).ok_or(())?;
        let ptr = akuma_exec::mmu::phys_to_virt(frame.addr) as *mut u8;
        let span = Span::from_base_size(ptr, n * PAGE_SIZE);
        unsafe { talc.claim(span).map(|_| ()).map_err(|_| ()) }
    }
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
