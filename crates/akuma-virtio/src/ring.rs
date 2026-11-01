//! Virtqueue memory: a `DmaArray` static per queue, and the glue that hands one
//! to an `akuma_virtqueue::SplitQueue` and to the device.
//!
//! Every queue in this crate lives in a `static`, so the rings sit at fixed
//! addresses for the kernel's lifetime and no heap allocation can fail or move
//! under a device. A [`Ring`] is claimed once by the driver that set it up; a
//! second claim is refused rather than shared.

use core::sync::atomic::{AtomicBool, Ordering};

use akuma_dma::DmaArray;
use akuma_dma::align::A64;
use akuma_primitives::addr::virt_to_phys;
use akuma_virtqueue::{Layout, QueueMem, SplitQueue};

use crate::mmio::{Error, Transport};

/// `W` 16-bit cells of queue memory (cache-line aligned, beyond the 16/2/4
/// bytes the three areas need).
pub struct Ring<const W: usize> {
    cells: DmaArray<u16, W, A64>,
    claimed: AtomicBool,
}

impl<const W: usize> Ring<W> {
    #[must_use]
    pub const fn new() -> Self {
        Self { cells: DmaArray::new(), claimed: AtomicBool::new(false) }
    }

    /// Take this ring for one device queue. `None` if it is already taken.
    pub fn claim(&'static self) -> Option<&'static Self> {
        (!self.claimed.swap(true, Ordering::AcqRel)).then_some(self)
    }

    /// The device address of byte `off` of the ring.
    fn phys(&self, off: usize) -> u64 {
        virt_to_phys(self.cells.addr() + off) as u64
    }
}

impl<const W: usize> Default for Ring<W> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const W: usize> QueueMem for Ring<W> {
    fn read(&self, word: usize) -> u16 {
        self.cells.get(word)
    }
    fn write(&self, word: usize, v: u16) {
        self.cells.set(word, v);
    }
}

/// A split queue of `Q` entries in a claimed ring of `W` cells.
pub struct Queue<const Q: usize, const W: usize> {
    pub(crate) sq: SplitQueue<Q>,
    pub(crate) mem: &'static Ring<W>,
    index: u16,
}

impl<const Q: usize, const W: usize> Queue<Q, W> {
    /// Zero `mem`, and register it with the device as queue `index`.
    #[allow(clippy::cast_possible_truncation)] // Q <= 32768, SplitQueue asserts it
    pub fn setup(t: &mut Transport, index: u16, mem: &'static Ring<W>) -> Result<Self, Error> {
        const { assert!(Layout::new(Q).words() <= W, "ring storage smaller than the queue") };
        let mut sq = SplitQueue::new();
        sq.reset(mem);
        let l = SplitQueue::<Q>::LAYOUT;
        t.setup_queue(index, Q as u16, mem.phys(l.desc), mem.phys(l.avail), mem.phys(l.used))?;
        Ok(Self { sq, mem, index })
    }

    /// Post `bufs` and notify the device if it wants that. Returns the token.
    pub fn submit(&mut self, t: &Transport, bufs: &[akuma_virtqueue::Buf]) -> Result<u16, akuma_virtqueue::queue::AddError> {
        let head = self.sq.add(self.mem, bufs)?;
        if self.sq.should_notify(self.mem) {
            t.notify(self.index);
        }
        Ok(head)
    }
}

/// The device address of a kernel buffer.
///
/// Kernel memory (heap and statics) is physically contiguous where it is
/// virtually contiguous — the identity map on AArch64, the physmap on amd64 — so
/// one address and the length describe the whole buffer. This is the same
/// assumption `virtio-drivers`' `Hal::share` made through the same function.
#[must_use]
pub fn phys_of(ptr: *const u8) -> u64 {
    virt_to_phys(ptr as usize) as u64
}
