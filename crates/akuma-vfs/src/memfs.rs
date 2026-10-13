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

/// A node's permission bits (`0o7777`: rwx for three classes plus setuid,
/// setgid and sticky) and owner. Recorded, never enforced here — permission
/// checks belong to the syscall layer.
#[derive(Clone, Copy)]
struct Perm {
    mode: u32,
    uid: u32,
    gid: u32,
}

impl Perm {
    const FILE: Self = Self { mode: 0o644, uid: 0, gid: 0 };
    const DIR: Self = Self { mode: 0o755, uid: 0, gid: 0 };
    /// A tmpfs root, as Linux mounts one by default: world-writable and sticky,
    /// so unprivileged processes can create files in it without being able to
    /// delete each other's.
    const ROOT: Self = Self { mode: 0o1777, uid: 0, gid: 0 };
}

#[derive(Clone)]
enum FsNode {
    File {
        data: Vec<u8>,
        created: u64,
        modified: u64,
        perm: Perm,
    },
    Directory {
        children: BTreeMap<String, Self>,
        created: u64,
        perm: Perm,
    },
}

impl FsNode {
    const fn new_directory(now: u64, perm: Perm) -> Self {
        Self::Directory {
            children: BTreeMap::new(),
            created: now,
            perm,
        }
    }

    const fn is_dir(&self) -> bool {
        matches!(self, Self::Directory { .. })
    }

    const fn perm_mut(&mut self) -> &mut Perm {
        match self {
            Self::File { perm, .. } | Self::Directory { perm, .. } => perm,
        }
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
            root: Spinlock::new(FsNode::new_directory(0, Perm::ROOT)),
            max_size: 0,
            time_fn: None,
        }
    }

    /// Create with a size limit.
    #[must_use]
    pub fn with_max_size(max_bytes: u64) -> Self {
        Self {
            root: Spinlock::new(FsNode::new_directory(0, Perm::ROOT)),
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

    fn navigate_mut<'a>(node: &'a mut FsNode, path: &str) -> Result<&'a mut FsNode, FsError> {
        let mut current = node;
        for component in path.split('/').filter(|s| !s.is_empty()) {
            match current {
                FsNode::Directory { children, .. } => {
                    current = children.get_mut(component).ok_or(FsError::NotFound)?;
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
        // Replacing a file's contents keeps its mode and owner, as a
        // truncate-and-write would.
        let perm = match parent.get(&filename) {
            Some(FsNode::File { perm, .. }) => *perm,
            _ => Perm::FILE,
        };
        parent.insert(
            filename,
            FsNode::File {
                data: data.to_vec(),
                created: now,
                modified: now,
                perm,
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


    fn create_dir(&self, path: &str) -> Result<(), FsError> {
        let mut root = self.root.lock();
        let (parent, dirname) = Self::navigate_parent(&mut root, path)?;

        if parent.contains_key(&dirname) {
            return Err(FsError::AlreadyExists);
        }

        parent.insert(dirname, FsNode::new_directory(self.now(), Perm::DIR));
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

    /// `atime` is accepted and dropped: `FsNode::File` has no access-time field
    /// (its `metadata` reports `accessed: None`), so there is nowhere to put it.
    /// That is not the silent-success failure the trait's default guards against
    /// — `mtime`, the one anything actually compares, is stored — but it is why
    /// `touch -a` on a memfs path is a no-op.
    fn set_times(
        &self,
        path: &str,
        _atime_secs: Option<u64>,
        mtime_secs: Option<u64>,
    ) -> Result<(), FsError> {
        let Some(m) = mtime_secs else { return Ok(()) };
        let mut root = self.root.lock();
        let (parent, filename) = Self::navigate_parent(&mut root, path)?;
        match parent.get_mut(&filename) {
            Some(FsNode::File { modified, .. }) => {
                *modified = m;
                Ok(())
            }
            Some(FsNode::Directory { .. }) => Err(FsError::NotAFile),
            None => Err(FsError::NotFound),
        }
    }

    fn chmod(&self, path: &str, mode: u32) -> Result<(), FsError> {
        let mut root = self.root.lock();
        Self::navigate_mut(&mut root, path)?.perm_mut().mode = mode & 0o7777;
        Ok(())
    }

    fn chown(&self, path: &str, uid: Option<u32>, gid: Option<u32>) -> Result<(), FsError> {
        let mut root = self.root.lock();
        let perm = Self::navigate_mut(&mut root, path)?.perm_mut();
        if let Some(u) = uid {
            perm.uid = u;
        }
        if let Some(g) = gid {
            perm.gid = g;
        }
        Ok(())
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
                perm,
            } => Ok(Metadata {
                is_dir: false,
                size: data.len() as u64,
                inode,
                mode: 0o10_0000 | perm.mode,
                created: Some(*created),
                modified: Some(*modified),
                accessed: None,
                links: None,
                uid: perm.uid,
                gid: perm.gid,
            }),
            FsNode::Directory { created, perm, .. } => Ok(Metadata {
                is_dir: true,
                size: 0,
                inode,
                mode: 0o4_0000 | perm.mode,
                created: Some(*created),
                modified: None,
                accessed: None,
                links: None,
                uid: perm.uid,
                gid: perm.gid,
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
