//! A task's page-table tree as a whole: a fresh root, the fork clone, the teardown.
//!
//! Moved from the amd64 half (B21 A5.5) so both
//! machines run one walker over `akuma-user-tree`'s per-entry plans.
//!
//! What differs per machine is behind [`UserTree`]: the entry format
//! ([`UserTree::LAYOUT`]), which root slots are the user's, which entries inside
//! a user table belong to the kernel, and how the kernel's part of a root is
//! installed. On amd64 the kernel is the upper half of the PML4 and nothing
//! inside a user table is the kernel's. On aarch64 the kernel shares TTBR0 with
//! the task: the device window is a root slot, and the RAM identity is two
//! 1 GiB blocks inside the first root slot's table, which the walkers copy
//! verbatim and never free.
//!
//! Depth counts the levels below a table's entries: the root's entries are at
//! depth 3, a leaf table's (4 KiB frames) at depth 0.

use akuma_user_tree::{ClonePlan, FreeStep, PteLayout, SharedSpans};

use crate::lazy;
use crate::mm::Mm;
use crate::mmu::{FreshPage, UserPages};

/// The machine's page-table tree, as the fork clone and the teardown see it.
pub trait UserTree: UserPages + Mm + lazy::Kernel {
    /// The entry format.
    const LAYOUT: PteLayout;
    /// Entry `i` of the table frame at `table`.
    fn entry(table: u64, i: usize) -> u64;
    /// Overwrite entry `i` of the table frame at `table`. No TLB maintenance.
    fn set_entry(table: u64, i: usize, value: u64);
    /// Is root slot `i` the task's (walked, cloned, freed)?
    fn user_root_slot(i: usize) -> bool;
    /// Is the entry at `depth` that covers `va`, inside a task's own table, the
    /// kernel's? Such an entry is copied verbatim into a clone and never freed.
    fn kernel_entry(depth: u32, va: u64) -> bool;
    /// Give the root `dst` the kernel's part of every address space. Called on
    /// a fresh root and after a clone filled the task's slots.
    fn install_kernel(dst: u64) -> bool;
    /// The root this core runs on now. Never freed from under itself.
    fn live_root() -> u64;
}

/// The address the entry at `i` of a table at `depth` covers, under `va_base`.
const fn entry_va(va_base: u64, depth: u32, i: usize) -> u64 {
    va_base | ((i as u64) << (12 + 9 * depth))
}

/// A fresh root: zeroed, with the kernel's part installed. `None` when a frame
/// cannot be had.
#[must_use]
pub fn new_root<M: UserTree>() -> Option<u64> {
    let root = <M::Frame as FreshPage>::alloc_zeroed()?.publish() as u64;
    if !M::install_kernel(root) {
        let _ = free_user_root::<M>(root);
        return None;
    }
    Some(root)
}

/// The fork's memory pass, into the fresh zeroed root `dst`.
///
/// The task slots of `parent` are cloned with leaves shared copy-on-write, then
/// the kernel's part is installed. `None` releases everything this pass linked; the root frame stays the
/// caller's. The parent is parked; nothing else edits either tree.
#[must_use]
pub fn clone_user_half<M: UserTree>(parent: u64, dst: u64) -> Option<()> {
    // The child inherits the parent's lazy file pages, so it holds their inodes too.
    if !lazy::fork_pins(parent, dst) || clone_root::<M>(parent, dst).is_none() {
        lazy::drop_pins(dst);
        return None;
    }
    if !M::install_kernel(dst) {
        release_prefix::<M>(dst, 512);
        lazy::drop_pins(dst);
        return None;
    }
    Some(())
}

fn clone_root<M: UserTree>(src: u64, dst: u64) -> Option<()> {
    for i in (0..512).filter(|&i| M::user_root_slot(i)) {
        let entry = M::entry(src, i);
        match M::LAYOUT.plan_clone(3, entry, false, SharedSpans::Copy) {
            ClonePlan::Skip => {}
            ClonePlan::Descend { table } => {
                let Some(sub) = clone_tree::<M>(table, 2, entry_va(0, 3, i)) else {
                    release_prefix::<M>(dst, i);
                    return None;
                };
                M::set_entry(dst, i, M::LAYOUT.relink(entry, sub));
            }
            // A 512 GiB leaf does not exist; release what this pass linked.
            ClonePlan::Refuse | ClonePlan::CopyLeaf { .. } | ClonePlan::ShareLeaf { .. } => {
                release_prefix::<M>(dst, i);
                return None;
            }
        }
    }
    Some(())
}

/// Give back the task slots below `upto` a failed clone linked into `dst`.
fn release_prefix<M: UserTree>(dst: u64, upto: usize) {
    for j in (0..upto).filter(|&j| M::user_root_slot(j)) {
        if let Some(FreeStep::Table(t)) = M::LAYOUT.plan_free(3, M::entry(dst, j)) {
            free_subtree::<M>(t, 2, entry_va(0, 3, j));
            M::set_entry(dst, j, 0);
        }
    }
}

/// One level of the clone. Leaves are SHARED (copy-on-write): the child's entry
/// names the parent's frame, the PMM counts the second holder, and a writable
/// leaf is demoted in BOTH trees (the parent's translation invalidated: a stale
/// writable one would write straight into the shared frame). A `MAP_SHARED`
/// span stays writable and shared. A lazy record is inherited as it stands. On
/// failure the partial subtree is released, so a refused fork leaks nothing.
fn clone_tree<M: UserTree>(table: u64, depth: u32, va_base: u64) -> Option<u64> {
    let out = <M::Frame as FreshPage>::alloc_zeroed()?.publish() as u64;
    for i in 0..512 {
        let entry = M::entry(table, i);
        let va = entry_va(va_base, depth, i);
        if M::kernel_entry(depth, va) {
            M::set_entry(out, i, entry);
            continue;
        }
        if depth == 0 && lazy::is_lazy(entry) {
            M::set_entry(out, i, entry);
            continue;
        }
        let in_shared = depth == 0 && M::with_task(|t| t.layout.in_shared(va));
        match M::LAYOUT.plan_clone(depth, entry, in_shared, SharedSpans::ShareWritable) {
            ClonePlan::Skip => {}
            ClonePlan::Refuse => {
                free_subtree::<M>(out, depth, va_base);
                return None;
            }
            ClonePlan::Descend { table: sub_src } => {
                let Some(sub) = clone_tree::<M>(sub_src, depth - 1, va) else {
                    free_subtree::<M>(out, depth, va_base);
                    return None;
                };
                M::set_entry(out, i, M::LAYOUT.relink(entry, sub));
            }
            ClonePlan::CopyLeaf { old } => {
                let Some(mut frame) = <M::Frame as FreshPage>::alloc() else {
                    free_subtree::<M>(out, depth, va_base);
                    return None;
                };
                frame.copy_from(old as usize);
                M::set_entry(out, i, M::LAYOUT.relink(entry, frame.publish() as u64));
            }
            ClonePlan::ShareLeaf { frame, parent_entry, child_entry, demoted } => {
                akuma_pmm::cow_ref_inc(frame as usize);
                if demoted {
                    M::set_entry(table, i, parent_entry);
                    M::invalidate(va);
                }
                M::set_entry(out, i, child_entry);
                crate::cow::count_shared();
            }
        }
    }
    Some(out)
}

/// Release a subtree `depth` levels deep at `table` (leaves through the PMM, so
/// a shared frame only loses a count), the table frame last. Returns the frames
/// released. The kernel's entries are skipped. The subtree is unreachable from
/// any live root.
fn free_subtree<M: UserTree>(table: u64, depth: u32, va_base: u64) -> usize {
    let tid = akuma_primitives::preempt::current_tid() as u32;
    let mut n = 0;
    for i in 0..512 {
        let entry = M::entry(table, i);
        let va = entry_va(va_base, depth, i);
        if M::kernel_entry(depth, va) {
            continue;
        }
        // A huge leaf the loader never builds is not ours to guess at.
        if depth > 0 && entry & M::LAYOUT.present != 0 && M::LAYOUT.is_huge(entry) {
            continue;
        }
        match M::LAYOUT.plan_free(depth, entry) {
            None => {}
            Some(FreeStep::Leaf(f)) => {
                akuma_pmm::free_page(f as usize, tid);
                n += 1;
            }
            Some(FreeStep::Table(t)) => n += free_subtree::<M>(t, depth - 1, va),
        }
    }
    akuma_pmm::free_page(table as usize, tid);
    n + 1
}

/// Free a task's address space: every task slot's subtree, then the root.
///
/// The kernel's part is shared by every root and never touched. Returns the pages
/// released. The root must not be the live one (`0` released when it is).
#[must_use = "the count of pages released"]
pub fn free_user_root<M: UserTree>(root: u64) -> usize {
    if root == 0 || root == M::live_root() {
        return 0;
    }
    lazy::drop_pins(root);
    let mut n = 0;
    for i in (0..512).filter(|&i| M::user_root_slot(i)) {
        if let Some(FreeStep::Table(t)) = M::LAYOUT.plan_free(3, M::entry(root, i)) {
            n += free_subtree::<M>(t, 2, entry_va(0, 3, i));
        }
    }
    akuma_pmm::free_page(root as usize, akuma_primitives::preempt::current_tid() as u32);
    // The root's shared file pages were written back at its exit or exec; any
    // whose last mapping this was leave `shmpages` now.
    crate::shmpages::sweep();
    n + 1
}
