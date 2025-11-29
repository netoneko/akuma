use spinning_top::Spinlock;
use talc::ErrOnOom;
use talc::{Span, Talc};

#[global_allocator]
static ALLOCATOR: Talck = Talck;

static TALC: Spinlock<Talc<ErrOnOom>> = Spinlock::new(Talc::new(ErrOnOom));

pub fn init(heap_start: usize, heap_size: usize) -> Result<(), &'static str> {
    if heap_size == 0 {
        return Err("Heap size cannot be zero");
    }

    if heap_start == 0 {
        return Err("Invalid heap start address");
    }

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
        unsafe {
            let result = TALC
                .lock()
                .malloc(layout)
                .map(|ptr| ptr.as_ptr())
                .unwrap_or(core::ptr::null_mut());

            // Log allocation failures - use only static strings to avoid recursion!
            if result.is_null() {
                crate::console::print("\n[ALLOC FAIL]\n");
            }

            result
        }
    }

    unsafe fn alloc_zeroed(&self, layout: core::alloc::Layout) -> *mut u8 {
        unsafe {
            let ptr = self.alloc(layout);
            if !ptr.is_null() {
                core::ptr::write_bytes(ptr, 0, layout.size());
            } else {
                crate::console::print("\n[ALLOC_ZEROED FAIL]\n");
            }
            ptr
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: core::alloc::Layout) {
        unsafe {
            TALC.lock()
                .free(core::ptr::NonNull::new_unchecked(ptr), layout);
        }
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
