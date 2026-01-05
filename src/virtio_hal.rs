// HAL implementation for virtio-drivers crate
//
// This module provides the hardware abstraction layer for virtio-drivers,
// handling DMA allocation and physical/virtual address translation.
//
// IMPORTANT: Uses virt_to_phys/phys_to_virt for address translation.
// See docs/IDENTITY_MAPPING_DEPENDENCIES.md for details.

use crate::mmu::{phys_to_virt, virt_to_phys};
use core::ptr::NonNull;
use spinning_top::Spinlock;
use virtio_drivers::Hal;

// Track which IRQs are registered for cleanup
static REGISTERED_IRQS: Spinlock<alloc::vec::Vec<u32>> = Spinlock::new(alloc::vec::Vec::new());

pub struct VirtioHal;

unsafe impl Hal for VirtioHal {
    fn dma_alloc(
        pages: usize,
        _direction: virtio_drivers::BufferDirection,
    ) -> (virtio_drivers::PhysAddr, NonNull<u8>) {
        use alloc::alloc::{Layout, alloc_zeroed};

        let layout = Layout::from_size_align(pages * 4096, 4096).unwrap();
        let virt = unsafe { alloc_zeroed(layout) };

        if virt.is_null() {
            panic!("DMA allocation failed");
        }

        // Convert virtual address to physical address for DMA
        let phys = virt_to_phys(virt as usize);
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
        // Convert physical MMIO address to virtual address
        unsafe { NonNull::new_unchecked(phys_to_virt(paddr)) }
    }

    unsafe fn share(
        buffer: NonNull<[u8]>,
        _direction: virtio_drivers::BufferDirection,
    ) -> virtio_drivers::PhysAddr {
        // Convert virtual buffer address to physical for DMA
        virt_to_phys(buffer.as_ptr() as *mut u8 as usize)
    }

    unsafe fn unshare(
        _paddr: virtio_drivers::PhysAddr,
        _buffer: NonNull<[u8]>,
        _direction: virtio_drivers::BufferDirection,
    ) {
        // No-op for identity mapping (no cache management needed on QEMU)
    }
}
