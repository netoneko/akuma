//! Box access control using ancestry chains.
//!
//! Pure logic for determining access permissions between boxes.
//! Fully host-testable - operates on a BTreeMap snapshot.

use alloc::collections::BTreeMap;
use super::BoxInfo;
use super::hierarchy;
use akuma_primitives::Pid;

/// Check if a process in `source_box` can access/create boxes in `target_box`.
///
/// Rules:
/// 1. Host (box 0) can access anything
/// 2. A box can access itself
/// 3. A box can access its descendants
/// 4. Creator PID check as fallback
#[must_use]
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

    if let Some(target) = registry.get(&target_box_id)
        && target.creator_pid == source_pid {
            return true;
        }

    false
}

/// Check if a box can be killed by a process.
///
/// Similar to `can_access_box` but also considers cascade implications.
/// A box can kill its descendants (which will cascade to their children).
#[must_use]
pub fn can_kill_box(
    registry: &BTreeMap<u64, BoxInfo>,
    killer_box_id: u64,
    target_box_id: u64,
    killer_pid: Pid,
) -> bool {
    can_access_box(registry, killer_box_id, target_box_id, killer_pid)
}

/// Decide whether a process may (re)register box `target_box_id` rooted at
/// `canonical_root_dir`, and what `parent_box_id` the registration should record.
///
/// Registration is a privilege boundary, not bookkeeping: the box's `root_dir`
/// becomes a `SubdirFs` jail that anything spawned into the box sees as `/`. Left
/// unchecked, a boxed process can mint a box rooted at `/` and spawn into it, or
/// overwrite the host box's entry.
///
/// `canonical_root_dir` must already be normalized (see
/// [`hierarchy::validate_nested_root`]).
///
/// Returns the parent box id to record, or the reason the caller may not do this.
/// `match`, not `if let`/`else`: the two arms are the two cases this function
/// exists to tell apart — re-registration of a live box vs. creation of a new one
/// — and each carries the comment explaining its rule. Collapsing it hides the
/// symmetry.
#[allow(clippy::single_match_else)]
pub fn can_register_box(
    registry: &BTreeMap<u64, BoxInfo>,
    caller_box_id: u64,
    caller_pid: Pid,
    target_box_id: u64,
    canonical_root_dir: &str,
) -> Result<Option<u64>, &'static str> {
    match registry.get(&target_box_id) {
        // Re-registration of a live box. herd does this legitimately — once with
        // a placeholder pid, then with the real one — so it must stay allowed for
        // whoever owns the box, and denied for everyone else. The recorded parent
        // is kept: a box must never be able to re-parent itself out from under
        // its creator and inherit the creator's reach.
        Some(existing) => {
            if !can_access_box(registry, caller_box_id, target_box_id, caller_pid) {
                return Err("Box is outside the caller's hierarchy");
            }
            validate_root_under_caller(registry, caller_box_id, canonical_root_dir)?;
            Ok(existing.parent_box_id)
        }
        // A brand-new box. Anyone may create one — nesting is a supported use —
        // but it becomes a child of the caller's box and its root must lie inside
        // the caller's own root, so a box can only ever subdivide its own jail.
        None => {
            if target_box_id == 0 {
                return Err("Box 0 is the host and cannot be created");
            }
            validate_root_under_caller(registry, caller_box_id, canonical_root_dir)?;
            Ok(Some(caller_box_id))
        }
    }
}

/// A root a caller is allowed to hand a box: inside the caller's own jail.
/// The host (box 0) is rooted at `/`, so nothing constrains it.
fn validate_root_under_caller(
    registry: &BTreeMap<u64, BoxInfo>,
    caller_box_id: u64,
    canonical_root_dir: &str,
) -> Result<(), &'static str> {
    if caller_box_id == 0 {
        return Ok(());
    }
    let caller = registry
        .get(&caller_box_id)
        .ok_or("Caller's own box is not registered")?;
    hierarchy::validate_nested_root(caller, canonical_root_dir)
}

/// Get the ordered list of box IDs to kill when cascade-killing `target_box_id`.
/// Returns descendants in reverse depth order (deepest children first)
/// so that cleanup proceeds leaf-to-root.
#[must_use]
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
    // The registry fixture both this module and its sibling assert against lives
    // one level up, in `box_mod` itself — see `make_test_registry`'s doc comment.
    use super::super::make_test_registry;

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
    fn test_can_register_box_host_may_register_anything() {
        let reg = make_test_registry();
        // New box, anywhere on the filesystem.
        assert_eq!(can_register_box(&reg, 0, 1, 42, "/srv/newbox"), Ok(Some(0)));
        // Re-registering an existing box keeps its recorded parent.
        assert_eq!(can_register_box(&reg, 0, 1, 2, "/containers/box1/nested"), Ok(Some(1)));
    }

    #[test]
    fn test_can_register_box_new_box_is_a_child_of_the_caller() {
        let reg = make_test_registry();
        // A process in box 1 creating box 42 under its own root: box 42's parent
        // is box 1, so box 1 keeps reach over it and box 3 does not.
        assert_eq!(can_register_box(&reg, 1, 101, 42, "/containers/box1/sub"), Ok(Some(1)));
    }

    #[test]
    fn test_can_register_box_rejects_root_outside_callers_jail() {
        let reg = make_test_registry();
        // The escape this check exists for: mint a box rooted at "/" and later
        // spawn into it for an unjailed view of the host filesystem.
        assert!(can_register_box(&reg, 1, 101, 42, "/").is_err());
        assert!(can_register_box(&reg, 1, 101, 42, "/etc").is_err());
        // A sibling box's root is off limits too.
        assert!(can_register_box(&reg, 1, 101, 42, "/containers/box3").is_err());
    }

    #[test]
    fn test_can_register_box_rejects_hijacking_the_host_entry() {
        let reg = make_test_registry();
        // Overwriting box 0's BoxInfo would reset the host's root_dir and pids.
        assert!(can_register_box(&reg, 1, 101, 0, "/containers/box1").is_err());
        assert!(can_register_box(&reg, 2, 103, 0, "/containers/box1/nested").is_err());
    }

    #[test]
    fn test_can_register_box_rejects_reregistering_an_unrelated_box() {
        let reg = make_test_registry();
        // Box 3 is a sibling of box 1, not a descendant.
        assert!(can_register_box(&reg, 1, 101, 3, "/containers/box1/sub").is_err());
        // ... and a child cannot re-register its parent.
        assert!(can_register_box(&reg, 2, 103, 1, "/containers/box1/nested/x").is_err());
    }

    #[test]
    fn test_can_register_box_rejects_unregistered_caller_box() {
        let reg = make_test_registry();
        // A caller claiming a box id that is not in the registry has no root to
        // validate against, so it gets nothing rather than a free pass.
        assert!(can_register_box(&reg, 77, 500, 42, "/srv/x").is_err());
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
