//! Virtual Filesystem (VFS) Layer
//!
//! Provides an abstraction layer for filesystem operations, allowing multiple
//! filesystem backends (ext2, in-memory, etc.) to be mounted and accessed
//! through a unified API.

pub mod ext2;
pub mod memory;
pub mod proc;

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use spinning_top::Spinlock;

// ============================================================================
// Error Types
// ============================================================================

/// Filesystem error type
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsError {
    /// Block device not initialized
    BlockDeviceNotInitialized,
    /// Filesystem not initialized/mounted
    NotInitialized,
    /// File or directory not found
    NotFound,
    /// Permission denied
    PermissionDenied,
    /// File or directory already exists
    AlreadyExists,
    /// Path is not a directory
    NotADirectory,
    /// Path is not a file
    NotAFile,
    /// Directory is not empty
    DirectoryNotEmpty,
    /// I/O error
    IoError,
    /// Invalid path
    InvalidPath,
    /// No space left on device
    NoSpace,
    /// Too many open files
    TooManyOpenFiles,
    /// Invalid file handle
    InvalidHandle,
    /// Filesystem is corrupt
    Corrupt,
    /// End of file reached
    EndOfFile,
    /// No filesystem found on device
    NoFilesystem,
    /// Internal error
    Internal,
    /// Read-only filesystem
    ReadOnly,
    /// Not supported by this filesystem
    NotSupported,
}

impl core::fmt::Display for FsError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            FsError::BlockDeviceNotInitialized => write!(f, "Block device not initialized"),
            FsError::NotInitialized => write!(f, "Filesystem not initialized"),
            FsError::NotFound => write!(f, "Not found"),
            FsError::PermissionDenied => write!(f, "Permission denied"),
            FsError::AlreadyExists => write!(f, "Already exists"),
            FsError::NotADirectory => write!(f, "Not a directory"),
            FsError::NotAFile => write!(f, "Not a file"),
            FsError::DirectoryNotEmpty => write!(f, "Directory not empty"),
            FsError::IoError => write!(f, "I/O error"),
            FsError::InvalidPath => write!(f, "Invalid path"),
            FsError::NoSpace => write!(f, "No space left"),
            FsError::TooManyOpenFiles => write!(f, "Too many open files"),
            FsError::InvalidHandle => write!(f, "Invalid file handle"),
            FsError::Corrupt => write!(f, "Filesystem corrupt"),
            FsError::EndOfFile => write!(f, "End of file"),
            FsError::NoFilesystem => write!(f, "No filesystem found"),
            FsError::Internal => write!(f, "Internal error"),
            FsError::ReadOnly => write!(f, "Read-only filesystem"),
            FsError::NotSupported => write!(f, "Operation not supported"),
        }
    }
}

// ============================================================================
// Directory Entry
// ============================================================================

/// Directory entry information
#[derive(Debug, Clone)]
pub struct DirEntry {
    /// Name of the file or directory
    pub name: String,
    /// Whether this is a directory
    pub is_dir: bool,
    /// Size in bytes (0 for directories)
    pub size: u64,
}

// ============================================================================
// Metadata
// ============================================================================

/// File or directory metadata
#[derive(Debug, Clone)]
pub struct Metadata {
    /// Whether this is a directory
    pub is_dir: bool,
    /// Size in bytes
    pub size: u64,
    /// Inode number (unique within filesystem)
    pub inode: u64,
    /// Creation time (Unix timestamp, if available)
    pub created: Option<u64>,
    /// Last modification time (Unix timestamp, if available)
    pub modified: Option<u64>,
    /// Last access time (Unix timestamp, if available)
    pub accessed: Option<u64>,
}

// ============================================================================
// Filesystem Statistics
// ============================================================================

/// Filesystem statistics
#[derive(Debug, Clone)]
pub struct FsStats {
    /// Block/cluster size in bytes
    pub block_size: u32,
    /// Total number of blocks
    pub total_blocks: u64,
    /// Number of free blocks
    pub free_blocks: u64,
}

impl FsStats {
    /// Get total size in bytes
    pub fn total_bytes(&self) -> u64 {
        self.total_blocks * self.block_size as u64
    }

    /// Get free space in bytes
    pub fn free_bytes(&self) -> u64 {
        self.free_blocks * self.block_size as u64
    }

    /// Get used space in bytes
    pub fn used_bytes(&self) -> u64 {
        self.total_bytes() - self.free_bytes()
    }
}

// ============================================================================
// Filesystem Trait
// ============================================================================

/// Trait for filesystem implementations
///
/// This trait is object-safe to allow for dynamic dispatch between different
/// filesystem backends (ext2, in-memory, etc.)
pub trait Filesystem: Send + Sync {
    /// Get the filesystem name/type (e.g., "ext2", "memfs")
    fn name(&self) -> &str;

    /// List directory contents
    fn read_dir(&self, path: &str) -> Result<Vec<DirEntry>, FsError>;

    /// Read entire file contents
    fn read_file(&self, path: &str) -> Result<Vec<u8>, FsError>;

    /// Write data to a file (creates or truncates)
    fn write_file(&self, path: &str, data: &[u8]) -> Result<(), FsError>;

    /// Append data to a file
    fn append_file(&self, path: &str, data: &[u8]) -> Result<(), FsError>;

    /// Read data from a specific offset within a file.
    /// Returns the number of bytes actually read (may be less than buf.len()
    /// at end-of-file, or 0 if offset is past the end).
    fn read_at(&self, path: &str, offset: usize, buf: &mut [u8]) -> Result<usize, FsError> {
        // Default: fall back to read-entire-file-and-slice (slow but correct)
        let data = self.read_file(path)?;
        if offset >= data.len() {
            return Ok(0);
        }
        let n = buf.len().min(data.len() - offset);
        buf[..n].copy_from_slice(&data[offset..offset + n]);
        Ok(n)
    }

    /// Write data at a specific offset within a file.
    /// Extends the file if offset + data.len() > current size.
    /// Returns the number of bytes written.
    fn write_at(&self, path: &str, offset: usize, data: &[u8]) -> Result<usize, FsError> {
        // Default: fall back to read-modify-write (slow but correct)
        let mut contents = self.read_file(path).unwrap_or_default();
        let end = offset + data.len();
        if end > contents.len() {
            contents.resize(end, 0);
        }
        contents[offset..end].copy_from_slice(data);
        self.write_file(path, &contents)?;
        Ok(data.len())
    }

    /// Create a directory
    fn create_dir(&self, path: &str) -> Result<(), FsError>;

    /// Remove a file
    fn remove_file(&self, path: &str) -> Result<(), FsError>;

    /// Remove an empty directory
    fn remove_dir(&self, path: &str) -> Result<(), FsError>;

    /// Check if a path exists
    fn exists(&self, path: &str) -> bool;

    /// Get metadata for a path
    fn metadata(&self, path: &str) -> Result<Metadata, FsError>;

    /// Rename/move a file or directory
    fn rename(&self, _old_path: &str, _new_path: &str) -> Result<(), FsError> {
        Err(FsError::NotSupported)
    }

    /// Get filesystem statistics
    fn stats(&self) -> Result<FsStats, FsError>;

    /// Sync/flush any cached data to disk
    fn sync(&self) -> Result<(), FsError> {
        Ok(()) // Default: no-op for filesystems that don't cache
    }
}

// ============================================================================
// Mount Table
// ============================================================================

/// Maximum number of mount points
const MAX_MOUNTS: usize = 8;

/// A mount point entry
struct MountEntry {
    /// Mount path (e.g., "/", "/tmp")
    path: String,
    /// The mounted filesystem
    fs: Box<dyn Filesystem>,
}

/// Global mount table
static MOUNT_TABLE: Spinlock<Option<MountTable>> = Spinlock::new(None);

/// Mount table managing all mounted filesystems
struct MountTable {
    mounts: Vec<MountEntry>,
}

impl MountTable {
    fn new() -> Self {
        Self {
            mounts: Vec::with_capacity(MAX_MOUNTS),
        }
    }

    /// Mount a filesystem at the given path
    fn mount(&mut self, path: &str, fs: Box<dyn Filesystem>) -> Result<(), FsError> {
        if self.mounts.len() >= MAX_MOUNTS {
            return Err(FsError::NoSpace);
        }

        // Check if already mounted
        if self.mounts.iter().any(|m| m.path == path) {
            return Err(FsError::AlreadyExists);
        }

        self.mounts.push(MountEntry {
            path: String::from(path),
            fs,
        });

        // Sort by path length (longest first) for proper matching
        self.mounts.sort_by(|a, b| b.path.len().cmp(&a.path.len()));

        Ok(())
    }

    /// Unmount a filesystem at the given path
    fn unmount(&mut self, path: &str) -> Result<(), FsError> {
        let idx = self
            .mounts
            .iter()
            .position(|m| m.path == path)
            .ok_or(FsError::NotFound)?;

        self.mounts.remove(idx);
        Ok(())
    }

    /// Find the filesystem and relative path for a given absolute path
    fn resolve<'a>(&'a self, path: &'a str) -> Option<(&'a dyn Filesystem, &'a str)> {
        // Normalize the path (add leading / if missing, remove trailing /)
        let normalized = normalize_path(path);

        for mount in &self.mounts {
            // Special case: root mount "/"
            if mount.path == "/" {
                // Root mount matches everything
                return Some((mount.fs.as_ref(), normalized));
            }

            // Exact match
            if normalized == mount.path {
                return Some((mount.fs.as_ref(), "/"));
            }

            // Prefix match (e.g., path="/tmp/foo" matches mount="/tmp")
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
}

// ============================================================================
// Path Utilities
// ============================================================================

/// Non-allocating normalization (trims trailing slashes)
fn normalize_path(path: &str) -> &str {
    let path = path.trim_end_matches('/');
    if path.is_empty() { "/" } else { path }
}

/// Robust normalization (resolves . and ..)
pub fn canonicalize_path(path: &str) -> String {
    let mut components: Vec<&str> = Vec::new();

    for component in path.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                components.pop();
            }
            c => {
                components.push(c);
            }
        }
    }

    if components.is_empty() {
        String::from("/")
    } else {
        let mut result = String::new();
        for c in components {
            result.push('/');
            result.push_str(c);
        }
        result
    }
}

/// Resolve a path relative to a base directory
pub fn resolve_path(base_cwd: &str, path: &str) -> String {
    if path.starts_with('/') {
        // Absolute path
        canonicalize_path(path)
    } else {
        // Relative path
        let full_path = if base_cwd == "/" {
            alloc::format!("/{}", path)
        } else {
            alloc::format!("{}/{}", base_cwd, path)
        };
        canonicalize_path(&full_path)
    }
}

/// Normalize path with allocation (adds leading / if missing)
fn normalize_path_owned(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        String::from("/")
    } else if !trimmed.starts_with('/') {
        alloc::format!("/{}", trimmed)
    } else {
        String::from(trimmed)
    }
}

/// Split a path into (parent_path, filename)
pub fn split_path(path: &str) -> (&str, &str) {
    let path = path.trim_start_matches('/').trim_end_matches('/');
    match path.rfind('/') {
        Some(idx) => (&path[..idx], &path[idx + 1..]),
        None => ("", path),
    }
}

/// Split path into components
pub fn path_components(path: &str) -> Vec<&str> {
    path.trim_matches('/')
        .split('/')
        .filter(|s| !s.is_empty())
        .collect()
}

// ============================================================================
// Public API - Mount Operations
// ============================================================================

/// Initialize the VFS subsystem
pub fn init() {
    let mut table = MOUNT_TABLE.lock();
    if table.is_none() {
        *table = Some(MountTable::new());
    }
}

/// Check if VFS is initialized
pub fn is_initialized() -> bool {
    MOUNT_TABLE.lock().is_some()
}

/// Mount a filesystem at the given path
pub fn mount(path: &str, fs: Box<dyn Filesystem>) -> Result<(), FsError> {
    let mut table = MOUNT_TABLE.lock();
    let table = table.as_mut().ok_or(FsError::NotInitialized)?;
    table.mount(path, fs)
}

/// Unmount a filesystem at the given path
pub fn unmount(path: &str) -> Result<(), FsError> {
    let mut table = MOUNT_TABLE.lock();
    let table = table.as_mut().ok_or(FsError::NotInitialized)?;
    table.unmount(path)
}

// ============================================================================
// Public API - File Operations (delegates to mounted filesystems)
// ============================================================================

/// Helper to get filesystem for a path
fn with_fs<F, R>(path: &str, f: F) -> Result<R, FsError>
where
    F: FnOnce(&dyn Filesystem, &str) -> Result<R, FsError>,
{
    let mut normalized = if let Some(proc) = crate::process::current_process() {
        // 1. Resolve relative path against process CWD
        let absolute = resolve_path(&proc.cwd, path);
        
        // 2. VFS SCOPING: Prepend process root_dir if not /
        if proc.root_dir != "/" {
            // Join root_dir and absolute path
            // e.g. root_dir="/box1", absolute="/etc" -> "/box1/etc"
            if proc.root_dir.ends_with('/') {
                alloc::format!("{}{}", proc.root_dir, &absolute[1..])
            } else {
                alloc::format!("{}{}", proc.root_dir, absolute)
            }
        } else {
            absolute
        }
    } else {
        // Fallback for kernel context (no process)
        normalize_path_owned(path)
    };

    let table = MOUNT_TABLE.lock();
    let table = table.as_ref().ok_or(FsError::NotInitialized)?;

    let (fs, relative_path) = table.resolve(&normalized).ok_or(FsError::NotFound)?;
    f(fs, relative_path)
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

/// Rename/move a file or directory
pub fn rename(old_path: &str, new_path: &str) -> Result<(), FsError> {
    // Both paths must be on the same filesystem for an atomic rename
    let mut old_full = if let Some(proc) = crate::process::current_process() {
        let abs = resolve_path(&proc.cwd, old_path);
        if proc.root_dir != "/" {
            if proc.root_dir.ends_with('/') {
                alloc::format!("{}{}", proc.root_dir, &abs[1..])
            } else {
                alloc::format!("{}{}", proc.root_dir, abs)
            }
        } else {
            abs
        }
    } else {
        normalize_path_owned(old_path)
    };

    let mut new_full = if let Some(proc) = crate::process::current_process() {
        let abs = resolve_path(&proc.cwd, new_path);
        if proc.root_dir != "/" {
            if proc.root_dir.ends_with('/') {
                alloc::format!("{}{}", proc.root_dir, &abs[1..])
            } else {
                alloc::format!("{}{}", proc.root_dir, abs)
            }
        } else {
            abs
        }
    } else {
        normalize_path_owned(new_path)
    };

    let table = MOUNT_TABLE.lock();
    let table = table.as_ref().ok_or(FsError::NotInitialized)?;

    let (old_fs, old_rel) = table.resolve(&old_full).ok_or(FsError::NotFound)?;
    let (new_fs, new_rel) = table.resolve(&new_full).ok_or(FsError::NotFound)?;

    // Check if they are the same filesystem instance
    if old_fs.name() != new_fs.name() {
        return Err(FsError::NotSupported); // Cross-FS rename not supported
    }

    old_fs.rename(old_rel, new_rel)
}

/// Get filesystem statistics for a path
pub fn stats(path: &str) -> Result<FsStats, FsError> {
    with_fs(path, |fs, _| fs.stats())
}

/// Sync all mounted filesystems
pub fn sync_all() -> Result<(), FsError> {
    let table = MOUNT_TABLE.lock();
    let table = table.as_ref().ok_or(FsError::NotInitialized)?;

    for mount in &table.mounts {
        mount.fs.sync()?;
    }
    Ok(())
}

// ============================================================================
// Mount Information
// ============================================================================

/// Information about a mounted filesystem
#[derive(Debug, Clone)]
pub struct MountInfo {
    /// Mount path (e.g., "/", "/proc")
    pub path: String,
    /// Filesystem type name (e.g., "ext2", "procfs")
    pub fs_type: String,
}

/// List all mounted filesystems
pub fn list_mounts() -> Result<Vec<MountInfo>, FsError> {
    let table = MOUNT_TABLE.lock();
    let table = table.as_ref().ok_or(FsError::NotInitialized)?;

    let mounts: Vec<MountInfo> = table
        .mounts
        .iter()
        .map(|m| MountInfo {
            path: m.path.clone(),
            fs_type: String::from(m.fs.name()),
        })
        .collect();

    Ok(mounts)
}

// ============================================================================
// Symlink Support
// ============================================================================

static SYMLINKS: Spinlock<Option<BTreeMap<String, String>>> = Spinlock::new(None);

pub fn create_symlink(link_path: &str, target: &str) -> Result<(), FsError> {
    let link = canonicalize_path(link_path);
    let mut table = SYMLINKS.lock();
    if table.is_none() { *table = Some(BTreeMap::new()); }
    table.as_mut().unwrap().insert(link, String::from(target));
    Ok(())
}

pub fn read_symlink(path: &str) -> Option<String> {
    let canonical = canonicalize_path(path);
    let table = SYMLINKS.lock();
    table.as_ref().and_then(|t| t.get(&canonical).cloned())
}

pub fn is_symlink(path: &str) -> bool {
    let canonical = canonicalize_path(path);
    let table = SYMLINKS.lock();
    table.as_ref().map_or(false, |t| t.contains_key(&canonical))
}

/// Resolve a path, following symlinks (up to 8 levels to prevent loops)
pub fn resolve_symlinks(path: &str) -> String {
    let mut resolved = canonicalize_path(path);
    for _ in 0..8 {
        let target = {
            let table = SYMLINKS.lock();
            table.as_ref().and_then(|t| t.get(&resolved).cloned())
        };
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
                // Built-in fallback: /bin/sh -> /bin/dash if dash exists
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

/// Get mount points that are direct children of a directory
/// Used by list_dir to include virtual mount points in directory listings
fn get_child_mount_points(parent_path: &str) -> Vec<DirEntry> {
    let table = MOUNT_TABLE.lock();
    let table = match table.as_ref() {
        Some(t) => t,
        None => return Vec::new(),
    };

    let parent = normalize_path(parent_path);
    let mut entries = Vec::new();

    for mount in &table.mounts {
        // Skip the root mount and the parent itself
        if mount.path == "/" || mount.path == parent {
            continue;
        }

        // Check if this mount is a direct child of parent
        // For parent="/", child mount "/proc" -> name is "proc"
        // For parent="/foo", child mount "/foo/bar" -> name is "bar"
        let mount_path = mount.path.as_str();

        if parent == "/" {
            // Direct children of root: /proc, /tmp, etc.
            // Check if mount path has exactly one component after root
            if mount_path.starts_with('/') && !mount_path[1..].contains('/') {
                let name = &mount_path[1..]; // Remove leading /
                entries.push(DirEntry {
                    name: String::from(name),
                    is_dir: true,
                    size: 0,
                });
            }
        } else {
            // Direct children of non-root: /foo -> /foo/bar
            let prefix = alloc::format!("{}/", parent);
            if mount_path.starts_with(&prefix) {
                let rest = &mount_path[prefix.len()..];
                // Only direct children (no more slashes)
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
