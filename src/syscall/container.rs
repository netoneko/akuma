use super::*;

pub(super) fn sys_register_box(id: u64, name_ptr: u64, name_len: usize, root_ptr: u64, root_len: usize, primary_pid: u32) -> u64 {
    if !validate_user_ptr(name_ptr, name_len) { return EFAULT; }
    if !validate_user_ptr(root_ptr, root_len) { return EFAULT; }

    let mut name_buf = alloc::vec![0u8; name_len];
    let mut root_buf = alloc::vec![0u8; root_len];

    if copy_from_user(&mut name_buf, name_ptr).is_err() {
        return EFAULT;
    }
    if copy_from_user(&mut root_buf, root_ptr).is_err() {
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

/// Soft terminal reset, written to a displaced holder's own channel right
/// before it's killed: exit alternate screen (`?1049l`), show the cursor
/// (`?25h`), clear character attributes (`0m`), DECSTR soft reset (`!p`), then
/// a fresh line. Same idea as `tput reset` / what `screen`/`tmux` send a
/// terminal on a forced detach — without it, whatever the grabbed app left
/// the terminal in (raw mode, alt-screen, hidden cursor) survives the
/// connection closing, because that state lives in the *client's* terminal
/// emulator, not anywhere this kernel can reach after the fact.
const TERM_RESET_SEQUENCE: &[u8] = b"\r\n\x1b[?1049l\x1b[?25h\x1b[0m\x1b[!p";

pub(super) fn sys_reattach(pid: u32, force: u32) -> u64 {
    match akuma_exec::process::reattach_process(pid, force != 0) {
        Ok(displaced) => {
            if let Some(previous_holder) = displaced {
                // Give the human on the other end a sane-looking terminal
                // before their connection disappears out from under them,
                // then let them go through the real signal-delivery path —
                // a disposition-aware SIGTERM and a full `exit_group` cleanup,
                // not the crate-level hard-stop used for actual force-kills
                // elsewhere — so their own wait loop exits the way a normal
                // process exit would, not as if it had been yanked.
                if let Some(proc) = akuma_exec::process::lookup_process_shared(previous_holder)
                    && let Some(ref channel) = proc.channel {
                        channel.write(TERM_RESET_SEQUENCE);
                    }
                super::proc::sys_kill(previous_holder, 15 /* SIGTERM */);
            }
            // Mirror what `screen`/`tmux` do on attach: hand the target the
            // caller's real terminal size and nudge it to repaint. Neither
            // step is required for correctness (a full-screen app that never
            // redraws just looks stale until its next natural refresh), but
            // without them every ncurses-style app looks frozen/misdrawn
            // after a `box grab` until something else prompts it to redraw.
            if let Some(caller) = akuma_exec::process::current_process_shared() {
                let (width, height) = {
                    let ts = caller.terminal_state.lock();
                    (ts.term_width, ts.term_height)
                };
                if let Some(target) = akuma_exec::process::lookup_process_shared(pid) {
                    let mut ts = target.terminal_state.lock();
                    ts.term_width = width;
                    ts.term_height = height;
                }
            }
            // SIGWINCH (28): the standard "your window changed, requery and
            // redraw" signal. Its POSIX default action is Ignore (confirmed
            // absent from `signal_is_fatal_default`), so a target with no
            // handler installed just drops it — this can never kill a plain
            // `cat`. Goes through the same disposition-aware path `kill(2)`
            // uses, not a raw pend, so SA_RESTART and everything else already
            // hardened there applies here too.
            super::proc::sys_kill(pid, 28);
            0
        }
        // `box grab` (screen -d-style) distinguishes "already attached" —
        // pass `force` to detach the previous holder — from every other
        // failure (unknown pid, permission denied), which the syscall
        // boundary has never distinguished from each other.
        Err("Already attached") => EBUSY,
        Err(_) => ESRCH,
    }
}

/// A box's mount namespace is composed **entirely from outside**, by box 0,
/// before anything runs in it (`MOUNT_IN_NS`). A boxed process may not change
/// it: not add a mount, not remove one, not re-root itself.
///
/// The reason is that a mount table is the box's whole view of the filesystem.
/// Anything a box can mount, it can mount *over* — its own `/proc`, a directory
/// its supervisor is watching, the path another process resolves against — and
/// a box that can shadow paths inside itself is a box whose isolation is
/// described by whatever it did last, not by what its creator set up. Composing
/// the namespace once, from the outside, keeps that description fixed.
///
/// It also means a container cannot build a container: assembling an OCI root
/// needs an overlay mount, and no box can mount at all. Nested **boxes** still
/// exist — they are process/network grouping — but nested OCI images do not.
#[cfg(feature = "sc-containers")]
fn caller_may_mount() -> bool {
    akuma_exec::process::current_process_shared().is_none_or(|p| p.box_id == 0)
}

/// Map a mount-table error to its Linux errno. The arms used to fold
/// everything into `EINVAL`, which made `mount` report nonsense for the two
/// cases scripts actually branch on: table full (`ENOMEM`) and target already
/// mounted (`EBUSY`) — `docs/archive/MOUNT_MISSING_SYSCALLS.md` §3.11.
fn mount_errno(e: crate::vfs::FsError) -> u64 {
    match e {
        crate::vfs::FsError::NoSpace => ENOMEM,
        crate::vfs::FsError::AlreadyExists => EBUSY,
        crate::vfs::FsError::NotFound => ENOENT,
        crate::vfs::FsError::NotADirectory => ENOTDIR,
        crate::vfs::FsError::ReadOnly => EROFS,
        _ => EINVAL,
    }
}

pub(super) fn sys_mount(
    source_ptr: u64,
    target_ptr: u64,
    fstype_ptr: u64,
    flags: u64,
    _data_ptr: u64,
) -> SysResult {
    if !caller_may_mount() {
        if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
            crate::safe_print!(128, "[mount] denied: boxed processes may not mount\n");
        }
        return Err(EPERM);
    }

    let target = copy_from_user_str(target_ptr, 256)?;
    let fstype = copy_from_user_str(fstype_ptr, 64)?;
    let source = if source_ptr != 0 {
        copy_from_user_str(source_ptr, 64)?
    } else {
        String::new()
    };

    // `MountNamespace::resolve` compares mount points literally, so an
    // un-normalized target ("/proc/", "/a/../proc") would register a mount point
    // no lookup can ever match — and would slip past the duplicate check that
    // keeps a box from shadowing its own root.
    let target = crate::vfs::canonicalize_path(&target);

    // Only box 0 reaches here, so this is always the global mount table.
    if flags & akuma_vfs::MS_REMOUNT != 0 {
        // Remount only flips stored flags; `fs`/`source` args are advisory.
        // Linux requires the target to already be a mount point.
        return match crate::vfs::remount(&target, flags) {
            Ok(()) => Ok(0),
            Err(e) => Err(mount_errno(e)),
        };
    }

    // A mount needs a directory to land on (Linux: `ENOTDIR`). Boot mounts
    // bypass this arm entirely, so this never gates `/` or `/proc` at boot.
    match crate::vfs::metadata(&target) {
        Ok(m) if m.is_dir => {}
        Ok(_) => return Err(ENOTDIR),
        Err(e) => return Err(mount_errno(e)),
    }

    let fs: alloc::sync::Arc<dyn crate::vfs::Filesystem> = match fstype.as_str() {
        "proc" => alloc::sync::Arc::new(crate::vfs::proc::ProcFilesystem::new()),
        "tmpfs" => alloc::sync::Arc::new(akuma_vfs::MemoryFilesystem::new()),
        "ext2" => {
            // The source must name a registered block device (vdb, /dev/vdb, …).
            // The boot disk (vda) is deliberately mountable here too — it is
            // already mounted at `/`, so the duplicate check is the guard.
            let Some(idx) = crate::block::device_index_by_name(&source) else {
                if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
                    crate::safe_print!(128, "[mount] no such device: {}\n", source);
                }
                return Err(ENODEV);
            };
            // Runtime data disks get a small, fixed cache budget: the global
            // cap belongs to the root filesystem, and the cache never shrinks
            // (`docs/archive/MOUNT_MISSING_SYSCALLS.md` §5 Tier B).
            const DATA_DISK_CACHE_BYTES: usize = 16 * 1024 * 1024;
            match crate::vfs::ext2::mount_device(idx, Some(DATA_DISK_CACHE_BYTES)) {
                Ok(fs) => fs,
                Err(_) => return Err(ENODEV), // no ext2 magic on that device
            }
        }
        _ => {
            if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
                crate::safe_print!(128, "[mount] unsupported fstype: {}\n", fstype);
            }
            return Err(ENODEV);
        }
    };

    let recorded_source = if source.is_empty() {
        fstype.as_str()
    } else {
        source.as_str()
    };
    match crate::vfs::mount_with(&target, Some(recorded_source), flags, fs) {
        Ok(()) => Ok(0),
        Err(e) => Err(mount_errno(e)),
    }
}

/// Unmount a global mount. Real since 2026-08-24 (before, every caller got
/// `EPERM`, host included).
///
/// Box policy is unchanged — see [`caller_may_mount`]: a box may not take
/// mounts away any more than it may add them; the specific hazard for `/` is
/// that emptying a box's namespace makes `with_fs` fall back to the GLOBAL
/// mount table, handing the box the whole host filesystem. Two guards keep
/// that invariant intact here:
///
/// - the **global** `/` is never unmountable (`EBUSY`, like Linux);
/// - this arm only ever touches the global table — a box's namespace mounts
///   are composed from outside via `MOUNT_IN_NS` and torn down with the box.
pub(super) fn sys_umount2(target_ptr: u64, _flags: i32) -> SysResult {
    if !caller_may_mount() {
        if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
            crate::safe_print!(128, "[umount2] denied: boxed processes may not unmount\n");
        }
        return Err(EPERM);
    }
    let target = copy_from_user_str(target_ptr, 256)?;
    let target = crate::vfs::canonicalize_path(&target);
    if target == "/" {
        return Err(EBUSY);
    }
    match crate::vfs::unmount(&target) {
        Ok(()) => Ok(0),
        Err(e) => Err(mount_errno(e)),
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

pub(super) fn sys_mount_in_ns(box_id: u64, target_ptr: u64, target_len: usize, fstype_ptr: u64, fstype_len: usize, data_ptr: u64) -> SysResult {
    let caller_box = akuma_exec::process::current_process_shared()
        .map_or(0, |p| p.box_id);
    if caller_box != 0 {
        return Err(EPERM);
    }

    if !validate_user_ptr(target_ptr, target_len) { return Err(EFAULT); }
    if !validate_user_ptr(fstype_ptr, fstype_len) { return Err(EFAULT); }

    let mut target_buf = alloc::vec![0u8; target_len];
    let mut fstype_buf = alloc::vec![0u8; fstype_len];
    
    if copy_from_user(&mut target_buf, target_ptr).is_err() {
        return Err(EFAULT);
    }
    if copy_from_user(&mut fstype_buf, fstype_ptr).is_err() {
        return Err(EFAULT);
    }

    // Same reason as sys_mount: mount points are matched literally.
    let target = crate::vfs::canonicalize_path(core::str::from_utf8(&target_buf).unwrap_or(""));
    let fstype = core::str::from_utf8(&fstype_buf).unwrap_or("");

    let fs: alloc::sync::Arc<dyn crate::vfs::Filesystem> = match fstype {
        "proc" => alloc::sync::Arc::new(crate::vfs::proc::ProcFilesystem::new()),
        "tmpfs" => alloc::sync::Arc::new(akuma_vfs::MemoryFilesystem::new()),
        "overlay" => build_overlay(data_ptr)?,
        _ => return Err(ENODEV),
    };

    // A box's namespace already has its `SubdirFs` jail at "/", and an overlay
    // root replaces that jail rather than stacking on it — `mount` would reject
    // the duplicate. Anywhere else, a duplicate really is a caller error.
    if target == "/" {
        // Re-rooting a box that is already running would move the filesystem
        // under processes that have paths and cwds resolved against the old one.
        // The legitimate caller does this between REGISTER_BOX and its first
        // spawn, so requiring the box to be empty costs nothing and takes the
        // whole class of live-swap games off the table. `replace_box_root` then
        // enforces that the root is still the pristine jail.
        if akuma_exec::process::list_processes().iter().any(|p| p.box_id == box_id) {
            if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
                crate::safe_print!(160, "[mount] refusing to re-root live box {}\n", box_id);
            }
            return Err(EPERM);
        }
        return match crate::vfs::replace_box_root(box_id, fs) {
            Ok(()) => Ok(0),
            Err(crate::vfs::FsError::PermissionDenied) => Err(EPERM),
            Err(_) => Err(EINVAL),
        };
    }

    match crate::vfs::mount_in_namespace(box_id, &target, fs) {
        Ok(()) => Ok(0),
        Err(_) => Err(EINVAL),
    }
}
