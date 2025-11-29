// HAL implementation for virtio-drivers crate

use virtio_drivers::Hal;
use core::ptr::NonNull;

pub struct VirtioHal;

unsafe impl Hal for VirtioHal {
    fn dma_alloc(pages: usize, _direction: virtio_drivers::BufferDirection) -> (virtio_drivers::PhysAddr, NonNull<u8>) {
        use alloc::alloc::{alloc_zeroed, Layout};
        
        let layout = Layout::from_size_align(pages * 4096, 4096).unwrap();
        let virt = unsafe { alloc_zeroed(layout) };
        
        if virt.is_null() {
            panic!("DMA allocation failed");
        }
        
        // On QEMU ARM64 virt machine, physical == virtual for RAM
        let phys = virt as usize;
        let ptr = unsafe { NonNull::new_unchecked(virt) };
        
        (phys, ptr)
    }

    unsafe fn dma_dealloc(_paddr: virtio_drivers::PhysAddr, vaddr: NonNull<u8>, pages: usize) -> i32 {
        use alloc::alloc::{dealloc, Layout};
        
        let layout = Layout::from_size_align(pages * 4096, 4096).unwrap();
        unsafe {
            dealloc(vaddr.as_ptr(), layout);
        }
        0
    }

    unsafe fn mmio_phys_to_virt(paddr: virtio_drivers::PhysAddr, _size: usize) -> NonNull<u8> {
        // Physical == virtual for MMIO on QEMU virt
        unsafe {
            NonNull::new_unchecked(paddr as *mut u8)
        }
    }

    unsafe fn share(buffer: NonNull<[u8]>, _direction: virtio_drivers::BufferDirection) -> virtio_drivers::PhysAddr {
        // Physical == virtual
        buffer.as_ptr() as *mut u8 as usize
    }

    unsafe fn unshare(_paddr: virtio_drivers::PhysAddr, _buffer: NonNull<[u8]>, _direction: virtio_drivers::BufferDirection) {
        // No-op for identity mapping
    }
}

