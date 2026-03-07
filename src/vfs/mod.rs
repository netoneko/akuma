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

pub use akuma_isolation::Namespace;

// ============================================================================
// Mount Table (kernel-side global)
// ============================================================================

static MOUNT_TABLE: Spinlock<Option<akuma_vfs::MountTable>> = Spinlock::new(None);

// ============================================================================
// Per-box Namespaces
// ============================================================================

static BOX_NAMESPACES: Spinlock<BTreeMap<u64, Arc<Namespace>>> = Spinlock::new(BTreeMap::new());

/// Per-thread namespace override for ELF loading during spawn.
/// When set, `with_fs` uses this namespace instead of the calling process's.
static SPAWN_NS_OVERRIDE: Spinlock<BTreeMap<usize, Arc<Namespace>>> = Spinlock::new(BTreeMap::new());

/// Set a namespace override for the current thread. All `with_fs` calls
/// on this thread will resolve through the given namespace until cleared.
pub fn set_spawn_namespace(ns: Arc<Namespace>) {
    let tid = akuma_exec::threading::current_thread_id();
    SPAWN_NS_OVERRIDE.lock().insert(tid, ns);
}

/// Clear the namespace override for the current thread.
pub fn clear_spawn_namespace() {
    let tid = akuma_exec::threading::current_thread_id();
    SPAWN_NS_OVERRIDE.lock().remove(&tid);
}

/// Create a new namespace for a box and return a shared reference.
/// If `root_dir` is non-"/" and the global root filesystem is available,
/// a `SubdirFs` scoped to `root_dir` is mounted at `/` in the new namespace.
pub fn create_box_namespace(box_id: u64, root_dir: &str) -> Arc<Namespace> {
    let ns = Arc::new(Namespace::new(box_id));
    if root_dir != "/" {
        if let Some(root_fs) = get_root_fs() {
            let subdir = Arc::new(akuma_isolation::subdir_fs::SubdirFs::new(root_fs, root_dir));
            let _ = ns.mount.lock().mount("/", subdir);
        }
    }
    BOX_NAMESPACES.lock().insert(box_id, ns.clone());
    ns
}

/// Remove a box's namespace from the registry.
pub fn remove_box_namespace(box_id: u64) {
    BOX_NAMESPACES.lock().remove(&box_id);
}

/// Look up a box's namespace.
pub fn get_box_namespace(box_id: u64) -> Option<Arc<Namespace>> {
    BOX_NAMESPACES.lock().get(&box_id).cloned()
}

/// Mount a filesystem into a specific box's namespace.
pub fn mount_in_namespace(box_id: u64, path: &str, fs: Arc<dyn Filesystem>) -> Result<(), FsError> {
    let namespaces = BOX_NAMESPACES.lock();
    let ns = namespaces.get(&box_id).ok_or(FsError::NotFound)?;
    ns.mount.lock().mount(path, fs)
}

/// Unmount a path from a specific box's namespace.
#[allow(dead_code)]
pub fn unmount_in_namespace(box_id: u64, path: &str) -> Result<(), FsError> {
    let namespaces = BOX_NAMESPACES.lock();
    let ns = namespaces.get(&box_id).ok_or(FsError::NotFound)?;
    ns.mount.lock().unmount(path)
}

/// List mounts in a specific box's namespace.
#[allow(dead_code)]
pub fn list_namespace_mounts(box_id: u64) -> Vec<MountInfo> {
    let namespaces = BOX_NAMESPACES.lock();
    namespaces.get(&box_id).map_or_else(Vec::new, |ns| ns.mount.lock().list_mounts())
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

/// Get the `Arc<dyn Filesystem>` for a global mount point (e.g., "/" for ext2).
pub fn get_root_fs() -> Option<Arc<dyn Filesystem>> {
    let table = MOUNT_TABLE.lock();
    table.as_ref().and_then(|t| t.get_fs("/"))
}

// ============================================================================
// Public API - File Operations (delegates to mounted filesystems)
// ============================================================================

/// Helper to get filesystem for a path.
///
/// Resolution order:
/// 1. Check per-thread spawn namespace override (used during ELF loading)
/// 2. Resolve relative path against CWD to get an absolute path
/// 3. Try the process's mount namespace
/// 4. Fall back to the global mount table
fn with_fs<F, R>(path: &str, f: F) -> Result<R, FsError>
where
    F: FnOnce(&dyn Filesystem, &str) -> Result<R, FsError>,
{
    // Check spawn namespace override (set during container ELF loading)
    {
        let tid = akuma_exec::threading::current_thread_id();
        let overrides = SPAWN_NS_OVERRIDE.lock();
        if let Some(ns) = overrides.get(&tid) {
            let cwd = akuma_exec::process::current_process()
                .map_or_else(|| String::from("/"), |p| p.cwd.clone());
            let absolute = resolve_path(&cwd, path);
            let ns_mount = ns.mount.lock();
            if let Some((fs, rel)) = ns_mount.resolve(&absolute) {
                return f(fs, rel);
            }
        }
    }

    if let Some(proc) = akuma_exec::process::current_process() {
        let absolute = resolve_path(&proc.cwd, path);

        {
            let ns_mount = proc.namespace.mount.lock();
            if let Some((fs, rel)) = ns_mount.resolve(&absolute) {
                return f(fs, rel);
            }
        }

        let table = MOUNT_TABLE.lock();
        let table = table.as_ref().ok_or(FsError::NotInitialized)?;
        let (fs, relative_path) = table.resolve(&absolute).ok_or(FsError::NotFound)?;
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

/// Resolve a path to its absolute form through the process's CWD.
fn resolve_absolute(path: &str) -> String {
    if let Some(proc) = akuma_exec::process::current_process() {
        resolve_path(&proc.cwd, path)
    } else {
        normalize_path_owned(path)
    }
}

/// Rename/move a file or directory
pub fn rename(old_path: &str, new_path: &str) -> Result<(), FsError> {
    let old_abs = resolve_absolute(old_path);
    let new_abs = resolve_absolute(new_path);

    // Try process namespace first
    if let Some(proc) = akuma_exec::process::current_process() {
        let ns_mount = proc.namespace.mount.lock();
        if let (Some((old_fs, old_rel)), Some((_, new_rel))) =
            (ns_mount.resolve(&old_abs), ns_mount.resolve(&new_abs))
        {
            return old_fs.rename(old_rel, new_rel);
        }
    }

    let table = MOUNT_TABLE.lock();
    let table = table.as_ref().ok_or(FsError::NotInitialized)?;

    let (old_fs, old_rel) = table.resolve(&old_abs).ok_or(FsError::NotFound)?;
    let (new_fs, new_rel) = table.resolve(&new_abs).ok_or(FsError::NotFound)?;

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

    if let Some(proc) = akuma_exec::process::current_process() {
        let ns_mount = proc.namespace.mount.lock();
        for entry in ns_mount.child_mount_points(parent_path) {
            entries.push(entry);
        }
    }

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
