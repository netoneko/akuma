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

    /// Mount `fs` at `path`, replacing whatever is already mounted there.
    ///
    /// The one caller is the overlay mount: a box's namespace is born with a
    /// `SubdirFs` at `/` (`create_box_namespace`), and turning that root into an
    /// overlay means swapping it, not stacking on it. There is deliberately no
    /// syscall that reaches this for `/` from userspace — `umount2` refuses to
    /// let a box drop the floor it stands on, and this must not become a way
    /// around that.
    pub fn mount_replace(&mut self, path: &str, fs: Arc<dyn Filesystem>) {
        self.mounts.retain(|m| m.path != path);
        self.mounts.push(NsMountEntry {
            path: String::from(path),
            fs,
        });
        self.mounts.sort_by(|a, b| b.path.len().cmp(&a.path.len()));
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

    /// Which filesystem `path` resolves to. A `/` mount is handed the whole
    /// path rather than a relative one, so the marker is read by its own name.
    fn tag_at(ns: &MountNamespace, path: &str) -> String {
        let (fs, _rel) = ns.resolve(path).expect("nothing mounted for path");
        String::from_utf8(fs.read_file("/tag").unwrap()).unwrap()
    }

    #[test]
    fn mount_rejects_a_duplicate_but_replace_swaps_it() {
        let mut ns = MountNamespace::new();
        ns.mount("/", tagged("subdir")).unwrap();
        assert_eq!(ns.mount("/", tagged("overlay")).unwrap_err(), FsError::AlreadyExists);
        assert_eq!(tag_at(&ns, "/tag"), "subdir");

        ns.mount_replace("/", tagged("overlay"));

        assert_eq!(tag_at(&ns, "/tag"), "overlay");
        assert_eq!(ns.list_mounts().len(), 1, "replace must not leave the old entry behind");
    }

    #[test]
    fn replacing_the_root_leaves_deeper_mounts_winning() {
        let mut ns = MountNamespace::new();
        ns.mount("/", tagged("root")).unwrap();
        ns.mount("/proc", tagged("proc")).unwrap();

        ns.mount_replace("/", tagged("overlay"));

        assert_eq!(tag_at(&ns, "/proc/tag"), "proc");
        assert_eq!(tag_at(&ns, "/etc/tag"), "overlay");
    }

    #[test]
    fn replace_on_an_empty_namespace_is_just_a_mount() {
        let mut ns = MountNamespace::new();
        ns.mount_replace("/", tagged("overlay"));
        assert_eq!(tag_at(&ns, "/tag"), "overlay");
    }
}
