//! Private mount namespaces: what `clone(CLONE_NEWNS)`, `mount`, `umount2` and `pivot_root` mean on Akuma.
//!
//! Akuma has boxes, not Linux namespaces, so a "namespace" is a box: `clone(CLONE_NEWNS)` from the host makes
//! a fresh box whose mount set starts empty (every path falls through to the host's view) and puts the child in
//! it. The child may `mount` into *that namespace only*. `pivot_root` re-roots the namespace; the first pivot keeps
//! the pre-pivot view attached at `put_old` until `umount2` (bubblewrap sources its binds from there), and the
//! final one (`put_old == new_root`) **mounts the old root nowhere** and locks the namespace. Nothing here touches
//! the global mount table. The whole design, and what it does not enforce, is `docs/archive/BWRAP_ON_BOXES.md`.
//!
//! Gated by `sc-private-ns`; without it every boxed `mount` is `EPERM` as before.
use super::*;
use akuma_primitives::NsCreator;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, Ordering};

const MS_RDONLY: u64 = 1;
const MS_REMOUNT: u64 = 32;
const MS_BIND: u64 = 4096;
const MS_UNBINDABLE: u64 = 1 << 17;
const MS_PRIVATE: u64 = 1 << 18;
const MS_SLAVE: u64 = 1 << 19;
const MS_SHARED: u64 = 1 << 20;
const MS_PROPAGATION: u64 = MS_UNBINDABLE | MS_PRIVATE | MS_SLAVE | MS_SHARED;

const CLONE_NEWNS: u64 = 0x0002_0000;
const CLONE_NEWUSER: u64 = 0x1000_0000;

/// Ids of private-namespace boxes start here, well clear of the hash-derived ids `box_id_for` gives named boxes.
const PRIVATE_BOX_BASE: u64 = 1 << 62;
static NEXT_PRIVATE_BOX: AtomicU64 = AtomicU64::new(PRIVATE_BOX_BASE);
/// A private namespace younger than this is never swept: its first process may not exist yet.
const SWEEP_GRACE_US: u64 = 5_000_000;

/// `path` as an absolute, normalised path in the caller's view.
fn absolute(path: &str) -> String {
    let cwd = akuma_exec::process::current_process_shared().map_or_else(|| String::from("/"), |p| p.cwd.get());
    akuma_vfs_glue::resolve_path(&cwd, path)
}

/// Tear down private namespaces nobody lives in any more (a bubblewrap run's box outlives the run otherwise).
fn sweep() {
    let now = akuma_primitives::clock::uptime_us();
    let stale = akuma_exec::box_registry::private_ns_born_before(now.saturating_sub(SWEEP_GRACE_US));
    if stale.is_empty() {
        return;
    }
    let live = akuma_exec::process::list_processes();
    for id in stale {
        if live.iter().any(|p| p.box_id == id && p.state != "zombie") {
            continue;
        }
        let _ = akuma_exec::process::unregister_box(id);
        akuma_vfs_glue::remove_box_namespace(id);
        akuma_exec::box_registry::unregister_private_ns(id);
    }
}

/// The `clone` side of `CLONE_NEWNS`.
///
/// Returns the `(box id, namespace)` the child is born into when `flags` ask for
/// `CLONE_NEWNS`, `None` when they do not (the other `CLONE_NEW*` bits are accepted and change nothing), or the
/// errno to answer.
///
/// Only the host may do it: a boxed process creating a namespace is nested namespacing, which is refused.
/// Linux wants `CAP_SYS_ADMIN`, or a user namespace made in the same call, so this wants uid 0 or `CLONE_NEWUSER`.
pub fn clone_namespace(flags: u64) -> Result<Option<(u64, Arc<akuma_isolation::Namespace>)>, u64> {
    if flags & CLONE_NEWNS == 0 {
        return Ok(None);
    }
    let Some(parent) = akuma_exec::process::current_process_shared() else { return Err(ESRCH) };
    if parent.box_id != 0 || !(parent.creds.get().is_root() || flags & CLONE_NEWUSER != 0) {
        if akuma_config::SYSCALL_DEBUG_INFO_ENABLED {
            akuma_primitives::safe_print!(128, "[clone] CLONE_NEWNS denied: box={} (only the host may make a namespace)\n", parent.box_id);
        }
        return Err(EPERM);
    }
    sweep();
    let id = NEXT_PRIVATE_BOX.fetch_add(1, Ordering::Relaxed);
    akuma_exec::process::register_box(akuma_exec::process::BoxInfo {
        id,
        name: alloc::format!("ns-{id:x}"),
        root_dir: String::from("/"),
        creator_pid: parent.pid,
        primary_pid: 0,
        parent_box_id: None,
    });
    let ns = akuma_vfs_glue::create_box_namespace(id, "/");
    akuma_exec::box_registry::register_private_ns(id, akuma_primitives::clock::uptime_us());
    Ok(Some((id, ns)))
}

/// `mount(2)` by the builder of a private namespace. Acts on that namespace only.
pub(super) fn mount(tok: NsCreator, source_ptr: UserAddr, target_ptr: UserAddr, fstype_ptr: UserAddr, flags: u64) -> SysResult {
    let (source_ptr, target_ptr, fstype_ptr) = (source_ptr.get(), target_ptr.get(), fstype_ptr.get());
    // Propagation changes (`mount --make-rslave /`) have nothing to do: a private namespace shares no subtree.
    if flags & MS_PROPAGATION != 0 && flags & (MS_BIND | MS_REMOUNT) == 0 {
        return Ok(0);
    }
    // The target, like a bind source, may be `/proc/self/fd/N`.
    let target = akuma_vfs_glue::resolve_symlinks(&absolute(&copy_from_user_str(target_ptr, 4096)?));

    if flags & MS_REMOUNT != 0 {
        return match akuma_vfs_glue::private_ns_remount(tok, &target, flags & MS_RDONLY) {
            Ok(()) => Ok(0),
            Err(_) => Err(Errno::EINVAL),
        };
    }

    if flags & MS_BIND != 0 {
        // bubblewrap names a source as `/proc/self/fd/N` (it opened the directory first); chase the link.
        let source = akuma_vfs_glue::resolve_symlinks(&absolute(&copy_from_user_str(source_ptr, 4096)?));
        return match akuma_vfs_glue::resolve_for_bind(&source) {
            Ok(akuma_vfs_glue::BindSource::Dir(fs, rel)) => {
                let view: Arc<dyn akuma_vfs_glue::Filesystem> =
                    Arc::new(akuma_isolation::subdir_fs::SubdirFs::new(fs, &rel));
                bound(akuma_vfs_glue::private_ns_mount(tok, &target, Some(&source), flags & MS_RDONLY, view))
            }
            // A device node bound over an empty file (`--dev` does this for /dev/null and friends). Devices are
            // served by path, so the mount is only a marker bubblewrap can find in `mountinfo`.
            Ok(akuma_vfs_glue::BindSource::DeviceNode(fs)) => {
                bound(akuma_vfs_glue::private_ns_mount(tok, &target, Some(&source), flags & MS_RDONLY, fs))
            }
            Err(akuma_vfs_glue::FsError::NotADirectory) => Err(Errno::ENOTDIR),
            Err(_) => Err(Errno::ENOENT),
        };
    }

    let fstype = copy_from_user_str(fstype_ptr, 64)?;
    let source = if source_ptr != 0 { copy_from_user_str(source_ptr, 64)? } else { String::new() };
    let fs: Arc<dyn akuma_vfs_glue::Filesystem> = match fstype.as_str() {
        "proc" => Arc::new(akuma_vfs_glue::proc::ProcFilesystem::new()),
        "tmpfs" => Arc::new(akuma_vfs::MemoryFilesystem::new()),
        // `/dev/pts` is synthesised by path; the mount that `--dev` asks for has nothing to add.
        "devpts" => return Ok(0),
        _ => return Err(Errno::ENODEV),
    };
    let recorded = if source.is_empty() { fstype.as_str() } else { source.as_str() };
    bound(akuma_vfs_glue::private_ns_mount(tok, &target, Some(recorded), flags & MS_RDONLY, fs))
}

fn bound(r: Result<(), akuma_vfs_glue::FsError>) -> SysResult {
    match r {
        Ok(()) => Ok(0),
        Err(akuma_vfs_glue::FsError::NoSpace) => Err(Errno::ENOMEM),
        Err(akuma_vfs_glue::FsError::AlreadyExists) => Err(Errno::EBUSY),
        Err(_) => Err(Errno::EINVAL),
    }
}

/// `umount2(2)` from a process in a private namespace. `Some(result)` when the caller is in one.
///
/// While the namespace is building: `put_old` (where the first pivot attached the pre-pivot view) detaches that
/// view; any other mount point is removed; a path that is not a mount point is `Ok` (a lazy unmount of nothing).
/// After the final pivot the namespace is locked: a path that is not a mount point is still `Ok`, a real mount is
/// `EPERM`. `/` is always `Ok` and always a no-op (the stacked-pivot idiom's last step).
pub(super) fn umount(target: &str) -> Option<SysResult> {
    let proc = akuma_exec::process::current_process_shared()?;
    let box_id = proc.box_id;
    if !akuma_exec::box_registry::is_private_ns(box_id) {
        return None;
    }
    let target = absolute(target);
    // `pivot_root(".", ".")` stacks the old root on the new one and `umount2(".", MNT_DETACH)` pops it. Akuma never
    // stacked one, so there is nothing to pop, and the namespace's own root is never unmounted.
    if target == "/" {
        return Some(Ok(0));
    }
    let Some(tok) = akuma_exec::process::ns_creator() else {
        let is_mount = proc.namespace.mount.lock().list_mounts().iter().any(|m| m.path == target);
        return Some(if is_mount && target != "/" { Err(Errno::EPERM) } else { Ok(0) });
    };
    if akuma_exec::box_registry::private_ns_old_root(box_id).as_deref() == Some(target.as_str()) {
        return Some(match akuma_vfs_glue::private_ns_detach(tok) {
            Ok(()) => Ok(0),
            Err(_) => Err(Errno::EINVAL),
        });
    }
    Some(match akuma_vfs_glue::private_ns_unmount(tok, &target) {
        Ok(()) | Err(akuma_vfs_glue::FsError::NotFound) => Ok(0),
        Err(_) => Err(Errno::EINVAL),
    })
}

/// `pivot_root(new_root, put_old)`: only the builder of a private namespace, once.
pub(super) fn pivot_root(new_ptr: UserAddr, old_ptr: UserAddr) -> SysResult {
    let (new_ptr, old_ptr) = (new_ptr.get(), old_ptr.get());
    let Some(tok) = akuma_exec::process::ns_creator() else { return Err(Errno::EPERM) };
    let new_root = absolute(&copy_from_user_str(new_ptr, 4096)?);
    let put_old = absolute(&copy_from_user_str(old_ptr, 4096)?);
    // Linux: `put_old` is at or under `new_root`.
    let under = put_old == new_root || put_old.strip_prefix(new_root.trim_end_matches('/')).is_some_and(|r| r.starts_with('/'));
    if !under {
        return Err(Errno::EINVAL);
    }
    match akuma_vfs_glue::private_ns_pivot(tok, &new_root, &put_old) {
        Ok(()) => Ok(0),
        Err(akuma_vfs_glue::FsError::PermissionDenied) => Err(Errno::EPERM),
        Err(akuma_vfs_glue::FsError::NotADirectory) => Err(Errno::ENOTDIR),
        Err(_) => Err(Errno::ENOENT),
    }
}
