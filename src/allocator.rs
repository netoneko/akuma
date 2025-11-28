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
        TALC.lock().claim(span).map_err(|_| "Failed to claim heap memory")?;
    }
    
    Ok(())
}

struct Talck;

unsafe impl core::alloc::GlobalAlloc for Talck {
    unsafe fn alloc(&self, layout: core::alloc::Layout) -> *mut u8 {
        unsafe {
            TALC.lock()
                .malloc(layout)
                .map(|ptr| ptr.as_ptr())
                .unwrap_or(core::ptr::null_mut())
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: core::alloc::Layout) {
        unsafe {
            TALC.lock()
                .free(core::ptr::NonNull::new_unchecked(ptr), layout);
        }
    }
}
