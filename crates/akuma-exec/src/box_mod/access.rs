//! Box access control using ancestry chains.
//!
//! Pure logic for determining access permissions between boxes.
//! Fully host-testable - operates on a BTreeMap snapshot.

use alloc::collections::BTreeMap;
use super::BoxInfo;
use super::hierarchy;
use crate::process::Pid;

/// Check if a process in `source_box` can access/create boxes in `target_box`.
///
/// Rules:
/// 1. Host (box 0) can access anything
/// 2. A box can access itself
/// 3. A box can access its descendants
/// 4. Creator PID check as fallback
pub fn can_access_box(
    registry: &BTreeMap<u64, BoxInfo>,
    source_box_id: u64,
    target_box_id: u64,
    source_pid: Pid,
) -> bool {
    if source_box_id == 0 {
        return true;
    }

    if source_box_id == target_box_id {
        return true;
    }

    if hierarchy::is_ancestor(registry, target_box_id, source_box_id) {
        return true;
    }

    if let Some(target) = registry.get(&target_box_id) {
        if target.creator_pid == source_pid {
            return true;
        }
    }

    false
}

/// Check if a box can be killed by a process.
///
/// Similar to `can_access_box` but also considers cascade implications.
/// A box can kill its descendants (which will cascade to their children).
pub fn can_kill_box(
    registry: &BTreeMap<u64, BoxInfo>,
    killer_box_id: u64,
    target_box_id: u64,
    killer_pid: Pid,
) -> bool {
    can_access_box(registry, killer_box_id, target_box_id, killer_pid)
}

/// Get the ordered list of box IDs to kill when cascade-killing `target_box_id`.
/// Returns descendants in reverse depth order (deepest children first)
/// so that cleanup proceeds leaf-to-root.
pub fn cascade_kill_order(
    registry: &BTreeMap<u64, BoxInfo>,
    target_box_id: u64,
) -> alloc::vec::Vec<u64> {
    let mut to_kill = hierarchy::get_descendants(registry, target_box_id);
    to_kill.push(target_box_id);
    to_kill
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate alloc;
    use alloc::collections::BTreeMap;
    use alloc::string::String;

    fn make_test_registry() -> BTreeMap<u64, super::BoxInfo> {
        let mut reg = BTreeMap::new();
        reg.insert(
            0,
            super::BoxInfo {
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
            super::BoxInfo {
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
            super::BoxInfo {
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
            super::BoxInfo {
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

    #[test]
    fn test_can_access_box_host_accesses_any() {
        let reg = make_test_registry();
        assert!(can_access_box(&reg, 0, 1, 0));
        assert!(can_access_box(&reg, 0, 2, 0));
        assert!(can_access_box(&reg, 0, 3, 0));
    }

    #[test]
    fn test_can_access_box_self() {
        let reg = make_test_registry();
        assert!(can_access_box(&reg, 1, 1, 101));
        assert!(can_access_box(&reg, 2, 2, 103));
    }

    #[test]
    fn test_can_access_box_parent_accesses_child() {
        let reg = make_test_registry();
        assert!(can_access_box(&reg, 1, 2, 101)); // box1 can access its child box2
        assert!(can_access_box(&reg, 0, 3, 1));   // host can access box3
    }

    #[test]
    fn test_can_access_box_child_cannot_access_parent() {
        let reg = make_test_registry();
        assert!(!can_access_box(&reg, 2, 1, 103)); // box2 cannot access box1
        assert!(!can_access_box(&reg, 1, 0, 101)); // box1 cannot access host
    }

    #[test]
    fn test_can_access_box_creator_pid_fallback() {
        let mut reg = make_test_registry();
        // Box 3 created by pid 200 (not in any box's primary)
        reg.get_mut(&3).unwrap().creator_pid = 200;
        // Process 200 in box 1 can access box 3 via creator fallback
        assert!(can_access_box(&reg, 1, 3, 200));
    }

    #[test]
    fn test_can_kill_box_same_rules_as_access() {
        let reg = make_test_registry();
        assert!(can_kill_box(&reg, 0, 1, 0));
        assert!(can_kill_box(&reg, 1, 1, 101));
        assert!(can_kill_box(&reg, 1, 2, 101));
        assert!(!can_kill_box(&reg, 2, 1, 103));
    }

    #[test]
    fn test_cascade_kill_order_includes_descendants_and_target() {
        let reg = make_test_registry();
        let order = cascade_kill_order(&reg, 0);
        assert_eq!(order.len(), 4);
        assert!(order.contains(&0));
        assert!(order.contains(&1));
        assert!(order.contains(&2));
        assert!(order.contains(&3));
    }

    #[test]
    fn test_cascade_kill_order_deeper_children_included() {
        let reg = make_test_registry();
        let order = cascade_kill_order(&reg, 1);
        assert_eq!(order.len(), 2); // box2 (child) + box1 (target)
        assert!(order.contains(&1));
        assert!(order.contains(&2));
    }
}
