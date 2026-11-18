//! The live box registry: every box a `REGISTER_BOX` created, under a
//! spinlock with IRQs disabled for safe access from syscall and interrupt
//! context.

use alloc::string::String;
use alloc::vec::Vec;


use akuma_primitives::Pid;

use super::{BoxInfo, hierarchy};

static BOX_REGISTRY: akuma_irq_lock::IrqSpinlock<alloc::collections::BTreeMap<u64, BoxInfo>> =
    akuma_irq_lock::IrqSpinlock::new(alloc::collections::BTreeMap::new());

/// Register a new box in the global registry
pub fn register_box(info: BoxInfo) {
    {
        BOX_REGISTRY.lock().insert(info.id, info);
    };
}

/// Unregister a box from the global registry
#[must_use]
pub fn unregister_box(id: u64) -> Option<BoxInfo> {
    {
        BOX_REGISTRY.lock().remove(&id)
    }
}

/// List all active boxes
#[must_use]
pub fn list_boxes() -> Vec<BoxInfo> {
    {
        BOX_REGISTRY.lock().values().cloned().collect()
    }
}

/// Find a box ID by name
#[must_use]
pub fn find_box_by_name(name: &str) -> Option<u64> {
    {
        BOX_REGISTRY.lock().values().find(|b| b.name == name).map(|b| b.id)
    }
}

/// Get a box's name by ID
#[must_use]
pub fn get_box_name(id: u64) -> Option<String> {
    {
        BOX_REGISTRY.lock().get(&id).map(|b| b.name.clone())
    }
}

/// Look up a box by ID (returns a clone)
#[must_use]
pub fn get_box_info(id: u64) -> Option<BoxInfo> {
    {
        BOX_REGISTRY.lock().get(&id).cloned()
    }
}

/// Find the box whose primary PID matches, excluding Box 0.
/// Returns the box ID if found.
#[must_use]
pub fn find_primary_box(pid: Pid) -> Option<u64> {
    {
        BOX_REGISTRY.lock().values()
            .find(|b| b.primary_pid == pid && b.id != 0)
            .map(|b| b.id)
    }
}

/// [`hierarchy::reaches`] against the live registry, under its lock — no
/// snapshot, so no allocation. For checks on hot paths (`kill`).
#[must_use]
pub fn box_reaches(source: u64, target: u64) -> bool {
    if source == 0 || source == target {
        return true;
    }
    hierarchy::reaches(&BOX_REGISTRY.lock(), source, target)
}

/// Initialize the box registry with Box 0 (Host)
pub fn init_box_registry() {
    register_box(BoxInfo {
        id: 0,
        name: String::from("host"),
        root_dir: String::from("/"),
        creator_pid: 0,
        primary_pid: 1,
        parent_box_id: None,
    });
}

/// Get a snapshot of the registry (for hierarchy queries without holding the lock)
#[must_use]
pub fn registry_snapshot() -> alloc::collections::BTreeMap<u64, BoxInfo> {
    {
        BOX_REGISTRY.lock().clone()
    }
}

/// A private mount namespace (`docs/archive/BWRAP_ON_BOXES.md`).
struct PrivateNs {
    /// The final `pivot_root` (one with no old root left to keep) has run: nothing more can be mounted.
    locked: bool,
    born_us: u64,
    /// While a `pivot_root` has attached the pre-pivot view of the world at this path (inside the new root), until
    /// `umount2` detaches it.
    old_root: Option<String>,
}

/// Boxes that are **private mount namespaces** made by `clone(CLONE_NEWNS)`.
static PRIVATE_NS: akuma_irq_lock::IrqSpinlock<alloc::collections::BTreeMap<u64, PrivateNs>> =
    akuma_irq_lock::IrqSpinlock::new(alloc::collections::BTreeMap::new());

/// Record `box_id` as a private namespace, still buildable, born at `now_us`.
pub fn register_private_ns(box_id: u64, now_us: u64) {
    PRIVATE_NS.lock().insert(box_id, PrivateNs { locked: false, born_us: now_us, old_root: None });
}

/// `box_id` is a private namespace that is not locked: its processes may mount and pivot.
#[must_use]
pub fn private_ns_building(box_id: u64) -> bool {
    PRIVATE_NS.lock().get(&box_id).is_some_and(|n| !n.locked)
}

/// `box_id` is a private namespace, building or locked.
#[must_use]
pub fn is_private_ns(box_id: u64) -> bool {
    PRIVATE_NS.lock().contains_key(&box_id)
}

/// Lock a private namespace for good. `false` when it was already locked or is not one.
pub fn lock_private_ns(box_id: u64) -> bool {
    let mut t = PRIVATE_NS.lock();
    match t.get_mut(&box_id) {
        Some(n) if !n.locked => {
            n.locked = true;
            n.old_root = None;
            true
        }
        _ => false,
    }
}

/// Record (or clear) where the pre-pivot view is attached.
pub fn set_private_ns_old_root(box_id: u64, at: Option<String>) {
    if let Some(n) = PRIVATE_NS.lock().get_mut(&box_id) {
        n.old_root = at;
    }
}

/// Where `box_id`'s pre-pivot view is attached, while it is.
#[must_use]
pub fn private_ns_old_root(box_id: u64) -> Option<String> {
    PRIVATE_NS.lock().get(&box_id).and_then(|n| n.old_root.clone())
}

/// Forget a private namespace (its box was torn down).
pub fn unregister_private_ns(box_id: u64) {
    PRIVATE_NS.lock().remove(&box_id);
}

/// Ids of the private namespaces born before `cutoff_us`: the candidates for the sweep that tears down the empty
/// ones. A younger one may be a box whose first process has not been created yet.
#[must_use]
pub fn private_ns_born_before(cutoff_us: u64) -> Vec<u64> {
    PRIVATE_NS.lock().iter().filter(|(_, n)| n.born_us < cutoff_us).map(|(id, _)| *id).collect()
}
