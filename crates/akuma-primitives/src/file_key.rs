//! The identity of a file: which filesystem issued the inode number, and the number.
//!
//! An inode number means nothing without the mount that issued it. The page cache was once keyed by inode alone, and
//! "a box and the host ... could read each other's file data" (`docs/archive/FPCACHE_MOUNT_IDENTITY.md`). Two adjacent
//! `u32` parameters `(mount_id, inode)` cannot say that, and swap without a compile error: twenty-five signatures
//! took the pair. [`FileKey`] is the pair as one value, built once per record from [`MountId`] and [`Ino`], which are
//! distinct types.
//!
//! Zero is "none" on both halves (`InodePin`'s "no inode", and a mount id that is never issued), so [`FileKey::is_shareable`]
//! is the one spelling of "this can name a file at all". The ids are plain `u32`, not `NonZeroU32`: a record that has
//! no inode yet is a normal state in callers that treat it as "do nothing", and a fallible constructor would have
//! changed each of them.

/// A mount's id as the VFS issues it (never 0 for a real mount).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(transparent)]
pub struct MountId(pub u32);

/// An inode number, meaningful only within one [`MountId`] (0 is "no inode").
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(transparent)]
pub struct Ino(pub u32);

/// A file: `(mount, inode)`, ordered mount first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FileKey {
    pub mount: MountId,
    pub ino: Ino,
}

impl FileKey {
    #[must_use]
    pub const fn new(mount: MountId, ino: Ino) -> Self {
        Self { mount, ino }
    }

    /// From a record's two raw fields, in the order the type name says. The one place a raw pair becomes a key: a
    /// signature takes a `FileKey`, so a swap can only happen here, next to the two field names.
    #[must_use]
    pub const fn from_raw(mount_id: u32, inode: u32) -> Self {
        Self { mount: MountId(mount_id), ino: Ino(inode) }
    }

    /// The raw `(mount_id, inode)`, for the `u32`-keyed tables and on-disk formats underneath.
    #[must_use]
    pub const fn parts(self) -> (u32, u32) {
        (self.mount.0, self.ino.0)
    }

    /// Whether this can name a file at all: a mount that was issued and an inode that exists.
    #[must_use]
    pub const fn is_shareable(self) -> bool {
        self.mount.0 != 0 && self.ino.0 != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mount_is_part_of_identity() {
        assert_ne!(FileKey::from_raw(1, 7), FileKey::from_raw(2, 7));
        assert_ne!(FileKey::from_raw(1, 7), FileKey::from_raw(7, 1));
        assert_eq!(FileKey::from_raw(3, 9).parts(), (3, 9));
    }

    #[test]
    fn zero_on_either_half_is_not_shareable() {
        assert!(FileKey::from_raw(1, 1).is_shareable());
        assert!(!FileKey::from_raw(0, 5).is_shareable());
        assert!(!FileKey::from_raw(5, 0).is_shareable());
    }

    #[test]
    fn ordering_is_mount_first() {
        assert!(FileKey::from_raw(1, 99) < FileKey::from_raw(2, 1));
    }
}
