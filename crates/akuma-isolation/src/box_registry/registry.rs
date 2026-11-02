//! The live box registry: every box a `REGISTER_BOX` created, under a
//! spinlock with IRQs disabled for safe access from syscall and interrupt
//! context.

use alloc::string::String;
use alloc::vec::Vec;

use spinning_top::Spinlock;

use akuma_primitives::Pid;
use akuma_primitives::irq::with_irqs_disabled;

use super::{BoxInfo, hierarchy};

static BOX_REGISTRY: Spinlock<alloc::collections::BTreeMap<u64, BoxInfo>> =
    Spinlock::new(alloc::collections::BTreeMap::new());

/// Register a new box in the global registry
pub fn register_box(info: BoxInfo) {
    with_irqs_disabled(|| {
        BOX_REGISTRY.lock().insert(info.id, info);
    });
}

/// Unregister a box from the global registry
#[must_use]
pub fn unregister_box(id: u64) -> Option<BoxInfo> {
    with_irqs_disabled(|| {
        BOX_REGISTRY.lock().remove(&id)
    })
}

/// List all active boxes
#[must_use]
pub fn list_boxes() -> Vec<BoxInfo> {
    with_irqs_disabled(|| {
        BOX_REGISTRY.lock().values().cloned().collect()
    })
}

/// Find a box ID by name
#[must_use]
pub fn find_box_by_name(name: &str) -> Option<u64> {
    with_irqs_disabled(|| {
        BOX_REGISTRY.lock().values().find(|b| b.name == name).map(|b| b.id)
    })
}

/// Get a box's name by ID
#[must_use]
pub fn get_box_name(id: u64) -> Option<String> {
    with_irqs_disabled(|| {
        BOX_REGISTRY.lock().get(&id).map(|b| b.name.clone())
    })
}

/// Look up a box by ID (returns a clone)
#[must_use]
pub fn get_box_info(id: u64) -> Option<BoxInfo> {
    with_irqs_disabled(|| {
        BOX_REGISTRY.lock().get(&id).cloned()
    })
}

/// Find the box whose primary PID matches, excluding Box 0.
/// Returns the box ID if found.
#[must_use]
pub fn find_primary_box(pid: Pid) -> Option<u64> {
    with_irqs_disabled(|| {
        BOX_REGISTRY.lock().values()
            .find(|b| b.primary_pid == pid && b.id != 0)
            .map(|b| b.id)
    })
}

/// [`hierarchy::reaches`] against the live registry, under its lock — no
/// snapshot, so no allocation. For checks on hot paths (`kill`).
#[must_use]
pub fn box_reaches(source: u64, target: u64) -> bool {
    if source == 0 || source == target {
        return true;
    }
    with_irqs_disabled(|| hierarchy::reaches(&BOX_REGISTRY.lock(), source, target))
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
    with_irqs_disabled(|| {
        BOX_REGISTRY.lock().clone()
    })
}
