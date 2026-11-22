//! The kernel-side VFS layer: the mount table, per-box namespaces, and `/proc`.
//!
//! `akuma-vfs` owns the *vocabulary* — the `Filesystem` trait, `DirEntry`,
//! `FsError`, path canonicalisation, the `MountTable` type. This crate owns the
//! kernel's single **instance** of all that: the global mount table behind a
//! spinlock, the per-box `Namespace` map that decides which mounts a container
//! can see, the ext2-over-virtio adapter, and the synthetic `/proc`.
//!
//! # Why it is a crate, as of 2026-09-01
//!
//! It was `src/vfs/`, and it could not leave while `proc.rs` read
//! `crate::syscall::log` and `crate::syscall::msgqueue` — because `src/syscall/`
//! reads 37 symbols back out of here, and Cargo cannot express a cycle. Those
//! two registries became `akuma-syscalls-log` and `akuma-syscalls-ipc` first;
//! this crate followed. It is in turn what unblocks `src/syscall/` itself.
//! `docs/archive/SRC_SYSCALL_EXTRACTION.md`.
//!
//! Everything it still needs from the binary is four function pointers
//! ([`VfsGlueHooks`]) and four booleans ([`VfsGlueConfig`]).

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod ext2;
pub mod fs;
pub mod proc;

use akuma_primitives::FileKey;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use spinning_top::Spinlock;

// Re-export everything from the crate so existing `use crate::*` keeps working.
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
pub fn mount_in_namespace(_root: akuma_primitives::HostRoot, box_id: u64, path: &str, fs: Arc<dyn Filesystem>) -> Result<(), FsError> {
    let namespaces = BOX_NAMESPACES.lock();
    let ns = namespaces.get(&box_id).ok_or(FsError::NotFound)?;
    ns.mount.lock().mount(path, fs)
}

/// The caller's namespace (its box's) for a private-namespace operation.
#[cfg(feature = "sc-private-ns")]
fn private_ns_of(tok: akuma_primitives::NsCreator) -> Result<Arc<Namespace>, FsError> {
    BOX_NAMESPACES.lock().get(&tok.box_id()).cloned().ok_or(FsError::NotFound)
}

/// Mount `fs` at `path` in the caller's own private namespace (never the global table): the `mount(2)` of a
/// process that is building the namespace a `clone(CLONE_NEWNS)` gave it. `path` is absolute.
///
/// # Errors
/// `NoSpace` past the mount limit; `NotFound` if the namespace is gone.
#[cfg(feature = "sc-private-ns")]
pub fn private_ns_mount(
    tok: akuma_primitives::NsCreator,
    path: &str,
    source: Option<&str>,
    flags: u64,
    fs: Arc<dyn Filesystem>,
) -> Result<(), FsError> {
    let ns = private_ns_of(tok)?;
    let mut set = ns.mount.lock();
    // Mounts stack on Linux (bubblewrap binds a directory onto itself to make it a mount point, then binds the real
    // thing over that). A mount set holds one filesystem per path, so mounting over a mount point replaces it: the
    // top of the stack is all anything can reach either way.
    let _ = set.unmount(path);
    set.mount_with(path, source, flags, fs)
}

/// `umount2` in a private namespace. `NotFound` when `path` is not a mount point there.
#[cfg(feature = "sc-private-ns")]
pub fn private_ns_unmount(tok: akuma_primitives::NsCreator, path: &str) -> Result<(), FsError> {
    private_ns_of(tok)?.mount.lock().unmount(path)
}

/// `mount(MS_REMOUNT)` in a private namespace: store `flags` (only `MS_RDONLY` is kept) on the mount at `path`.
#[cfg(feature = "sc-private-ns")]
pub fn private_ns_remount(tok: akuma_primitives::NsCreator, path: &str, flags: u64) -> Result<(), FsError> {
    private_ns_of(tok)?.mount.lock().remount(path, flags)
}

/// What a bind mount's source is.
#[cfg(feature = "sc-private-ns")]
pub enum BindSource {
    /// A directory: `(filesystem, path inside it)`.
    Dir(Arc<dyn Filesystem>, String),
    /// One of the host's device nodes, seen through the old-root stand-in's `/dev`, as the one-node filesystem to
    /// mount at the bind's target. Devices are served by path (`/dev/null` opens as the null device wherever it is
    /// named), so the mount only has to exist: bubblewrap looks it up in `mountinfo` afterwards.
    DeviceNode(Arc<dyn Filesystem>),
}

/// The caller's view of `path` as something a bind mount can serve, resolved through the caller's namespace and
/// then the global table, as any open would be.
///
/// # Errors
/// `NotFound` when nothing resolves; `NotADirectory` for any other file (a file bind is not supported).
#[cfg(feature = "sc-private-ns")]
pub fn resolve_for_bind(path: &str) -> Result<BindSource, FsError> {
    let r = resolve_mount(path).ok_or(FsError::NotFound)?;
    // A path that is exactly a mount point comes back relative as "".
    let rel = if r.rel.is_empty() { String::from("/") } else { r.rel };
    if r.fs.name() == StandInDev::NAME && rel != "/" {
        let node = StandInDev::node(&rel).ok_or(FsError::NotFound)?;
        return Ok(BindSource::DeviceNode(Arc::new(DevNodeFile(node))));
    }
    if !r.fs.metadata(&rel)?.is_dir {
        return Err(FsError::NotADirectory);
    }
    Ok(BindSource::Dir(r.fs, rel))
}

/// One device node as a filesystem whose root *is* the node (a file bind's mount).
#[cfg(feature = "sc-private-ns")]
struct DevNodeFile((&'static str, u32, u32));

#[cfg(feature = "sc-private-ns")]
impl Filesystem for DevNodeFile {
    fn name(&self) -> &'static str {
        "devnode"
    }
    fn read_dir(&self, _path: &str) -> Result<Vec<DirEntry>, FsError> {
        Err(FsError::NotADirectory)
    }
    fn read_file(&self, _path: &str) -> Result<Vec<u8>, FsError> {
        Ok(Vec::new())
    }
    fn write_file(&self, _path: &str, _data: &[u8]) -> Result<(), FsError> {
        Ok(())
    }
    fn create_dir(&self, _path: &str) -> Result<(), FsError> {
        Err(FsError::NotADirectory)
    }
    fn remove_file(&self, _path: &str) -> Result<(), FsError> {
        Err(FsError::ReadOnly)
    }
    fn remove_dir(&self, _path: &str) -> Result<(), FsError> {
        Err(FsError::NotADirectory)
    }
    fn exists(&self, _path: &str) -> bool {
        true
    }
    fn metadata(&self, _path: &str) -> Result<Metadata, FsError> {
        Ok(StandInDev::meta(false, 2 + u64::from(self.0.1 * 16 + self.0.2)))
    }
    fn stats(&self) -> Result<akuma_vfs::FsStats, FsError> {
        Ok(akuma_vfs::FsStats { block_size: 4096, total_blocks: 0, free_blocks: 0 })
    }
}

/// The host's device nodes a sandbox's `--dev` binds (`null zero full random urandom tty`), as a read-only
/// filesystem mounted at `put_old/dev` while the pre-pivot view is attached. Without it `oldroot/dev/null` does not
/// exist (Akuma serves `/dev` by absolute path, not from the root filesystem) and `--dev` cannot find its sources.
#[cfg(feature = "sc-private-ns")]
struct StandInDev;

#[cfg(feature = "sc-private-ns")]
impl StandInDev {
    const NAME: &'static str = "devfs-standin";
    /// `(name, major, minor)`.
    const NODES: [(&'static str, u32, u32); 6] =
        [("null", 1, 3), ("zero", 1, 5), ("full", 1, 7), ("random", 1, 8), ("urandom", 1, 9), ("tty", 5, 0)];

    fn node(path: &str) -> Option<(&'static str, u32, u32)> {
        let name = path.trim_matches('/');
        Self::NODES.iter().copied().find(|n| n.0 == name)
    }

    fn meta(is_dir: bool, ino: u64) -> Metadata {
        Metadata {
            is_dir,
            size: 0,
            inode: ino,
            mode: if is_dir { 0o40755 } else { 0o20666 },
            created: None,
            modified: None,
            accessed: None,
            links: None,
            uid: 0,
            gid: 0,
        }
    }
}

#[cfg(feature = "sc-private-ns")]
impl Filesystem for StandInDev {
    fn name(&self) -> &str {
        Self::NAME
    }
    fn is_read_only(&self) -> bool {
        true
    }
    fn read_dir(&self, path: &str) -> Result<Vec<DirEntry>, FsError> {
        if !path.trim_matches('/').is_empty() {
            return Err(FsError::NotADirectory);
        }
        Ok(Self::NODES
            .iter()
            .map(|n| DirEntry { name: String::from(n.0), is_dir: false, is_symlink: false, size: 0 })
            .collect())
    }
    fn read_file(&self, path: &str) -> Result<Vec<u8>, FsError> {
        Self::node(path).map(|_| Vec::new()).ok_or(FsError::NotFound)
    }
    fn write_file(&self, _path: &str, _data: &[u8]) -> Result<(), FsError> {
        Err(FsError::ReadOnly)
    }
    fn create_dir(&self, _path: &str) -> Result<(), FsError> {
        Err(FsError::ReadOnly)
    }
    fn remove_file(&self, _path: &str) -> Result<(), FsError> {
        Err(FsError::ReadOnly)
    }
    fn remove_dir(&self, _path: &str) -> Result<(), FsError> {
        Err(FsError::ReadOnly)
    }
    fn exists(&self, path: &str) -> bool {
        path.trim_matches('/').is_empty() || Self::node(path).is_some()
    }
    fn metadata(&self, path: &str) -> Result<Metadata, FsError> {
        if path.trim_matches('/').is_empty() {
            return Ok(Self::meta(true, 1));
        }
        Self::node(path).map(|n| Self::meta(false, 2 + u64::from(n.1 * 16 + n.2))).ok_or(FsError::NotFound)
    }
    fn stats(&self) -> Result<akuma_vfs::FsStats, FsError> {
        Ok(akuma_vfs::FsStats { block_size: 4096, total_blocks: 0, free_blocks: 0 })
    }
}

/// `pivot_root(new_root, put_old)` for a private namespace: the namespace becomes `new_root` as `/`, keeping only the
/// mounts that were under it.
///
/// * `put_old` **under** `new_root`: the pre-pivot view of the world (the caller's namespace and the global table) is
///   attached at `put_old`, as Linux leaves the old root mounted there. bubblewrap needs it: it pivots into a
///   scratch tmpfs first and then binds every source from `oldroot/…`. The namespace stays buildable and
///   [`private_ns_detach`] (`umount2(put_old)`) removes the view.
/// * `put_old == new_root` (`pivot_root(".", ".")`, bubblewrap's second, "Russian doll" pivot): there is no old root
///   to keep. It is dropped, mounted nowhere, and the namespace is **locked**: nothing can be mounted, pivoted or
///   unshared again.
///
/// Both paths are absolute and already resolved against the caller's cwd, and `put_old` is at or under `new_root`.
///
/// # Errors
/// `NotFound`/`NotADirectory` for a `new_root` that is not a directory in the caller's view;
/// `PermissionDenied` if the namespace is already locked.
#[cfg(feature = "sc-private-ns")]
pub fn private_ns_pivot(tok: akuma_primitives::NsCreator, new_root: &str, put_old: &str) -> Result<(), FsError> {
    let ns = private_ns_of(tok)?;
    if !akuma_exec::box_registry::private_ns_building(tok.box_id()) {
        return Err(FsError::PermissionDenied);
    }
    let r = resolve_mount(new_root).ok_or(FsError::NotFound)?;
    let rel = if r.rel.is_empty() { String::from("/") } else { r.rel };
    if !r.fs.metadata(&rel)?.is_dir {
        return Err(FsError::NotADirectory);
    }
    let root_fs: Arc<dyn Filesystem> = if rel == "/" {
        r.fs
    } else {
        Arc::new(akuma_isolation::subdir_fs::SubdirFs::new(r.fs, &rel))
    };
    let new_trim = new_root.trim_end_matches('/');
    // `put_old` as the new root will name it: `/tmp/newroot/oldroot` under `/tmp/newroot` is `/oldroot`.
    let attach = put_old.strip_prefix(new_trim).filter(|rel| rel.len() > 1).map(String::from);

    // The pre-pivot view, taken before the namespace is replaced: its own mounts first (they win), then the global
    // table's for every path the namespace did not cover.
    let old_view = attach.as_ref().map(|_| {
        let mut v = ns.mount.lock().entries();
        if let Some(t) = MOUNT_TABLE.lock().as_ref() {
            for e in t.entries() {
                if !v.iter().any(|(p, ..)| *p == e.0) {
                    v.push(e);
                }
            }
        }
        v
    });

    let mut set = ns.mount.lock();
    let mut after = set.rebased(new_root, root_fs, r.flags);
    if let (Some(rel), Some(view)) = (&attach, old_view) {
        for (path, source, flags, fs, _) in view {
            let at = if path == "/" { rel.clone() } else { alloc::format!("{rel}{path}") };
            let _ = after.mount_with(&at, source.as_deref(), flags, fs);
        }
        let _ = after.mount_with(&alloc::format!("{rel}/dev"), Some("dev"), MS_RDONLY, Arc::new(StandInDev));
    }
    *set = after;
    drop(set);
    match attach {
        Some(rel) => akuma_exec::box_registry::set_private_ns_old_root(tok.box_id(), Some(rel)),
        None => {
            let _ = akuma_exec::box_registry::lock_private_ns(tok.box_id());
        }
    }
    Ok(())
}

/// `umount2(put_old, MNT_DETACH)` after the first pivot: the attached pre-pivot view and everything under it go.
/// `Ok` when there is nothing attached (a lazy unmount is idempotent).
#[cfg(feature = "sc-private-ns")]
pub fn private_ns_detach(tok: akuma_primitives::NsCreator) -> Result<(), FsError> {
    let ns = private_ns_of(tok)?;
    if let Some(rel) = akuma_exec::box_registry::private_ns_old_root(tok.box_id()) {
        ns.mount.lock().unmount_tree(&rel);
        akuma_exec::box_registry::set_private_ns_old_root(tok.box_id(), None);
    }
    Ok(())
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

/// Sync every filesystem visible anywhere.
///
/// The global mount table and every
/// box namespace (the same fs may appear in both; `sync` is idempotent, and a
/// duplicate flush of an already-clean cache is free). Called from
/// `sys_reboot` before PSCI reset/poweroff — with the ext2 write-back cache,
/// dirty data otherwise dies with the machine — and, since 2026-10-05, from
/// `sync(2)` itself, which used to have no working arm at all on this tree
/// (corruption doc §11). Collects the `Arc<dyn Filesystem>` set under the
/// table/namespace locks, then syncs lock-free: `sync` does real block I/O
/// and must not run under a mount-table lock.
///
/// Was gated on `sc-reboot` while that syscall was its only caller;
/// `sync(2)`'s ungated arm is a caller on every build now, so the gate is
/// gone.
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

// ── orphaned-lock recovery: which mounts a dead thread's locks can be on ──────

/// The mounted filesystems that want a dead thread's locks released, for the
/// sweep [`reap_dead_thread`] performs.
///
/// `akuma-locks-rw` owns no global registry on purpose: enumerating locks is the
/// business of whoever owns them (`docs/archive/AKUMA_EXT2_CLEANUP.md` §4.5). This is
/// that owner.
///
/// **Why not the VFS mount table**, which already enumerates every filesystem and has
/// the `sync_all` precedent: `MountTable::resolve` hands out a `&dyn Filesystem`
/// borrowed from the table, so its callers hold [`MOUNT_TABLE`] *across* filesystem
/// calls. A reaper that took that lock would invert against them — thread A holds the
/// mount table and blocks in ext2 on a lock the dead thread left held, while the
/// recycler blocks on the mount table trying to release it. Nothing would ever run
/// again. This lock is taken only to write a slot at mount and to read the slots at
/// reap; it never covers device I/O or a filesystem call, so it cannot be part of a
/// cycle.
///
/// **Why `dyn Filesystem` and not a concrete type.** It was
/// `Weak<Ext2Filesystem<KernelBlockDevice>>` until 2026-09-20, and that is a set the
/// amd64 kernel cannot join: its root is `Ext2Filesystem<RootDevice>` — an enum over
/// virtio, a GRUB-placed RAM image and a USB partition, two of which are not block
/// devices this crate can name. So that kernel had orphaned-lock recovery silently off
/// while every hook *looked* wired, and the first fix for it was a second registry in
/// `crates/akuma-kernel-amd64/src/fs.rs`. One registry over the trait object both kernels already mount
/// through is the shape that cannot drift again
/// (`docs/archive/AKUMA_AMD64_NO_SLOT_RECYCLER.md` §8.3).
///
/// `Weak`, not `Arc`, so a registration cannot pin an unmounted filesystem forever.
const MAX_REAPABLE_MOUNTS: usize = 4;
static REAPABLE_MOUNTS: Spinlock<[Option<Weak<dyn Filesystem>>; MAX_REAPABLE_MOUNTS]> =
    Spinlock::new([const { None }; MAX_REAPABLE_MOUNTS]);

/// Record a mount for the sweep, reusing a slot whose filesystem has been dropped.
///
/// Called from [`mount_with`], so a filesystem is enrolled by being mounted and no
/// kernel has to remember to do it — which is the half of the amd64 gap that was a
/// missing call rather than a wrong type.
///
/// Filesystems answering `needs_tid_reap() == false` are skipped: `/proc`, `/dev` and
/// every container bind would otherwise take a slot to answer "nothing to release",
/// and the warning below would stop meaning what it says.
///
/// A full table is not an error worth failing a mount over — it costs orphaned-lock
/// recovery on that one filesystem, and the waiter-side backstop still unblocks the
/// system — so it is reported and ignored.
fn register_for_reap(fs: &Arc<dyn Filesystem>) {
    if !fs.needs_tid_reap() {
        return;
    }
    let mut slots = REAPABLE_MOUNTS.lock();
    for slot in slots.iter_mut() {
        if slot.as_ref().is_none_or(|w| w.strong_count() == 0) {
            *slot = Some(Arc::downgrade(fs));
            return;
        }
    }
    akuma_primitives::safe_print!(
        128,
        "[vfs] WARNING: more than {} reapable mounts — orphaned-lock recovery is off for this one\n",
        MAX_REAPABLE_MOUNTS
    );
}

/// Release everything the dead thread `tid` held on any mounted filesystem.
///
/// Registered with `akuma_exec::threading::set_slot_reap_callback` (AArch64, from
/// [`fs::init`]) and `akuma_threading::set_slot_reap_callback` (amd64, from its own
/// `fs::install_reap_hooks`), so it runs at the TERMINATED→FREE transition where the
/// tid is known dead and its slot cannot yet be reissued — the contract
/// `RecoverableRwLock::abandon_tid` is written against.
///
/// The upgraded handles are collected into a fixed array and the registry lock is
/// dropped **before** any of them is swept, so the sweep itself holds nothing.
/// How many mounts the sweep would currently visit.
///
/// Exists to be asserted on at boot. The failure this registry has actually had
/// is not a wrong sweep but an **empty** one — a kernel whose hooks were all
/// registered and whose set of filesystems to sweep was silently nil, because
/// its root was a type the registry could not hold. Nothing about that is
/// visible from the reap side: `reap_dead_thread` over zero mounts returns
/// exactly as it does over four. So both kernels check this is non-zero once
/// the root is mounted, which is the assertion that would have caught it.
#[must_use]
pub fn reapable_mount_count() -> usize {
    REAPABLE_MOUNTS
        .lock()
        .iter()
        .filter(|s| s.as_ref().is_some_and(|w| w.strong_count() > 0))
        .count()
}

pub fn reap_dead_thread(tid: usize) {
    let mut live: [Option<Arc<dyn Filesystem>>; MAX_REAPABLE_MOUNTS] =
        [const { None }; MAX_REAPABLE_MOUNTS];
    {
        let slots = REAPABLE_MOUNTS.lock();
        for (out, slot) in live.iter_mut().zip(slots.iter()) {
            *out = slot.as_ref().and_then(Weak::upgrade);
        }
    }
    for fs in live.iter().flatten() {
        fs.abandon_tid(tid);
    }
}

/// [`mount`] recording the mount's `source` (`/dev/vda`, `proc`, …) and
/// `MS_*` flags. Only `MS_RDONLY` is stored; the kernel's write chokepoints
/// enforce it as `FsError::ReadOnly`.
pub fn mount_with(
    _root: akuma_primitives::HostRoot,
    path: &str,
    source: Option<&str>,
    flags: u64,
    fs: Arc<dyn Filesystem>,
) -> Result<(), FsError> {
    // Before `MOUNT_TABLE` is taken, so the two locks never nest, and before the
    // filesystem is reachable, so no thread can take a write lock on a mount the
    // sweep cannot see. A registration whose mount then fails is harmless: the
    // caller drops the last `Arc` and the `Weak` slot is reused.
    register_for_reap(&fs);
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
pub fn unmount(_root: akuma_primitives::HostRoot, path: &str) -> Result<(), FsError> {
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
                .map_or_else(|| String::from("/"), |p| p.cwd.get());
            let absolute = resolve_path(&cwd, path);
            let ns_mount = ns.mount.lock();
            return ns_mount.resolve_arc_full(&absolute);
        }
    }

    if let Some(proc) = akuma_exec::process::current_process_shared() {
        let absolute = resolve_path(&proc.cwd.get(), path);
        // `chroot(2)`: the path above is in the process's own view (its `..` were
        // folded against the chroot's `/`); place it under the real root. `None`
        // — the case for every process that never chrooted — allocates nothing.
        let absolute = akuma_vfs::under_root(&proc.root.get(), &absolute).unwrap_or(absolute);

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
    if is_pts_dir(path) && pts_visible() {
        return Ok(pts_entries());
    }
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
    if r.is_ok() {
        // A whole-file rewrite: everything past the new contents reads zero,
        // and the contents themselves are a write at 0.
        notify_mapped_zeroed(path, data.len(), usize::MAX);
        notify_mapped_write(path, 0, data);
    }
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
    if !crate::cfg_shared_file_pages_enabled() || akuma_fpcache::len() == 0 {
        return;
    }
    if let Ok(inode) = resolve_inode(path) {
        akuma_fpcache::invalidate_inode(inode);
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
#[must_use]
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
#[must_use]
pub fn open_file_ids(path: &str) -> Option<(u32, u32)> {
    if is_mtab(path) {
        return None;
    }
    resolve_file_id(path)
}

/// `(filesystem identity, inode)` of the file at `path`: the key `akuma_primitives::text_busy` uses.
///
/// The inode number alone repeats across mounts. `None` for a path with no inode, or on a filesystem with no identity (procfs, the memory
/// filesystem), or a missing file.
#[must_use]
pub fn file_identity(path: &str) -> Option<(u32, u32)> {
    let (mount, inode) = open_file_ids(path)?;
    let fs = fs_for_mount_id(mount)?;
    match fs.identity() {
        0 => None,
        id => Some((id, inode)),
    }
}

/// Mark the file at `path` as being executed (`akuma_primitives::text_busy`), to be taken **before** its bytes are read.
///
/// An inert pin for a path with no trackable identity (procfs, the memory filesystem, a missing file): there is nothing to protect, and
/// the read that follows reports the real error.
///
/// # Errors
/// `Err(())` when the table of executing files is full: refuse the `execve`.
#[allow(clippy::result_unit_err)]
pub fn text_pin(path: &str) -> Result<akuma_primitives::TextPin, ()> {
    match file_identity(path) {
        Some((fs, inode)) => akuma_primitives::TextPin::try_new(fs, inode).ok_or(()),
        None => Ok(akuma_primitives::TextPin::none()),
    }
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
    key: Option<FileKey>,
    offset: usize,
    buf: &mut [u8],
) -> Result<usize, FsError> {
    // `None`: the descriptor has no resolved identity, and the path is all there is.
    let Some(key) = key else {
        return read_at(path, offset, buf);
    };
    let (mount_id, inode) = key.parts();
    let fs = fs_for_mount_id(mount_id).ok_or(FsError::NotFound)?;
    fs.read_at_by_inode(inode, offset, buf)
}

/// Read a file named only by `(mount id, inode)` — a lazy file region's record.
///
/// [`read_at_open_file`] without its by-path fallback: a region records no
/// path, so `inode == 0` or an unknown mount is an error, never a guess.
pub fn read_at_by_mount_inode(key: FileKey, offset: usize, buf: &mut [u8]) -> Result<usize, FsError> {
    let (mount_id, inode) = key.parts();
    let fs = fs_for_mount_id(mount_id).ok_or(FsError::NotFound)?;
    fs.read_at_by_inode(inode, offset, buf)
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
/// `key` is `None` for a descriptor with no resolved identity: the path is all there is.
pub fn metadata_open_file(path: &str, key: Option<FileKey>) -> Result<Metadata, FsError> {
    let Some(key) = key else {
        return metadata(path);
    };
    let (mount_id, inode) = key.parts();
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

/// [`write_at`] by inode, for a file that may no longer have a name.
///
/// The inode is the one an open file description or a mapping resolved.
///
/// The write half of [`read_at_open_file`], with its mount-aliasing caveat:
/// `mount_id` selects the filesystem. No path-keyed cache is told about the
/// write (there may be no path); the caller is the shared-mapping write-back,
/// whose pages are the authority for these bytes until they land.
///
/// The shared file-page cache is told, by inode: it is keyed by inode, so it
/// needs no path, and a cached read-only page of this file would otherwise keep
/// serving the bytes from before the write - to an unlinked file's readers,
/// whose path-keyed invalidation in [`write_at_open_file_or_path`] resolves to
/// nothing, and after every shared-mapping write-back.
pub fn write_at_open_file(key: FileKey, offset: usize, data: &[u8]) -> Result<usize, FsError> {
    let (mount_id, inode) = key.parts();
    let fs = fs_for_mount_id(mount_id).ok_or(FsError::NotFound)?;
    let r = fs.write_at_by_inode(inode, offset, data);
    if cfg_shared_file_pages_enabled() {
        akuma_fpcache::invalidate_inode(inode);
    }
    r
}

/// Write for an open file description: by the inode `open(2)` bound to it when
/// there is one, by path otherwise — the write half of [`read_at_open_file`].
///
/// `pwrite(2)` used to write by path while `pread(2)` read by inode, so for a
/// file unlinked after it was opened (Chromium's shared-memory files, created
/// and unlinked at once) the write went to the wrong place and the read saw
/// nothing (`pwv2probe.c`, 2026-10-08). The path-keyed caches are still told —
/// harmlessly, when the name is gone or now belongs to another file — so a file
/// that still has its name stays coherent with mappings and the page cache.
pub fn write_at_open_file_or_path(
    path: &str,
    key: Option<FileKey>,
    offset: usize,
    data: &[u8],
) -> Result<usize, FsError> {
    let Some(key) = key.filter(|_| !is_mtab(path)) else {
        return write_at(path, offset, data);
    };
    let r = write_at_open_file(key, offset, data);
    invalidate_file_pages(path);
    if let Ok(n) = r {
        notify_mapped_write(path, offset, &data[..n.min(data.len())]);
    }
    r
}

/// Write data at a specific offset within a file
pub fn write_at(path: &str, offset: usize, data: &[u8]) -> Result<usize, FsError> {
    if is_mtab(path) {
        return Err(FsError::NotSupported); // virtual, kernel-owned
    }
    let r = with_fs_write(path, |fs, rel| fs.write_at(rel, offset, data));
    invalidate_file_pages(path);
    if let Ok(n) = r {
        notify_mapped_write(path, offset, &data[..n.min(data.len())]);
    }
    r
}

/// `O_APPEND` write for an open file description.
///
/// By path while the name still finds the file (the filesystem picks the end
/// inside its own write lock, so two appenders cannot overwrite each other);
/// when the name is gone — the file was unlinked after `open` — by inode at the
/// size the inode reports, which is not atomic against another appender but is
/// the only way to reach the file.
pub fn append_open_file(
    path: &str,
    key: Option<FileKey>,
    data: &[u8],
) -> Result<(usize, usize), FsError> {
    match append(path, data) {
        Err(FsError::NotFound) if key.is_some() => {
            let size = metadata_open_file(path, key)?.size as usize;
            let n = write_at_open_file_or_path(path, key, size, data)?;
            Ok((size, n))
        }
        other => other,
    }
}

/// `O_APPEND` write: at the end of the file as the filesystem sees it inside
/// its own write lock. Returns `(offset written at, bytes written)`. See
/// `Filesystem::append` for why this is not `file_size` + [`write_at`].
pub fn append(path: &str, data: &[u8]) -> Result<(usize, usize), FsError> {
    if is_mtab(path) {
        return Err(FsError::NotSupported);
    }
    let r = with_fs_write(path, |fs, rel| fs.append(rel, data));
    invalidate_file_pages(path);
    if let Ok((offset, n)) = r {
        notify_mapped_write(path, offset, &data[..n.min(data.len())]);
    }
    r
}

/// Create a directory
pub fn create_dir(path: &str) -> Result<(), FsError> {
    // Linux looks the name up before it asks whether the mount is writable, so `mkdir` of a directory that is
    // already there is `EEXIST` even on a read-only mount (a sandbox's `--dev /dev` over a `--ro-bind / /` relies on
    // it) and `EROFS` only for one that is not.
    if exists(path) {
        return Err(FsError::AlreadyExists);
    }
    with_fs_write(path, |fs, rel| fs.create_dir(rel))
}

/// Flush the filesystem holding an open file description.
///
/// `fsync(2)` / `syncfs(2)`. The mount id (not the path) selects the filesystem, exactly as
/// [`metadata_open_file`] does: an fd's own filesystem must keep answering
/// after the name is unlinked, and that is the case that matters — `sqlite`
/// journals and `cargo`'s package database fsync an fd they still hold open.
pub fn sync_open_file(path: &str, key: Option<FileKey>) -> Result<(), FsError> {
    let Some(key) = key else {
        return sync_path(path);
    };
    let (mount_id, _) = key.parts();
    let fs = fs_for_mount_id(mount_id).ok_or(FsError::NotFound)?;
    fs.sync()
}

/// Flush the filesystem a path resolves to. The path-based form of
/// [`sync_open_file`], for the fallback arms.
fn sync_path(path: &str) -> Result<(), FsError> {
    let resolved = resolve_mount(path).ok_or(FsError::NotFound)?;
    resolved.fs.sync()
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
#[must_use]
pub fn exists(path: &str) -> bool {
    if is_mtab(path) {
        return true;
    }
    if dev_node(path).is_some() || pts_metadata(path).is_some() {
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

/// [`metadata`] and the id of the mount that answered, for `stat`'s `st_dev`. `0` for the synthetic nodes (device
/// nodes, `/dev/pts`, the mtab), which keep reporting device 1.
pub fn metadata_with_mount(path: &str) -> Result<(Metadata, u32), FsError> {
    if mtab_rows(path).is_some() || dev_node(path).is_some() || pts_metadata(path).is_some() {
        return metadata(path).map(|m| (m, 0));
    }
    let r = resolve_mount(path).ok_or(FsError::NotFound)?;
    r.fs.metadata(&r.rel).map(|m| (m, r.id)).or_else(|e| dev_dir_metadata(path).map(|m| (m, 0)).ok_or(e))
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
            links: None,
            uid: 0,
            gid: 0,
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
            links: None,
            uid: 0,
            gid: 0,
        });
    }
    if let Some(m) = pts_metadata(path) {
        return Ok(m);
    }
    with_fs(path, |fs, rel| fs.metadata(rel)).or_else(|e| dev_dir_metadata(path).ok_or(e))
}

/// Change file permissions
pub fn chmod(path: &str, mode: u32) -> Result<(), FsError> {
    with_fs_write(path, |fs, rel| fs.chmod(rel, mode))
}

/// Change a file's owner and/or group (`None` = leave). See `Filesystem::chown`.
pub fn chown(path: &str, uid: Option<u32>, gid: Option<u32>) -> Result<(), FsError> {
    with_fs_write(path, |fs, rel| fs.chown(rel, uid, gid))
}

/// Set a file's access/modification times (seconds since the epoch); `None`
/// leaves that one alone. See `Filesystem::set_times`.
pub fn set_times(path: &str, atime_secs: Option<u64>, mtime_secs: Option<u64>) -> Result<(), FsError> {
    with_fs_write(path, |fs, rel| fs.set_times(rel, atime_secs, mtime_secs))
}

/// Truncate a file to a specified length
pub fn truncate(path: &str, length: u64) -> Result<(), FsError> {
    let r = with_fs_write(path, |fs, rel| fs.truncate(rel, length));
    invalidate_file_pages(path);
    if r.is_ok() {
        notify_mapped_zeroed(path, usize::try_from(length).unwrap_or(usize::MAX), usize::MAX);
    }
    r
}

/// [`truncate`] for an open file description, which may have lost its name.
///
/// By path while the path still names the fd's inode (so every path-keyed
/// notification runs as before), by inode once it does not — `ftruncate` on an
/// unlinked-but-open file, which is how Chromium sizes its shared memory
/// (create, unlink, then size).
pub fn truncate_open_file(path: &str, key: Option<FileKey>, length: u64) -> Result<(), FsError> {
    let Some(key) = key.filter(|k| resolve_file_id(path) != Some(k.parts())) else {
        return truncate(path, length);
    };
    let (mount_id, inode) = key.parts();
    let fs = fs_for_mount_id(mount_id).ok_or(FsError::NotFound)?;
    let r = fs.truncate_by_inode(inode, length);
    if crate::cfg_shared_file_pages_enabled() {
        akuma_fpcache::invalidate_inode(inode);
    }
    r
}

/// [`fallocate`] for an open file description — see [`truncate_open_file`].
pub fn fallocate_open_file(path: &str, key: Option<FileKey>, mode: i32, offset: u64, len: u64) -> Result<(), FsError> {
    let Some(key) = key.filter(|k| resolve_file_id(path) != Some(k.parts())) else {
        return fallocate(path, mode, offset, len);
    };
    let (mount_id, inode) = key.parts();
    let fs = fs_for_mount_id(mount_id).ok_or(FsError::NotFound)?;
    let r = fs.fallocate_by_inode(inode, mode, offset, len);
    if crate::cfg_shared_file_pages_enabled() {
        akuma_fpcache::invalidate_inode(inode);
    }
    r
}

/// Preallocate disk space for a file
pub fn fallocate(path: &str, mode: i32, offset: u64, len: u64) -> Result<(), FsError> {
    let r = with_fs_write(path, |fs, rel| fs.fallocate(rel, mode, offset, len));
    invalidate_file_pages(path);
    // `FALLOC_FL_PUNCH_HOLE` (0x02) and `FALLOC_FL_ZERO_RANGE` (0x10) make the
    // range read zero; plain preallocation changes no byte.
    if r.is_ok() && mode & (0x02 | 0x10) != 0 {
        let from = usize::try_from(offset).unwrap_or(usize::MAX);
        notify_mapped_zeroed(path, from, from.saturating_add(usize::try_from(len).unwrap_or(usize::MAX)));
    }
    r
}

/// Resolve a path to its absolute form through the process's CWD.
fn resolve_absolute(path: &str) -> String {
    if let Some(proc) = akuma_exec::process::current_process_shared() {
        resolve_path(&proc.cwd.get(), path)
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

    let (fs, old_rel, new_rel) = resolve_same_mount(&old_abs, &new_abs)?;
    fs.rename(&old_rel, &new_rel)
}

/// `link(2)`: `new_path` becomes another name for `old_path`'s inode. Both must
/// be on one filesystem (`CrossDevice` = `EXDEV` otherwise). See
/// `Filesystem::link`.
pub fn link(old_path: &str, new_path: &str) -> Result<(), FsError> {
    let old_abs = resolve_absolute(old_path);
    let new_abs = resolve_absolute(new_path);
    let (fs, old_rel, new_rel) = resolve_same_mount(&old_abs, &new_abs)?;
    fs.link(&old_rel, &new_rel)
}

/// Resolve two absolute paths for an operation that must stay on one
/// filesystem (`rename`, `link`): the caller's namespace first, then the global
/// table, with no lock held across the filesystem call that follows.
///
/// Same-mount is `Arc::ptr_eq`, **not** a name comparison: two ext2 instances
/// (two disks) share the name "ext2", and an operation between them is a
/// cross-device `EXDEV`, not a same-fs one that happens to typecheck. That was
/// `NotSupported` (→ `EIO`) until 2026-10-14, which `mv` reports as a failure
/// instead of falling back to copy-and-delete.
fn resolve_same_mount(
    old_abs: &str,
    new_abs: &str,
) -> Result<(Arc<dyn Filesystem>, String, String), FsError> {
    if let Some(proc) = akuma_exec::process::current_process_shared() {
        let ns_arcs = {
            let ns_mount = proc.namespace.mount.lock();
            match (
                ns_mount.resolve_arc_with_flags(old_abs),
                ns_mount.resolve_arc_with_flags(new_abs),
            ) {
                (Some((o_fs, o_rel, o_flags)), Some((n_fs, n_rel, n_flags))) => {
                    Some((o_fs, o_rel, o_flags, n_fs, n_rel, n_flags))
                }
                _ => None,
            }
        };
        if let Some((old_fs, old_rel, old_flags, new_fs, new_rel, new_flags)) = ns_arcs {
            if !Arc::ptr_eq(&old_fs, &new_fs) {
                return Err(FsError::CrossDevice);
            }
            if old_flags & MS_RDONLY != 0 || new_flags & MS_RDONLY != 0 {
                return Err(FsError::ReadOnly);
            }
            return Ok((old_fs, old_rel, new_rel));
        }
    }

    let table = MOUNT_TABLE.lock();
    let table = table.as_ref().ok_or(FsError::NotInitialized)?;
    let (old_fs, old_rel, old_flags) =
        table.resolve_arc_with_flags(old_abs).ok_or(FsError::NotFound)?;
    let (new_fs, new_rel, new_flags) =
        table.resolve_arc_with_flags(new_abs).ok_or(FsError::NotFound)?;
    if !Arc::ptr_eq(&old_fs, &new_fs) {
        return Err(FsError::CrossDevice);
    }
    if old_flags & MS_RDONLY != 0 || new_flags & MS_RDONLY != 0 {
        return Err(FsError::ReadOnly);
    }
    Ok((old_fs, old_rel, new_rel))
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

/// Create and pin a pathname socket node in the caller's mount namespace.
pub fn create_socket_node(path: &str) -> Result<(u32, akuma_primitives::InodePin), FsError> {
    let mount = resolve_mount(path).ok_or(FsError::NotFound)?;
    if mount.flags & MS_RDONLY != 0 {
        return Err(FsError::ReadOnly);
    }
    let inode = mount.fs.create_socket_node(&mount.rel)?;
    Ok((mount.id, inode))
}

/// Pin the node found by resolving a socket pathname in the caller's view.
pub fn lookup_socket_node(path: &str) -> Result<(u32, akuma_primitives::InodePin), FsError> {
    let mount = resolve_mount(path).ok_or(FsError::NotFound)?;
    let inode = mount.fs.lookup_socket_node(&mount.rel)?;
    Ok((mount.id, inode))
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

/// Resolve a path, following symlinks **anywhere** in it (up to 16 expansions
/// to bound loops).
///
/// The walk itself is [`akuma_vfs::resolve_symlink_chain`], host-tested in
/// `akuma-vfs`; this supplies the filesystem's answers and the devbox's
/// `/bin/sh` → `/bin/dash` redirect, which stays a decision about the *final*
/// path, after every link has been chased.
#[must_use]
pub fn resolve_symlinks(path: &str) -> String {
    let resolved = akuma_vfs::resolve_symlink_chain(path, 16, read_symlink);
    if resolved == "/bin/sh" && crate::fs_exists("/bin/dash") {
        return String::from("/bin/dash");
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

/// `/dev/pts`: the live pseudo-terminal pairs, bit `N` for `/dev/pts/N`.
///
/// The pair table is `akuma-syscalls-glue::pty`, which sits *above* this crate,
/// so it publishes into this word instead of being asked. 64 bits because the
/// number space is 64 (`akuma_tty::pty::MAX_PTYS`). Readers want a listing and an
/// existence answer; a mask gives both with no allocation and no lock.
static PTS_LIVE: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Mark `/dev/pts/{n}` as existing or gone (`akuma-syscalls-glue::pty`, at
/// pair creation and at the pair's last close).
pub fn set_pts_live(n: u32, live: bool) {
    if n < 64 {
        let bit = 1u64 << n;
        if live {
            PTS_LIVE.fetch_or(bit, core::sync::atomic::Ordering::Relaxed);
        } else {
            PTS_LIVE.fetch_and(!bit, core::sync::atomic::Ordering::Relaxed);
        }
    }
}

const PTS_DIR: &str = "/dev/pts";
/// Synthetic inode for `/dev/pts`, beside `/dev`'s.
const PTS_DIR_INO: u64 = u64::MAX - 3;

/// Whether `path` resolves to `/dev/pts` itself.
#[must_use]
pub fn is_pts_dir(path: &str) -> bool {
    let trimmed = path.strip_suffix('/').unwrap_or(path);
    if trimmed == PTS_DIR {
        return true;
    }
    !trimmed.starts_with('/') && resolve_absolute(trimmed) == PTS_DIR
}

/// The live pair `/dev/pts/N` names, for an absolute path.
fn pts_slot(path: &str) -> Option<u32> {
    let digits = path.strip_prefix("/dev/pts/")?;
    if digits.is_empty() || digits.len() > 2 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n: u32 = digits.parse().ok()?;
    (n < 64 && PTS_LIVE.load(core::sync::atomic::Ordering::Relaxed) & (1 << n) != 0).then_some(n)
}

/// Whether `path` (absolute) names a live, visible pty slave `/dev/pts/N`.
///
/// The slave's ownership and mode are not stored anywhere, so
/// the callers that would change them (`chown`, `chmod`) accept and ignore it —
/// OpenSSH's `pty_setowner` is one, and it `fatal()`s on the `ENOENT` an
/// `exists()` probe gives for a node that lives only in this table.
#[must_use]
pub fn is_pty_slave(path: &str) -> bool {
    pts_visible() && pts_slot(path).is_some()
}

/// Neither `/dev/pts` nor its entries exist inside a box, for the reason
/// `DevProbe::in_box` gives for `/dev`.
fn pts_visible() -> bool {
    akuma_exec::process::current_process_shared().is_none_or(|p| p.box_id == 0)
}

/// `/dev/pts`'s listing: one character-device entry per live pair.
fn pts_entries() -> Vec<DirEntry> {
    let mask = PTS_LIVE.load(core::sync::atomic::Ordering::Relaxed);
    (0..64u32)
        .filter(|n| mask & (1 << n) != 0)
        .map(|n| DirEntry { name: alloc::format!("{n}"), is_dir: false, is_symlink: false, size: 0 })
        .collect()
}

/// Metadata for `/dev/pts` and `/dev/pts/N`. The slave's numbers (136:N) are
/// `akuma-syscalls-glue::pty::stat_of`'s, which `stat` uses; this answers the
/// `metadata` callers (`access`, `exists`, `O_DIRECTORY` checks).
fn pts_metadata(path: &str) -> Option<Metadata> {
    if !pts_visible() {
        return None;
    }
    let (is_dir, inode, mode) = if is_pts_dir(path) {
        (true, PTS_DIR_INO, 0o40755)
    } else {
        let n = pts_slot(path)?;
        (false, 0x7074_0000 + u64::from(n), 0o20620)
    };
    Some(Metadata { is_dir, size: 0, inode, mode, created: None, modified: None, accessed: None, links: None, uid: 0, gid: 0 })
}

/// Whether the kernel offers `/dev/fb0`. Set once by the amd64 kernel when it
/// booted with a direct-colour framebuffer; never set on AArch64.
static FRAMEBUFFER_PRESENT: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Announce that `/dev/fb0` exists (amd64, from boot). The node then appears in
/// `stat`/`ls /dev`; opening it is the kernel's own `openat` arm.
pub fn set_framebuffer_present(present: bool) {
    FRAMEBUFFER_PRESENT.store(present, core::sync::atomic::Ordering::Relaxed);
}

/// The renderer behind `/proc/power`, if this kernel has a power source.
///
/// A registered function rather than a [`VfsGlueHooks`] field: that struct is a
/// literal at every kernel's call site, and a kernel with no battery (every
/// AArch64 target) should not have to name a hook it can only answer "none" to.
/// Unregistered, `/proc/power` does not exist — not an empty file, which a
/// reader would take for "no battery" when the truth is "no way to know".
static POWER_RENDER: spinning_top::Spinlock<Option<fn(&mut [u8]) -> usize>> =
    spinning_top::Spinlock::new(None);

/// Register the `/proc/power` renderer: fills the buffer, returns bytes written.
/// Must not allocate (it runs on every read) and must tolerate a short buffer.
pub fn set_power_renderer(f: fn(&mut [u8]) -> usize) {
    *POWER_RENDER.lock() = Some(f);
}

/// The registered `/proc/power` renderer.
pub(crate) fn power_renderer() -> Option<fn(&mut [u8]) -> usize> {
    *POWER_RENDER.lock()
}

/// Extra `key value` lines for `/proc/health`, from state `akuma-vfs-glue`
/// cannot name (the pipe table lives in `akuma-syscalls-glue`, which depends on
/// this crate). Appended after the lines this crate renders itself.
static HEALTH_EXTRA: spinning_top::Spinlock<Option<fn(&mut [u8]) -> usize>> =
    spinning_top::Spinlock::new(None);

/// Register extra `/proc/health` lines: fills the buffer, returns bytes
/// written. Must not allocate, must tolerate a short buffer, and must write
/// whole `key value\n` lines.
pub fn set_health_extra(f: fn(&mut [u8]) -> usize) {
    *HEALTH_EXTRA.lock() = Some(f);
}

pub(crate) fn health_extra() -> Option<fn(&mut [u8]) -> usize> {
    *HEALTH_EXTRA.lock()
}

/// The `/proc/cpuinfo` block renderer: `f(core, buf)` writes the one block that
/// describes logical CPU `core` (ending in the blank line that separates
/// blocks) and returns the bytes written. Per-core rather than whole-file so
/// the renderer never needs a buffer sized by the core count, and so `read_at`
/// at any offset re-renders one block at a time on the stack. Must not allocate
/// and must tolerate a short buffer. Unregistered, the file still exists as bare
/// `processor : N` blocks: the core count is known on every kernel, the CPU's
/// description is not (2026-10-09: neither kernel had the file at all, and
/// Chromium logged `Failed to initialize cpuinfo`).
static CPUINFO_RENDER: spinning_top::Spinlock<Option<fn(usize, &mut [u8]) -> usize>> =
    spinning_top::Spinlock::new(None);

/// Register the `/proc/cpuinfo` block renderer.
pub fn set_cpuinfo_renderer(f: fn(usize, &mut [u8]) -> usize) {
    *CPUINFO_RENDER.lock() = Some(f);
}

pub(crate) fn cpuinfo_renderer() -> Option<fn(usize, &mut [u8]) -> usize> {
    *CPUINFO_RENDER.lock()
}

/// Whether the kernel offers `/dev/wifi0`. Set once at boot by a kernel with a
/// wifi backend (amd64: a radio, or `wifisim`).
static WIFI_PRESENT: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Announce that `/dev/wifi0` exists. The node then appears in `stat`/`ls /dev`;
/// opening it is the kernel's own `openat` arm.
pub fn set_wifi_present(present: bool) {
    WIFI_PRESENT.store(present, core::sync::atomic::Ordering::Relaxed);
}

static KVM_CTL_PRESENT: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
static KVM_API_PRESENT: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Announce the hypervisor's nodes: `ctl` is `/dev/kvm0`, `api` is `/dev/kvm`. They then
/// appear in `stat`/`ls /dev` for the host; opening is the kernel's own `openat` arm.
pub fn set_kvm_present(ctl: bool, api: bool) {
    KVM_CTL_PRESENT.store(ctl, core::sync::atomic::Ordering::Relaxed);
    KVM_API_PRESENT.store(api, core::sync::atomic::Ordering::Relaxed);
}

/// Live kernel state plus caller identity, assembled for `akuma_vfs::dev`.
fn dev_probe() -> DevProbe {
    let mut block_slots = 0u8;
    for idx in 0..akuma_vfs::dev::MAX_BLOCK_SLOTS {
        if akuma_virtio::block::device_name(idx).is_some() {
            block_slots |= 1 << idx;
        }
    }
    DevProbe {
        audio: crate::audio_is_available(),
        framebuffer: FRAMEBUFFER_PRESENT.load(core::sync::atomic::Ordering::Relaxed),
        wifi: WIFI_PRESENT.load(core::sync::atomic::Ordering::Relaxed),
        kvm_ctl: KVM_CTL_PRESENT.load(core::sync::atomic::Ordering::Relaxed),
        kvm_api: KVM_API_PRESENT.load(core::sync::atomic::Ordering::Relaxed),
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
#[must_use]
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
#[must_use]
pub fn dev_node_named(name: &str) -> Option<DevNode> {
    akuma_vfs::dev::lookup(dev_probe(), name)
}

/// Whether `path` resolves to `/dev` itself.
#[must_use]
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
/// source of any mount in the global table.
///
/// This is the check that keeps a raw
/// write-open off a device `Ext2Filesystem` is caching
/// (`docs/archive/RAW_BLOCK_DEVICE_FD.md` §3). Root mounts with source
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
    let probe = dev_probe();
    let mut entries: Vec<DirEntry> = akuma_vfs::dev::list(probe)
        .map(|node| DirEntry {
            name: String::from(node.name),
            is_dir: false,
            is_symlink: false,
            size: 0,
        })
        .collect();
    if !probe.in_box {
        entries.push(DirEntry { name: String::from("pts"), is_dir: true, is_symlink: false, size: 0 });
    }
    entries
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
        links: None,
        uid: 0,
        gid: 0,
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
/// List the target's namespace mounts. A namespace with its own root covers
/// every path, so the global table is only consulted for views without one.
///
/// Box policy (`docs/archive/MOUNT_MISSING_SYSCALLS.md` §2): a boxed viewer
/// learns *which* paths are mounted into it, never *where they came from* —
/// the source column is `none`. The host sees real sources.
///
/// Allocation-free end to end: rows render into `buf` via [`FmtBuf`] while the
/// mount lock is held, and the caller copies the bytes out afterwards.
pub fn render_mounts(viewer_box_id: u64, target_pid: Option<u32>, buf: &mut [u8]) -> usize {
    render_mounts_inner(viewer_box_id, target_pid, buf)
}

/// The id of the mount `path` resolves to in the caller's view (`/proc/<pid>/fdinfo`'s `mnt_id`), the same number
/// `mountinfo` prints for that mount.
#[must_use]
pub fn mount_id_of(path: &str) -> Option<u32> {
    resolve_mount(path).map(|r| r.id)
}

/// `/proc/<pid>/mountinfo` for `target_pid`.
///
/// One line per mount in the process's view — its own namespace, plus
/// the global table **unless the namespace has a `/` of its own** (then nothing falls through to the host, as
/// resolution does not). `id parent 0:id / <mount point> <opts> - <fstype> <source> <opts>`; the parent is the
/// deepest other mount that contains it.
///
/// Allocates (a `Vec` of rows, bounded by the mounts there are): this is a procfs read, whose `Vec<u8>` result is
/// an allocation anyway.
#[must_use]
#[allow(clippy::useless_let_if_seq)]
pub fn render_mountinfo(viewer_box_id: u64, target_pid: Option<u32>) -> Vec<u8> {
    use core::fmt::Write as _;
    let mut rows: Vec<(String, Option<String>, u64, Arc<dyn Filesystem>, u32)> = Vec::new();
    let mut ns_has_root = false;
    if let Some(pid) = target_pid
        && let Some(proc) = akuma_exec::process::lookup_process_shared(pid)
        && proc.box_id != 0
    {
        rows = proc.namespace.mount.lock().entries();
        ns_has_root = rows.iter().any(|(p, ..)| p == "/");
    }
    if !ns_has_root
        && let Some(t) = MOUNT_TABLE.lock().as_ref()
    {
        for e in t.entries() {
            if !rows.iter().any(|(p, ..)| *p == e.0) {
                rows.push(e);
            }
        }
    }
    // Shallow first, so a parent's id exists before its children name it.
    rows.sort_by(|a, b| a.0.len().cmp(&b.0.len()).then_with(|| a.0.cmp(&b.0)));
    let mut out = String::new();
    for (i, (path, source, flags, fs, id)) in rows.iter().enumerate() {
        let parent = rows[..i]
            .iter()
            .enumerate()
            .rev()
            .find(|(_, (p, ..))| {
                path.strip_prefix(p.trim_end_matches('/')).is_some_and(|r| r.starts_with('/'))
            })
            .map_or(*id, |(_, r)| r.4);
        let opts = if flags & MS_RDONLY != 0 { "ro" } else { "rw" };
        // A box never sees where a mount came from (as in `render_mounts`).
        let src = if viewer_box_id == 0 { source.as_deref().unwrap_or("none") } else { "none" };
        let _ = writeln!(out, "{id} {parent} 0:{id} / {path} {opts},relatime - {} {src} {opts}", fs.name());
    }
    out.into_bytes()
}

fn render_mounts_inner(viewer_box_id: u64, target_pid: Option<u32>, buf: &mut [u8]) -> usize {
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
        let mut has_root = false;
        ns_mount.for_each_mount(|row| {
            has_root |= row.path == "/";
            emit(&mut w, &mut seen, true, viewer_box_id, row);
        });
        if has_root {
            return pos;
        }
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

// ===========================================================================
// What this layer still needs from the binary
// ===========================================================================

/// The four things `/proc` reports that only the binary can answer.
///
/// Deliberately tiny, and it stayed tiny by checking each candidate first:
/// `crate::block` turned out to be a re-export of `akuma_virtio::block`,
/// `crate::pmm::stats` and the file-page-cache pair already live in
/// `akuma-pmm`/`akuma-fpcache`, and `crate::timer::uptime_us` is
/// `akuma_primitives::clock`. What is left is genuinely binary-local: an inline
/// `mod audio` in `main.rs`, the VFS-glue-facing half of `src/fs.rs`, the SMP
/// topology probe, and the wall clock (which needs the binary's boot uptime to
/// turn monotonic microseconds into UTC).
///
/// Unregistered is quiet, not fatal — every getter below has a defined answer
/// with no hooks installed, so host tests and early boot read a coherent
/// (if empty) `/proc` rather than panicking. Same contract as
/// `akuma_primitives::console::print_str`.
#[derive(Clone, Copy)]
pub struct VfsGlueHooks {
    /// `crate::audio::is_available` — whether `/proc/audio` reports a device.
    pub audio_is_available: fn() -> bool,
    /// `crate::fs::exists` — path probe against the binary's own FS front door.
    pub fs_exists: fn(&str) -> bool,
    /// `crate::smp_shared::probed_core_count` — DTB-probed core count for `/proc/cores`.
    pub probed_core_count: fn() -> usize,
    /// `crate::timer::utc_time_us` — wall clock, `None` before NTP/RTC sets it.
    pub utc_time_us: fn() -> Option<u64>,
    /// `crate::file_page_cache::init` — the binary's shim, which supplies the
    /// four `FpcacheConfig` fields from `src/config.rs`. Called with total RAM
    /// in bytes from [`fs::init`]. **Not** `akuma_fpcache::init` directly: that
    /// takes the config, and duplicating it here would give `src/config.rs` a
    /// second copy.
    pub fpcache_init: fn(usize),
    /// One process's mappings, ascending, for `/proc/<pid>/maps` and
    /// `/proc/<pid>/statm` — or `None` from a kernel that cannot describe an
    /// address space.
    ///
    /// **A hook rather than code in `proc.rs`, because the walk is not
    /// portable.** Reporting a *mapping* means reporting more than the region
    /// list: an ELF image and its initial stack are placed by the loader and
    /// are deliberately not regions, so a walk of the regions alone renders an
    /// **empty** `maps` for an ordinary program — worse than not serving the
    /// file, because a reader scanning for the mapping that contains an address
    /// gets a confident "there is none". The rest comes from the page-table
    /// leaves, and that walk (`UserAddressSpace::for_each_user_leaf`) exists
    /// only for x86_64 — it is `x86_walk_leaves` underneath, with no AArch64
    /// counterpart.
    ///
    /// So the kernel that *can* walk registers this; the one that cannot
    /// registers a function returning `None`, and procfs reports both files as
    /// absent — exactly what it did before the hook existed.
    ///
    /// PHASE-2-PROCFS (D27): the walk itself moved to
    /// `akuma-exec::process::maps_rows::pid_map_rows` — it reads only shared
    /// `Process` facts, and its leaf iterator (`for_each_user_leaf`) lives in
    /// `akuma-mmu`, so the "x86-only" caveat above is historical: both kernels
    /// may register the same fn now. What stays per-kernel is only the
    /// *registration*.
    pub pid_map_rows: fn(u32) -> Option<alloc::vec::Vec<MapRow>>,
    /// One process's auxv table (`/proc/<pid>/auxv`, PHASE-2-PROCFS D28) —
    /// the pairs the loader handed the image at exec, **without** the `AT_NULL`
    /// terminator (the renderer adds it, matching the stack's own vector), or
    /// `None` from a kernel that retained nothing — which keeps the file
    /// absent rather than empty, the `maps` rule. A hook for the same reason
    /// `pid_map_rows` is: the pairs live where the kernel's exec path put
    /// them, and this file must not reach into a Process.
    pub pid_auxv: fn(u32) -> Option<alloc::vec::Vec<(u64, u64)>>,
}

/// One mapping: `(start, end, readable, writable, executable, private)`.
///
/// `end` is exclusive. `private` is `p` in the `maps` permission column; a
/// shared anonymous mapping is `s`.
pub use akuma_exec::process::maps_rows::MapRow;

static HOOKS: akuma_primitives::OnceCopy<VfsGlueHooks> = akuma_primitives::OnceCopy::new();

/// Install the binary's callbacks. Idempotent, per `OnceCopy`.
pub fn set_hooks(h: VfsGlueHooks) {
    HOOKS.set(h);
}

/// Observers for writes to files that a writable `MAP_SHARED` mapping may hold
/// live in memory — the amd64 kernel's shared writable page table
/// (`akuma-fpcache-rw`, 2026-10-03).
///
/// A unified page cache makes `write(2)` visible through `mmap` because both are
/// the same page. This tree has no unified cache, so a mapped page and the file
/// would drift apart the moment anyone wrote the file by descriptor; these
/// hooks copy each mutation **into** the mapped frames after it lands. SQLite
/// depends on it: it grows its `-shm` index by `pwrite`-ing one byte per new
/// page while other processes hold the earlier pages mapped.
///
/// Separate from [`VfsGlueHooks`] because only one kernel has the table: the
/// AArch64 kernel registers nothing, and every call below then costs one
/// `OnceCopy` load. **Called after the mutation**, with no filesystem lock held.
#[derive(Clone, Copy)]
pub struct MappedFileHooks {
    /// Is anything mapped (or being filled) that a write could need to reach?
    /// Asked first, so a kernel with no shared-writable mappings pays no path
    /// resolution per write.
    pub active: fn() -> bool,
    /// `data` was written at byte `offset` of `(mount_id, inode)`.
    pub wrote: fn(key: FileKey, offset: usize, data: &[u8]),
    /// Bytes `[from, to)` of `(mount_id, inode)` now read as zero (truncate:
    /// `to == usize::MAX`; a punched hole otherwise).
    pub zeroed: fn(key: FileKey, from: usize, to: usize),
}

static MAPPED_FILE_HOOKS: akuma_primitives::OnceCopy<MappedFileHooks> = akuma_primitives::OnceCopy::new();

/// Install the mapped-file observers. Idempotent, per `OnceCopy`.
pub fn set_mapped_file_hooks(h: MappedFileHooks) {
    MAPPED_FILE_HOOKS.set(h);
}

/// Wire `akuma_exec::shm`, the shared writable page table, to the filesystem.
///
/// The same on both kernels: every `write(2)`/truncate reaches the mapped frames,
/// and ext2 freeing an inode number drops both caches' pages for it. Before any
/// mapping can be made: ext2 reissues inode numbers, so without the hook a new
/// file inherits the cached pages of whatever last held its number
/// (`docs/archive/SELFHOST_ZERO_PAGE_HUNT.md` §15).
pub fn init_shared_write() {
    fn inode_freed(inode: u32) {
        akuma_fpcache::invalidate_inode(inode);
        akuma_exec::shm::inode_freed(inode);
    }
    akuma_ext2::init_inode_freed_hook(inode_freed);
    set_mapped_file_hooks(MappedFileHooks {
        active: akuma_exec::shm::active,
        wrote: akuma_exec::shm::wrote,
        zeroed: akuma_exec::shm::zeroed,
    });
}

/// The registered observers, if any and if they have anything to observe.
fn mapped_file_hooks() -> Option<MappedFileHooks> {
    MAPPED_FILE_HOOKS.get().filter(|h| (h.active)())
}

/// Tell the mapped-file observers that `data` landed at `offset` of `path`.
fn notify_mapped_write(path: &str, offset: usize, data: &[u8]) {
    if data.is_empty() {
        return;
    }
    if let Some(h) = mapped_file_hooks()
        && let Some((mount_id, inode)) = resolve_file_id(path)
        && let Some(key) = FileKey::try_from_raw(mount_id, inode)
    {
        (h.wrote)(key, offset, data);
    }
}

/// Tell the mapped-file observers that `[from, to)` of `path` reads zero now.
fn notify_mapped_zeroed(path: &str, from: usize, to: usize) {
    if let Some(h) = mapped_file_hooks()
        && let Some((mount_id, inode)) = resolve_file_id(path)
        && let Some(key) = FileKey::try_from_raw(mount_id, inode)
    {
        (h.zeroed)(key, from, to);
    }
}

fn audio_is_available() -> bool {
    HOOKS.get().is_some_and(|h| (h.audio_is_available)())
}

fn fs_exists(path: &str) -> bool {
    HOOKS.get().is_some_and(|h| (h.fs_exists)(path))
}

/// See [`VfsGlueHooks::pid_map_rows`]. `None` when no kernel registered one, or
/// when the one that did cannot describe this pid.
pub(crate) fn pid_map_rows(pid: u32) -> Option<alloc::vec::Vec<MapRow>> {
    HOOKS.get().and_then(|h| (h.pid_map_rows)(pid))
}

/// See [`VfsGlueHooks::pid_auxv`]. `None` when no kernel registered one, or
/// when it retained no table for this pid.
pub(crate) fn pid_auxv(pid: u32) -> Option<alloc::vec::Vec<(u64, u64)>> {
    HOOKS.get().and_then(|h| (h.pid_auxv)(pid))
}

/// The kernel's core count. Every kernel supplies the hook (the main kernel
/// answers 1 without `smp-shared`; the tokio kernels their online count).
fn probed_core_count() -> usize {
    HOOKS.get().map_or(1, |h| (h.probed_core_count)())
}

fn utc_time_us() -> Option<u64> {
    HOOKS.get().and_then(|h| (h.utc_time_us)())
}

fn fpcache_init(total_ram_bytes: usize) {
    if let Some(h) = HOOKS.get() {
        (h.fpcache_init)(total_ram_bytes);
    }
}

/// Tunables. `src/config.rs` stays the single source of truth; `src/vfs.rs`
/// reads the consts and calls [`init`].
///
/// The tunables, read as `const`s from `akuma_config` rather than handed over.
///
/// This crate briefly carried a `VfsGlueConfig` struct and `src/vfs.rs` populated
/// it at boot. That is the pattern `akuma-config` exists to retire: a `const`
/// that becomes runtime config stops const-folding whatever it gates, and the
/// two `/proc` flags below cost a page of `.text` while they were loads.
/// They fold again now, with no `#[cfg]` duplication needed here at all.
const fn cfg_proc_syscall_log_enabled() -> bool {
    akuma_config::PROC_SYSCALL_LOG_ENABLED
}

const fn cfg_proc_sysvipc_enabled() -> bool {
    akuma_config::PROC_SYSVIPC_ENABLED
}

const fn cfg_shared_file_pages_enabled() -> bool {
    // `akuma_config::SHARED_FILE_PAGES_ENABLED` removed (config flag sweep,
    // 2026-11-11): never flipped since 2026-08-05. Kept as a function so every
    // call site here is unchanged.
    true
}

const fn cfg_syscall_debug_info_enabled() -> bool {
    akuma_config::SYSCALL_DEBUG_INFO_ENABLED
}

const fn cfg_syscall_debug_io_enabled() -> bool {
    akuma_config::SYSCALL_DEBUG_IO_ENABLED
}

const fn cfg_max_threads() -> usize {
    akuma_config::MAX_THREADS
}

const fn cfg_proc_stdout_max_size() -> usize {
    akuma_config::PROC_STDOUT_MAX_SIZE
}

