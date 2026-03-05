use core::ptr::NonNull;
use virtio_drivers::Hal;

use crate::runtime::runtime;

/// `VirtIO` HAL implementation for the networking crate.
///
/// Dispatches `virt_to_phys`/`phys_to_virt` through the registered
/// `NetRuntime` function pointers, and uses the global allocator for DMA
/// buffers (identical logic to the kernel's `VirtioHal`).
pub struct NetHal;

unsafe impl Hal for NetHal {
    fn dma_alloc(
        pages: usize,
        _direction: virtio_drivers::BufferDirection,
    ) -> (virtio_drivers::PhysAddr, NonNull<u8>) {
        use alloc::alloc::{Layout, alloc_zeroed};

        let layout = Layout::from_size_align(pages * 4096, 4096).unwrap();
        let virt = unsafe { alloc_zeroed(layout) };

        assert!(!virt.is_null(), "DMA allocation failed");

        let phys = (runtime().virt_to_phys)(virt as usize);
        let ptr = unsafe { NonNull::new_unchecked(virt) };

        (phys, ptr)
    }

    unsafe fn dma_dealloc(
        _paddr: virtio_drivers::PhysAddr,
        vaddr: NonNull<u8>,
        pages: usize,
    ) -> i32 {
        use alloc::alloc::{Layout, dealloc};

        let layout = Layout::from_size_align(pages * 4096, 4096).unwrap();
        unsafe {
            dealloc(vaddr.as_ptr(), layout);
        }
        0
    }

    unsafe fn mmio_phys_to_virt(paddr: virtio_drivers::PhysAddr, _size: usize) -> NonNull<u8> {
        unsafe { NonNull::new_unchecked((runtime().phys_to_virt)(paddr)) }
    }

    unsafe fn share(
        buffer: NonNull<[u8]>,
        _direction: virtio_drivers::BufferDirection,
    ) -> virtio_drivers::PhysAddr {
        (runtime().virt_to_phys)(buffer.as_ptr().cast::<u8>() as usize)
    }

    unsafe fn unshare(
        _paddr: virtio_drivers::PhysAddr,
        _buffer: NonNull<[u8]>,
        _direction: virtio_drivers::BufferDirection,
    ) {
    }
}
