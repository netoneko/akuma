//! The box registry of a kernel without containers: the host, box 0, and
//! nothing else.
//!
//! The same public names as the live registry, so `akuma-exec` and glue call
//! them unchanged. Nothing can register a box (the syscalls that would are
//! behind `sc-containers`), so every answer is the one box 0 gives: every
//! process is in it, it reaches itself, it is no process's primary box, and a
//! kill cascade has no descendants.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use akuma_primitives::Pid;

use super::BoxInfo;

/// The host box, as the live registry's `init_box_registry` registers it.
fn host() -> BoxInfo {
    BoxInfo {
        id: 0,
        name: String::from("host"),
        root_dir: String::from("/"),
        creator_pid: 0,
        primary_pid: 1,
        parent_box_id: None,
    }
}

/// No box but the host exists, so there is nothing to record.
pub fn register_box(_info: BoxInfo) {}

/// Nothing was registered, so nothing is removed.
#[must_use]
pub fn unregister_box(_id: u64) -> Option<BoxInfo> {
    None
}

/// The host alone.
#[must_use]
pub fn list_boxes() -> Vec<BoxInfo> {
    vec![host()]
}

/// `"host"` is box 0; no other name exists.
#[must_use]
pub fn find_box_by_name(name: &str) -> Option<u64> {
    (name == "host").then_some(0)
}

/// Box 0's name; no other id exists.
#[must_use]
pub fn get_box_name(id: u64) -> Option<String> {
    (id == 0).then(|| host().name)
}

/// Box 0's record; no other id exists.
#[must_use]
pub fn get_box_info(id: u64) -> Option<BoxInfo> {
    (id == 0).then(host)
}

/// No process is the primary of a box other than the host.
#[must_use]
pub fn find_primary_box(_pid: Pid) -> Option<u64> {
    None
}

/// Every process is in box 0, which reaches everything.
#[must_use]
pub fn box_reaches(_source: u64, _target: u64) -> bool {
    true
}

/// Box 0 exists without being registered.
pub fn init_box_registry() {}

/// The host alone, in the live registry's shape.
#[must_use]
pub fn registry_snapshot() -> BTreeMap<u64, BoxInfo> {
    BTreeMap::from([(0, host())])
}

/// The two access answers code outside the container syscalls asks for
/// (`spawn_ext`'s and `set_box_stack`'s box check; `kill_box`'s cascade).
pub mod access {
    use alloc::collections::BTreeMap;
    use alloc::vec;
    use alloc::vec::Vec;

    use akuma_primitives::Pid;

    use super::BoxInfo;

    /// Every caller is in box 0, and the host may act on any box (the live
    /// registry's first rule); a box other than 0 does not exist to act on.
    #[must_use]
    pub fn can_access_box(_registry: &BTreeMap<u64, BoxInfo>, _source: u64, _target: u64, _pid: Pid) -> bool {
        true
    }

    /// No box has descendants, so a cascade is the box itself.
    #[must_use]
    pub fn cascade_kill_order(_registry: &BTreeMap<u64, BoxInfo>, target: u64) -> Vec<u64> {
        vec![target]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The stub answers as the live registry does after `init_box_registry`
    /// with no box registered.
    #[test]
    fn only_the_host_exists() {
        assert_eq!(list_boxes().len(), 1);
        assert_eq!(find_box_by_name("host"), Some(0));
        assert_eq!(find_box_by_name("box1"), None);
        assert_eq!(get_box_name(0).as_deref(), Some("host"));
        assert!(get_box_info(1).is_none());
        assert_eq!(find_primary_box(1), None);
        assert!(box_reaches(0, 0));
        assert_eq!(registry_snapshot().keys().copied().collect::<Vec<_>>(), [0]);
        assert_eq!(access::cascade_kill_order(&registry_snapshot(), 0), [0]);
    }
}
