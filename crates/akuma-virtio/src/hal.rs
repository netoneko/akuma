//! The one `virtio_drivers::Hal` implementation.
//!
//! # Why there is exactly one
//!
//! There used to be two, byte-for-byte equivalent: `src/virtio_hal.rs`
//! (`VirtioHal`, used by the block and sound drivers) and
//! `crates/akuma-net/src/hal.rs` (`NetHal`, used by `smoltcp_net` and
//! `rump_tap`). They differed in exactly one respect — how they reached
//! `virt_to_phys`/`phys_to_virt`. `VirtioHal` called `akuma_primitives::addr`
//! directly; `NetHal` dispatched through `NetRuntime`'s registered function
//! pointers.
//!
//! That single substituted call expression was enough to hide two thirds of the
//! clone from token-based clone detection, which is why the pair is the worked
//! example in `docs/archive/TRIMMING_FAT_EMBARASSING_DUPLICATIONS.md` §6 of what
//! CPD cannot see.
//!
//! The `NetRuntime` indirection was not load-bearing by the time it was removed.
//! It existed so `akuma-net` would not need to depend on `akuma-exec` — but
//! `akuma-net` acquired that dependency anyway, for the `PreemptGuard` re-export,
//! so the decoupling it bought had already been spent. Nothing ever registered a
//! translator other than `akuma_primitives::addr`'s, and no test injected a fake.
//!
//! # Why this calls `akuma_primitives::addr` instead of doing nothing
//!
//! Both translators are currently the identity:
//!
//! ```ignore
//! pub fn phys_to_virt(paddr: usize) -> *mut u8 { paddr as *mut u8 }
//! pub fn virt_to_phys(vaddr: usize) -> usize { vaddr }
//! ```
//!
//! so every call below compiles to nothing. They are called regardless, because
//! the identity is a property of the kernel's identity mapping rather than of
//! DMA — it is the documented seam (`docs/IDENTITY_MAPPING_DEPENDENCIES.md`), and
//! open-coding `paddr as *mut u8` here would bury the assumption in a driver
//! crate where nobody changing the mapping would think to look.

use core::ptr::NonNull;

use akuma_primitives::addr::{phys_to_virt, virt_to_phys};
use virtio_drivers::{BufferDirection, Hal, PhysAddr};

/// DMA allocation and address translation for every virtio device in the tree.
pub struct VirtioHal;

// SAFETY: `dma_alloc` returns page-aligned, zeroed, uniquely-owned memory whose
// physical address is valid for the returned page count, and `share`/`unshare`
// need no cache maintenance under the kernel's identity mapping (QEMU virt is
// coherent with respect to virtio DMA).
unsafe impl Hal for VirtioHal {
    fn dma_alloc(pages: usize, _direction: BufferDirection) -> (PhysAddr, NonNull<u8>) {
        use alloc::alloc::{Layout, alloc_zeroed};

        let layout = Layout::from_size_align(pages * 4096, 4096).unwrap();
        // SAFETY: `layout` has non-zero size (`pages >= 1` for every caller in
        // virtio-drivers) and a valid power-of-two alignment.
        let virt = unsafe { alloc_zeroed(layout) };

        assert!(!virt.is_null(), "DMA allocation failed");

        let phys = virt_to_phys(virt as usize);
        // SAFETY: just asserted non-null.
        let ptr = unsafe { NonNull::new_unchecked(virt) };

        (phys, ptr)
    }

    unsafe fn dma_dealloc(_paddr: PhysAddr, vaddr: NonNull<u8>, pages: usize) -> i32 {
        use alloc::alloc::{Layout, dealloc};

        let layout = Layout::from_size_align(pages * 4096, 4096).unwrap();
        // SAFETY: caller contract — `vaddr`/`pages` come from a prior
        // `dma_alloc`, so this reconstructs that allocation's exact layout.
        unsafe {
            dealloc(vaddr.as_ptr(), layout);
        }
        0
    }

    unsafe fn mmio_phys_to_virt(paddr: PhysAddr, _size: usize) -> NonNull<u8> {
        // SAFETY: caller contract — `paddr` is a mapped MMIO region, so its
        // linear-map VA is non-null.
        unsafe { NonNull::new_unchecked(phys_to_virt(paddr)) }
    }

    unsafe fn share(buffer: NonNull<[u8]>, _direction: BufferDirection) -> PhysAddr {
        virt_to_phys(buffer.as_ptr().cast::<u8>() as usize)
    }

    unsafe fn unshare(_paddr: PhysAddr, _buffer: NonNull<[u8]>, _direction: BufferDirection) {
        // No-op: identity mapping, and QEMU virt needs no cache maintenance.
    }
}
