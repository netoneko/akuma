//! `ProcessMemory` — the per-process mmap arena.
//!
//! The rest of what was `process/types.rs` moved to `akuma-exec-core` on
//! 2026-09-01 (`docs/archive/AKUMA_EXEC_AUDIT.md`). **This type did not**, and
//! the reason is the rule that crate's header states: `alloc_mmap` needs
//! `mmu::kernel_va_end()` — the dynamic top of the kernel identity-map hole,
//! which scales with detected RAM — so it wants the MMU, and a type that wants
//! the MMU does not belong in a crate whose whole point is that it cannot reach
//! one.
//!
//! Passing the value in instead was considered and rejected: as an argument it
//! touches ~40 `alloc_mmap` call sites in the boot suite, and as a constructor
//! parameter it touches all 18 `ProcessMemory::new` sites. Neither is worth it
//! to relocate 118 lines. Move the seam, not the dependency — and here the seam
//! is exactly where it already is.

use core::sync::atomic::{AtomicUsize, Ordering};

pub use akuma_exec_core::process::*;

/// Memory regions for a process.
///
/// Every field is interior-mutable: `next_mmap` because `CLONE_VM` siblings
/// place mappings on the shared `Process` (it is read and written only under
/// that process's region lock, see [`ProcessMemory::place`]), and the scalars
/// because `execve` re-sets them — via [`ProcessMemory::reset`] on `&self`, so
/// `replace_image` never needs `&mut Process`. §5-bis said those five were
/// "set at construction and never assigned again"; that was true field by
/// field but not across the wholesale `self.memory = ProcessMemory::new(…)`
/// that `replace_image` used to do (§6.E group 2b).
#[derive(Debug)]
pub struct ProcessMemory {
    pub code_end: AtomicUsize,
    pub brk: AtomicUsize,
    pub stack_bottom: AtomicUsize,
    pub stack_top: AtomicUsize,
    /// The placer's cursor: where the next search starts. A hint, never a
    /// contract (`akuma_mmap::place`).
    pub next_mmap: AtomicUsize,
    /// Bottom of the automatic-placement window, where the search wraps to.
    pub mmap_base: AtomicUsize,
    pub mmap_limit: AtomicUsize,
}

impl Clone for ProcessMemory {
    fn clone(&self) -> Self {
        Self {
            code_end: AtomicUsize::new(self.code_end.load(Ordering::Relaxed)),
            brk: AtomicUsize::new(self.brk.load(Ordering::Relaxed)),
            stack_bottom: AtomicUsize::new(self.stack_bottom.load(Ordering::Relaxed)),
            stack_top: AtomicUsize::new(self.stack_top.load(Ordering::Relaxed)),
            next_mmap: AtomicUsize::new(self.next_mmap.load(Ordering::Relaxed)),
            mmap_base: AtomicUsize::new(self.mmap_base.load(Ordering::Relaxed)),
            mmap_limit: AtomicUsize::new(self.mmap_limit.load(Ordering::Relaxed)),
        }
    }
}

impl ProcessMemory {
    pub fn new(code_end: usize, stack_bottom: usize, stack_top: usize, mmap_floor: usize) -> Self {
        let base = (code_end + 0x1000_0000) & !0xFFFF;
        let mmap_start = core::cmp::max(base, mmap_floor);
        let mmap_limit = stack_bottom.saturating_sub(0x10_0000);

        Self {
            code_end: AtomicUsize::new(code_end),
            brk: AtomicUsize::new(code_end),
            stack_bottom: AtomicUsize::new(stack_bottom),
            stack_top: AtomicUsize::new(stack_top),
            next_mmap: AtomicUsize::new(mmap_start),
            mmap_base: AtomicUsize::new(mmap_start),
            mmap_limit: AtomicUsize::new(mmap_limit),
        }
    }

    /// Re-point every field for a new image, through `&self`. The `execve`
    /// equivalent of `*self = ProcessMemory::new(…)`, minus the `&mut`.
    pub fn reset(&self, code_end: usize, stack_bottom: usize, stack_top: usize, mmap_floor: usize) {
        let base = (code_end + 0x1000_0000) & !0xFFFF;
        let mmap_start = core::cmp::max(base, mmap_floor);
        let mmap_limit = stack_bottom.saturating_sub(0x10_0000);
        self.code_end.store(code_end, Ordering::Relaxed);
        self.brk.store(code_end, Ordering::Relaxed);
        self.stack_bottom.store(stack_bottom, Ordering::Relaxed);
        self.stack_top.store(stack_top, Ordering::Relaxed);
        self.next_mmap.store(mmap_start, Ordering::Relaxed);
        self.mmap_base.store(mmap_start, Ordering::Relaxed);
        self.mmap_limit.store(mmap_limit, Ordering::Relaxed);
    }

    pub fn overlaps_stack(&self, addr: usize, size: usize) -> bool {
        let end = addr.saturating_add(size);
        addr < self.stack_top.load(Ordering::Relaxed) && end > self.stack_bottom.load(Ordering::Relaxed)
    }

    pub const KERNEL_VA_START: usize = 0x4000_0000;
    /// Fallback top of the kernel-RAM identity-map VA hole, used only before the
    /// MMU knows the real RAM size (host unit tests).  At runtime the kernel uses
    /// the dynamic `crate::mmu::kernel_va_end()`, which scales with detected RAM —
    /// the 0xC000_0000 value here corresponds to a 2GB-RAM machine.  See
    /// `kernel_va_end()` for why this must track RAM size.
    pub const KERNEL_VA_END: usize   = 0xC000_0000;

    /// Place `size` bytes: first fit over `regions` from the cursor, wrapping
    /// once, inside `[mmap_base, mmap_limit)` and never in the kernel RAM
    /// identity-map hole. Advances the cursor past the placement.
    ///
    /// `regions` is this process's list and the caller **holds its lock**,
    /// which is what serializes `CLONE_VM` siblings — and the caller records
    /// the range in it before releasing the lock, or the next placement sees
    /// it free (`Process::vm_alloc_mmap` reserves it).
    pub fn place(&self, regions: &mut [akuma_mmap::MmapRegion], size: usize) -> Option<usize> {
        let w = akuma_mmap::place::Window {
            lo: self.mmap_base.load(Ordering::Relaxed),
            hi: self.mmap_limit.load(Ordering::Relaxed),
            // Dynamic top, which scales with RAM: the 2 GB-machine const put
            // mappings inside the identity map on bigger machines (the rustc
            // MEMORY>2GB crash).
            hole: Self::KERNEL_VA_START..crate::mmu::kernel_va_end(),
        };
        let base = akuma_mmap::place::find_free_va_from(regions, size, self.next_mmap.load(Ordering::Relaxed), &w).0?;
        self.next_mmap.store(base + size, Ordering::Relaxed);
        Some(base)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_memory_new() {
        let mem = ProcessMemory::new(0x10000, 0x7FFF_0000, 0x8000_0000, 0);
        assert_eq!(mem.code_end.load(Ordering::Relaxed), 0x10000);
        assert_eq!(mem.brk.load(Ordering::Relaxed), 0x10000);
        assert_eq!(mem.stack_bottom.load(Ordering::Relaxed), 0x7FFF_0000);
        assert_eq!(mem.stack_top.load(Ordering::Relaxed), 0x8000_0000);
    }

    #[test]
    fn process_memory_reset_repoints_every_field() {
        let mem = ProcessMemory::new(0x10000, 0x7FFF_0000, 0x8000_0000, 0);
        mem.next_mmap.store(0x2000_0000, Ordering::Relaxed);
        mem.reset(0x20000, 0x6FFF_0000, 0x7000_0000, 0);
        assert_eq!(mem.code_end.load(Ordering::Relaxed), 0x20000);
        assert_eq!(mem.brk.load(Ordering::Relaxed), 0x20000);
        assert_eq!(mem.stack_bottom.load(Ordering::Relaxed), 0x6FFF_0000);
        assert_eq!(mem.stack_top.load(Ordering::Relaxed), 0x7000_0000);
        assert_eq!(mem.next_mmap.load(Ordering::Relaxed), mem.mmap_base.load(Ordering::Relaxed), "reset rewinds the cursor");
    }

    #[test]
    fn process_memory_overlaps_stack() {
        let mem = ProcessMemory::new(0x10000, 0x7FFF_0000, 0x8000_0000, 0);
        assert!(mem.overlaps_stack(0x7FFF_0000, 0x1000));
        assert!(!mem.overlaps_stack(0x1000, 0x1000));
    }

    #[test]
    fn process_memory_place_advances_the_cursor() {
        // With no regions the cursor alone keeps successive placements apart:
        // two `CLONE_VM` siblings under the region lock never share a VA.
        let mem = ProcessMemory::new(0x10000, 0x3000_0000, 0x3010_0000, 0);
        let a1 = mem.place(&mut [], 0x1000).unwrap();
        let a2 = mem.place(&mut [], 0x1000).unwrap();
        assert_ne!(a1, a2);
        assert_eq!(a1, mem.mmap_base.load(Ordering::Relaxed));
    }

    #[test]
    fn process_memory_place_never_straddles_the_kernel_va_hole() {
        let mem = ProcessMemory::new(0x1000_0000, 0xD000_0000, 0xD010_0000, 0);
        mem.next_mmap.store(ProcessMemory::KERNEL_VA_START - 0x1000, Ordering::Relaxed);
        let addr = mem.place(&mut [], 2 * 0x1000).unwrap();
        assert!(addr >= ProcessMemory::KERNEL_VA_END, "straddled the hole: {addr:#x}");
    }

    #[test]
    fn process_memory_place_reuses_an_unmapped_hole_after_the_wrap() {
        // The free list's job, done by the region list: once the window above
        // the cursor is full, a range nothing maps any more is found again.
        let mem = ProcessMemory::new(0x10000, 0x3000_0000, 0x3010_0000, 0);
        let base = mem.mmap_base.load(Ordering::Relaxed);
        let top = mem.mmap_limit.load(Ordering::Relaxed);
        let rest = (top - (base + 0x2000)) / 0x1000;
        let mut regions = [
            akuma_mmap::MmapRegion::inherited_with_prot(base, 1, akuma_mmap::Prot::RW),
            akuma_mmap::MmapRegion::inherited_with_prot(base + 0x2000, rest, akuma_mmap::Prot::RW),
        ];
        mem.next_mmap.store(base + 0x3000, Ordering::Relaxed);
        assert_eq!(mem.place(&mut regions, 0x1000), Some(base + 0x1000));
    }
}


