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
use spinning_top::Spinlock;

// Re-export everything from the crate so existing `use crate::vfs::*` keeps working.
pub use akuma_vfs::{
    DevNode, DevProbe, DirEntry, Filesystem, FsError, FsStats, Metadata, MountInfo, ResolvedMount,
    MS_RDONLY, ST_RDONLY,
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
///
/// Idempotent: if the box already has a namespace, the existing one is returned
/// unchanged. herd calls `register_box` (→ this) twice — once with a placeholder
/// pid, then with the real pid — and recreating the namespace the second time
/// would drop any mounts added in between (e.g. a `/proc` mounted for the box's
/// sshd). Keeping the first namespace preserves those mounts.
#[cfg(feature = "sc-containers")]
pub fn create_box_namespace(box_id: u64, root_dir: &str) -> Arc<Namespace> {
    if let Some(existing) = BOX_NAMESPACES.lock().get(&box_id).cloned() {
        return existing;
    }
    let ns = Arc::new(Namespace::new(box_id));
    if root_dir != "/"
        && let Some(root_fs) = get_root_fs() {
            let subdir = Arc::new(akuma_isolation::subdir_fs::SubdirFs::new(root_fs, root_dir));
            let _ = ns.mount.lock().mount("/", subdir);
        }
    BOX_NAMESPACES.lock().insert(box_id, ns.clone());
    ns
}

/// Remove a box's namespace from the registry.
#[cfg(feature = "sc-containers")]
pub fn remove_box_namespace(box_id: u64) {
    BOX_NAMESPACES.lock().remove(&box_id);
}

/// Look up a box's namespace.
pub fn get_box_namespace(box_id: u64) -> Option<Arc<Namespace>> {
    BOX_NAMESPACES.lock().get(&box_id).cloned()
}

/// Mount a filesystem into a specific box's namespace.
#[cfg(feature = "sc-containers")]
pub fn mount_in_namespace(box_id: u64, path: &str, fs: Arc<dyn Filesystem>) -> Result<(), FsError> {
    let namespaces = BOX_NAMESPACES.lock();
    let ns = namespaces.get(&box_id).ok_or(FsError::NotFound)?;
    ns.mount.lock().mount(path, fs)
}

/// Turn a box's pristine `SubdirFs` root into `fs` (an overlay of image layers).
///
/// Only `MOUNT_IN_NS` with fstype `overlay` reaches this. A box is born with a
/// `SubdirFs` jail at `/`, and making its root an overlay is a swap, not a
/// stack — the ordinary mount path would reject the duplicate.
///
/// `replace_pristine_root` enforces that the root really is that untouched jail,
/// so this is a one-shot at box-creation time and never a way to redirect a root
/// that has already been established.
#[cfg(feature = "sc-containers")]
pub fn replace_box_root(box_id: u64, fs: Arc<dyn Filesystem>) -> Result<(), FsError> {
    let namespaces = BOX_NAMESPACES.lock();
    let ns = namespaces.get(&box_id).ok_or(FsError::NotFound)?;
    ns.mount.lock().replace_pristine_root("subdirfs", fs)
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

/// Resolve `path` with no process CWD to resolve against — i.e. as if the CWD
/// were `/`.
///
/// This is the fallback arm of every "resolve a path" site below, taken when
/// there is no current process (early boot, kernel threads). It used to be a
/// local `normalize_path_owned` that trimmed trailing slashes and prepended a
/// leading one but **did not resolve `.` / `..`** — so `..` was resolved when a
/// process existed and left in the path when one did not, and both arms then fed
/// the same `MountTable::resolve_arc`. Two normalisation semantics behind one
/// call, differing by whether a process happened to be current.
///
/// `resolve_path("/", path)` is the same function the with-process arm uses,
/// with the CWD it actually has. Identical on `""`, `"/"`, `"/foo/"` and `"foo"`;
/// on `"foo/../bar"` it yields `/bar` where the old code yielded
/// `/foo/../bar`, which is the correction. See
/// `docs/archive/TRIM_FAT_EMBARASSING_DUPLICATIONS.md` §4.
#[inline]
fn resolve_without_cwd(path: &str) -> String {
    resolve_path("/", path)
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

/// Sync every filesystem visible anywhere: the global mount table and every
/// box namespace (the same fs may appear in both; `sync` is idempotent, and a
/// duplicate flush of an already-clean cache is free). Called from
/// `sys_reboot` before PSCI reset/poweroff — with the ext2 write-back cache,
/// dirty data otherwise dies with the machine. Collects the `Arc<dyn
/// Filesystem>` set under the table/namespace locks, then syncs lock-free:
/// `sync` does real block I/O and must not run under a mount-table lock.
///
/// Gated on `sc-reboot` because that syscall is its only caller: `extreme-size`
/// builds `--no-default-features` without it, and an ungated definition is
/// dead code there (`-D dead-code` fails the build).
#[cfg(feature = "sc-reboot")]
pub fn sync_all_filesystems() -> Result<(), FsError> {
    let mut seen: Vec<Arc<dyn Filesystem>> = Vec::new();
    {
        let table = MOUNT_TABLE.lock();
        if let Some(t) = table.as_ref() {
            seen.extend(t.filesystems());
        }
    }
    {
        let namespaces = BOX_NAMESPACES.lock();
        for ns in namespaces.values() {
            let mounts = ns.mount.lock();
            seen.extend(mounts.filesystems());
        }
    }
    for fs in &seen {
        fs.sync()?;
    }
    Ok(())
}

/// [`mount`] recording the mount's `source` (`/dev/vda`, `proc`, …) and
/// `MS_*` flags. Only `MS_RDONLY` is stored; the kernel's write chokepoints
/// enforce it as `FsError::ReadOnly`.
pub fn mount_with(
    path: &str,
    source: Option<&str>,
    flags: u64,
    fs: Arc<dyn Filesystem>,
) -> Result<(), FsError> {
    let mut table = MOUNT_TABLE.lock();
    let table = table.as_mut().ok_or(FsError::NotInitialized)?;
    table.mount_with(path, source, flags, fs)
}

/// Replace the flags of an existing global mount (`MS_REMOUNT` leg of
/// `mount(2)`; only the `MS_RDONLY` bit is kept).
///
/// Only consumed by `syscall::container::sys_mount`, so it follows the same gate.
#[cfg(feature = "sc-containers")]
pub fn remount(path: &str, flags: u64) -> Result<(), FsError> {
    let mut table = MOUNT_TABLE.lock();
    let table = table.as_mut().ok_or(FsError::NotInitialized)?;
    table.remount(path, flags)
}

/// Unmount a path from the global table. Refuses `/` — the boot root stays
/// for the boot's lifetime (`umount2` maps this to `EBUSY`).
///
/// Only consumed by `syscall::container::sys_umount2`, so it follows the same gate.
#[cfg(feature = "sc-containers")]
pub fn unmount(path: &str) -> Result<(), FsError> {
    let normalized = canonicalize_path(path);
    if normalized == "/" {
        return Err(FsError::PermissionDenied);
    }
    let mut table = MOUNT_TABLE.lock();
    let table = table.as_mut().ok_or(FsError::NotInitialized)?;
    table.unmount(&normalized)
}

/// Get the `Arc<dyn Filesystem>` for a global mount point (e.g., "/" for ext2).
/// Currently only consumed by `create_box_namespace`, so it follows the same gate.
#[cfg(feature = "sc-containers")]
pub fn get_root_fs() -> Option<Arc<dyn Filesystem>> {
    let table = MOUNT_TABLE.lock();
    table.as_ref().and_then(|t| t.get_fs("/"))
}

// ============================================================================
// Public API - File Operations (delegates to mounted filesystems)
// ============================================================================

/// Resolve `path` to `(filesystem, relative_path, mount_flags)` following the
/// same order [`with_fs`] documents: spawn override → process namespace →
/// global table. `None` when nothing resolves.
///
/// Locks are held for the resolution only (the `Arc` outlives them), so no I/O
/// or allocation happens under a mount-table lock.
fn resolve_mount(path: &str) -> Option<ResolvedMount> {
    // Check spawn namespace override (set during container ELF loading).
    {
        let tid = akuma_exec::threading::current_thread_id();
        let overrides = SPAWN_NS_OVERRIDE.lock();
        if let Some(ns) = overrides.get(&tid) {
            let cwd = akuma_exec::process::current_process_shared()
                .map_or_else(|| String::from("/"), |p| p.cwd.clone());
            let absolute = resolve_path(&cwd, path);
            let ns_mount = ns.mount.lock();
            return ns_mount.resolve_arc_full(&absolute);
        }
    }

    if let Some(proc) = akuma_exec::process::current_process_shared() {
        let absolute = resolve_path(&proc.cwd, path);

        // Try process namespace first (lock released before I/O).
        if let Some(resolved) = proc.namespace.mount.lock().resolve_arc_full(&absolute) {
            return Some(resolved);
        }

        // Fall back to global mount table (lock released before I/O).
        let table = MOUNT_TABLE.lock();
        return table.as_ref()?.resolve_arc_full(&absolute);
    }

    let normalized = resolve_without_cwd(path);
    let table = MOUNT_TABLE.lock();
    table.as_ref()?.resolve_arc_full(&normalized)
}

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
    let r = resolve_mount(path).ok_or(FsError::NotFound)?;
    f(r.fs.as_ref(), &r.rel)
}

/// [`with_fs`] for mutating operations: the resolved mount's `MS_RDONLY` flag
/// is consulted first, so every write chokepoint below enforces read-only
/// mounts uniformly (`FsError::ReadOnly` → `EROFS` at the syscall boundary).
fn with_fs_write<F, R>(path: &str, f: F) -> Result<R, FsError>
where
    F: FnOnce(&dyn Filesystem, &str) -> Result<R, FsError>,
{
    let r = resolve_mount(path).ok_or(FsError::NotFound)?;
    if r.flags & MS_RDONLY != 0 {
        return Err(FsError::ReadOnly);
    }
    f(r.fs.as_ref(), &r.rel)
}

/// List directory contents
/// 
/// This includes both entries from the underlying filesystem and any
/// mount points that appear as direct children of the listed directory.
pub fn list_dir(path: &str) -> Result<Vec<DirEntry>, FsError> {
    // Synthetic /dev nodes, computed before the on-disk read so a `/dev` that
    // isn't on the image at all still lists. Empty for every other path, and
    // empty inside a box.
    let dev_entries = dev_entries(path);

    let mut entries = match with_fs(path, |fs, rel| fs.read_dir(rel)) {
        Ok(entries) => entries,
        // No real `/dev` directory, but the table has nodes: list those rather
        // than reporting a `/dev` that `stat` and `open` both say exists.
        Err(_) if !dev_entries.is_empty() => Vec::new(),
        Err(e) => return Err(e),
    };

    // A real on-disk node of the same name shadows the synthetic one, matching
    // how a mount point shadows an existing directory below.
    for entry in dev_entries {
        if !entries.iter().any(|e| e.name == entry.name) {
            entries.push(entry);
        }
    }

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
    if let Some(rows) = mtab_rows(path) {
        return Ok(rows);
    }
    with_fs(path, |fs, rel| fs.read_file(rel))
}


/// Write data to a file (creates or truncates)
pub fn write_file(path: &str, data: &[u8]) -> Result<(), FsError> {
    if is_mtab(path) {
        return Err(FsError::NotSupported); // virtual, kernel-owned
    }
    let r = with_fs_write(path, |fs, rel| fs.write_file(rel, data));
    invalidate_file_pages(path);
    r
}


/// Drop any shared read-only file pages cached for `path`.
///
/// Every mutating entry point below calls this, because a stale shared page is a
/// silent wrong-bytes bug rather than a crash: `rustc` mmaps `.rlib`/`.rmeta`
/// files that `cargo` later rewrites, and ext2 reuses inode numbers.
///
/// Invalidation happens *after* the mutation so the window a concurrent fault
/// could re-cache into is the write itself (already an application-level race),
/// not the interval between invalidation and the new bytes landing.
///
/// The `len() == 0` guard keeps the extra path walk off the write path entirely
/// on kernels where nothing has been mmap'd yet; once populated, `resolve_inode`
/// costs one walk, against a write that already does its own.
fn invalidate_file_pages(path: &str) {
    if !crate::config::SHARED_FILE_PAGES_ENABLED || crate::file_page_cache::len() == 0 {
        return;
    }
    if let Ok(inode) = resolve_inode(path) {
        crate::file_page_cache::invalidate_inode(inode);
    }
}

/// Read data from a specific offset within a file
pub fn read_at(path: &str, offset: usize, buf: &mut [u8]) -> Result<usize, FsError> {
    if let Some(rows) = mtab_rows(path) {
        if offset >= rows.len() {
            return Ok(0);
        }
        let n = buf.len().min(rows.len() - offset);
        buf[..n].copy_from_slice(&rows[offset..offset + n]);
        return Ok(n);
    }
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

/// Resolve `path` to the `(mount id, inode)` pair that actually names a file.
///
/// An inode number on its own does not identify a file: a second `mount(2)`
/// (`MOUNT_IN_NS`) brings up another filesystem issuing numbers from the same
/// range, so anything that stores an inode across time — the file page cache, a
/// lazy mapping, an fd — must store which mount it came from too. That is
/// finding **F-1** of `docs/archive/EXT2_WRITEBACK_DESIGN.md`.
///
/// The id comes from the mount table, which assigns it and never reuses it. It
/// is deliberately not something the filesystem reports about itself.
///
/// `None` when the path does not resolve, or when the filesystem has no inode
/// addressing at all (procfs, memfs, synthetic nodes) — callers then fall back to
/// working by path, exactly as before.
pub fn resolve_file_id(path: &str) -> Option<(u32, u32)> {
    let resolved = resolve_mount(path)?;
    let inode = resolved.fs.resolve_inode(&resolved.rel).ok()?;
    (inode != 0).then_some((resolved.id, inode))
}

/// The inode `open(2)` should bind an fd to, or `None` when this fd has to go on
/// reading by path.
///
/// `read(2)` re-resolved `KernelFile::path` on every call — a full
/// `lookup_path_internal` directory walk plus a `read_inode`, per syscall, on
/// the hot on-disk read path. Resolving once here and reading by inode
/// afterwards is what `docs/archive/EXT2_WRITEBACK_DESIGN.md` § D-4 identified
/// as the next real read lever after the block cache, and it is what the
/// mmap/exec fault path has always done.
///
/// `None` for anything whose bytes do not come from an inode this filesystem
/// can address:
///
/// - **Filesystems with no inode addressing.** `resolve_inode` /
///   `read_at_by_inode` default to `NotSupported` in the `Filesystem` trait, so
///   procfs, the memory filesystem and every synthetic node land here and keep
///   their existing path-based read.
/// - **`/etc/mtab`.** It is a resolve-time synthetic served by [`read_at`]
///   *ahead of* `with_fs`; an image that happens to carry a real `/etc/mtab`
///   file would otherwise have its stale on-disk bytes served instead of the
///   live mount list.
/// - **Inode 0**, which is `InodePin`'s "no inode" sentinel and never a file.
///
/// Directories need no exclusion: `read_at_by_inode` refuses `S_IFDIR` exactly
/// where the path-based `read_at` does, so a `read(2)` on a directory fd still
/// gets `NotAFile` either way.
pub fn open_file_ids(path: &str) -> Option<(u32, u32)> {
    if is_mtab(path) {
        return None;
    }
    resolve_file_id(path)
}

/// The filesystem an fd (or a mapping) captured, found by the mount id it stored.
///
/// Searched in the same order [`resolve_mount`] searches for a path — spawn
/// override, the process's namespace, then the global table — so an fd resolves
/// through the same view its opener did.
///
/// `None` means that mount is **gone**: unmounted, or re-rooted, which mints a
/// new id precisely so the old one stops resolving. Ids are never reused, so this
/// can never return a different filesystem than the caller meant.
///
/// Callers must **not** fall back to resolving the path when this returns `None`.
/// That is the whole hazard: the path would resolve to whatever is mounted there
/// now, and applying the fd's inode number to that filesystem is exactly the
/// cross-mount aliasing the id exists to prevent. A vanished mount is an error.
fn fs_for_mount_id(id: u32) -> Option<Arc<dyn Filesystem>> {
    if id == 0 {
        return None;
    }
    {
        let tid = akuma_exec::threading::current_thread_id();
        let overrides = SPAWN_NS_OVERRIDE.lock();
        if let Some(ns) = overrides.get(&tid) {
            let found = ns.mount.lock().fs_by_id(id);
            if found.is_some() {
                return found;
            }
        }
    }
    if let Some(proc) = akuma_exec::process::current_process_shared()
        && let Some(fs) = proc.namespace.mount.lock().fs_by_id(id)
    {
        return Some(fs);
    }
    let table = MOUNT_TABLE.lock();
    table.as_ref()?.fs_by_id(id)
}

/// Read for an open file description, by the inode `open(2)` bound to it when
/// there is one and by path otherwise.
///
/// The path is still what selects the *mount* (`with_fs`), so this inherits the
/// same aliasing exposure the mmap fill path has carried since it started
/// reading by inode: if the mount under `path` is replaced while the fd is open,
/// or the fd is used from a process whose namespace resolves `path` to a
/// different mount, the inode number is interpreted against the wrong
/// filesystem. Closing that properly means an fd holding its resolved
/// `Arc<dyn Filesystem>` — a real open-file object, which this kernel does not
/// have — so it is recorded here rather than papered over.
pub fn read_at_open_file(
    path: &str,
    mount_id: u32,
    inode: u32,
    offset: usize,
    buf: &mut [u8],
) -> Result<usize, FsError> {
    if inode == 0 {
        return read_at(path, offset, buf);
    }
    let _ = mount_id;
    with_fs(path, |fs, _rel| fs.read_at_by_inode(inode, offset, buf))
}

/// [`metadata`] for an open file description, by the inode `open(2)` bound to it
/// when there is one and by path otherwise.
///
/// The `stat` counterpart of [`read_at_open_file`], and it exists for the same
/// two reasons: it skips a directory walk, and it keeps answering after the fd's
/// name is gone. Before this, an unlinked-but-open fd could `read` fine while
/// `fstat` on the same fd returned `ENOENT` — the fd knew which file it held and
/// `stat` did not.
///
/// Inherits [`read_at_open_file`]'s mount-aliasing caveat verbatim: the path
/// still selects the filesystem.
pub fn metadata_open_file(path: &str, mount_id: u32, inode: u32) -> Result<Metadata, FsError> {
    if inode == 0 {
        return metadata(path);
    }
    let fs = fs_for_mount_id(mount_id).ok_or(FsError::NotFound)?;
    fs.metadata_by_inode(inode)
        // A filesystem that resolved an inode for this fd but cannot stat by one
        // is not a case that exists today (the two are implemented together), but
        // falling back costs nothing and keeps this from being a new way to fail.
        // Safe to resolve by path here, unlike the vanished-mount case above: the
        // mount is the one this fd was opened on, so the path is being asked of
        // the right filesystem.
        .or_else(|_| metadata(path))
}

/// Write data at a specific offset within a file
pub fn write_at(path: &str, offset: usize, data: &[u8]) -> Result<usize, FsError> {
    if is_mtab(path) {
        return Err(FsError::NotSupported); // virtual, kernel-owned
    }
    let r = with_fs_write(path, |fs, rel| fs.write_at(rel, offset, data));
    invalidate_file_pages(path);
    r
}

/// Create a directory
pub fn create_dir(path: &str) -> Result<(), FsError> {
    with_fs_write(path, |fs, rel| fs.create_dir(rel))
}

/// Remove a file
pub fn remove_file(path: &str) -> Result<(), FsError> {
    // Resolve BEFORE the unlink — afterwards the path no longer names the inode,
    // and ext2 is free to hand that number to the next file created.
    invalidate_file_pages(path);
    with_fs_write(path, |fs, rel| fs.remove_file(rel))
}

/// Remove an empty directory
pub fn remove_dir(path: &str) -> Result<(), FsError> {
    with_fs_write(path, |fs, rel| fs.remove_dir(rel))
}

/// Check if a path exists
pub fn exists(path: &str) -> bool {
    if is_mtab(path) {
        return true;
    }
    if dev_node(path).is_some() {
        return true;
    }
    if with_fs(path, |fs, rel| Ok(fs.exists(rel))).unwrap_or(false) {
        return true;
    }
    dev_dir_metadata(path).is_some()
}

/// Get file size
pub fn file_size(path: &str) -> Result<u64, FsError> {
    with_fs(path, |fs, rel| fs.metadata(rel).map(|m| m.size))
}

/// Get metadata for a path
pub fn metadata(path: &str) -> Result<Metadata, FsError> {
    if let Some(rows) = mtab_rows(path) {
        return Ok(Metadata {
            is_dir: false,
            size: rows.len() as u64,
            inode: u64::MAX - 1, // synthetic, stable
            mode: 0o100444,
            created: None,
            modified: None,
            accessed: None,
        });
    }
    // Device nodes. `Metadata` carries no `rdev`, so `sys_newfstatat` /
    // `sys_statx` call `dev_node` directly for the full `stat` — this arm is
    // what makes `faccessat2` and every other `metadata` caller agree with
    // them about what exists.
    if let Some(node) = dev_node(path) {
        return Ok(Metadata {
            is_dir: false,
            size: 0,
            inode: node.ino,
            mode: node.mode(),
            created: None,
            modified: None,
            accessed: None,
        });
    }
    with_fs(path, |fs, rel| fs.metadata(rel)).or_else(|e| dev_dir_metadata(path).ok_or(e))
}

/// Change file permissions
pub fn chmod(path: &str, mode: u32) -> Result<(), FsError> {
    with_fs_write(path, |fs, rel| fs.chmod(rel, mode))
}

/// Truncate a file to a specified length
pub fn truncate(path: &str, length: u64) -> Result<(), FsError> {
    let r = with_fs_write(path, |fs, rel| fs.truncate(rel, length));
    invalidate_file_pages(path);
    r
}

/// Preallocate disk space for a file
pub fn fallocate(path: &str, mode: i32, offset: u64, len: u64) -> Result<(), FsError> {
    let r = with_fs_write(path, |fs, rel| fs.fallocate(rel, mode, offset, len));
    invalidate_file_pages(path);
    r
}

/// Resolve a path to its absolute form through the process's CWD.
fn resolve_absolute(path: &str) -> String {
    if let Some(proc) = akuma_exec::process::current_process_shared() {
        resolve_path(&proc.cwd, path)
    } else {
        resolve_without_cwd(path)
    }
}

/// Rename/move a file or directory
pub fn rename(old_path: &str, new_path: &str) -> Result<(), FsError> {
    let old_abs = resolve_absolute(old_path);
    let new_abs = resolve_absolute(new_path);

    // Both sides, before the rename: `new_path` may be an existing file this call
    // is about to replace (the build-tool "write temp, rename over" idiom), which
    // unlinks its inode; `old_path` stops naming its inode once the rename lands.
    invalidate_file_pages(&old_abs);
    invalidate_file_pages(&new_abs);

    // Try process namespace first (lock released before I/O).
    if let Some(proc) = akuma_exec::process::current_process_shared() {
        let ns_arcs = {
            let ns_mount = proc.namespace.mount.lock();
            match (
                ns_mount.resolve_arc_with_flags(&old_abs),
                ns_mount.resolve_arc_with_flags(&new_abs),
            ) {
                (Some((o_fs, o_rel, o_flags)), Some((n_fs, n_rel, n_flags))) => {
                    Some((o_fs, o_rel, o_flags, n_fs, n_rel, n_flags))
                }
                _ => None,
            }
        };
        if let Some((old_fs, old_rel, old_flags, new_fs, new_rel, new_flags)) = ns_arcs {
            // Same-mount check is `Arc::ptr_eq`, **not** a name comparison:
            // two ext2 instances (two disks) share the name "ext2", and a
            // rename between them is a cross-device `EXDEV`, not a same-fs
            // operation that happens to typecheck.
            if !Arc::ptr_eq(&old_fs, &new_fs) {
                return Err(FsError::NotSupported);
            }
            if old_flags & MS_RDONLY != 0 || new_flags & MS_RDONLY != 0 {
                return Err(FsError::ReadOnly);
            }
            return old_fs.rename(&old_rel, &new_rel);
        }
    }

    let (old_arc, old_rel, new_arc, new_rel) = {
        let table = MOUNT_TABLE.lock();
        let table = table.as_ref().ok_or(FsError::NotInitialized)?;
        let (old_fs, old_r, old_flags) =
            table.resolve_arc_with_flags(&old_abs).ok_or(FsError::NotFound)?;
        let (new_fs, new_r, new_flags) =
            table.resolve_arc_with_flags(&new_abs).ok_or(FsError::NotFound)?;
        // `Arc::ptr_eq`, not name comparison — see the namespace arm above.
        if !Arc::ptr_eq(&old_fs, &new_fs) {
            return Err(FsError::NotSupported);
        }
        if old_flags & MS_RDONLY != 0 || new_flags & MS_RDONLY != 0 {
            return Err(FsError::ReadOnly);
        }
        (old_fs, old_r, new_fs, new_r)
    };
    drop(new_arc);
    old_arc.rename(&old_rel, &new_rel)
}



// ============================================================================
// Symlink Support
// ============================================================================

/// Legacy in-memory symlink table (fallback for filesystems that don't support symlinks)
static SYMLINKS: Spinlock<Option<BTreeMap<String, String>>> = Spinlock::new(None);

pub fn create_symlink(link_path: &str, target: &str) -> Result<(), FsError> {
    // Try on-disk first via the mounted filesystem
    match with_fs_write(link_path, |fs, rel| fs.create_symlink(rel, target)) {
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

/// Create the socket node for an AF_UNIX pathname `bind(2)`.
///
/// No in-memory fallback, unlike [`create_symlink`]: a socket node's whole
/// purpose is that `stat` reports `S_ISSOCK` and `unlink` removes it, and a
/// kernel-side table entry gives neither. If the mounted filesystem cannot
/// represent the type, the caller is told so and can decide — falling back to a
/// regular file is the substitution that made a conformant client refuse to
/// connect to a working socket (`docs/archive/UNIX_SOCKET_IMPROVEMENTS.md` G7).
pub fn create_socket_node(path: &str) -> Result<(), FsError> {
    with_fs_write(path, |fs, rel| fs.create_socket_node(rel))
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
    if let Ok(result) = with_fs(path, |fs, rel| Ok(fs.is_symlink(rel)))
        && result {
            return true;
        }
    // Fallback to in-memory table
    let canonical = canonicalize_path(path);
    let table = SYMLINKS.lock();
    table.as_ref().is_some_and(|t| t.contains_key(&canonical))
}

pub fn remove_symlink(path: &str) -> bool {
    let canonical = canonicalize_path(path);
    let mut table = SYMLINKS.lock();
    table.as_mut().is_some_and(|t| t.remove(&canonical).is_some())
}

/// Resolve a path, following symlinks (up to 8 levels to prevent loops)
pub fn resolve_symlinks(path: &str) -> String {
    let mut resolved = canonicalize_path(path);
    for _ in 0..8 {
        let target = read_symlink(&resolved);
        if let Some(t) = target {
            if t.starts_with('/') {
                resolved = canonicalize_path(&t);
            } else {
                let (parent, _) = split_path(&resolved);
                resolved = resolve_path(parent, &t);
            }
        } else {
            if resolved == "/bin/sh" && crate::fs::exists("/bin/dash") {
                resolved = String::from("/bin/dash");
                continue;
            }
            break;
        }
    }
    resolved
}

fn get_child_mount_points(parent_path: &str) -> Vec<DirEntry> {
    let mut entries = Vec::new();

    if let Some(proc) = akuma_exec::process::current_process_shared() {
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

// ============================================================================
// statfs / mount listing
// ============================================================================

/// `/etc/mtab` is virtual: same rows as `/proc/mounts`, rendered from the
/// live mount tables on every read. Nothing is stored on disk — an ext2 file
/// would drift stale the first time a mount changed, which is exactly when
/// `umount`-family tools read it. Intercepts at these chokepoints *before*
/// `with_fs` so the on-disk `/etc` never participates; a cheap `ends_with`
/// pre-check keeps the hot path allocation-free.
const MTAB_PATH: &str = "/etc/mtab";

/// `Some(rows)` when `path` resolves to the virtual `/etc/mtab`, `None` for
/// every other path (the caller proceeds normally).
fn mtab_rows(path: &str) -> Option<Vec<u8>> {
    if !path.ends_with("mtab") {
        return None;
    }
    if resolve_absolute(path) != MTAB_PATH {
        return None;
    }
    let proc = akuma_exec::process::current_process_shared();
    Some(proc::mounts_bytes(proc.as_ref().map(|p| p.pid), proc.as_ref().map_or(0, |p| p.box_id)).unwrap_or_default())
}

/// Whether `path` resolves to the virtual `/etc/mtab` (existence check only —
/// no render, no allocation).
fn is_mtab(path: &str) -> bool {
    path.ends_with("mtab") && resolve_absolute(path) == MTAB_PATH
}

// ============================================================================
// /dev
// ============================================================================

/// `/dev` is virtual in the same shape `/etc/mtab` is: a resolve-time check
/// ahead of `with_fs`, not a mounted `Filesystem`. The nodes themselves live in
/// `akuma_vfs::dev` as pure data; this section is the only thing that knows
/// what the kernel actually probed (`crate::block`, `crate::audio`) and who is
/// asking (`box_id`). Background: `docs/archive/DEVFS_MISSING.md`.
///
/// Content is *not* served here — `sys_openat` intercepts a device path before
/// the VFS is reached and serves bytes from the `FileDescriptor` variant. This
/// layer answers "what exists" only.
const DEV_DIR: &str = "/dev";

/// Synthetic inode for the `/dev` directory itself, when the image has no real
/// one. Sits next to `/etc/mtab`'s `u64::MAX - 1`, well clear of ext2's range.
const DEV_DIR_INO: u64 = u64::MAX - 2;

/// Live kernel state plus caller identity, assembled for `akuma_vfs::dev`.
fn dev_probe() -> DevProbe {
    let mut block_slots = 0u8;
    for idx in 0..akuma_vfs::dev::MAX_BLOCK_SLOTS {
        if crate::block::device_name(idx).is_some() {
            block_slots |= 1 << idx;
        }
    }
    DevProbe {
        audio: crate::audio::is_available(),
        block_slots,
        // Boxes get no synthetic /dev — see `DevProbe::in_box` for why, and
        // for the two carve-outs (`null`/`zero` stat, and `/dev/net/tap0`,
        // which was never in this table and keeps working through `sys_openat`
        // so a `stack = rump` box still has a NIC).
        in_box: akuma_exec::process::current_process_shared().is_some_and(|p| p.box_id != 0),
    }
}

/// The device node `path` names, if it is one and the caller may see it.
///
/// The single lookup behind `stat`/`statx`/`access` on a device path — the
/// replacement for the four copy-pasted `if resolved_path == "/dev/null"`
/// blocks `DEVFS_MISSING.md` §1.2 catalogued.
///
/// Absolute paths — the shape every syscall-layer caller passes — resolve with
/// no allocation. A relative one pays a single `String`, the same one
/// `resolve_mount` allocates on the very next line.
pub fn dev_node(path: &str) -> Option<DevNode> {
    if let Some(name) = path.strip_prefix("/dev/") {
        return dev_node_named(name);
    }
    if path.starts_with('/') {
        return None;
    }
    let absolute = resolve_absolute(path);
    dev_node_named(absolute.strip_prefix("/dev/")?)
}

/// [`dev_node`] for a caller that already knows the entry sits directly under
/// `/dev` and has only the bare name — `sys_getdents64`, filling `d_type`.
/// Saves building a path just to strip the prefix back off.
///
/// `name` is `/dev`-relative and must not contain a slash; a nested path such
/// as `net/tap0` simply matches nothing.
pub fn dev_node_named(name: &str) -> Option<DevNode> {
    akuma_vfs::dev::lookup(dev_probe(), name)
}

/// Whether `path` resolves to `/dev` itself.
pub fn is_dev_dir(path: &str) -> bool {
    let trimmed = path.strip_suffix('/').unwrap_or(path);
    if trimmed == DEV_DIR {
        return true;
    }
    if trimmed.starts_with('/') {
        return false;
    }
    resolve_absolute(trimmed) == DEV_DIR
}

/// Whether `name` (a `/dev`-relative block device name, e.g. `"vda"`) is the
/// source of any mount in the global table — the check that keeps a raw
/// write-open off a device `Ext2Filesystem` is caching
/// (`proposals/RAW_BLOCK_DEVICE_FD.md` §3). Root mounts with source
/// `/dev/vda` (`src/fs.rs`), so this strips an optional `/dev/` prefix off
/// each recorded source before comparing.
///
/// Only the global table needs scanning: block nodes are invisible inside a
/// box (`DevProbe::in_box`), so a raw block open never reaches here for a
/// per-namespace mount.
pub fn device_is_mounted(name: &str) -> bool {
    let table = MOUNT_TABLE.lock();
    let Some(t) = table.as_ref() else { return false };
    let mut found = false;
    t.for_each_mount(|row| {
        if let Some(source) = row.source
            && source.strip_prefix("/dev/").unwrap_or(source) == name
        {
            found = true;
        }
    });
    found
}

/// The synthetic entries `ls /dev` should show, empty for every other path.
fn dev_entries(path: &str) -> Vec<DirEntry> {
    if !is_dev_dir(path) {
        return Vec::new();
    }
    akuma_vfs::dev::list(dev_probe())
        .map(|node| DirEntry {
            name: String::from(node.name),
            is_dir: false,
            is_symlink: false,
            size: 0,
        })
        .collect()
}

/// Metadata for the `/dev` *directory*, used only when the image has no real
/// one but the table does have nodes to show. Without this, `ls /dev` would
/// fail at the `open("/dev")` existence probe before ever reaching
/// [`dev_entries`].
fn dev_dir_metadata(path: &str) -> Option<Metadata> {
    // `list` is an iterator, so "has any node" costs one step, not a listing.
    if !is_dev_dir(path) || akuma_vfs::dev::list(dev_probe()).next().is_none() {
        return None;
    }
    Some(Metadata {
        is_dir: true,
        size: 0,
        inode: DEV_DIR_INO,
        mode: 0o40755,
        created: None,
        modified: None,
        accessed: None,
    })
}

/// What `statfs`/`fstatfs` need from the mount a path resolves to.
pub struct FsView {
    pub stats: FsStats,
    /// `ST_RDONLY` when the resolved mount is read-only, else `0`.
    pub flags: u64,
    pub fs_name: String,
}

/// Resolve `path` the way file operations do and report the mount's real
/// statistics. `statfs(2)` and `fstatfs(2)` both land here. All locks are
/// released by `resolve_mount` before the (allocating) name copy runs.
pub fn stats_for_path(path: &str) -> Result<FsView, FsError> {
    let r = resolve_mount(path).ok_or(FsError::NotFound)?;
    let stats = r.fs.stats()?;
    let fs_name = String::from(r.fs.name());
    Ok(FsView {
        stats,
        flags: if r.flags & MS_RDONLY != 0 { ST_RDONLY } else { 0 },
        fs_name,
    })
}

/// Render `/proc/mounts` rows for the process identified by `target_pid`
/// (whose namespace decides the mount set), as seen by `viewer_box_id`, into
/// `buf`; returns the byte count written.
///
/// Mount set = the target's namespace mounts, then any global mounts whose
/// path the namespace does not already cover — the same set the target can
/// actually *resolve* (`with_fs` tries the namespace, then falls back to the
/// global table), so the listing cannot advertise a mount the target cannot
/// reach, nor hide one it can.
///
/// Box policy (`docs/archive/MOUNT_MISSING_SYSCALLS.md` §2): a boxed viewer
/// learns *which* paths are mounted into it, never *where they came from* —
/// the source column is `none`. The host sees real sources.
///
/// Allocation-free end to end: rows render into `buf` via [`FmtBuf`] while the
/// mount lock is held, and the caller copies the bytes out afterwards.
pub fn render_mounts(viewer_box_id: u64, target_pid: Option<u32>, buf: &mut [u8]) -> usize {
    use akuma_primitives::console::FmtBuf;
    use core::fmt::Write as _;

    let mut pos = 0usize;
    let mut w = FmtBuf { buf, pos: &mut pos };

    // Namespace paths already listed, so the global pass can skip entries it
    // shadows (resolution tries the namespace first — the listing must agree).
    // Fixed store: `for_each_mount` runs under the namespace spinlock, where
    // nothing may allocate.
    struct Seen {
        paths: [[u8; 64]; 16],
        lens: [usize; 16],
        n: usize,
    }
    impl Seen {
        fn record(&mut self, path: &[u8]) {
            if self.n < self.paths.len() && !path.is_empty() && path.len() <= 64 {
                self.paths[self.n][..path.len()].copy_from_slice(path);
                self.lens[self.n] = path.len();
                self.n += 1;
            }
        }
        fn contains(&self, path: &[u8]) -> bool {
            (0..self.n).any(|i| {
                let l = self.lens[i];
                path.len() == l && &self.paths[i][..l] == path
            })
        }
    }

    fn emit(w: &mut FmtBuf<'_>, seen: &mut Seen, record: bool, viewer_box_id: u64,
            row: akuma_vfs::MountSnapshot<'_>) {
        if record {
            seen.record(row.path.as_bytes());
        }
        // A box never sees where a mount came from — see the doc comment.
        let source = if viewer_box_id == 0 {
            row.source.unwrap_or("none")
        } else {
            "none"
        };
        let opts = if row.flags & MS_RDONLY != 0 { "ro" } else { "rw" };
        let _ = writeln!(w, "{source} {} {} {opts} 0 0", row.path, row.fs_type);
    }

    let mut seen = Seen { paths: [[0; 64]; 16], lens: [0; 16], n: 0 };

    if let Some(pid) = target_pid
        && let Some(proc) = akuma_exec::process::lookup_process_shared(pid)
        && proc.box_id != 0
    {
        let ns_mount = proc.namespace.mount.lock();
        ns_mount.for_each_mount(|row| emit(&mut w, &mut seen, true, viewer_box_id, row));
    }

    let table = MOUNT_TABLE.lock();
    if let Some(t) = table.as_ref() {
        t.for_each_mount(|row| {
            // Skip global mounts the namespace already lists at this path.
            if !seen.contains(row.path.as_bytes()) {
                emit(&mut w, &mut seen, false, viewer_box_id, row);
            }
        });
    }

    pos
}
