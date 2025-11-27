use spinning_top::Spinlock;
use talc::ErrOnOom;
use talc::{Span, Talc};
#[global_allocator]
static ALLOCATOR: Talck = Talck;

const HEAP_SIZE: usize = 1024 * 1024; // 1MB
static mut HEAP_MEM: [u8; HEAP_SIZE] = [0; HEAP_SIZE];
static TALC: Spinlock<Talc<ErrOnOom>> = Spinlock::new(Talc::new(ErrOnOom));

pub fn init() {
    unsafe {
        TALC.lock().claim(Span::from_array(&raw mut HEAP_MEM)).unwrap();
    }
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
