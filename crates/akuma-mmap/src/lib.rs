//! Virtual-memory **region bookkeeping** — the `mmap` family's pure data structure
//! and the algebra over it.
//!
//! # What is in here
//!
//! - [`PhysFrame`], the physical-frame handle a region's owned pages are recorded as.
//! - [`MmapRegion`], the eager-mapping record, and its constructors.
//! - [`inherit_mmap_regions_for_cow_child`] — what a CoW-forked child's region list is.
//! - [`detach_eager_regions_in_range`] — `munmap`'s clip-and-split.
//! - [`flags`] and [`user_flags`], the PTE permission vocabulary the two speak.
//!
//! # What is deliberately NOT in here
//!
//! Everything that has an *effect*. This crate cannot allocate or free a frame, edit
//! a page table, issue TLB maintenance, take a lock, or name a `Process` — it has no
//! dependencies at all, which is what makes those impossible rather than merely
//! discouraged. In particular the following stay in `akuma-exec`, and each for a
//! reason that is not stylistic:
//!
//! - `Process::mmap_regions` and the `vm_lock` / `vm_with_regions` discipline. That
//!   lock exists to close a `CLONE_VM` data race; it is a locking argument, not
//!   region algebra. A crate that cannot lock cannot get it wrong.
//! - `eager_region_flags_for_page_fault`, `eager_regions_containing`,
//!   `update_eager_region_flags`, `munmap_lazy_regions_in_range`,
//!   `record_mmap_region`, `remove_mmap_region`, `share_rw_range` — all pid-keyed:
//!   they resolve a process before they can touch a region list.
//! - `FaultAccess` / `lazy_map_flags` (`akuma_exec::mmu::types`) — demand-paging
//!   *policy*, the seam between the data-abort and instruction-abort arms of the EL0
//!   handler. It belongs with the fault path it serves.
//! - `PageTable`, `MAIR_*`, `attr_index` — page-table structure and memory
//!   attributes, which no region ever names.
//!
//! # Why `PhysFrame` lives here
//!
//! [`MmapRegion::frames`] is a `Vec<PhysFrame>`, so the frame handle has to sit at or
//! below this crate. It is a `Copy` newtype over a `usize` with **no `Drop` impl** —
//! a plain value, not an ownership token — so hosting it costs this crate no
//! allocator, no lock and no PMM. It was in `akuma_exec::runtime` before; the
//! `akuma-pmm` crate could never name it (it sits below `akuma-exec` and speaks in
//! raw `usize` addresses), and moving it down here does not change that, it just
//! stops the type being defined above every one of its users.
//!
//! `akuma_exec::runtime`, `akuma_exec::mmu::types` and `akuma_exec::process::types`
//! re-export everything this crate owns, so no call site outside changed when it
//! moved.
//!
//! # Testing
//!
//! Host tests only, and that is the point of the crate existing: region splitting is
//! where this code has gone wrong before (`docs/archive/CARGO_HEAP_NULL_RC.md` D8/D9,
//! `docs/archive/FORK_EXEC_HEAP_LAZY_REGION_SIGSEGV.md`), and every shape that
//! matters is reachable from a plain `Vec` with no live process, no address space and
//! no boot. Run them with the rest of the workspace:
//!
//! ```text
//! cargo test --target $(rustc -vV | grep '^host:' | cut -d' ' -f2)
//! ```

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

mod frame;
mod region;
mod types;

pub use frame::PhysFrame;
pub use region::{
    MmapRegion, detach_eager_regions_in_range, inherit_mmap_regions_for_cow_child,
    mprotect_eager_regions_in_range,
};
pub use types::{PAGE_SHIFT, PAGE_SIZE, flags, user_flags};
