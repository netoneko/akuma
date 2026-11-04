//! A task's memory record and the per-core choice of which one the syscall path acts on.
//!
//! The record is a root page table (named by physical address, whatever the
//! machine's table format), the layout `akuma-user-layout` decides over, and
//! the exit code once captured. Descriptors, cwd and signal state are the
//! registered `akuma-exec` Process's. The address-space layout below is the
//! same on both architectures: every constant sits under 2^47, inside amd64's
//! canonical user half and AArch64's 48-bit `TTBR0` range.

use core::sync::atomic::{AtomicUsize, Ordering::Relaxed};

use akuma_primitives::cell::BorrowCell;

/// Top of the task's stack: canonical user half, far from anything the kernel
/// places. 16 pages — far more than musl's startup touches, and cheap.
pub const USER_STACK_TOP: u64 = 0x0000_7000_0000_0000;
pub const USER_STACK_PAGES: u64 = 16;
/// The anonymous `mmap` slab's size: 16 GiB of address space.
///
/// `brk` and anonymous `mmap` each get a fixed slab, registered as user
/// regions up front so every user-pointer check is one range scan. Sized by what the heaviest guest needs, not by what hello needs: rustc's
/// `--version` maps ~1 GiB of address space (measured; 64 MiB made it
/// `ENOMEM` before it printed). The cost is only pages actually touched, and
/// the range is far from PIE code (`PIE_BASE`) and the stack. The bounds are
/// constants for now; the per-process memory model comes with stage 12.
///
/// 16 GiB since 2026-10-30: compiling this kernel in the guest needs more than 1 GiB of
/// address space in ONE rustc - it maps the 311 MB `librustc_driver`, a ~17 MB stack per
/// codegen thread and every dependency's rlib - and the old 1 GiB slab answered `ENOMEM` to
/// the stack `mmap`, which musl's `pthread_create` reports as `EAGAIN` ("failed to spawn work
/// thread", an ICE) with 2 GiB of RAM still free (`docs/archive/TAOXICHUAN_BUGS_FOUND_2026_10_30.md`).
/// Address space only: a page costs memory when it is mapped, not when it is reserved.
pub const MMAP_SLAB: u64 = 0x4_0000_0000;
/// Hole-table size per task (the mmap slab's holes in [`UserLayout`]).
pub const MAX_HOLES: usize = 64;
/// `MAP_SHARED` mappings tracked per task: writable file ones (written back) and
/// anonymous ones (shared across fork).
pub const MAX_SHARED: usize = 32;
/// The `brk` window's size: 256 MiB.
///
/// `brk` sits above the image at `PIE_BASE` (0x1000_0000); the dynamic linker
/// is placed at `INTERP_BASE` (0x3000_0000), so the brk window must stay under
/// 512 MiB less the image: 256 MiB, not the 512 the plan first named.
pub const BRK_SLAB: u64 = 0x1000_0000;
pub const MMAP_BASE: u64 = 0x0000_6000_0000_0000;
/// 16 user regions, [`MAX_HOLES`] slab holes, [`MAX_SHARED`] shared file mappings.
pub type UserLayout = akuma_user_layout::Layout<16, MAX_HOLES, MAX_SHARED>;

/// A task's memory record.
pub struct Task {
    /// The root page table's physical address (0: no address space).
    pub root_pa: u64,
    /// The user ranges a pointer may name, the brk window, the mmap slab and its
    /// holes, the writable shared file mappings.
    pub layout: UserLayout,
    /// The exit code, once captured.
    pub exit: Option<u64>,
}

impl Task {
    /// An empty record.
    #[must_use]
    pub const fn new() -> Self {
        Self { root_pa: 0, layout: UserLayout::new(MMAP_BASE, MMAP_SLAB), exit: None }
    }
}

impl Default for Task {
    fn default() -> Self {
        Self::new()
    }
}

/// `N` task records and, per core (`C` of them), the index of the record the
/// syscall path on that core acts on (set on every switch-in; 0 is the boot
/// thread).
///
/// The records are a [`BorrowCell`]: two overlapping borrows are a panic naming
/// the bug. A closure therefore never calls anything that reaches a task record
/// again (the user-pointer checks, the user copies, the page-table walkers):
/// read what you need out, drop the borrow, then call.
pub struct Tasks<const N: usize, const C: usize> {
    records: BorrowCell<[Task; N]>,
    current: [AtomicUsize; C],
}

impl<const N: usize, const C: usize> Default for Tasks<N, C> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize, const C: usize> Tasks<N, C> {
    /// Every record empty, every core on record 0.
    #[must_use]
    pub const fn new() -> Self {
        Self { records: BorrowCell::new([const { Task::new() }; N]), current: [const { AtomicUsize::new(0) }; C] }
    }

    /// The record core `cpu` acts on.
    #[must_use]
    pub fn current(&self, cpu: usize) -> usize {
        self.current[cpu].load(Relaxed)
    }

    /// Core `cpu` acts on record `slot` from now on.
    pub fn set_current(&self, cpu: usize, slot: usize) {
        self.current[cpu].store(slot, Relaxed);
    }

    /// Run `f` on record `slot`.
    pub fn with_slot<R>(&self, slot: usize, f: impl FnOnce(&mut Task) -> R) -> R {
        self.records.with_mut(|t| f(&mut t[slot]))
    }

    /// The page-table root of record `slot`.
    #[must_use]
    pub fn root_of(&self, slot: usize) -> u64 {
        self.records.with(|t| t[slot].root_pa)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cores_pick_records_independently() {
        let t: Tasks<4, 2> = Tasks::new();
        t.set_current(1, 3);
        t.with_slot(3, |r| r.root_pa = 0x5000);
        assert_eq!((t.current(0), t.current(1)), (0, 3));
        assert_eq!(t.root_of(t.current(1)), 0x5000);
        assert_eq!(t.root_of(t.current(0)), 0);
    }

    #[test]
    #[should_panic]
    fn a_nested_borrow_is_a_panic() {
        let t: Tasks<2, 1> = Tasks::new();
        t.with_slot(0, |_| t.root_of(1));
    }

    #[test]
    fn layout_fits_both_user_halves() {
        for top in [USER_STACK_TOP, MMAP_BASE + MMAP_SLAB] {
            assert!(top < 1 << 47);
        }
    }
}
