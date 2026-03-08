//! Box (container) registry
//!
//! Tracks active boxes and their metadata. The registry is global and
//! protected by a spinlock with IRQs disabled for safe access from
//! syscall and interrupt context.

use alloc::string::String;
use alloc::vec::Vec;

use spinning_top::Spinlock;

use crate::process::Pid;
use crate::runtime::with_irqs_disabled;

/// Information about an active box (container)
#[derive(Debug, Clone)]
pub struct BoxInfo {
    pub id: u64,
    pub name: String,
    pub root_dir: String,
    pub creator_pid: Pid,
    pub primary_pid: Pid,
}

static BOX_REGISTRY: Spinlock<alloc::collections::BTreeMap<u64, BoxInfo>> =
    Spinlock::new(alloc::collections::BTreeMap::new());

/// Register a new box in the global registry
pub fn register_box(info: BoxInfo) {
    with_irqs_disabled(|| {
        BOX_REGISTRY.lock().insert(info.id, info);
    })
}

/// Unregister a box from the global registry
pub fn unregister_box(id: u64) -> Option<BoxInfo> {
    with_irqs_disabled(|| {
        BOX_REGISTRY.lock().remove(&id)
    })
}

/// List all active boxes
pub fn list_boxes() -> Vec<BoxInfo> {
    with_irqs_disabled(|| {
        BOX_REGISTRY.lock().values().cloned().collect()
    })
}

/// Find a box ID by name
pub fn find_box_by_name(name: &str) -> Option<u64> {
    with_irqs_disabled(|| {
        BOX_REGISTRY.lock().values().find(|b| b.name == name).map(|b| b.id)
    })
}

/// Get a box's name by ID
pub fn get_box_name(id: u64) -> Option<String> {
    with_irqs_disabled(|| {
        BOX_REGISTRY.lock().get(&id).map(|b| b.name.clone())
    })
}

/// Look up a box by ID (returns a clone)
pub fn get_box_info(id: u64) -> Option<BoxInfo> {
    with_irqs_disabled(|| {
        BOX_REGISTRY.lock().get(&id).cloned()
    })
}

/// Find the box whose primary PID matches, excluding Box 0.
/// Returns the box ID if found.
pub fn find_primary_box(pid: Pid) -> Option<u64> {
    with_irqs_disabled(|| {
        BOX_REGISTRY.lock().values()
            .find(|b| b.primary_pid == pid && b.id != 0)
            .map(|b| b.id)
    })
}

/// Initialize the box registry with Box 0 (Host)
pub fn init_box_registry() {
    register_box(BoxInfo {
        id: 0,
        name: String::from("host"),
        root_dir: String::from("/"),
        creator_pid: 0,
        primary_pid: 1,
    });
}
