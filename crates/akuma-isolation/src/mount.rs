//! Container mount namespace.
//!
//! `MountNamespace` is `akuma_vfs::MountSet<16>` — the same mount-set
//! implementation the kernel's global `MountTable` uses, at a larger capacity.
//!
//! It used to be a near-verbatim 200-line copy of that table:
//! `mount`/`unmount`/`resolve`/`resolve_arc`/`list_mounts`/`child_mount_points`
//! byte-identical apart from the capacity constant, plus the same
//! trailing-slash trim inlined three times where `MountTable` called a helper.
//! `TRIMMING_FAT_EMBARASSING_DUPLICATIONS.md` §4 flagged it as the cross-crate
//! clone most likely to drift silently, and `akuma-vfs` is the crate this one
//! already depends on — so the shared half moved there and this is an alias.
//!
//! `replace_pristine_root` (the one-shot root swap a box needs, and the only
//! guarded write to an existing mount's `fs` in the tree) moved with it, guard
//! included — see `akuma_vfs::MountSet::replace_pristine_root` for why the guard
//! travels with the operation instead of living in a wrapper.
//!
//! The tests below stay here: they cover box-root semantics, which is this
//! crate's concern rather than the mount set's.

/// A container's mount namespace: up to 16 mounts, independent of the global table.
pub type MountNamespace = akuma_vfs::MountSet<16>;

#[cfg(test)]
mod tests {
    use super::MountNamespace;
    use akuma_vfs::{DirEntry, Filesystem, FsError, MemoryFilesystem};
    use alloc::string::String;
    use alloc::sync::Arc;
    use alloc::vec::Vec;

    /// A filesystem carrying one marker file, so `resolve` results are
    /// distinguishable by content.
    fn tagged(tag: &str) -> Arc<dyn Filesystem> {
        let fs = MemoryFilesystem::new();
        fs.write_file("/tag", tag.as_bytes()).unwrap();
        Arc::new(fs)
    }

    /// A stand-in for the overlay: same `Filesystem`, a name that is not the
    /// pristine one, so the one-shot rule can be exercised.
    fn overlay_stand_in() -> Arc<dyn Filesystem> {
        struct Named(Arc<MemoryFilesystem>);
        impl Filesystem for Named {
            fn name(&self) -> &str { "overlay" }
            fn read_dir(&self, p: &str) -> Result<Vec<DirEntry>, FsError> { self.0.read_dir(p) }
            fn read_file(&self, p: &str) -> Result<Vec<u8>, FsError> { self.0.read_file(p) }
            fn write_file(&self, p: &str, d: &[u8]) -> Result<(), FsError> { self.0.write_file(p, d) }
            fn create_dir(&self, p: &str) -> Result<(), FsError> { self.0.create_dir(p) }
            fn remove_file(&self, p: &str) -> Result<(), FsError> { self.0.remove_file(p) }
            fn remove_dir(&self, p: &str) -> Result<(), FsError> { self.0.remove_dir(p) }
            fn exists(&self, p: &str) -> bool { self.0.exists(p) }
            fn metadata(&self, p: &str) -> Result<akuma_vfs::Metadata, FsError> { self.0.metadata(p) }
            fn stats(&self) -> Result<akuma_vfs::FsStats, FsError> { self.0.stats() }
        }
        let inner = MemoryFilesystem::new();
        inner.write_file("/tag", b"overlay").unwrap();
        Arc::new(Named(Arc::new(inner)))
    }

    /// Which filesystem `path` resolves to. A `/` mount is handed the whole
    /// path rather than a relative one, so the marker is read by its own name.
    fn tag_at(ns: &MountNamespace, path: &str) -> String {
        let (fs, _rel) = ns.resolve(path).expect("nothing mounted for path");
        String::from_utf8(fs.read_file("/tag").unwrap()).unwrap()
    }

    #[test]
    fn mount_rejects_a_duplicate_but_a_pristine_root_can_be_swapped() {
        let mut ns = MountNamespace::new();
        ns.mount("/", tagged("memfs")).unwrap();
        assert_eq!(ns.mount("/", tagged("memfs")).unwrap_err(), FsError::AlreadyExists);

        ns.replace_pristine_root("memfs", overlay_stand_in()).unwrap();

        assert_eq!(tag_at(&ns, "/tag"), "overlay");
        assert_eq!(ns.list_mounts().len(), 1, "replace must not leave the old entry behind");
    }

    /// The one-shot rule: once the root is no longer the pristine jail, nothing
    /// can point it somewhere else.
    #[test]
    fn a_root_that_is_not_pristine_cannot_be_replaced_again() {
        let mut ns = MountNamespace::new();
        ns.mount("/", tagged("memfs")).unwrap();
        ns.replace_pristine_root("memfs", overlay_stand_in()).unwrap();

        assert_eq!(
            ns.replace_pristine_root("memfs", tagged("attacker")).unwrap_err(),
            FsError::PermissionDenied
        );
        assert_eq!(tag_at(&ns, "/tag"), "overlay", "the root did not move");
    }

    #[test]
    fn a_namespace_with_no_root_cannot_have_one_installed_this_way() {
        let mut ns = MountNamespace::new();
        assert_eq!(
            ns.replace_pristine_root("memfs", overlay_stand_in()).unwrap_err(),
            FsError::PermissionDenied
        );
        assert!(ns.resolve("/tag").is_none());
    }

    #[test]
    fn replacing_the_root_leaves_deeper_mounts_winning() {
        let mut ns = MountNamespace::new();
        ns.mount("/", tagged("memfs")).unwrap();
        ns.mount("/proc", tagged("proc")).unwrap();

        ns.replace_pristine_root("memfs", overlay_stand_in()).unwrap();

        assert_eq!(tag_at(&ns, "/proc/tag"), "proc");
        assert_eq!(tag_at(&ns, "/etc/tag"), "overlay");
    }
}
