use core::sync::atomic::{AtomicUsize, Ordering};
use spinning_top::Spinlock;
use talc::ErrOnOom;
use talc::{Span, Talc};

#[global_allocator]
static ALLOCATOR: Talck = Talck;

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

/// Run a closure with IRQs disabled to prevent context switch during allocation
/// This is always enabled once preemption starts - we unconditionally disable IRQs
/// because the check itself could race with preemption
#[inline(never)]
fn with_irqs_disabled<T, F: FnOnce() -> T>(f: F) -> T {
    let daif: u64;
    unsafe {
        // Save current interrupt state
        core::arch::asm!("mrs {}, daif", out(reg) daif, options(nomem, nostack));
        // Disable IRQs
        core::arch::asm!("msr daifset, #2", options(nomem, nostack));
        // Memory barrier to ensure IRQs are disabled before we proceed
        core::arch::asm!("isb", options(nomem, nostack));
    }
    let result = f();
    unsafe {
        // Restore previous interrupt state
        core::arch::asm!("msr daif, {}", in(reg) daif, options(nomem, nostack));
    }
    result
}

pub fn init(heap_start: usize, heap_size: usize) -> Result<(), &'static str> {
    if heap_size == 0 {
        return Err("Heap size cannot be zero");
    }

    if heap_start == 0 {
        return Err("Invalid heap start address");
    }

    // Store heap size for stats
    HEAP_SIZE.store(heap_size, Ordering::Relaxed);

    unsafe {
        let heap_ptr = heap_start as *mut u8;
        let span = Span::from_base_size(heap_ptr, heap_size);
        TALC.lock()
            .claim(span)
            .map_err(|_| "Failed to claim heap memory")?;
    }

    Ok(())
}

struct Talck;

unsafe impl core::alloc::GlobalAlloc for Talck {
    unsafe fn alloc(&self, layout: core::alloc::Layout) -> *mut u8 {
        // Always disable IRQs during allocation to prevent context switch deadlock
        with_irqs_disabled(|| unsafe {
            let result = TALC
                .lock()
                .malloc(layout)
                .map(|ptr| ptr.as_ptr())
                .unwrap_or(core::ptr::null_mut());

            if result.is_null() {
                // Log allocation failures - use only static strings to avoid recursion!
                crate::console::print("[ALLOC FAIL]");
            } else {
                // Track allocation
                let new_allocated =
                    ALLOCATED_BYTES.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
                ALLOCATION_COUNT.fetch_add(1, Ordering::Relaxed);
                // Update peak if needed
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

    unsafe fn alloc_zeroed(&self, layout: core::alloc::Layout) -> *mut u8 {
        unsafe {
            let ptr = self.alloc(layout);
            if !ptr.is_null() {
                core::ptr::write_bytes(ptr, 0, layout.size());
            }
            ptr
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: core::alloc::Layout) {
        // Always disable IRQs during deallocation to prevent context switch deadlock
        with_irqs_disabled(|| unsafe {
            TALC.lock()
                .free(core::ptr::NonNull::new_unchecked(ptr), layout);
            // Track deallocation
            ALLOCATED_BYTES.fetch_sub(layout.size(), Ordering::Relaxed);
        })
    }

    unsafe fn realloc(
        &self,
        ptr: *mut u8,
        layout: core::alloc::Layout,
        new_size: usize,
    ) -> *mut u8 {
        unsafe {
            // Handle zero-sized allocations
            if new_size == 0 {
                self.dealloc(ptr, layout);
                return core::ptr::null_mut();
            }

            // Create new layout with the new size
            let new_layout = match core::alloc::Layout::from_size_align(new_size, layout.align()) {
                Ok(layout) => layout,
                Err(_) => return core::ptr::null_mut(),
            };

            // Allocate new memory
            let new_ptr = self.alloc(new_layout);
            if new_ptr.is_null() {
                // Allocation failed - return null but don't free old memory
                return core::ptr::null_mut();
            }

            // Only copy if we have valid old data
            if !ptr.is_null() && layout.size() > 0 {
                // Copy old data to new location (copy the minimum of old and new sizes)
                let copy_size = core::cmp::min(layout.size(), new_size);
                if copy_size > 0 {
                    core::ptr::copy_nonoverlapping(ptr, new_ptr, copy_size);
                }

                // Free old memory
                self.dealloc(ptr, layout);
            }

            new_ptr
        }
    }
}
