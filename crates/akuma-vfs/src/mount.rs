//! Mount table — maps paths to filesystem implementations.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use crate::types::{DirEntry, Filesystem, FsError, MountInfo};

const MAX_MOUNTS: usize = 8;

struct MountEntry {
    path: String,
    fs: Box<dyn Filesystem>,
}

/// A table of mounted filesystems.
///
/// Not global — the kernel owns the singleton and provides process-aware
/// path resolution on top.
pub struct MountTable {
    mounts: Vec<MountEntry>,
}

impl MountTable {
    #[must_use]
    pub fn new() -> Self {
        Self {
            mounts: Vec::with_capacity(MAX_MOUNTS),
        }
    }

    pub fn mount(&mut self, path: &str, fs: Box<dyn Filesystem>) -> Result<(), FsError> {
        if self.mounts.len() >= MAX_MOUNTS {
            return Err(FsError::NoSpace);
        }
        if self.mounts.iter().any(|m| m.path == path) {
            return Err(FsError::AlreadyExists);
        }
        self.mounts.push(MountEntry {
            path: String::from(path),
            fs,
        });
        self.mounts
            .sort_by(|a, b| b.path.len().cmp(&a.path.len()));
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

    /// Resolve an **absolute** path to `(filesystem, relative_path)`.
    #[must_use] 
    pub fn resolve<'a>(&'a self, path: &'a str) -> Option<(&'a dyn Filesystem, &'a str)> {
        let normalized = normalize_path(path);

        for mount in &self.mounts {
            if mount.path == "/" {
                return Some((mount.fs.as_ref(), normalized));
            }
            if normalized == mount.path {
                return Some((mount.fs.as_ref(), "/"));
            }
            if normalized.starts_with(&mount.path) {
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

    /// List all mounted filesystems.
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

    /// Get mount points that are direct children of a directory.
    #[must_use]
    pub fn child_mount_points(&self, parent_path: &str) -> Vec<DirEntry> {
        let parent = normalize_path(parent_path);
        let mut entries = Vec::new();

        for mount in &self.mounts {
            if mount.path == "/" || mount.path == parent {
                continue;
            }
            let mount_path = mount.path.as_str();

            if parent == "/" {
                if mount_path.starts_with('/') && !mount_path[1..].contains('/') {
                    entries.push(DirEntry {
                        name: String::from(&mount_path[1..]),
                        is_dir: true,
                        size: 0,
                    });
                }
            } else {
                let prefix = format!("{parent}/");
                if mount_path.starts_with(&prefix) {
                    let rest = &mount_path[prefix.len()..];
                    if !rest.contains('/') {
                        entries.push(DirEntry {
                            name: String::from(rest),
                            is_dir: true,
                            size: 0,
                        });
                    }
                }
            }
        }

        entries
    }

    /// Sync all mounted filesystems.
    pub fn sync_all(&self) -> Result<(), FsError> {
        for mount in &self.mounts {
            mount.fs.sync()?;
        }
        Ok(())
    }
}

impl Default for MountTable {
    fn default() -> Self {
        Self::new()
    }
}

fn normalize_path(path: &str) -> &str {
    let path = path.trim_end_matches('/');
    if path.is_empty() { "/" } else { path }
}
