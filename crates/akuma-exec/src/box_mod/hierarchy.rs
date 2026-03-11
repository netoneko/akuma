//! Box hierarchy operations.
//!
//! Pure logic for ancestry traversal, descendant enumeration, and
//! nested root validation. Fully host-testable - operates on a
//! BTreeMap snapshot rather than the global registry.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use super::BoxInfo;

/// Get the ancestry chain from a box to the root (host).
/// Returns `[box_id, parent_id, grandparent_id, ..., 0]` where 0 is the host.
/// Returns an empty vec if `box_id` is not in the registry.
pub fn get_ancestry_chain(registry: &BTreeMap<u64, BoxInfo>, box_id: u64) -> Vec<u64> {
    let mut chain = Vec::new();
    let mut current = box_id;

    loop {
        chain.push(current);
        if current == 0 {
            break;
        }
        match registry.get(&current) {
            Some(info) => match info.parent_box_id {
                Some(parent) => current = parent,
                None => {
                    chain.push(0);
                    break;
                }
            },
            None => break,
        }
    }

    chain
}

/// Check if `ancestor_id` is an ancestor of `box_id`.
pub fn is_ancestor(registry: &BTreeMap<u64, BoxInfo>, box_id: u64, ancestor_id: u64) -> bool {
    if box_id == ancestor_id {
        return false;
    }
    let chain = get_ancestry_chain(registry, box_id);
    chain.contains(&ancestor_id)
}

/// Get all direct children of a box.
pub fn get_children(registry: &BTreeMap<u64, BoxInfo>, parent_id: u64) -> Vec<u64> {
    registry.values()
        .filter(|b| b.parent_box_id == Some(parent_id) || (parent_id == 0 && b.parent_box_id.is_none() && b.id != 0))
        .map(|b| b.id)
        .collect()
}

/// Get all descendants (children, grandchildren, etc.) of a box.
pub fn get_descendants(registry: &BTreeMap<u64, BoxInfo>, parent_id: u64) -> Vec<u64> {
    let mut result = Vec::new();
    let mut stack = get_children(registry, parent_id);

    while let Some(id) = stack.pop() {
        result.push(id);
        let children = get_children(registry, id);
        stack.extend(children);
    }

    result
}

/// Validate that `child_root_dir` is within the parent's visible namespace.
/// A child's root must be a sub-path of the parent's root.
pub fn validate_nested_root(parent_info: &BoxInfo, child_root_dir: &str) -> Result<(), &'static str> {
    if child_root_dir.is_empty() {
        return Err("Child root_dir is empty");
    }

    let parent_root = parent_info.root_dir.as_str();

    if parent_root == "/" {
        return Ok(());
    }

    if child_root_dir.starts_with(parent_root) {
        Ok(())
    } else {
        Err("Child root_dir is not within parent's namespace")
    }
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
    fn test_get_ancestry_chain_host() {
        let reg = make_test_registry();
        assert_eq!(get_ancestry_chain(&reg, 0), [0]);
    }

    #[test]
    fn test_get_ancestry_chain_direct_child() {
        let reg = make_test_registry();
        assert_eq!(get_ancestry_chain(&reg, 1), [1, 0]);
    }

    #[test]
    fn test_get_ancestry_chain_grandchild() {
        let reg = make_test_registry();
        assert_eq!(get_ancestry_chain(&reg, 2), [2, 1, 0]);
    }

    #[test]
    fn test_get_ancestry_chain_missing_box() {
        let reg = make_test_registry();
        assert_eq!(get_ancestry_chain(&reg, 99), [99]);
    }

    #[test]
    fn test_is_ancestor_parent_of_child() {
        let reg = make_test_registry();
        assert!(is_ancestor(&reg, 2, 1));
        assert!(is_ancestor(&reg, 1, 0));
    }

    #[test]
    fn test_is_ancestor_host_of_all() {
        let reg = make_test_registry();
        assert!(is_ancestor(&reg, 1, 0));
        assert!(is_ancestor(&reg, 2, 0));
        assert!(is_ancestor(&reg, 3, 0));
    }

    #[test]
    fn test_is_ancestor_not_self() {
        let reg = make_test_registry();
        assert!(!is_ancestor(&reg, 1, 1));
        assert!(!is_ancestor(&reg, 0, 0));
    }

    #[test]
    fn test_get_children_host() {
        let reg = make_test_registry();
        let children = get_children(&reg, 0);
        assert!(children.contains(&1));
        assert!(children.contains(&3));
        assert_eq!(children.len(), 2);
    }

    #[test]
    fn test_get_children_child_box() {
        let reg = make_test_registry();
        let children = get_children(&reg, 1);
        assert_eq!(children, [2]);
    }

    #[test]
    fn test_get_children_leaf_box() {
        let reg = make_test_registry();
        let children = get_children(&reg, 2);
        assert!(children.is_empty());
        let children = get_children(&reg, 3);
        assert!(children.is_empty());
    }

    #[test]
    fn test_get_descendants_host() {
        let reg = make_test_registry();
        let descendants = get_descendants(&reg, 0);
        assert_eq!(descendants.len(), 3);
        assert!(descendants.contains(&1));
        assert!(descendants.contains(&2));
        assert!(descendants.contains(&3));
    }

    #[test]
    fn test_validate_nested_root_valid() {
        let reg = make_test_registry();
        let parent = reg.get(&1).unwrap();
        assert!(validate_nested_root(parent, "/containers/box1/nested").is_ok());
        assert!(validate_nested_root(parent, "/containers/box1/sub").is_ok());
    }

    #[test]
    fn test_validate_nested_root_invalid() {
        let reg = make_test_registry();
        let parent = reg.get(&1).unwrap();
        assert!(validate_nested_root(parent, "/containers/box3").is_err());
        assert!(validate_nested_root(parent, "/other").is_err());
    }

    #[test]
    fn test_validate_nested_root_empty() {
        let reg = make_test_registry();
        let parent = reg.get(&1).unwrap();
        assert!(validate_nested_root(parent, "").is_err());
    }

    #[test]
    fn test_validate_nested_root_parent_root_slash() {
        let reg = make_test_registry();
        let parent = reg.get(&0).unwrap(); // host has root_dir "/"
        assert!(validate_nested_root(parent, "/containers/box1").is_ok());
        assert!(validate_nested_root(parent, "/anything").is_ok());
    }
}
