//! The inodes one address space's file mappings keep alive.
//!
//! A demand-paged file mapping reads its file long after `mmap` returns, so the
//! file's data must outlive an `unlink`. ext2 asks `akuma_primitives::inode_pin::is_pinned`
//! before freeing an inode's blocks, and this set is what answers `true` for
//! the files an address space maps.
//!
//! **One pin per distinct inode, not per mapping.** The pin table is global and
//! bounded; `ld.so` maps one shared object once per segment, and a pin held per
//! mapping is how that table saturates. A saturated table answers "pinned" for
//! every inode and stalls ext2's deferred frees
//! (`docs/archive/EXT2_UNLINK_INODE_BLOCK_LEAK.md`).
//!
//! **Released with the last mapping that names the inode**, not with the
//! address space. The caller decides when that is, because only its region list
//! knows: [`MappingPins::unpin`] after removing a region whose inode no surviving
//! region names, or [`MappingPins::retain`] with a predicate over the survivors.
//! Without a release, a long-lived process kept a claim on every file it had
//! ever mapped (`ext2probe`'s `reclaim[pinned]` phase: 0 % of deleted bytes
//! returned, against 85 % with it).
//!
//! It lives in the address space rather than in the region record because
//! `akuma_mmap::MmapRegion` cannot hold a pin: that crate's dependency table is
//! empty on purpose. Dropping the address space (exit, `exec`) drops every pin
//! with no teardown path having to know they exist.

use akuma_primitives::InodePin;
use alloc::vec::Vec;

/// The set of inodes an address space's file mappings pin. See the module docs.
#[derive(Debug, Default)]
pub struct MappingPins {
    pins: Vec<InodePin>,
}

impl MappingPins {
    #[must_use]
    pub const fn new() -> Self {
        Self { pins: Vec::new() }
    }

    /// Pin `inode`, unless it is pinned already. `inode == 0` means "no inode
    /// identity" and pins nothing.
    pub fn pin(&mut self, inode: u32) {
        if inode == 0 || self.contains(inode) {
            return;
        }
        self.pins.push(InodePin::new(inode));
    }

    /// Release the pin on `inode`, if held. The caller has established that no
    /// mapping in this address space names `inode` any more.
    pub fn unpin(&mut self, inode: u32) {
        self.pins.retain(|p| p.inode() != inode);
    }

    /// Keep only the pins whose inode `still_mapped` answers `true` for.
    pub fn retain(&mut self, still_mapped: impl Fn(u32) -> bool) {
        self.pins.retain(|p| still_mapped(p.inode()));
    }

    /// Take every pin `other` holds: a `fork` child inherits its parent's file
    /// mappings, so it inherits their claim on the files. Without this the
    /// child's mappings are kept alive only by the parent's pins, and a parent
    /// that execs or exits first leaves the child faulting on a freed inode.
    pub fn inherit(&mut self, other: &Self) {
        for pin in &other.pins {
            self.pin(pin.inode());
        }
    }

    #[must_use]
    pub fn contains(&self, inode: u32) -> bool {
        self.pins.iter().any(|p| p.inode() == inode)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.pins.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pins.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::MappingPins;
    use akuma_primitives::inode_pin::pin_count;

    // The pin table is global and `cargo test` runs threads, so every test owns
    // inodes no other test (here or in `akuma-primitives`) uses.

    #[test]
    fn one_pin_per_inode_however_often_it_is_mapped() {
        let ino = 0x5A11_0001;
        let mut p = MappingPins::new();
        p.pin(ino);
        p.pin(ino);
        p.pin(ino);
        assert_eq!(p.len(), 1);
        assert_eq!(pin_count(ino), 1);
        drop(p);
        assert_eq!(pin_count(ino), 0);
    }

    #[test]
    fn inode_zero_pins_nothing() {
        let mut p = MappingPins::new();
        p.pin(0);
        assert!(p.is_empty());
    }

    #[test]
    fn unpin_releases_only_that_inode() {
        let (a, b) = (0x5A11_0011, 0x5A11_0012);
        let mut p = MappingPins::new();
        p.pin(a);
        p.pin(b);
        p.unpin(a);
        assert_eq!(pin_count(a), 0);
        assert_eq!(pin_count(b), 1);
        assert!(!p.contains(a) && p.contains(b));
        p.unpin(a); // not held: no effect
        assert_eq!(pin_count(b), 1);
    }

    #[test]
    fn retain_keeps_what_the_predicate_names() {
        let (a, b, c) = (0x5A11_0021, 0x5A11_0022, 0x5A11_0023);
        let mut p = MappingPins::new();
        for i in [a, b, c] {
            p.pin(i);
        }
        p.retain(|i| i == b);
        assert_eq!((pin_count(a), pin_count(b), pin_count(c)), (0, 1, 0));
        assert_eq!(p.len(), 1);
    }

    #[test]
    fn inherit_takes_a_second_reference_per_inode() {
        let (a, b) = (0x5A11_0031, 0x5A11_0032);
        let mut parent = MappingPins::new();
        parent.pin(a);
        parent.pin(b);
        let mut child = MappingPins::new();
        child.pin(a); // already holds one of them
        child.inherit(&parent);
        assert_eq!(child.len(), 2);
        assert_eq!((pin_count(a), pin_count(b)), (2, 2));
        drop(parent);
        assert_eq!((pin_count(a), pin_count(b)), (1, 1));
        drop(child);
        assert_eq!((pin_count(a), pin_count(b)), (0, 0));
    }
}
