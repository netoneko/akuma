use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use akuma_vfs::{DirEntry, Filesystem, FsError, MountInfo};

const MAX_NS_MOUNTS: usize = 16;

pub struct MountNamespace {
    mounts: Vec<NsMountEntry>,
}

struct NsMountEntry {
    path: String,
    fs: Arc<dyn Filesystem>,
}

impl MountNamespace {
    #[must_use]
    pub fn new() -> Self {
        Self {
            mounts: Vec::with_capacity(MAX_NS_MOUNTS),
        }
    }

    pub fn mount(&mut self, path: &str, fs: Arc<dyn Filesystem>) -> Result<(), FsError> {
        if self.mounts.len() >= MAX_NS_MOUNTS {
            return Err(FsError::NoSpace);
        }
        if self.mounts.iter().any(|m| m.path == path) {
            return Err(FsError::AlreadyExists);
        }
        self.mounts.push(NsMountEntry {
            path: String::from(path),
            fs,
        });
        self.mounts.sort_by(|a, b| b.path.len().cmp(&a.path.len()));
        Ok(())
    }

    /// Replace the namespace's root mount, but **only** while it is still the
    /// pristine one the namespace was born with.
    ///
    /// This exists for exactly one move: a box's namespace is created with a
    /// `SubdirFs` jail at `/` (`create_box_namespace`), and turning that root
    /// into an overlay is a swap, not a stack — `mount` would reject the
    /// duplicate path.
    ///
    /// Swapping a root is dangerous in a way a plain mount is not: every path a
    /// running process resolves, and every relative lookup it has in flight,
    /// silently starts landing somewhere else. So this is written as a one-shot
    /// rather than a general "replace": it fails unless `/` currently holds a
    /// filesystem named `expected`, which is true only before anything has
    /// re-rooted the box. A second call cannot undo the first, and no caller can
    /// use it to point a live box's `/` at a directory of its choosing.
    ///
    /// The complementary rule lives in `umount2`, which refuses to let a box
    /// drop its own `/`. Between them, a box's root can be set once and never
    /// removed or redirected.
    ///
    /// # Errors
    /// `PermissionDenied` if `/` is missing or is not `expected`.
    pub fn replace_pristine_root(
        &mut self,
        expected: &str,
        fs: Arc<dyn Filesystem>,
    ) -> Result<(), FsError> {
        let current = self
            .mounts
            .iter_mut()
            .find(|m| m.path == "/")
            .ok_or(FsError::PermissionDenied)?;
        if current.fs.name() != expected {
            return Err(FsError::PermissionDenied);
        }
        current.fs = fs;
        Ok(())
    }

    pub fn unmount(&mut self, path: &str) -> Result<(), FsError> {
        let idx = self
            .mounts
            .iter()
            .position(|m| m.path == path)
            .ok_or(FsError::NotFound)?;
        self.mounts.remove(idx);
        Ok(())
    }

    #[must_use]
    pub fn resolve<'a>(&'a self, path: &'a str) -> Option<(&'a dyn Filesystem, &'a str)> {
        let normalized = path.trim_end_matches('/');
        let normalized = if normalized.is_empty() {
            "/"
        } else {
            normalized
        };

        for mount in &self.mounts {
            if mount.path == "/" {
                return Some((mount.fs.as_ref(), normalized));
            }
            if normalized == mount.path {
                return Some((mount.fs.as_ref(), "/"));
            }
            if normalized.starts_with(&mount.path[..]) {
                let rest = &normalized[mount.path.len()..];
                if rest.is_empty() {
                    return Some((mount.fs.as_ref(), "/"));
                }
                if rest.starts_with('/') {
                    return Some((mount.fs.as_ref(), rest));
                }
            }
        }

        None
    }

    /// Like `resolve` but returns owned types so the lock can be released before I/O.
    #[must_use]
    pub fn resolve_arc(&self, path: &str) -> Option<(Arc<dyn Filesystem>, String)> {
        let normalized = path.trim_end_matches('/');
        let normalized = if normalized.is_empty() { "/" } else { normalized };

        for mount in &self.mounts {
            if mount.path == "/" {
                return Some((mount.fs.clone(), normalized.into()));
            }
            if normalized == mount.path {
                return Some((mount.fs.clone(), String::from("/")));
            }
            if normalized.starts_with(&mount.path[..]) {
                let rest = &normalized[mount.path.len()..];
                if rest.is_empty() {
                    return Some((mount.fs.clone(), String::from("/")));
                }
                if rest.starts_with('/') {
                    return Some((mount.fs.clone(), rest.into()));
                }
            }
        }

        None
    }

    #[must_use]
    pub fn list_mounts(&self) -> Vec<MountInfo> {
        self.mounts
            .iter()
            .map(|m| MountInfo {
                path: m.path.clone(),
                fs_type: String::from(m.fs.name()),
            })
            .collect()
    }

    #[must_use]
    pub fn child_mount_points(&self, parent_path: &str) -> Vec<DirEntry> {
        let mut entries = Vec::new();
        let parent = parent_path.trim_end_matches('/');
        let parent = if parent.is_empty() { "/" } else { parent };

        for m in &self.mounts {
            if m.path == "/" || m.path == parent {
                continue;
            }
            if parent == "/" {
                if m.path.starts_with('/') && !m.path[1..].contains('/') {
                    entries.push(DirEntry {
                        name: String::from(&m.path[1..]),
                        is_dir: true,
                        is_symlink: false,
                        size: 0,
                    });
                }
            } else {
                let prefix = alloc::format!("{parent}/");
                if m.path.starts_with(&prefix) {
                    let rest = &m.path[prefix.len()..];
                    if !rest.contains('/') {
                        entries.push(DirEntry {
                            name: String::from(rest),
                            is_dir: true,
                            is_symlink: false,
                            size: 0,
                        });
                    }
                }
            }
        }

        entries
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.mounts.is_empty()
    }
}

impl Default for MountNamespace {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use akuma_vfs::MemoryFilesystem;

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
