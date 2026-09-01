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

use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};

pub use akuma_exec_core::process::*;

/// Memory regions for a process
#[derive(Debug)]
pub struct ProcessMemory {
    pub code_end: usize,
    pub brk: usize,
    pub stack_bottom: usize,
    pub stack_top: usize,
    /// Next available mmap VA. AtomicUsize so CLONE_VM goroutine threads
    /// (which share the parent Process via lookup_process) can race-free
    /// advance it using CAS without disabling IRQs.
    pub next_mmap: AtomicUsize,
    pub mmap_limit: usize,
    pub free_regions: Vec<(usize, usize)>,
}

impl Clone for ProcessMemory {
    fn clone(&self) -> Self {
        Self {
            code_end: self.code_end,
            brk: self.brk,
            stack_bottom: self.stack_bottom,
            stack_top: self.stack_top,
            next_mmap: AtomicUsize::new(self.next_mmap.load(Ordering::Relaxed)),
            mmap_limit: self.mmap_limit,
            free_regions: self.free_regions.clone(),
        }
    }
}

impl ProcessMemory {
    pub fn new(code_end: usize, stack_bottom: usize, stack_top: usize, mmap_floor: usize) -> Self {
        let base = (code_end + 0x1000_0000) & !0xFFFF;
        let mmap_start = core::cmp::max(base, mmap_floor);
        let mmap_limit = stack_bottom.saturating_sub(0x10_0000);

        Self {
            code_end,
            brk: code_end,
            stack_bottom,
            stack_top,
            next_mmap: AtomicUsize::new(mmap_start),
            mmap_limit,
            free_regions: Vec::new(),
        }
    }

    pub fn overlaps_stack(&self, addr: usize, size: usize) -> bool {
        let end = addr.saturating_add(size);
        addr < self.stack_top && end > self.stack_bottom
    }

    pub const KERNEL_VA_START: usize = 0x4000_0000;
    /// Fallback top of the kernel-RAM identity-map VA hole, used only before the
    /// MMU knows the real RAM size (host unit tests).  At runtime the kernel uses
    /// the dynamic `crate::mmu::kernel_va_end()`, which scales with detected RAM —
    /// the 0xC000_0000 value here corresponds to a 2GB-RAM machine.  See
    /// `kernel_va_end()` for why this must track RAM size.
    pub const KERNEL_VA_END: usize   = 0xC000_0000;

    pub fn alloc_mmap(&mut self, size: usize) -> Option<usize> {
        // Dynamic top of the kernel identity-map hole (scales with RAM size).
        let kva_end = crate::mmu::kernel_va_end();
        for i in 0..self.free_regions.len() {
            let (start, f_size) = self.free_regions[i];

            // Skip regions that overlap the kernel RAM identity map.
            if start < kva_end && start + f_size > Self::KERNEL_VA_START {
                continue;
            }

            if f_size >= size {
                self.free_regions.remove(i);
                if f_size > size {
                    self.free_regions.push((start + size, f_size - size));
                }
                return Some(start);
            }
        }

        // CAS loop: race-free advance of next_mmap vs CLONE_VM sibling goroutine threads.
        // All goroutine threads share the parent Process via lookup_process(owner_pid),
        // so next_mmap is genuinely shared. CAS prevents two goroutines from receiving
        // the same VA (goroutine stack aliasing → WILD-IA crash).
        loop {
            let cur = self.next_mmap.load(Ordering::Relaxed);
            let mut candidate = cur;

            // Skip over the kernel RAM identity-map range if the allocation would
            // overlap it. Jump to the dynamic top (kva_end), NOT the 2GB-machine
            // const — otherwise the bump pointer lands at 0xC000_0000 inside the
            // real identity map on >2GB-RAM machines (the rustc MEMORY>2GB crash).
            if candidate < kva_end && candidate + size > Self::KERNEL_VA_START {
                candidate = kva_end;
            }

            if self.overlaps_stack(candidate, size) {
                return None;
            }
            if candidate + size > self.mmap_limit {
                return None;
            }

            if self.next_mmap
                .compare_exchange(cur, candidate + size, Ordering::SeqCst, Ordering::Relaxed)
                .is_ok()
            {
                return Some(candidate);
            }
            // CAS failed: a CLONE_VM sibling updated next_mmap concurrently.
            // Reload and retry — they got a different address, so will we.
        }
    }

    pub fn free_mmap(&mut self, start: usize, size: usize) {
        self.free_regions.push((start, size));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_memory_new() {
        let mem = ProcessMemory::new(0x10000, 0x7FFF_0000, 0x8000_0000, 0);
        assert_eq!(mem.code_end, 0x10000);
        assert_eq!(mem.brk, 0x10000);
        assert_eq!(mem.stack_bottom, 0x7FFF_0000);
        assert_eq!(mem.stack_top, 0x8000_0000);
    }

    #[test]
    fn process_memory_overlaps_stack() {
        let mem = ProcessMemory::new(0x10000, 0x7FFF_0000, 0x8000_0000, 0);
        assert!(mem.overlaps_stack(0x7FFF_0000, 0x1000));
        assert!(!mem.overlaps_stack(0x1000, 0x1000));
    }

    #[test]
    fn process_memory_alloc_mmap_sequential() {
        let mut mem = ProcessMemory::new(0x10000, 0x3000_0000, 0x3010_0000, 0);
        let a1 = mem.alloc_mmap(0x1000);
        let a2 = mem.alloc_mmap(0x1000);
        assert!(a1.is_some());
        assert!(a2.is_some());
        assert_ne!(a1, a2);
    }

    #[test]
    fn process_memory_alloc_mmap_skips_kernel_va() {
        let mut mem = ProcessMemory::new(0x3FFF_0000, 0xD000_0000, 0xD010_0000, 0);
        let addr = mem.alloc_mmap(0x1000);
        if let Some(a) = addr {
            assert!(a < ProcessMemory::KERNEL_VA_START || a >= ProcessMemory::KERNEL_VA_END);
        }
    }

    #[test]
    fn process_memory_alloc_mmap_straddle_kernel_va_start() {
        // Regression: allocation starting one page before KERNEL_VA_START with size > 1 page
        // would straddle the boundary and land inside the kernel VA hole.
        let mut mem = ProcessMemory::new(0x1000_0000, 0xD000_0000, 0xD010_0000, 0);
        mem.next_mmap.store(ProcessMemory::KERNEL_VA_START - 0x1000, Ordering::Relaxed);
        let addr = mem.alloc_mmap(2 * 0x1000).unwrap();
        assert!(
            addr >= ProcessMemory::KERNEL_VA_END,
            "alloc straddled kernel VA hole: {:#x}",
            addr
        );
    }

    #[test]
    fn process_memory_free_and_reuse() {
        let mut mem = ProcessMemory::new(0x10000, 0x3000_0000, 0x3010_0000, 0);
        let a1 = mem.alloc_mmap(0x1000).unwrap();
        mem.free_mmap(a1, 0x1000);
        let a2 = mem.alloc_mmap(0x1000).unwrap();
        assert_eq!(a2, a1);
    }

    #[test]
    fn process_memory_alloc_no_duplicate_addresses() {
        // Two sequential alloc_mmap calls must return different addresses.
        // Regression: a race between CLONE_VM goroutine threads reading
        // next_mmap before either write could return the same VA to both.
        let mut mem = ProcessMemory::new(0x10000, 0x3000_0000, 0x3010_0000, 0);
        let a1 = mem.alloc_mmap(0x1000).unwrap();
        let a2 = mem.alloc_mmap(0x1000).unwrap();
        assert_ne!(a1, a2, "alloc_mmap returned same address twice: {:#x}", a1);
    }

    #[test]
    fn process_memory_lazy_munmap_no_recycle() {
        // Verifies that NOT calling free_mmap after a lazy munmap causes the
        // next alloc to advance past the freed range (no recycling loop).
        // Contrast with eager munmap where free_mmap IS called and reuse occurs.
        let mut mem = ProcessMemory::new(0x10000, 0x3000_0000, 0x3010_0000, 0);

        // Eager munmap pattern: free_mmap called → address reused.
        let a1 = mem.alloc_mmap(0x1000).unwrap();
        mem.free_mmap(a1, 0x1000);
        let a2 = mem.alloc_mmap(0x1000).unwrap();
        assert_eq!(a2, a1, "eager freed region should be reused");

        // Lazy munmap pattern: free_mmap NOT called → next alloc advances.
        let b1 = mem.alloc_mmap(0x1000).unwrap();
        // (simulate lazy munmap: skip free_mmap)
        let b2 = mem.alloc_mmap(0x1000).unwrap();
        assert_ne!(b2, b1, "lazy VA range must not be recycled without free_mmap");
    }

}


