//! In-Memory Filesystem
//!
//! A simple RAM-based filesystem for temporary storage.
//! Files and directories exist only in memory and are lost on reboot.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use spinning_top::Spinlock;

use super::{DirEntry, Filesystem, FsError, FsStats, Metadata};

// ============================================================================
// File System Node
// ============================================================================

/// A node in the filesystem tree
#[derive(Clone)]
enum FsNode {
    /// A file with its contents
    File {
        data: Vec<u8>,
        created: u64,
        modified: u64,
    },
    /// A directory containing child nodes
    Directory {
        children: BTreeMap<String, FsNode>,
        created: u64,
    },
}

impl FsNode {
    fn new_file() -> Self {
        let now = current_time();
        FsNode::File {
            data: Vec::new(),
            created: now,
            modified: now,
        }
    }

    fn new_directory() -> Self {
        FsNode::Directory {
            children: BTreeMap::new(),
            created: current_time(),
        }
    }

    fn is_dir(&self) -> bool {
        matches!(self, FsNode::Directory { .. })
    }
}

/// Get current time (Unix timestamp)
fn current_time() -> u64 {
    crate::timer::utc_time_us()
        .map(|us| us / 1_000_000)
        .unwrap_or(0)
}

// ============================================================================
// Memory Filesystem
// ============================================================================

/// In-memory filesystem implementation
pub struct MemoryFilesystem {
    root: Spinlock<FsNode>,
    /// Maximum size in bytes (0 = unlimited)
    max_size: u64,
}

impl MemoryFilesystem {
    fn path_inode(path: &str) -> u64 {
        let mut h: u64 = 0xcbf29ce484222325;
        for b in path.bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        h
    }

    /// Create a new empty in-memory filesystem
    pub fn new() -> Self {
        Self {
            root: Spinlock::new(FsNode::new_directory()),
            max_size: 0,
        }
    }

    /// Create with a size limit
    pub fn with_max_size(max_bytes: u64) -> Self {
        Self {
            root: Spinlock::new(FsNode::new_directory()),
            max_size: max_bytes,
        }
    }

    /// Navigate to a node by path, returning a reference
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

    /// Navigate to parent directory and get the filename
    fn navigate_parent<'a>(
        node: &'a mut FsNode,
        path: &str,
    ) -> Result<(&'a mut BTreeMap<String, FsNode>, String), FsError> {
        let path = path.trim_matches('/');
        let (parent_path, filename) = match path.rfind('/') {
            Some(idx) => (&path[..idx], &path[idx + 1..]),
            None => ("", path),
        };

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

    /// Calculate total size of all files
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
    fn name(&self) -> &str {
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

        // Check size limit
        if self.max_size > 0 {
            let current_size = Self::total_size(&root);
            if current_size + data.len() as u64 > self.max_size {
                return Err(FsError::NoSpace);
            }
        }

        let (parent, filename) = Self::navigate_parent(&mut root, path)?;

        let now = current_time();
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
            Some(FsNode::File { data: file_data, modified, .. }) => {
                let end = offset + data.len();
                if end > file_data.len() {
                    file_data.resize(end, 0);
                }
                file_data[offset..end].copy_from_slice(data);
                *modified = current_time();
                Ok(data.len())
            }
            Some(FsNode::Directory { .. }) => Err(FsError::NotAFile),
            None => Err(FsError::NotFound),
        }
    }

    fn append_file(&self, path: &str, data: &[u8]) -> Result<(), FsError> {
        let mut root = self.root.lock();

        // Check size limit
        if self.max_size > 0 {
            let current_size = Self::total_size(&root);
            if current_size + data.len() as u64 > self.max_size {
                return Err(FsError::NoSpace);
            }
        }

        let (parent, filename) = Self::navigate_parent(&mut root, path)?;

        let now = current_time();
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

        parent.insert(dirname, FsNode::new_directory());
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

        // 1. Get the node from old_path
        let (old_parent, old_filename) = Self::navigate_parent(&mut root, old_path)?;
        let node = old_parent.remove(&old_filename).ok_or(FsError::NotFound)?;

        // 2. Insert into new_path
        // We need to re-navigate because removing from old_parent might have changed the tree structure 
        // if old_parent and new_parent are the same or related.
        // Actually, we need to be careful with borrowing.
        
        // Since we already have the node, we just need to find the new parent.
        // Re-locking or re-navigating might be needed if we didn't use a single lock.
        // But we have a single lock on the entire root, so we are safe.
        
        // We need to re-navigate because we can't have two mutable references to different parts of the tree 
        // easily without unsafe or RefCell. But since we already removed the node, we can just navigate again.
        
        let (new_parent, new_filename) = match Self::navigate_parent(&mut root, new_path) {
            Ok(p) => p,
            Err(e) => {
                // Restore the node if navigation fails
                let (old_parent_retry, _) = Self::navigate_parent(&mut root, old_path)?;
                old_parent_retry.insert(old_filename, node);
                return Err(e);
            }
        };

        if new_parent.contains_key(&new_filename) {
            // Restore the node if destination exists
            // Linux rename replaces the destination if it's a file, but let's be safe for now
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
                mode: 0o100644,
                created: Some(*created),
                modified: Some(*modified),
                accessed: None,
            }),
            FsNode::Directory { created, .. } => Ok(Metadata {
                is_dir: true,
                size: 0,
                inode,
                mode: 0o40755,
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
            // Report available heap memory as total
            let heap_stats = crate::allocator::stats();
            heap_stats.free as u64 + used
        };

        Ok(FsStats {
            block_size: 4096,
            total_blocks: total / 4096,
            free_blocks: (total - used) / 4096,
        })
    }
}
