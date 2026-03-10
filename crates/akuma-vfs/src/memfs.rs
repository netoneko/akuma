//! In-Memory Filesystem
//!
//! A simple RAM-based filesystem for temporary storage.

// The spinlock guard must stay alive while we borrow from the locked data.
#![allow(clippy::significant_drop_tightening)]

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use spinning_top::Spinlock;

use crate::types::{DirEntry, Filesystem, FsError, FsStats, Metadata};

#[derive(Clone)]
enum FsNode {
    File {
        data: Vec<u8>,
        created: u64,
        modified: u64,
    },
    Directory {
        children: BTreeMap<String, Self>,
        created: u64,
    },
}

impl FsNode {
    const fn new_directory(now: u64) -> Self {
        Self::Directory {
            children: BTreeMap::new(),
            created: now,
        }
    }

    const fn is_dir(&self) -> bool {
        matches!(self, Self::Directory { .. })
    }
}

/// In-memory filesystem implementation.
///
/// Optionally takes a `time_fn` callback to provide timestamps.
/// If `None`, all timestamps are 0.
pub struct MemoryFilesystem {
    root: Spinlock<FsNode>,
    max_size: u64,
    time_fn: Option<fn() -> u64>,
}

impl MemoryFilesystem {
    fn now(&self) -> u64 {
        self.time_fn.map_or(0, |f| f())
    }

    fn path_inode(path: &str) -> u64 {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in path.bytes() {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
        h
    }

    #[must_use]
    pub fn new() -> Self {
        Self {
            root: Spinlock::new(FsNode::new_directory(0)),
            max_size: 0,
            time_fn: None,
        }
    }

    /// Create with a size limit.
    #[must_use]
    pub fn with_max_size(max_bytes: u64) -> Self {
        Self {
            root: Spinlock::new(FsNode::new_directory(0)),
            max_size: max_bytes,
            time_fn: None,
        }
    }

    /// Set a callback that provides the current Unix time in seconds.
    pub fn set_time_fn(&mut self, f: fn() -> u64) {
        self.time_fn = Some(f);
    }

    fn navigate<'a>(node: &'a FsNode, path: &str) -> Result<&'a FsNode, FsError> {
        let components: Vec<&str> = path
            .trim_matches('/')
            .split('/')
            .filter(|s| !s.is_empty())
            .collect();

        let mut current = node;
        for component in components {
            match current {
                FsNode::Directory { children, .. } => {
                    current = children.get(component).ok_or(FsError::NotFound)?;
                }
                FsNode::File { .. } => return Err(FsError::NotADirectory),
            }
        }
        Ok(current)
    }

    fn navigate_parent<'a>(
        node: &'a mut FsNode,
        path: &str,
    ) -> Result<(&'a mut BTreeMap<String, FsNode>, String), FsError> {
        let path = path.trim_matches('/');
        let (parent_path, filename) =
            path.rfind('/').map_or(("", path), |idx| (&path[..idx], &path[idx + 1..]));

        if filename.is_empty() {
            return Err(FsError::InvalidPath);
        }

        let components: Vec<&str> = parent_path.split('/').filter(|s| !s.is_empty()).collect();

        let mut current = node;
        for component in components {
            match current {
                FsNode::Directory { children, .. } => {
                    current = children.get_mut(component).ok_or(FsError::NotFound)?;
                }
                FsNode::File { .. } => return Err(FsError::NotADirectory),
            }
        }

        match current {
            FsNode::Directory { children, .. } => Ok((children, String::from(filename))),
            FsNode::File { .. } => Err(FsError::NotADirectory),
        }
    }

    fn total_size(node: &FsNode) -> u64 {
        match node {
            FsNode::File { data, .. } => data.len() as u64,
            FsNode::Directory { children, .. } => children.values().map(Self::total_size).sum(),
        }
    }
}

impl Default for MemoryFilesystem {
    fn default() -> Self {
        Self::new()
    }
}

impl Filesystem for MemoryFilesystem {
    fn name(&self) -> &'static str {
        "memfs"
    }

    fn read_dir(&self, path: &str) -> Result<Vec<DirEntry>, FsError> {
        let root = self.root.lock();
        let node = Self::navigate(&root, path)?;

        match node {
            FsNode::Directory { children, .. } => {
                let entries = children
                    .iter()
                    .map(|(name, child)| DirEntry {
                        name: name.clone(),
                        is_dir: child.is_dir(),
                        is_symlink: false,
                        size: match child {
                            FsNode::File { data, .. } => data.len() as u64,
                            FsNode::Directory { .. } => 0,
                        },
                    })
                    .collect();
                Ok(entries)
            }
            FsNode::File { .. } => Err(FsError::NotADirectory),
        }
    }

    fn read_file(&self, path: &str) -> Result<Vec<u8>, FsError> {
        let root = self.root.lock();
        let node = Self::navigate(&root, path)?;

        match node {
            FsNode::File { data, .. } => Ok(data.clone()),
            FsNode::Directory { .. } => Err(FsError::NotAFile),
        }
    }

    fn read_at(&self, path: &str, offset: usize, buf: &mut [u8]) -> Result<usize, FsError> {
        let root = self.root.lock();
        let node = Self::navigate(&root, path)?;

        match node {
            FsNode::File { data, .. } => {
                if offset >= data.len() {
                    return Ok(0);
                }
                let n = buf.len().min(data.len() - offset);
                buf[..n].copy_from_slice(&data[offset..offset + n]);
                Ok(n)
            }
            FsNode::Directory { .. } => Err(FsError::NotAFile),
        }
    }

    fn write_file(&self, path: &str, data: &[u8]) -> Result<(), FsError> {
        let mut root = self.root.lock();

        if self.max_size > 0 {
            let current_size = Self::total_size(&root);
            if current_size + data.len() as u64 > self.max_size {
                return Err(FsError::NoSpace);
            }
        }

        let (parent, filename) = Self::navigate_parent(&mut root, path)?;

        let now = self.now();
        parent.insert(
            filename,
            FsNode::File {
                data: data.to_vec(),
                created: now,
                modified: now,
            },
        );

        Ok(())
    }

    fn write_at(&self, path: &str, offset: usize, data: &[u8]) -> Result<usize, FsError> {
        if data.is_empty() {
            return Ok(0);
        }

        let mut root = self.root.lock();
        let (parent, filename) = Self::navigate_parent(&mut root, path)?;

        match parent.get_mut(&filename) {
            Some(FsNode::File {
                data: file_data,
                modified,
                ..
            }) => {
                let end = offset + data.len();
                if end > file_data.len() {
                    file_data.resize(end, 0);
                }
                file_data[offset..end].copy_from_slice(data);
                *modified = self.now();
                Ok(data.len())
            }
            Some(FsNode::Directory { .. }) => Err(FsError::NotAFile),
            None => Err(FsError::NotFound),
        }
    }

    fn append_file(&self, path: &str, data: &[u8]) -> Result<(), FsError> {
        let mut root = self.root.lock();

        if self.max_size > 0 {
            let current_size = Self::total_size(&root);
            if current_size + data.len() as u64 > self.max_size {
                return Err(FsError::NoSpace);
            }
        }

        let (parent, filename) = Self::navigate_parent(&mut root, path)?;

        let now = self.now();
        match parent.get_mut(&filename) {
            Some(FsNode::File {
                data: existing,
                modified,
                ..
            }) => {
                existing.extend_from_slice(data);
                *modified = now;
            }
            Some(FsNode::Directory { .. }) => return Err(FsError::NotAFile),
            None => {
                parent.insert(
                    filename,
                    FsNode::File {
                        data: data.to_vec(),
                        created: now,
                        modified: now,
                    },
                );
            }
        }

        Ok(())
    }

    fn create_dir(&self, path: &str) -> Result<(), FsError> {
        let mut root = self.root.lock();
        let (parent, dirname) = Self::navigate_parent(&mut root, path)?;

        if parent.contains_key(&dirname) {
            return Err(FsError::AlreadyExists);
        }

        parent.insert(dirname, FsNode::new_directory(self.now()));
        Ok(())
    }

    fn remove_file(&self, path: &str) -> Result<(), FsError> {
        let mut root = self.root.lock();
        let (parent, filename) = Self::navigate_parent(&mut root, path)?;

        match parent.get(&filename) {
            Some(FsNode::File { .. }) => {
                parent.remove(&filename);
                Ok(())
            }
            Some(FsNode::Directory { .. }) => Err(FsError::NotAFile),
            None => Err(FsError::NotFound),
        }
    }

    fn remove_dir(&self, path: &str) -> Result<(), FsError> {
        let mut root = self.root.lock();
        let (parent, dirname) = Self::navigate_parent(&mut root, path)?;

        match parent.get(&dirname) {
            Some(FsNode::Directory { children, .. }) => {
                if !children.is_empty() {
                    return Err(FsError::DirectoryNotEmpty);
                }
                parent.remove(&dirname);
                Ok(())
            }
            Some(FsNode::File { .. }) => Err(FsError::NotADirectory),
            None => Err(FsError::NotFound),
        }
    }

    fn rename(&self, old_path: &str, new_path: &str) -> Result<(), FsError> {
        let mut root = self.root.lock();

        let (old_parent, old_filename) = Self::navigate_parent(&mut root, old_path)?;
        let node = old_parent.remove(&old_filename).ok_or(FsError::NotFound)?;

        let (new_parent, new_filename) = match Self::navigate_parent(&mut root, new_path) {
            Ok(p) => p,
            Err(e) => {
                let (old_parent_retry, _) = Self::navigate_parent(&mut root, old_path)?;
                old_parent_retry.insert(old_filename, node);
                return Err(e);
            }
        };

        if new_parent.contains_key(&new_filename) {
            let (old_parent_retry, _) = Self::navigate_parent(&mut root, old_path)?;
            old_parent_retry.insert(old_filename, node);
            return Err(FsError::AlreadyExists);
        }

        new_parent.insert(new_filename, node);
        Ok(())
    }

    fn exists(&self, path: &str) -> bool {
        let root = self.root.lock();
        Self::navigate(&root, path).is_ok()
    }

    fn metadata(&self, path: &str) -> Result<Metadata, FsError> {
        let root = self.root.lock();
        let node = Self::navigate(&root, path)?;
        let inode = Self::path_inode(path);

        match node {
            FsNode::File {
                data,
                created,
                modified,
            } => Ok(Metadata {
                is_dir: false,
                size: data.len() as u64,
                inode,
                mode: 0o10_0644,
                created: Some(*created),
                modified: Some(*modified),
                accessed: None,
            }),
            FsNode::Directory { created, .. } => Ok(Metadata {
                is_dir: true,
                size: 0,
                inode,
                mode: 0o4_0755,
                created: Some(*created),
                modified: None,
                accessed: None,
            }),
        }
    }

    fn stats(&self) -> Result<FsStats, FsError> {
        let root = self.root.lock();
        let used = Self::total_size(&root);
        let total = if self.max_size > 0 {
            self.max_size
        } else {
            used + 64 * 1024 * 1024 // report 64 MiB headroom when unlimited
        };

        Ok(FsStats {
            block_size: 4096,
            total_blocks: total / 4096,
            free_blocks: (total - used) / 4096,
        })
    }
}
