//! Virtual Filesystem (VFS) Layer
//!
//! Kernel-side VFS: owns the global mount table, provides process-aware path
//! resolution, and re-exports types from the `akuma_vfs` crate.

pub mod ext2;
pub mod proc;

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use alloc::format;
use spinning_top::Spinlock;

// Re-export everything from the crate so existing `use crate::vfs::*` keeps working.
pub use akuma_vfs::{
    DirEntry, Filesystem, FsError, FsStats, Metadata, MountInfo,
    canonicalize_path, resolve_path, split_path,
};

// ============================================================================
// Mount Table (kernel-side global)
// ============================================================================

static MOUNT_TABLE: Spinlock<Option<akuma_vfs::MountTable>> = Spinlock::new(None);

// ============================================================================
// Per-box Mount Namespaces
// ============================================================================

static MOUNT_NAMESPACES: Spinlock<BTreeMap<u64, MountNamespace>> = Spinlock::new(BTreeMap::new());

const MAX_NS_MOUNTS: usize = 8;

/// Per-box mount namespace. Each box can have its own set of mounts
/// (e.g., /proc, /tmp) that override the global mount table for processes
/// in that box. Paths are box-local (pre-root_dir-scoping).
pub struct MountNamespace {
    mounts: Vec<NsMountEntry>,
}

struct NsMountEntry {
    path: String,
    fs: Arc<dyn Filesystem>,
}

impl MountNamespace {
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

    fn resolve<'a>(&'a self, path: &'a str) -> Option<(&'a dyn Filesystem, &'a str)> {
        let normalized = path.trim_end_matches('/');
        let normalized = if normalized.is_empty() { "/" } else { normalized };

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

    #[allow(dead_code)]
    pub fn list_mounts(&self) -> Vec<MountInfo> {
        self.mounts
            .iter()
            .map(|m| MountInfo {
                path: m.path.clone(),
                fs_type: String::from(m.fs.name()),
            })
            .collect()
    }
}

/// Create a new mount namespace for a box.
pub fn create_mount_namespace(box_id: u64) {
    MOUNT_NAMESPACES.lock().insert(box_id, MountNamespace::new());
}

/// Remove a box's mount namespace.
pub fn remove_mount_namespace(box_id: u64) {
    MOUNT_NAMESPACES.lock().remove(&box_id);
}

/// Mount a filesystem into a specific box's namespace.
pub fn mount_in_namespace(box_id: u64, path: &str, fs: Arc<dyn Filesystem>) -> Result<(), FsError> {
    let mut namespaces = MOUNT_NAMESPACES.lock();
    let ns = namespaces.get_mut(&box_id).ok_or(FsError::NotFound)?;
    ns.mount(path, fs)
}

/// Unmount a path from a specific box's namespace.
pub fn unmount_in_namespace(box_id: u64, path: &str) -> Result<(), FsError> {
    let mut namespaces = MOUNT_NAMESPACES.lock();
    let ns = namespaces.get_mut(&box_id).ok_or(FsError::NotFound)?;
    ns.unmount(path)
}

/// List mounts in a specific box's namespace.
#[allow(dead_code)]
pub fn list_namespace_mounts(box_id: u64) -> Vec<MountInfo> {
    let namespaces = MOUNT_NAMESPACES.lock();
    namespaces.get(&box_id).map_or_else(Vec::new, |ns| ns.list_mounts())
}

/// Normalize path with allocation (adds leading / if missing)
fn normalize_path_owned(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        String::from("/")
    } else if !trimmed.starts_with('/') {
        format!("/{}", trimmed)
    } else {
        String::from(trimmed)
    }
}

// ============================================================================
// Public API - Mount Operations
// ============================================================================

/// Initialize the VFS subsystem
pub fn init() {
    let mut table = MOUNT_TABLE.lock();
    if table.is_none() {
        *table = Some(akuma_vfs::MountTable::new());
    }
}

/// Mount a filesystem at the given path
pub fn mount(path: &str, fs: Arc<dyn Filesystem>) -> Result<(), FsError> {
    let mut table = MOUNT_TABLE.lock();
    let table = table.as_mut().ok_or(FsError::NotInitialized)?;
    table.mount(path, fs)
}

// ============================================================================
// Public API - File Operations (delegates to mounted filesystems)
// ============================================================================

/// Helper to get filesystem for a path.
///
/// Resolution order:
/// 1. Resolve relative path against CWD to get an absolute path
/// 2. If the process is in a box (box_id > 0), check the box-local mount
///    namespace first (using the pre-scoped absolute path)
/// 3. Apply root_dir scoping (prepend root_dir to path)
/// 4. Resolve against the global (host) mount table
fn with_fs<F, R>(path: &str, f: F) -> Result<R, FsError>
where
    F: FnOnce(&dyn Filesystem, &str) -> Result<R, FsError>,
{
    if let Some(proc) = akuma_exec::process::current_process() {
        let absolute = resolve_path(&proc.cwd, path);

        // Check box-local mount namespace before root_dir scoping
        if proc.box_id > 0 {
            let namespaces = MOUNT_NAMESPACES.lock();
            if let Some(ns) = namespaces.get(&proc.box_id) {
                if let Some((fs, rel)) = ns.resolve(&absolute) {
                    return f(fs, rel);
                }
            }
        }

        // Fall through to root_dir scoping + global mount table
        let normalized = if proc.root_dir != "/" {
            if proc.root_dir.ends_with('/') {
                format!("{}{}", proc.root_dir, &absolute[1..])
            } else {
                format!("{}{}", proc.root_dir, absolute)
            }
        } else {
            absolute
        };

        let table = MOUNT_TABLE.lock();
        let table = table.as_ref().ok_or(FsError::NotInitialized)?;
        let (fs, relative_path) = table.resolve(&normalized).ok_or(FsError::NotFound)?;
        f(fs, relative_path)
    } else {
        let normalized = normalize_path_owned(path);

        let table = MOUNT_TABLE.lock();
        let table = table.as_ref().ok_or(FsError::NotInitialized)?;
        let (fs, relative_path) = table.resolve(&normalized).ok_or(FsError::NotFound)?;
        f(fs, relative_path)
    }
}

/// List directory contents
/// 
/// This includes both entries from the underlying filesystem and any
/// mount points that appear as direct children of the listed directory.
pub fn list_dir(path: &str) -> Result<Vec<DirEntry>, FsError> {
    let mut entries = with_fs(path, |fs, rel| fs.read_dir(rel))?;

    // Add mount points that are direct children of this directory
    let mount_entries = get_child_mount_points(path);
    for mount_entry in mount_entries {
        // Only add if not already present (mount point shadows existing dir)
        if !entries.iter().any(|e| e.name == mount_entry.name) {
            entries.push(mount_entry);
        }
    }

    Ok(entries)
}

/// Read entire file contents as bytes
pub fn read_file(path: &str) -> Result<Vec<u8>, FsError> {
    with_fs(path, |fs, rel| fs.read_file(rel))
}

/// Read file contents as a string
pub fn read_to_string(path: &str) -> Result<String, FsError> {
    let bytes = read_file(path)?;
    String::from_utf8(bytes).map_err(|_| FsError::IoError)
}

/// Write data to a file (creates or truncates)
pub fn write_file(path: &str, data: &[u8]) -> Result<(), FsError> {
    with_fs(path, |fs, rel| fs.write_file(rel, data))
}

/// Append data to a file
pub fn append_file(path: &str, data: &[u8]) -> Result<(), FsError> {
    with_fs(path, |fs, rel| fs.append_file(rel, data))
}

/// Read data from a specific offset within a file
pub fn read_at(path: &str, offset: usize, buf: &mut [u8]) -> Result<usize, FsError> {
    with_fs(path, |fs, rel| fs.read_at(rel, offset, buf))
}

/// Resolve a file path to an inode number for use with read_at_by_inode.
pub fn resolve_inode(path: &str) -> Result<u32, FsError> {
    with_fs(path, |fs, rel| fs.resolve_inode(rel))
}

/// Read from a file by inode number, bypassing path lookup.
pub fn read_at_by_inode(path: &str, inode: u32, offset: usize, buf: &mut [u8]) -> Result<usize, FsError> {
    with_fs(path, |fs, _rel| fs.read_at_by_inode(inode, offset, buf))
}

/// Write data at a specific offset within a file
pub fn write_at(path: &str, offset: usize, data: &[u8]) -> Result<usize, FsError> {
    with_fs(path, |fs, rel| fs.write_at(rel, offset, data))
}

/// Create a directory
pub fn create_dir(path: &str) -> Result<(), FsError> {
    with_fs(path, |fs, rel| fs.create_dir(rel))
}

/// Remove a file
pub fn remove_file(path: &str) -> Result<(), FsError> {
    with_fs(path, |fs, rel| fs.remove_file(rel))
}

/// Remove an empty directory
pub fn remove_dir(path: &str) -> Result<(), FsError> {
    with_fs(path, |fs, rel| fs.remove_dir(rel))
}

/// Check if a path exists
pub fn exists(path: &str) -> bool {
    with_fs(path, |fs, rel| Ok(fs.exists(rel))).unwrap_or(false)
}

/// Get file size
pub fn file_size(path: &str) -> Result<u64, FsError> {
    with_fs(path, |fs, rel| fs.metadata(rel).map(|m| m.size))
}

/// Get metadata for a path
pub fn metadata(path: &str) -> Result<Metadata, FsError> {
    with_fs(path, |fs, rel| fs.metadata(rel))
}

/// Change file permissions
pub fn chmod(path: &str, mode: u32) -> Result<(), FsError> {
    with_fs(path, |fs, rel| fs.chmod(rel, mode))
}

/// Resolve a path to its fully-scoped form (applies CWD + root_dir).
fn scope_path(path: &str) -> String {
    if let Some(proc) = akuma_exec::process::current_process() {
        let abs = resolve_path(&proc.cwd, path);
        if proc.root_dir != "/" {
            if proc.root_dir.ends_with('/') {
                format!("{}{}", proc.root_dir, &abs[1..])
            } else {
                format!("{}{}", proc.root_dir, abs)
            }
        } else {
            abs
        }
    } else {
        normalize_path_owned(path)
    }
}

/// Rename/move a file or directory
pub fn rename(old_path: &str, new_path: &str) -> Result<(), FsError> {
    let old_full = scope_path(old_path);
    let new_full = scope_path(new_path);

    let table = MOUNT_TABLE.lock();
    let table = table.as_ref().ok_or(FsError::NotInitialized)?;

    let (old_fs, old_rel) = table.resolve(&old_full).ok_or(FsError::NotFound)?;
    let (new_fs, new_rel) = table.resolve(&new_full).ok_or(FsError::NotFound)?;

    if old_fs.name() != new_fs.name() {
        return Err(FsError::NotSupported);
    }

    old_fs.rename(old_rel, new_rel)
}

/// Get filesystem statistics for a path
pub fn stats(path: &str) -> Result<FsStats, FsError> {
    with_fs(path, |fs, _| fs.stats())
}

/// List all mounted filesystems
pub fn list_mounts() -> Result<Vec<MountInfo>, FsError> {
    let table = MOUNT_TABLE.lock();
    let table = table.as_ref().ok_or(FsError::NotInitialized)?;
    Ok(table.list_mounts())
}

// ============================================================================
// Symlink Support
// ============================================================================

/// Legacy in-memory symlink table (fallback for filesystems that don't support symlinks)
static SYMLINKS: Spinlock<Option<BTreeMap<String, String>>> = Spinlock::new(None);

pub fn create_symlink(link_path: &str, target: &str) -> Result<(), FsError> {
    // Try on-disk first via the mounted filesystem
    match with_fs(link_path, |fs, rel| fs.create_symlink(rel, target)) {
        Ok(()) => return Ok(()),
        Err(FsError::NotSupported) => {}
        Err(e) => return Err(e),
    }
    // Fallback to in-memory table
    let link = canonicalize_path(link_path);
    let mut table = SYMLINKS.lock();
    if table.is_none() { *table = Some(BTreeMap::new()); }
    table.as_mut().unwrap().insert(link, String::from(target));
    Ok(())
}

pub fn read_symlink(path: &str) -> Option<String> {
    // Try on-disk first
    if let Ok(target) = with_fs(path, |fs, rel| fs.read_symlink(rel)) {
        return Some(target);
    }
    // Fallback to in-memory table
    let canonical = canonicalize_path(path);
    let table = SYMLINKS.lock();
    table.as_ref().and_then(|t| t.get(&canonical).cloned())
}

pub fn is_symlink(path: &str) -> bool {
    // Try on-disk first
    if let Ok(result) = with_fs(path, |fs, rel| Ok(fs.is_symlink(rel))) {
        if result {
            return true;
        }
    }
    // Fallback to in-memory table
    let canonical = canonicalize_path(path);
    let table = SYMLINKS.lock();
    table.as_ref().map_or(false, |t| t.contains_key(&canonical))
}

pub fn remove_symlink(path: &str) -> bool {
    let canonical = canonicalize_path(path);
    let mut table = SYMLINKS.lock();
    table.as_mut().map_or(false, |t| t.remove(&canonical).is_some())
}

/// Resolve a path, following symlinks (up to 8 levels to prevent loops)
pub fn resolve_symlinks(path: &str) -> String {
    let mut resolved = canonicalize_path(path);
    for _ in 0..8 {
        let target = read_symlink(&resolved);
        match target {
            Some(t) => {
                if t.starts_with('/') {
                    resolved = canonicalize_path(&t);
                } else {
                    let (parent, _) = split_path(&resolved);
                    resolved = resolve_path(parent, &t);
                }
            }
            None => {
                if resolved == "/bin/sh" && crate::fs::exists("/bin/dash") {
                    resolved = String::from("/bin/dash");
                    continue;
                }
                break;
            }
        }
    }
    resolved
}

fn get_child_mount_points(parent_path: &str) -> Vec<DirEntry> {
    let mut entries = Vec::new();

    // If the current process is in a box, include mount points from its namespace
    if let Some(proc) = akuma_exec::process::current_process() {
        if proc.box_id > 0 {
            let namespaces = MOUNT_NAMESPACES.lock();
            if let Some(ns) = namespaces.get(&proc.box_id) {
                let parent = parent_path.trim_end_matches('/');
                let parent = if parent.is_empty() { "/" } else { parent };
                for m in &ns.mounts {
                    if m.path == "/" || m.path == parent {
                        continue;
                    }
                    if parent == "/" {
                        if m.path.starts_with('/') && !m.path[1..].contains('/') {
                            entries.push(DirEntry {
                                name: String::from(&m.path[1..]),
                                is_dir: true,
                                size: 0,
                            });
                        }
                    } else {
                        let prefix = format!("{}/", parent);
                        if m.path.starts_with(&prefix) {
                            let rest = &m.path[prefix.len()..];
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
            }
        }
    }

    // Also include global mount table entries
    let table = MOUNT_TABLE.lock();
    if let Some(t) = table.as_ref() {
        for entry in t.child_mount_points(parent_path) {
            if !entries.iter().any(|e| e.name == entry.name) {
                entries.push(entry);
            }
        }
    }

    entries
}
