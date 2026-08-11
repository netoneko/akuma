use super::*;
use akuma_exec::mmu::user_access::copy_from_user_safe;

pub(super) fn sys_register_box(id: u64, name_ptr: u64, name_len: usize, root_ptr: u64, root_len: usize, primary_pid: u32) -> u64 {
    if !validate_user_ptr(name_ptr, name_len) { return EFAULT; }
    if !validate_user_ptr(root_ptr, root_len) { return EFAULT; }

    let mut name_buf = alloc::vec![0u8; name_len];
    let mut root_buf = alloc::vec![0u8; root_len];

    if unsafe { copy_from_user_safe(name_buf.as_mut_ptr(), name_ptr as *const u8, name_len).is_err() } {
        return EFAULT;
    }
    if unsafe { copy_from_user_safe(root_buf.as_mut_ptr(), root_ptr as *const u8, root_len).is_err() } {
        return EFAULT;
    }

    let name = core::str::from_utf8(&name_buf).unwrap_or("unknown");
    let root = core::str::from_utf8(&root_buf).unwrap_or("/");
    let (caller_box, caller_pid) = caller_box_and_pid();

    // Normalize before deciding anything: `root` becomes the box's `SubdirFs`
    // jail, and an unresolved `..` would sail through the containment check
    // below and then resolve on disk to somewhere else entirely.
    let root = crate::vfs::canonicalize_path(root);

    // Registration is a privilege boundary — the caller is naming a filesystem
    // subtree that anything spawned into the box will see as `/`. Without this
    // a boxed process mints a box rooted at `/` and spawns into it.
    let registry = akuma_exec::process::registry_snapshot();
    let parent_box_id = match akuma_exec::process::box_access::can_register_box(
        &registry, caller_box, caller_pid, id, &root,
    ) {
        Ok(parent) => parent,
        Err(reason) => {
            if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
                crate::safe_print!(192, "[register_box] denied box={} root={} caller_box={}: {}\n",
                    id, root, caller_box, reason);
            }
            return EPERM;
        }
    };

    akuma_exec::process::register_box(akuma_exec::process::BoxInfo {
        id,
        name: String::from(name),
        root_dir: root.clone(),
        creator_pid: caller_pid,
        primary_pid,
        parent_box_id,
    });

    crate::vfs::create_box_namespace(id, &root);

    0
}

pub(super) fn sys_kill_box(box_id: u64) -> u64 {
    let (caller_box, caller_pid) = caller_box_and_pid();
    let registry = akuma_exec::process::registry_snapshot();
    if !akuma_exec::process::box_access::can_kill_box(&registry, caller_box, box_id, caller_pid) {
        return EPERM;
    }

    // kill_box only fails when the box id is unknown (or is box 0, which is
    // never killable). Drop the namespace only once the kill has taken, so a
    // rejected call cannot strand a live box without its mounts.
    if akuma_exec::process::kill_box(box_id).is_err() {
        return ESRCH;
    }
    crate::vfs::remove_box_namespace(box_id);
    0
}

pub(super) fn sys_reattach(pid: u32) -> u64 {

    // reattach_process only fails when the target pid does not exist.
    if akuma_exec::process::reattach_process(pid).is_ok() { 0 } else { ESRCH }

}

pub(super) fn sys_mount(_source_ptr: u64, target_ptr: u64, fstype_ptr: u64, _flags: u64, _data_ptr: u64) -> u64 {
    let target = match copy_from_user_str(target_ptr, 256) {
        Ok(s) => s,
        Err(e) => return e,
    };
    let fstype = match copy_from_user_str(fstype_ptr, 64) {
        Ok(s) => s,
        Err(e) => return e,
    };

    let fs: alloc::sync::Arc<dyn crate::vfs::Filesystem> = match fstype.as_str() {
        "proc" => alloc::sync::Arc::new(crate::vfs::proc::ProcFilesystem::new()),
        "tmpfs" => alloc::sync::Arc::new(akuma_vfs::MemoryFilesystem::new()),
        _ => {
            if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
                crate::safe_print!(128, "[mount] unsupported fstype: {}\n", fstype);
            }
            return ENODEV;
        }
    };

    // `MountNamespace::resolve` compares mount points literally, so an
    // un-normalized target ("/proc/", "/a/../proc") would register a mount point
    // no lookup can ever match — and would slip past the duplicate check that
    // keeps a box from shadowing its own root.
    let target = crate::vfs::canonicalize_path(&target);

    if let Some(proc) = akuma_exec::process::current_process_shared() {
        if proc.box_id == 0 {
            match crate::vfs::mount(&target, fs) {
                Ok(()) => 0,
                Err(_) => EINVAL,
            }
        } else {
            match proc.namespace.mount.lock().mount(&target, fs) {
                Ok(()) => 0,
                Err(_) => EINVAL,
            }
        }
    } else {
        EPERM
    }
}

pub(super) fn sys_umount2(target_ptr: u64, _flags: i32) -> u64 {
    let target = match copy_from_user_str(target_ptr, 256) {
        Ok(s) => s,
        Err(e) => return e,
    };
    let target = crate::vfs::canonicalize_path(&target);

    if let Some(proc) = akuma_exec::process::current_process_shared() {
        if proc.box_id == 0 {
            EPERM
        } else if target == "/" {
            // "/" is the box's `SubdirFs` jail root. Unmounting it empties the
            // namespace, and `with_fs` then falls back to the GLOBAL mount table
            // — i.e. the whole host filesystem, read and write. A box may drop a
            // mount it added (its /proc, a tmpfs); never the floor it stands on.
            EPERM
        } else {
            match proc.namespace.mount.lock().unmount(&target) {
                Ok(()) => 0,
                Err(_) => EINVAL,
            }
        }
    } else {
        EPERM
    }
}

/// Build an `OverlayFs` from a `lowerdir=…,upperdir=…` option string.
///
/// Every layer becomes a `SubdirFs` over the **global** root filesystem, not the
/// box's — the layer store lives outside any container, and the box is only ever
/// handed the union. Each directory has to exist and be a directory: a typo that
/// silently produced an empty layer would surface much later as a missing file
/// inside the container, with nothing pointing back here.
#[cfg(feature = "sc-containers")]
fn build_overlay(data_ptr: u64) -> Result<alloc::sync::Arc<dyn crate::vfs::Filesystem>, u64> {
    use akuma_isolation::overlay_fs::{OverlayFs, parse_options};

    if data_ptr == 0 {
        return Err(EINVAL);
    }
    let data = copy_from_user_str(data_ptr, 4096)?;

    let opts = parse_options(&data).map_err(|reason| {
        if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
            crate::safe_print!(192, "[mount] overlay options rejected: {}\n", reason);
        }
        EINVAL
    })?;

    let root_fs = crate::vfs::get_root_fs().ok_or(ENODEV)?;

    let mut dirs = alloc::vec::Vec::with_capacity(opts.lowerdirs.len() + 1);
    dirs.push(crate::vfs::canonicalize_path(&opts.upperdir));
    for lower in &opts.lowerdirs {
        dirs.push(crate::vfs::canonicalize_path(lower));
    }

    let mut layers = alloc::vec::Vec::with_capacity(dirs.len());
    for dir in &dirs {
        if !crate::vfs::metadata(dir).is_ok_and(|m| m.is_dir) {
            if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
                crate::safe_print!(192, "[mount] overlay layer missing: {}\n", dir);
            }
            return Err(ENOENT);
        }
        layers.push(alloc::sync::Arc::new(
            akuma_isolation::subdir_fs::SubdirFs::new(root_fs.clone(), dir),
        ) as alloc::sync::Arc<dyn crate::vfs::Filesystem>);
    }

    let upper = layers.remove(0);
    Ok(alloc::sync::Arc::new(OverlayFs::new(upper, layers)))
}

pub(super) fn sys_mount_in_ns(box_id: u64, target_ptr: u64, target_len: usize, fstype_ptr: u64, fstype_len: usize, data_ptr: u64) -> u64 {
    let caller_box = akuma_exec::process::current_process_shared()
        .map_or(0, |p| p.box_id);
    if caller_box != 0 {
        return EPERM;
    }

    if !validate_user_ptr(target_ptr, target_len) { return EFAULT; }
    if !validate_user_ptr(fstype_ptr, fstype_len) { return EFAULT; }

    let mut target_buf = alloc::vec![0u8; target_len];
    let mut fstype_buf = alloc::vec![0u8; fstype_len];
    
    if unsafe { copy_from_user_safe(target_buf.as_mut_ptr(), target_ptr as *const u8, target_len).is_err() } {
        return EFAULT;
    }
    if unsafe { copy_from_user_safe(fstype_buf.as_mut_ptr(), fstype_ptr as *const u8, fstype_len).is_err() } {
        return EFAULT;
    }

    // Same reason as sys_mount: mount points are matched literally.
    let target = crate::vfs::canonicalize_path(core::str::from_utf8(&target_buf).unwrap_or(""));
    let fstype = core::str::from_utf8(&fstype_buf).unwrap_or("");

    let fs: alloc::sync::Arc<dyn crate::vfs::Filesystem> = match fstype {
        "proc" => alloc::sync::Arc::new(crate::vfs::proc::ProcFilesystem::new()),
        "tmpfs" => alloc::sync::Arc::new(akuma_vfs::MemoryFilesystem::new()),
        "overlay" => match build_overlay(data_ptr) {
            Ok(fs) => fs,
            Err(e) => return e,
        },
        _ => return ENODEV,
    };

    // A box's namespace already has its `SubdirFs` jail at "/", and an overlay
    // root replaces that jail rather than stacking on it — `mount` would reject
    // the duplicate. Anywhere else, a duplicate really is a caller error.
    if target == "/" {
        return match crate::vfs::mount_replace_in_namespace(box_id, &target, fs) {
            Ok(()) => 0,
            Err(_) => EINVAL,
        };
    }

    match crate::vfs::mount_in_namespace(box_id, &target, fs) {
        Ok(()) => 0,
        Err(_) => EINVAL,
    }
}
