//! The physical-frame handle a region's owned pages are recorded as.

use crate::types::PAGE_SIZE;

/// Physical page frame (mirrors kernel `pmm::PhysFrame`).
///
/// A `Copy` newtype over a page-aligned physical address, with **no `Drop` impl**:
/// holding one confers no ownership and dropping one frees nothing. Frame ownership
/// is tracked by `UserAddressSpace::user_frames` and the PMM's refcounts, never by
/// this value — which is why a crate that cannot call the PMM can still hold a
/// `Vec<PhysFrame>` (see [`MmapRegion::frames`](crate::MmapRegion::frames)).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PhysFrame {
    pub addr: usize,
}

impl PhysFrame {
    /// The frame containing `addr`, rounded **down** to a page boundary.
    #[must_use]
    pub const fn new(addr: usize) -> Self {
        Self {
            addr: addr & !(PAGE_SIZE - 1),
        }
    }

    /// Alias for [`PhysFrame::new`], for call sites where the rounding is the point.
    #[must_use]
    pub const fn containing_address(addr: usize) -> Self {
        Self::new(addr)
    }

    /// The frame's page-aligned physical base address.
    #[must_use]
    pub const fn start_address(&self) -> usize {
        self.addr
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Construction rounds down, and does so for every offset within the page —
    /// `map_user_page` and the region frame lists both assume a `PhysFrame` is
    /// already aligned and never re-mask.
    #[test]
    fn new_rounds_down_to_the_page() {
        assert_eq!(PhysFrame::new(0x4000_0000).addr, 0x4000_0000);
        assert_eq!(PhysFrame::new(0x4000_0001).addr, 0x4000_0000);
        assert_eq!(PhysFrame::new(0x4000_0fff).addr, 0x4000_0000);
        assert_eq!(PhysFrame::new(0x4000_1000).addr, 0x4000_1000);
        assert_eq!(PhysFrame::containing_address(0x4000_0338).addr, 0x4000_0000);
    }

    /// `start_address` is the identity on an already-aligned frame; it exists so
    /// call sites read as intent rather than field access.
    #[test]
    fn start_address_is_the_aligned_base() {
        assert_eq!(PhysFrame::new(0x4000_2345).start_address(), 0x4000_2000);
        assert_eq!(PhysFrame::new(0).start_address(), 0);
    }
}
