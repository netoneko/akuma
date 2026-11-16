//! The identity of a file: which filesystem issued the inode number, and the number.
//!
//! An inode number means nothing without the mount that issued it. The page cache was once keyed by inode alone, and
//! "a box and the host ... could read each other's file data" (`docs/archive/FPCACHE_MOUNT_IDENTITY.md`). Two adjacent
//! `u32` parameters `(mount_id, inode)` cannot say that, and swap without a compile error: twenty-five signatures
//! took the pair. [`FileKey`] is the pair as one value, built from [`MountId`] and [`Ino`], which are distinct types.
//!
//! **Both ids are non-zero by construction** (`NonZeroU32`). Zero is "none" on the records the kernel keeps (a descriptor
//! with no resolved inode, `InodePin`'s "no inode", a mount id that is never issued), and it used to travel through
//! these signatures as a sentinel that each callee re-tested (`if inode == 0 { ... }`, thirteen times). A `FileKey` cannot
//! be zero, so the test happens once, where the record becomes a key: [`FileKey::try_from_raw`] is `None` for "no stable
//! identity", and a function that can fall back to the path takes `Option<FileKey>`.

use core::num::NonZeroU32;

/// A mount's id as the VFS issues it (never 0 for a real mount).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(transparent)]
pub struct MountId(NonZeroU32);

impl MountId {
    #[must_use]
    pub const fn new(id: u32) -> Option<Self> {
        match NonZeroU32::new(id) {
            Some(n) => Some(Self(n)),
            None => None,
        }
    }
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0.get()
    }
}

/// An inode number, meaningful only within one [`MountId`] (0 is "no inode" and is not representable).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(transparent)]
pub struct Ino(NonZeroU32);

impl Ino {
    #[must_use]
    pub const fn new(ino: u32) -> Option<Self> {
        match NonZeroU32::new(ino) {
            Some(n) => Some(Self(n)),
            None => None,
        }
    }
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0.get()
    }
}

/// A file: `(mount, inode)`, ordered mount first. Neither half can be zero; `Option<FileKey>` is 8 bytes.
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

    /// From a record's two raw fields, in the order the type name says: `None` when either is zero, i.e. when the record
    /// has no stable identity (no resolved inode, or no mount). The one place a raw pair becomes a key, next to the two
    /// field names.
    #[must_use]
    pub const fn try_from_raw(mount_id: u32, inode: u32) -> Option<Self> {
        match (MountId::new(mount_id), Ino::new(inode)) {
            (Some(mount), Some(ino)) => Some(Self { mount, ino }),
            _ => None,
        }
    }

    /// The raw `(mount_id, inode)`, for the `u32`-keyed tables and on-disk formats underneath.
    #[must_use]
    pub const fn parts(self) -> (u32, u32) {
        (self.mount.get(), self.ino.get())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(m: u32, i: u32) -> FileKey {
        FileKey::try_from_raw(m, i).expect("non-zero ids")
    }

    #[test]
    fn mount_is_part_of_identity() {
        assert_ne!(k(1, 7), k(2, 7));
        assert_ne!(k(1, 7), k(7, 1));
        assert_eq!(k(3, 9).parts(), (3, 9));
    }

    #[test]
    fn zero_on_either_half_has_no_key() {
        assert!(FileKey::try_from_raw(1, 1).is_some());
        assert_eq!(FileKey::try_from_raw(0, 5), None);
        assert_eq!(FileKey::try_from_raw(5, 0), None);
        assert_eq!(FileKey::try_from_raw(0, 0), None);
    }

    #[test]
    fn ordering_is_mount_first() {
        assert!(k(1, 99) < k(2, 1));
    }

    #[test]
    fn the_option_is_free() {
        assert_eq!(core::mem::size_of::<FileKey>(), 8);
        assert_eq!(core::mem::size_of::<Option<FileKey>>(), 8);
    }
}
