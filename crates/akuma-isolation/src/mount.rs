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
