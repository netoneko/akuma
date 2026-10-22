//! First-fit extents over the reserved guest-RAM block.
//!
//! `kvmmem=<MiB>` makes the kernel take one contiguous block at boot, while memory is still
//! unfragmented; each VM's RAM (and RAM-disk copy) is a slice of it. The arena knows offsets
//! only — it never touches memory — so it is pure and the kernel adds the block's base.

/// Most live slices: a guest's RAM and its disk copy, for a few guests.
pub const MAX_EXTENTS: usize = 8;

#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
struct Extent {
    off: u64,
    len: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Arena {
    total: u64,
    ext: [Extent; MAX_EXTENTS],
}

impl Arena {
    #[must_use]
    pub const fn new(total: u64) -> Self {
        Self { total, ext: [Extent { off: 0, len: 0 }; MAX_EXTENTS] }
    }

    #[must_use]
    pub const fn total(&self) -> u64 {
        self.total
    }

    #[must_use]
    pub fn used(&self) -> u64 {
        self.ext.iter().map(|e| e.len).sum()
    }

    fn fits(&self, off: u64, len: u64) -> bool {
        off.checked_add(len).is_some_and(|end| end <= self.total)
            && self.ext.iter().all(|e| e.len == 0 || off + len <= e.off || e.off + e.len <= off)
    }

    /// The lowest `align`-aligned offset where `len` bytes fit, claimed. `align` is a power of two.
    pub fn alloc(&mut self, len: u64, align: u64) -> Option<u64> {
        if len == 0 || !align.is_power_of_two() {
            return None;
        }
        let free = self.ext.iter().position(|e| e.len == 0)?;
        let up = |v: u64| v.checked_add(align - 1).map(|v| v & !(align - 1));
        // Candidates: the start, and the end of each live extent.
        let mut best: Option<u64> = None;
        for start in core::iter::once(0).chain(self.ext.iter().filter(|e| e.len != 0).map(|e| e.off + e.len)) {
            if let Some(off) = up(start)
                && self.fits(off, len)
                && best.is_none_or(|b| off < b)
            {
                best = Some(off);
            }
        }
        let off = best?;
        self.ext[free] = Extent { off, len };
        Some(off)
    }

    /// Release the slice that starts at `off`. False if none does.
    pub fn free(&mut self, off: u64) -> bool {
        match self.ext.iter_mut().find(|e| e.len != 0 && e.off == off) {
            Some(e) => {
                *e = Extent::default();
                true
            }
            None => false,
        }
    }
}
