//! The decisions of a copy-on-write fork over a 4-level user page-table tree.
//!
//! A fork clones the parent's user half subtree by subtree: tables are always copied, leaf frames are
//! **shared** (a reference taken, a writable leaf demoted to read-only + the CoW marker in *both* trees)
//! except where policy says copy. The kernel's walker does the touching - read an entry, allocate a frame,
//! store an entry, `invlpg`. What is decided *per entry* is here, so it can be tested on a host with a
//! fake tree instead of a `-j4` build:
//!
//! * [`PteLayout::plan_clone`]: skip / refuse / descend / copy / share-and-demote, and the exact entries
//!   to store.
//! * [`PteLayout::plan_free`] and [`FreeStep`]: what releasing a (partial) tree visits, and in which order.
//! * [`SharedSpans`]: the one place the two kernels differ today, as a named choice.
//!
//! # What this crate does not do
//! No allocation, no reference counts, no page tables, no TLB. The share count itself lives in the PMM
//! (`akuma-pmm::cow_ref_*`); the crate only says *when* it is taken. Plans are returned by value, one per
//! entry - never a `Vec`, so a 100k-page fork pays no heap for the plan.
//!
//! # The invariants the model tests pin
//! 1. **Zero-sum:** cloning creates no leaf frame except for a copied shared span; every shared frame gains
//!    exactly one reference.
//! 2. **Demote:** exactly the writable leaves become read-only + marker, in the parent and in the child.
//! 3. **Free is exact:** freeing a clone and then its parent returns every reference and every table; the host
//!    twin of `scripts/kvm/goose/cow_leak_gate.py`'s `delta=0`.
//! 4. **Failure is exact:** a clone that runs out of frames at any point and is then released leaves every
//!    reference as it was before the attempt.

#![no_std]
#![forbid(unsafe_code)]

/// x86_64 page-table entry bits, as the plans read them. Taking them as a value keeps the walker's
/// encodings in the kernel (taoxichuan's CoW marker is bit 9, `PTE_COW`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PteLayout {
    /// Present.
    pub present: u64,
    /// Writable.
    pub writable: u64,
    /// Page-size bit: a 1 GiB / 2 MiB leaf at a table level.
    pub huge: u64,
    /// The software "this read-only page is a CoW share" marker.
    pub cow_marker: u64,
    /// The physical-address field.
    pub addr_mask: u64,
}

/// x86_64 4-level paging with the CoW marker in software bit 9.
pub const X86_64: PteLayout = PteLayout {
    present: 1,
    writable: 1 << 1,
    huge: 1 << 7,
    cow_marker: 1 << 9,
    addr_mask: 0x000f_ffff_ffff_f000,
};

/// What a fork does with a leaf that lies in a writable `MAP_SHARED` file mapping.
///
/// The kernels differ today and neither is wrong: the divergence is recorded here as a named choice with a
/// test per value, not as an accident of two copies.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SharedSpans {
    /// Copy the frame eagerly (taoxichuan): parent and child stop sharing writes through the fork, which a
    /// `MAP_SHARED` mapping must NOT do, but no shared-page cache exists to keep them coherent.
    Copy,
    /// Share the frame like any other leaf (amd64, whose `akuma-fpcache-rw` keeps shared writable pages coherent).
    ShareFrames,
}

/// What to do with one entry while cloning. `depth` is the number of levels BELOW the table the entry is in:
/// 2 = a PDPT entry, 1 = a PD entry, 0 = a leaf-table (PT) entry naming a 4 KiB frame.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ClonePlan {
    /// Not present: nothing to do.
    Skip,
    /// A huge page at a table level. The caller releases what it built and fails the fork.
    Refuse,
    /// A table: recurse into `table` at `depth - 1`, then store [`PteLayout::relink`] of the entry with the
    /// new child table's address.
    Descend {
        /// Physical address of the parent's child table.
        table: u64,
    },
    /// A leaf to copy eagerly: allocate a frame, copy `old` into it, store [`PteLayout::relink`] of the entry.
    CopyLeaf {
        /// The frame to copy from.
        old: u64,
    },
    /// A leaf to share: take one reference on `frame`; store `parent_entry` back into the parent's table if
    /// `demoted` (and `invlpg` that VA - the parent's TLB may hold it writable) and `child_entry` into the child's.
    ShareLeaf {
        /// The shared frame.
        frame: u64,
        /// The entry the parent keeps (the demoted one when `demoted`).
        parent_entry: u64,
        /// The entry the child gets (identical to `parent_entry`).
        child_entry: u64,
        /// The parent's entry changed and must be rewritten and flushed.
        demoted: bool,
    },
}

/// One thing a release of a table visits.
///
/// The order is: every entry's subtree first (a leaf is a reference dropped, a child table is released
/// recursively), the table's own frame last. A walker that frees the table first reads freed memory.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FreeStep {
    /// A leaf frame: drop one reference (a shared frame only loses a count).
    Leaf(u64),
    /// A child table: release it recursively (at `depth - 1`), then it is a table frame to free.
    Table(u64),
}

impl PteLayout {
    /// The frame address an entry names.
    #[must_use]
    pub const fn frame(&self, entry: u64) -> u64 {
        entry & self.addr_mask
    }

    /// `entry` re-pointed at `frame`, keeping every flag.
    #[must_use]
    pub const fn relink(&self, entry: u64, frame: u64) -> u64 {
        (entry & !self.addr_mask) | (frame & self.addr_mask)
    }

    /// A writable leaf as the share leaves it: read-only plus the marker. Anything else is unchanged.
    #[must_use]
    pub const fn demote(&self, entry: u64) -> u64 {
        if entry & self.writable != 0 { (entry & !self.writable) | self.cow_marker } else { entry }
    }

    /// Decide what to do with `entry` at `depth` levels above the leaves of its table.
    ///
    /// `in_shared_span` is whether the VA this entry maps lies in a writable `MAP_SHARED` file mapping
    /// (only meaningful at `depth == 0`).
    #[must_use]
    pub const fn plan_clone(&self, depth: u32, entry: u64, in_shared_span: bool, policy: SharedSpans) -> ClonePlan {
        if entry & self.present == 0 {
            return ClonePlan::Skip;
        }
        if depth > 0 {
            if entry & self.huge != 0 {
                return ClonePlan::Refuse;
            }
            return ClonePlan::Descend { table: self.frame(entry) };
        }
        let frame = self.frame(entry);
        if in_shared_span && matches!(policy, SharedSpans::Copy) {
            return ClonePlan::CopyLeaf { old: frame };
        }
        let shared = self.demote(entry);
        ClonePlan::ShareLeaf { frame, parent_entry: shared, child_entry: shared, demoted: shared != entry }
    }

    /// What releasing `entry` (at `depth`) visits, or `None` when it is not present.
    #[must_use]
    pub const fn plan_free(&self, depth: u32, entry: u64) -> Option<FreeStep> {
        if entry & self.present == 0 {
            return None;
        }
        Some(if depth == 0 { FreeStep::Leaf(self.frame(entry)) } else { FreeStep::Table(self.frame(entry)) })
    }
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests;
