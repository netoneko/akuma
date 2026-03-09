use super::*;

pub(super) fn sys_register_box(id: u64, name_ptr: u64, name_len: usize, root_ptr: u64, root_len: usize, primary_pid: u32) -> u64 {
    if !validate_user_ptr(name_ptr, name_len) { return EFAULT; }
    if !validate_user_ptr(root_ptr, root_len) { return EFAULT; }
    let name = unsafe { core::str::from_utf8(core::slice::from_raw_parts(name_ptr as *const u8, name_len)).unwrap_or("unknown") };
    let root = unsafe { core::str::from_utf8(core::slice::from_raw_parts(root_ptr as *const u8, root_len)).unwrap_or("/") };
    let creator_pid = akuma_exec::process::read_current_pid().unwrap_or(0);

    akuma_exec::process::register_box(akuma_exec::process::BoxInfo {
        id,
        name: String::from(name),
        root_dir: String::from(root),
        creator_pid,
        primary_pid,
    });

    crate::vfs::create_box_namespace(id, root);

    0
}

pub(super) fn sys_kill_box(box_id: u64) -> u64 {
    crate::vfs::remove_box_namespace(box_id);

    if akuma_exec::process::kill_box(box_id).is_ok() { 0 } else { !0u64 }
}

pub(super) fn sys_reattach(pid: u32) -> u64 {

    if akuma_exec::process::reattach_process(pid).is_ok() { 0 } else { !0u64 }

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

    if let Some(proc) = akuma_exec::process::current_process() {
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

    if let Some(proc) = akuma_exec::process::current_process() {
        if proc.box_id == 0 {
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

pub(super) fn sys_mount_in_ns(box_id: u64, target_ptr: u64, target_len: usize, fstype_ptr: u64, fstype_len: usize) -> u64 {
    let caller_box = akuma_exec::process::current_process()
        .map(|p| p.box_id)
        .unwrap_or(0);
    if caller_box != 0 {
        return EPERM;
    }

    if !validate_user_ptr(target_ptr, target_len) { return EFAULT; }
    if !validate_user_ptr(fstype_ptr, fstype_len) { return EFAULT; }

    let target = unsafe {
        core::str::from_utf8(core::slice::from_raw_parts(target_ptr as *const u8, target_len))
            .unwrap_or("")
    };
    let fstype = unsafe {
        core::str::from_utf8(core::slice::from_raw_parts(fstype_ptr as *const u8, fstype_len))
            .unwrap_or("")
    };

    let fs: alloc::sync::Arc<dyn crate::vfs::Filesystem> = match fstype {
        "proc" => alloc::sync::Arc::new(crate::vfs::proc::ProcFilesystem::new()),
        "tmpfs" => alloc::sync::Arc::new(akuma_vfs::MemoryFilesystem::new()),
        _ => return ENODEV,
    };

    match crate::vfs::mount_in_namespace(box_id, target, fs) {
        Ok(()) => 0,
        Err(_) => EINVAL,
    }
}
