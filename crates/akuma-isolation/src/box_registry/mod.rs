//! Box (container) registry
//!
//! Tracks active boxes and their metadata. The registry is global and
//! protected by a spinlock with IRQs disabled for safe access from
//! syscall and interrupt context.
//!
//! # Why this lives in `akuma-isolation`
//!
//! It was `akuma_exec::box_registry` until 2026-08-30. Nothing in it is about
//! *executing* anything: a `BoxInfo` is an id, a name, a root directory and two
//! pids, and the `hierarchy`/`access` halves are a tree walk and a permission
//! predicate over that. What it *is* about is the container identity sitting
//! directly on top of this crate's mount and network namespaces — the same
//! subsystem (`docs/reference/subsystems/containers.md`), split across two crates
//! for no reason but where the first caller happened to be.
//!
//! Moving it here cost no `cargo tree` edge, because `akuma-exec` already
//! depended on `akuma-isolation`, and it brought 496 lines (55% of them test)
//! inside this crate's `#![forbid(unsafe_code)]`. `akuma_exec::box_registry` and
//! `akuma_exec::process::box_*` remain as re-exports, so no call site moved.
//! Survey and rationale: `docs/archive/AKUMA_EXEC_SPLIT_AGAIN.md` §3.1.

pub mod hierarchy;
pub mod access;

use alloc::string::String;
use alloc::vec::Vec;

use spinning_top::Spinlock;

use akuma_primitives::Pid;
use akuma_primitives::irq::with_irqs_disabled;

/// Information about an active box (container)
#[derive(Debug, Clone)]
pub struct BoxInfo {
    pub id: u64,
    pub name: String,
    pub root_dir: String,
    pub creator_pid: Pid,
    pub primary_pid: Pid,
    /// Parent box ID. None for top-level boxes (direct children of the host).
    pub parent_box_id: Option<u64>,
}

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

/// The four-box registry both [`hierarchy`] and [`access`] test against:
/// host(0) → box1(1) → nested(2), plus box3(3) as a second child of the host.
///
/// It was defined byte-identically in each of those two files' `mod tests`, which
/// is the whole of what CPD reported as a 60-line clone between them — the two
/// *functions* the survey named (`cascade_kill_order`, `validate_nested_root`)
/// share no logic at all (`TRIM_FAT_EMBARASSING_DUPLICATIONS.md` §4).
///
/// The shape is load-bearing for both sides and neither can shrink it: `hierarchy`
/// needs depth ≥ 2 to distinguish an ancestry *chain* from a parent lookup, and a
/// sibling subtree (box3) so a descendant walk that over-collects is visible;
/// `access` needs the same two properties to tell "host reaches everything" apart
/// from "everyone reaches everything".
#[cfg(test)]
pub(crate) fn make_test_registry() -> alloc::collections::BTreeMap<u64, BoxInfo> {
    let mut reg = alloc::collections::BTreeMap::new();
    reg.insert(
        0,
        BoxInfo {
            id: 0,
            name: String::from("host"),
            root_dir: String::from("/"),
            creator_pid: 0,
            primary_pid: 1,
            parent_box_id: None,
        },
    );
    reg.insert(
        1,
        BoxInfo {
            id: 1,
            name: String::from("box1"),
            root_dir: String::from("/containers/box1"),
            creator_pid: 100,
            primary_pid: 101,
            parent_box_id: Some(0),
        },
    );
    reg.insert(
        2,
        BoxInfo {
            id: 2,
            name: String::from("nested"),
            root_dir: String::from("/containers/box1/nested"),
            creator_pid: 102,
            primary_pid: 103,
            parent_box_id: Some(1),
        },
    );
    reg.insert(
        3,
        BoxInfo {
            id: 3,
            name: String::from("box3"),
            root_dir: String::from("/containers/box3"),
            creator_pid: 104,
            primary_pid: 105,
            parent_box_id: Some(0),
        },
    );
    reg
}
