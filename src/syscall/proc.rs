use super::*;
use akuma_net::socket::libc_errno;

fn encode_wait_status(code: i32) -> u32 {
    if code < 0 {
        let sig = (-code) as u32 & 0x7F;
        sig
    } else {
        ((code as u32) & 0xFF) << 8
    }
}

pub(super) fn sys_set_tpidr_el0(address: u64) -> u64 {
    unsafe {
        core::arch::asm!("msr tpidr_el0, {}", "isb", in(reg) address);
    }
    0
}

pub(super) fn sys_setpgid(pid: u32, pgid: u32) -> u64 {
    let target_pid = if pid == 0 {
        match akuma_exec::process::read_current_pid() { Some(p) => p, None => return !0u64 }
    } else {
        pid
    };

    let target_pgid = if pgid == 0 { target_pid } else { pgid };

    if let Some(proc) = akuma_exec::process::lookup_process(target_pid) {
        if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
            crate::safe_print!(128, "[syscall] setpgid(pid={}, pgid={}): old={}, new={}\n", target_pid, pgid, proc.pgid, target_pgid);
        }
        proc.pgid = target_pgid;
        0
    } else {
        ENOENT
    }
}

pub(super) fn sys_getpgid(pid: u32) -> u64 {
    let target_pid = if pid == 0 {
        match akuma_exec::process::read_current_pid() { 
            Some(p) => p, 
            None => {
                let tid = akuma_exec::threading::current_thread_id();
                if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
                    crate::safe_print!(128, "[syscall] getpgid(0) kernel fallback: returning TID {}\n", tid);
                }
                return tid as u64;
            }
        }
    } else {
        pid
    };

    if let Some(proc) = akuma_exec::process::lookup_process(target_pid) {
        if crate::config::SYSCALL_DEBUG_INFO_ENABLED && pid == 0 {
            crate::safe_print!(128, "[syscall] getpgid(0) for PID {}: returning PGID {}\n", target_pid, proc.pgid);
        }
        proc.pgid as u64
    } else {
        if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
            crate::safe_print!(128, "[syscall] getpgid({}) not found: returning TID fallback {}\n", target_pid, target_pid);
        }
        target_pid as u64
    }
}

pub(super) fn sys_setsid() -> u64 {
    if let Some(proc) = akuma_exec::process::current_process() {
        proc.pgid = proc.pid;
        proc.pid as u64
    } else {
        !0u64
    }
}

pub(super) fn sys_uname(buf: u64) -> u64 {
    const FIELD_LEN: usize = 65;
    if !validate_user_ptr(buf, FIELD_LEN * 6) { return EFAULT; }

    let mut kernel_buf = [0u8; FIELD_LEN * 6];

    fn write_field(base: &mut [u8], offset: usize, value: &[u8]) {
        let start = offset * FIELD_LEN;
        let len = value.len().min(FIELD_LEN - 1);
        base[start..start + len].copy_from_slice(&value[..len]);
    }

    write_field(&mut kernel_buf, 0, b"Akuma");
    write_field(&mut kernel_buf, 1, b"akuma");
    write_field(&mut kernel_buf, 2, b"0.1.0");
    write_field(&mut kernel_buf, 3, b"Akuma OS");
    write_field(&mut kernel_buf, 4, b"aarch64");
    write_field(&mut kernel_buf, 5, b"(none)");

    if unsafe { copy_to_user_safe(buf as *mut u8, kernel_buf.as_ptr(), kernel_buf.len()).is_err() } {
        return EFAULT;
    }
    0
}

pub(super) fn sys_set_tid_address(tidptr: u64) -> u64 {
    if let Some(proc) = akuma_exec::process::current_process() {
        proc.clear_child_tid = tidptr;
        return proc.pid as u64;
    }
    1
}

pub(super) fn sys_set_robust_list(head: u64, len: usize) -> u64 {
    if len != 24 { return EINVAL; }
    if let Some(proc) = akuma_exec::process::current_process() {
        proc.robust_list_head = head;
        proc.robust_list_len = len;
        return 0;
    }
    ENOSYS
}

pub(super) fn sys_exit(code: i32) -> u64 {
    if let Some(proc) = akuma_exec::process::current_process() {
        if crate::config::SYSCALL_DEBUG_NET_ENABLED {
            let elapsed_us = crate::timer::uptime_us().saturating_sub(proc.start_time_us);
            let secs = elapsed_us / 1_000_000;
            let frac = (elapsed_us % 1_000_000) / 10_000;
            crate::tprint!(128, "[exit] tid={} pid={} name={} code={} after {}.{:02}s\n", 
                akuma_exec::threading::current_thread_id(), proc.pid, proc.name, code, secs, frac);
        }
        proc.exited = true;
        proc.exit_code = code;
        proc.state = akuma_exec::process::ProcessState::Zombie(code);
    }
    code as u64
}

pub(super) fn sys_exit_group(code: i32) -> u64 {
    if let Some(proc) = akuma_exec::process::current_process() {
        if crate::config::SYSCALL_DEBUG_NET_ENABLED {
            let elapsed_us = crate::timer::uptime_us().saturating_sub(proc.start_time_us);
            let secs = elapsed_us / 1_000_000;
            let frac = (elapsed_us % 1_000_000) / 10_000;
            crate::tprint!(128, "[exit_group] pid={} name={} code={} after {}.{:02}s\n", 
                proc.pid, proc.name, code, secs, frac);
        }
        let l0_phys = proc.address_space.l0_phys();
        proc.exited = true;
        proc.exit_code = code;
        proc.state = akuma_exec::process::ProcessState::Zombie(code);
        akuma_exec::process::kill_thread_group(proc.pid, l0_phys);
    }
    code as u64
}

pub(super) fn sys_clone(flags: u64, stack: u64, parent_tid: u64, tls: u64, child_tid: u64) -> u64 {
    const CLONE_VM: u64 = 0x100;
    const CLONE_THREAD: u64 = 0x10000;
    const CLONE_VFORK: u64 = 0x4000;

    if crate::config::SYSCALL_DEBUG_INFO_ENABLED || crate::config::SYSCALL_DEBUG_NET_ENABLED {
        crate::tprint!(128, "[clone] flags=0x{:x} stack=0x{:x}\n", flags, stack);
    }

    if flags & CLONE_THREAD != 0 && flags & CLONE_VM != 0 {
        match akuma_exec::process::clone_thread(stack, tls, parent_tid, child_tid) {
            Ok(tid) => {
                if crate::config::SYSCALL_DEBUG_NET_ENABLED {
                    crate::tprint!(64, "[clone] new thread TID={}\n", tid);
                }
                return tid as u64;
            }
            Err(e) => {
                crate::safe_print!(128, "[syscall] clone_thread failed: {}\n", e);
                return EAGAIN;
            }
        }
    }

    if flags & CLONE_VFORK != 0 || flags & 0x11 == 0x11 {
        let parent_proc = match akuma_exec::process::current_process() {
            Some(p) => p,
            None => return !0u64,
        };

        let child_pid = akuma_exec::process::allocate_pid();

        if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
            crate::safe_print!(128, "[syscall] clone: forking PID {} -> {} (vfork-like)\n", parent_proc.pid, child_pid);
        }

        match akuma_exec::process::fork_process(child_pid, stack) {
            Ok(new_pid) => {
                return new_pid as u64;
            },
            Err(e) => {
                crate::safe_print!(128, "[syscall] clone: fork failed: {}\n", e);
                return !0u64;
            }
        }
    }

    if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
        crate::safe_print!(128, "[syscall] clone: flags not supported, returning ENOSYS\n");
    }
    ENOSYS
}

pub(super) fn sys_clone3(cl_args_ptr: u64, size: usize) -> u64 {
    #[repr(C)]
    #[derive(Default)]
    struct CloneArgs {
        flags: u64,
        pidfd: u64,
        child_tid: u64,
        parent_tid: u64,
        exit_signal: u64,
        stack: u64,
        stack_size: u64,
        tls: u64,
    }

    let struct_size = size.min(core::mem::size_of::<CloneArgs>());
    if !validate_user_ptr(cl_args_ptr, struct_size) {
        return EFAULT;
    }

    let mut cl_args = CloneArgs::default();
    if unsafe { copy_from_user_safe(&mut cl_args as *mut CloneArgs as *mut u8, cl_args_ptr as *const u8, struct_size).is_err() } {
        return EFAULT;
    }

    let flags = cl_args.flags | cl_args.exit_signal;
    let stack = if cl_args.stack != 0 {
        cl_args.stack + cl_args.stack_size
    } else {
        0
    };

    if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
        crate::tprint!(128, "[syscall] clone3(flags=0x{:x}, stack=0x{:x})\n", flags, stack);
    }

    sys_clone(flags, stack, cl_args.parent_tid, cl_args.tls, cl_args.child_tid)
}

pub(super) fn sys_execve(path_ptr: u64, argv_ptr: u64, envp_ptr: u64) -> u64 {
    if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
        crate::tprint!(128, "[syscall] execve(path_ptr=0x{:x}, argv_ptr=0x{:x}, envp_ptr=0x{:x})\n", path_ptr, argv_ptr, envp_ptr);
    }
    let path = match copy_from_user_str(path_ptr, 1024) {
        Ok(p) => p,
        Err(e) => {
            crate::safe_print!(64, "[syscall] execve: path copy failed with {}\n", e as i64);
            return e;
        },
    };

    let resolved_path = if path.starts_with('/') {
        path
    } else {
        if let Some(proc) = akuma_exec::process::current_process() {
            crate::vfs::resolve_path(&proc.cwd, &path)
        } else {
            path
        }
    };
    let resolved_path = crate::vfs::resolve_symlinks(&resolved_path);

    let mut args = Vec::new();
    if argv_ptr != 0 {
        let mut i = 0;
        loop {
            if !validate_user_ptr(argv_ptr + i * 8, 8) { break; }
            let mut str_ptr: u64 = 0;
            if unsafe { copy_from_user_safe(&mut str_ptr as *mut u64 as *mut u8, (argv_ptr + i * 8) as *const u8, 8).is_err() } {
                break;
            }
            if str_ptr == 0 { break; }
            if let Ok(s) = copy_from_user_str(str_ptr, 1024) {
                args.push(s);
            } else {
                if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
                    crate::safe_print!(64, "[syscall] execve: failed to copy argv[{}]\n", i);
                }
                break;
            }
            i += 1;
        }
    }

    let env = parse_argv_array(envp_ptr);

    let pid = akuma_exec::process::read_current_pid().unwrap_or(0);
    crate::tprint!(192, "[syscall] execve(path=\"{}\", args={:?}) PID {}\n", resolved_path, args, pid);

    do_execve(resolved_path, args, env)
}

fn do_execve(resolved_path: String, args: Vec<String>, env: Vec<String>) -> u64 {
    let file_data = match crate::fs::read_file(&resolved_path) {
        Ok(data) => Some(data),
        Err(crate::vfs::FsError::Internal) => None,
        Err(_) => {
            crate::safe_print!(128, "[syscall] execve: failed to read {}\n", resolved_path);
            return ENOENT;
        }
    };

    if let Some(ref data) = file_data {
        if data.len() >= 2 && data[0] == b'#' && data[1] == b'!' {
            return exec_shebang(&resolved_path, data, args, env);
        }
    }

    let proc = match akuma_exec::process::current_process() {
        Some(p) => p,
        None => return !0u64,
    };

    let closed_fds = proc.close_cloexec_fds();
    for (_fd, entry) in closed_fds {
        match entry {
            akuma_exec::process::FileDescriptor::PipeWrite(pipe_id) => super::pipe::pipe_close_write(pipe_id),
            akuma_exec::process::FileDescriptor::PipeRead(pipe_id) => super::pipe::pipe_close_read(pipe_id),
            akuma_exec::process::FileDescriptor::Socket(idx) => akuma_net::socket::remove_socket(idx),
            akuma_exec::process::FileDescriptor::ChildStdout(child_pid) => {
                akuma_exec::process::remove_child_channel(child_pid);
            }
            akuma_exec::process::FileDescriptor::EventFd(efd_id) => super::eventfd::eventfd_close(efd_id),
            akuma_exec::process::FileDescriptor::EpollFd(epoll_id) => super::poll::epoll_destroy(epoll_id),
            _ => {}
        }
    }

    let replace_result = if let Some(ref data) = file_data {
        proc.replace_image(data, &args, &env)
    } else {
        let file_size = match crate::vfs::file_size(&resolved_path) {
            Ok(sz) => sz as usize,
            Err(_) => {
                crate::safe_print!(128, "[syscall] execve: failed to stat {}\n", resolved_path);
                return ENOENT;
            }
        };
        proc.replace_image_from_path(&resolved_path, file_size, &args, &env)
    };

    if let Err(e) = replace_result {
        crate::safe_print!(128, "[syscall] execve: replace_image failed for {}: {}\n", resolved_path, e);
        return ENOENT;
    }

    proc.name = resolved_path.clone();

    if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
        crate::safe_print!(128, "[syscall] execve: replaced image for PID {} with {}\n", proc.pid, resolved_path);
    }

    proc.address_space.activate();

    unsafe {
        akuma_exec::process::enter_user_mode(&proc.context);
    }
}

fn exec_shebang(script_path: &str, file_data: &[u8], original_args: Vec<String>, env: Vec<String>) -> u64 {
    let line_end = file_data.iter().position(|&b| b == b'\n').unwrap_or(file_data.len().min(256));
    let shebang_line = match core::str::from_utf8(&file_data[2..line_end]) {
        Ok(s) => s.trim(),
        Err(_) => {
            crate::safe_print!(128, "[syscall] execve: invalid shebang in {}\n", script_path);
            return ENOENT;
        }
    };

    if shebang_line.is_empty() {
        crate::safe_print!(128, "[syscall] execve: empty shebang in {}\n", script_path);
        return ENOENT;
    }

    let (interpreter, shebang_arg) = match shebang_line.split_once(char::is_whitespace) {
        Some((interp, arg)) => (interp.trim(), Some(arg.trim())),
        None => (shebang_line, None),
    };

    let interpreter = crate::vfs::resolve_symlinks(interpreter);

    if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
        if let Some(arg) = shebang_arg {
            crate::safe_print!(128, "[syscall] execve: shebang {} {} {}\n", interpreter, arg, script_path);
        } else {
            crate::safe_print!(128, "[syscall] execve: shebang {} {}\n", interpreter, script_path);
        }
    }

    let mut new_args = Vec::new();
    new_args.push(interpreter.clone());
    if let Some(arg) = shebang_arg {
        if !arg.is_empty() {
            new_args.push(String::from(arg));
        }
    }
    new_args.push(String::from(script_path));
    if original_args.len() > 1 {
        new_args.extend_from_slice(&original_args[1..]);
    }

    do_execve(interpreter, new_args, env)
}

pub(super) fn sys_wait4(pid: i32, status_ptr: u64, options: i32, rusage_ptr: u64) -> u64 {
    if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
        crate::safe_print!(128, "[syscall] wait4(pid={}, options=0x{:x})\n", pid, options);
    }

    const RUSAGE_SIZE: usize = 144;
    if rusage_ptr != 0 && validate_user_ptr(rusage_ptr, RUSAGE_SIZE) {
        let zero = [0u8; RUSAGE_SIZE];
        let _ = unsafe { copy_to_user_safe(rusage_ptr as *mut u8, zero.as_ptr(), RUSAGE_SIZE) };
    }

    let wnohang = options & 1 != 0;

    let current_pid = match akuma_exec::process::read_current_pid() {
        Some(p) => p,
        None => return (-libc_errno::ECHILD as i64) as u64,
    };

    if pid > 0 {
        let p = pid as u32;
        if let Some(ch) = akuma_exec::process::get_child_channel(p) {
            loop {
                if ch.has_exited() {
                    let code = ch.exit_code();
                    if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
                        crate::safe_print!(128, "[syscall] wait4: PID {} exited with code {}\n", p, code);
                    }
                    if status_ptr != 0 && validate_user_ptr(status_ptr, 4) {
                        let status = encode_wait_status(code);
                        let _ = unsafe { copy_to_user_safe(status_ptr as *mut u8, &status as *const u32 as *const u8, 4) };
                    }
                    akuma_exec::process::remove_child_channel(p);
                    return p as u64;
                }

                if wnohang {
                    return 0;
                }
                akuma_exec::threading::yield_now();
            }
        }
    } else if pid == -1 || pid == 0 {
        if !akuma_exec::process::has_children(current_pid) {
            if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
                crate::safe_print!(128, "[syscall] wait4: no children for PID {}\n", current_pid);
            }
            return (-libc_errno::ECHILD as i64) as u64;
        }

        loop {
            if let Some((child_pid, ch)) = akuma_exec::process::find_exited_child(current_pid) {
                let code = ch.exit_code();
                if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
                    crate::safe_print!(128, "[syscall] wait4: PID {} exited with code {}\n", child_pid, code);
                }
                if status_ptr != 0 && validate_user_ptr(status_ptr, 4) {
                    let status = encode_wait_status(code);
                    let _ = unsafe { copy_to_user_safe(status_ptr as *mut u8, &status as *const u32 as *const u8, 4) };
                }
                akuma_exec::process::remove_child_channel(child_pid);
                return child_pid as u64;
            }

            if wnohang {
                return 0;
            }
            akuma_exec::threading::yield_now();
        }
    }

    if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
        crate::safe_print!(128, "[syscall] wait4: no child found for PID {}\n", pid);
    }
    (-libc_errno::ECHILD as i64) as u64
}

pub(super) fn sys_prlimit64(_pid: u32, resource: u32, _new_rlim: u64, old_rlim: u64) -> u64 {
    if old_rlim != 0 {
        if !validate_user_ptr(old_rlim, 16) { return EFAULT; }
        #[repr(C)]
        struct Rlimit {
            rlim_cur: u64,
            rlim_max: u64,
        }
        const RLIM_INFINITY: u64 = !0u64;
        let (cur, max) = match resource {
            3 => {
                let stack_size = akuma_exec::runtime::config().user_stack_size as u64;
                (stack_size, stack_size)
            },
            7 => (1024, 1024),
            _ => (RLIM_INFINITY, RLIM_INFINITY),
        };
        let rlim = Rlimit { rlim_cur: cur, rlim_max: max };
        if unsafe { copy_to_user_safe(old_rlim as *mut u8, &rlim as *const Rlimit as *const u8, 16).is_err() } {
            return EFAULT;
        }
    }
    0
}

pub(super) fn sys_sysinfo(info_ptr: usize) -> u64 {
    if !validate_user_ptr(info_ptr as u64, 112) { return EFAULT; }
    let mut info = [0u8; 112];
    let total_pages = crate::pmm::total_count() as u64;
    let free_pages = crate::pmm::free_count() as u64;
    let uptime_secs = crate::timer::uptime_us() / 1_000_000;
    // struct sysinfo layout (AArch64, 8-byte unsigned long):
    //   0: uptime (8), 8: loads[3] (24), 32: totalram (8), 40: freeram (8),
    //   48: sharedram (8), 56: bufferram (8), 64: totalswap (8), 72: freeswap (8),
    //   80: procs (2), 82: pad (2), 84: [align 4], 88: totalhigh (8),
    //   96: freehigh (8), 104: mem_unit (4), 108: _f[0], pad to 112
    unsafe {
        let ptr = info.as_mut_ptr() as *mut u64;
        core::ptr::write(ptr.add(0), uptime_secs);          // offset 0
        core::ptr::write(ptr.add(4), total_pages * 4096);   // offset 32: totalram
        core::ptr::write(ptr.add(5), free_pages * 4096);    // offset 40: freeram
        let procs_ptr = info.as_mut_ptr().add(80) as *mut u16;
        core::ptr::write(procs_ptr, 1);                     // offset 80: procs
        let memunit_ptr = info.as_mut_ptr().add(104) as *mut u32;
        core::ptr::write(memunit_ptr, 1);                   // offset 104: mem_unit
    }
    if unsafe { copy_to_user_safe(info_ptr as *mut u8, info.as_ptr(), 112).is_err() } {
        return EFAULT;
    }
    0
}

pub(super) fn sys_getpid() -> u64 {
    akuma_exec::process::read_current_pid().map_or(!0u64, |pid| pid as u64)
}

pub(super) fn sys_getppid() -> u64 {
    if let Some(proc) = akuma_exec::process::current_process() {
        proc.parent_pid as u64
    } else {
        !0u64
    }
}

pub(super) fn sys_geteuid() -> u64 {
    0
}

pub(super) fn sys_getrandom(ptr: u64, len: usize) -> u64 {
    if !validate_user_ptr(ptr, len) { return EFAULT; }
    let mut remaining = len;
    let mut current_ptr = ptr;
    while remaining > 0 {
        let chunk = remaining.min(256);
        let mut kernel_buf = alloc::vec![0u8; chunk];
        if crate::rng::fill_bytes(&mut kernel_buf).is_ok() {
            if unsafe { copy_to_user_safe(current_ptr as *mut u8, kernel_buf.as_ptr(), chunk).is_err() } {
                return EFAULT;
            }
        } else {
            return !0u64;
        }
        remaining -= chunk;
        current_ptr += chunk as u64;
    }
    len as u64
}

#[repr(C)]
pub struct SpawnOptions {
    pub cwd_ptr: u64,
    pub cwd_len: usize,
    pub root_dir_ptr: u64,
    pub root_dir_len: usize,
    pub args_ptr: u64,
    pub args_len: usize,
    pub stdin_ptr: u64,
    pub stdin_len: usize,
    pub box_id: u64,
}

pub(super) fn parse_argv_array(ptr: u64) -> Vec<String> {
    if ptr == 0 { return Vec::new(); }
    let mut args = Vec::new();
    let mut i = 0;
    loop {
        if !BYPASS_VALIDATION.load(Ordering::Acquire) {
            if !validate_user_ptr(ptr + i * 8, 8) { break; }
        }
        let mut str_ptr: u64 = 0;
        if unsafe { copy_from_user_safe(&mut str_ptr as *mut u64 as *mut u8, (ptr + i * 8) as *const u8, 8).is_err() } {
            break;
        }
        if str_ptr == 0 { break; }
        
        match copy_from_user_str(str_ptr, 1024) {
            Ok(s) => args.push(s),
            Err(_) => break,
        }
        i += 1;
    }
    args
}

pub(super) fn sys_spawn(path_ptr: u64, argv_ptr: u64, envp_ptr: u64, stdin_ptr: u64, stdin_len: usize, _a5: u64) -> u64 {
    let path = match copy_from_user_str(path_ptr, 512) {
        Ok(p) => p,
        Err(e) => return e,
    };
    
    let args_vec = parse_argv_array(argv_ptr);
    let env_vec = parse_argv_array(envp_ptr);
    
    let args_refs: Vec<&str> = if args_vec.len() > 1 {
        args_vec.iter().skip(1).map(|s| s.as_str()).collect()
    } else {
        Vec::new()
    };
    
    let stdin_data = if stdin_ptr != 0 {
        if !BYPASS_VALIDATION.load(Ordering::Acquire) {
            if !validate_user_ptr(stdin_ptr, stdin_len) { return EFAULT; }
        }
        let mut data = alloc::vec![0u8; stdin_len];
        if unsafe { copy_from_user_safe(data.as_mut_ptr(), stdin_ptr as *const u8, stdin_len).is_err() } {
            return EFAULT;
        }
        Some(data)
    } else {
        None
    };
    
    let stdin_slice = stdin_data.as_deref();

    if let Ok((_tid, ch, pid)) = akuma_exec::process::spawn_process_with_channel_cwd(&path, Some(&args_refs), Some(&env_vec), stdin_slice, None) {
        if let Some(proc) = akuma_exec::process::current_process() {
            akuma_exec::process::register_child_channel(pid, ch, proc.pid);
            return (pid as u64) | ((proc.alloc_fd(akuma_exec::process::FileDescriptor::ChildStdout(pid)) as u64) << 32);
        }
    }
    !0u64
}

pub(super) fn sys_spawn_ext(path_ptr: u64, options_ptr: u64, _a2: u64, _a3: u64, _a4: u64, _a5: u64) -> u64 {
    let path = match copy_from_user_str(path_ptr, 512) {
        Ok(p) => p,
        Err(e) => return e,
    };

    if options_ptr == 0 { return !0u64; }
    if !validate_user_ptr(options_ptr, core::mem::size_of::<SpawnOptions>()) { return EFAULT; }

    let mut o = SpawnOptions { cwd_ptr: 0, cwd_len: 0, root_dir_ptr: 0, root_dir_len: 0, args_ptr: 0, args_len: 0, stdin_ptr: 0, stdin_len: 0, box_id: 0 };
    if unsafe { copy_from_user_safe(&mut o as *mut SpawnOptions as *mut u8, options_ptr as *const u8, core::mem::size_of::<SpawnOptions>()).is_err() } {
        return EFAULT;
    }

    let cwd = if o.cwd_ptr != 0 {
        let mut kernel_cwd = alloc::vec![0u8; o.cwd_len];
        if unsafe { copy_from_user_safe(kernel_cwd.as_mut_ptr(), o.cwd_ptr as *const u8, o.cwd_len).is_err() } {
            return EFAULT;
        }
        Some(alloc::string::String::from_utf8(kernel_cwd).unwrap_or_else(|_| String::from("/")))
    } else {
        None
    };
    
    let cwd_ref = cwd.as_deref();

    let args_vec = parse_argv_array(o.args_ptr);
    let args_refs: Vec<&str> = if args_vec.len() > 1 {
        args_vec.iter().skip(1).map(|s| s.as_str()).collect()
    } else {
        args_vec.iter().map(|s| s.as_str()).collect()
    };
    let args_opt = if args_refs.is_empty() { None } else { Some(args_refs.as_slice()) };

    let stdin_data = if o.stdin_ptr != 0 {
        let mut data = alloc::vec![0u8; o.stdin_len];
        if unsafe { copy_from_user_safe(data.as_mut_ptr(), o.stdin_ptr as *const u8, o.stdin_len).is_err() } {
            return EFAULT;
        }
        Some(data)
    } else {
        None
    };
    
    let stdin_slice = stdin_data.as_deref();

    if let Ok((_tid, ch, pid)) = akuma_exec::process::spawn_process_with_channel_ext(&path, args_opt, None, stdin_slice, cwd_ref, o.box_id) {
        if let Some(proc) = akuma_exec::process::current_process() {
            akuma_exec::process::register_child_channel(pid, ch, proc.pid);
            return (pid as u64) | ((proc.alloc_fd(akuma_exec::process::FileDescriptor::ChildStdout(pid)) as u64) << 32);
        }
    }
    !0u64
}

pub(super) fn sys_kill(pid: u32, _sig: u32) -> u64 {
    if pid == 0 { return 0; }
    if pid <= 1 { return !0u64; }
    if akuma_exec::process::kill_process(pid).is_ok() { 0 } else { !0u64 }
}

pub(super) fn sys_waitpid(pid: u32, status_ptr: u64) -> u64 {
    if status_ptr != 0 && !validate_user_ptr(status_ptr, 4) { return EFAULT; }

    if let Some(ch) = akuma_exec::process::get_child_channel(pid) {
        if ch.has_exited() {
            if status_ptr != 0 { 
                let status = encode_wait_status(ch.exit_code());
                if unsafe { copy_to_user_safe(status_ptr as *mut u8, &status as *const u32 as *const u8, 4).is_err() } {
                    return EFAULT;
                }
            }
            return pid as u64;
        }
    }
    0
}

/// prctl - process control
pub(super) fn sys_prctl(option: i32, arg2: u64, arg3: u64, arg4: u64, arg5: u64) -> u64 {
    const PR_SET_NAME: i32 = 15;
    const PR_GET_NAME: i32 = 16;
    const PR_SET_PDEATHSIG: i32 = 1;
    const PR_GET_PDEATHSIG: i32 = 2;
    const PR_SET_DUMPABLE: i32 = 4;
    const PR_GET_DUMPABLE: i32 = 3;
    const PR_SET_SECCOMP: i32 = 22;
    const PR_GET_SECCOMP: i32 = 21;
    const PR_SET_NO_NEW_PRIVS: i32 = 38;
    const PR_GET_NO_NEW_PRIVS: i32 = 39;
    const PR_SET_VMA: i32 = 0x53564d41; // "SVMA"
    const PR_CAPBSET_READ: i32 = 23;
    const PR_CAPBSET_DROP: i32 = 24;
    const PR_CAP_AMBIENT: i32 = 47;
    const PR_SET_PTRACER: i32 = 42;

    match option {
        PR_SET_NAME => {
            // Set process name (up to 16 chars including null)
            if arg2 != 0 && validate_user_ptr(arg2, 16) {
                let mut name_bytes = [0u8; 16];
                if unsafe { copy_from_user_safe(name_bytes.as_mut_ptr(), arg2 as *const u8, 16).is_err() } {
                    return EFAULT;
                }
                let end = name_bytes.iter().position(|&b| b == 0).unwrap_or(16);
                if let Ok(name) = core::str::from_utf8(&name_bytes[..end]) {
                    if let Some(proc) = akuma_exec::process::current_process() {
                        proc.name = alloc::string::String::from(name);
                    }
                }
            }
            0
        }
        PR_GET_NAME => {
            // Get process name
            if arg2 != 0 && validate_user_ptr(arg2, 16) {
                if let Some(proc) = akuma_exec::process::current_process() {
                    let name = proc.name.as_bytes();
                    let len = name.len().min(15);
                    let mut kernel_buf = [0u8; 16];
                    kernel_buf[..len].copy_from_slice(&name[..len]);
                    if unsafe { copy_to_user_safe(arg2 as *mut u8, kernel_buf.as_ptr(), 16).is_err() } {
                        return EFAULT;
                    }
                }
            }
            0
        }
        PR_SET_PDEATHSIG | PR_SET_DUMPABLE | PR_SET_NO_NEW_PRIVS | PR_SET_VMA => {
            // Accept but ignore these settings
            0
        }
        PR_GET_PDEATHSIG => {
            // Return 0 (no signal set)
            if arg2 != 0 && validate_user_ptr(arg2, 4) {
                let zero: i32 = 0;
                let _ = unsafe { copy_to_user_safe(arg2 as *mut u8, &zero as *const i32 as *const u8, 4) };
            }
            0
        }
        PR_GET_DUMPABLE => {
            // Return 1 (dumpable)
            1
        }
        PR_GET_NO_NEW_PRIVS => {
            // Return 0 (not set)
            0
        }
        PR_SET_SECCOMP | PR_GET_SECCOMP => {
            // Return -EINVAL for seccomp (not supported)
            EINVAL
        }
        PR_CAPBSET_READ => {
            // Return 1 for all capabilities (we have all caps)
            1
        }
        PR_CAPBSET_DROP | PR_CAP_AMBIENT => {
            // Accept but ignore capability operations
            0
        }
        PR_SET_PTRACER => {
            // Accept but ignore - allows process to be traced by specific PID
            0
        }
        _ => {
            crate::tprint!(128, "[prctl] unsupported option={} arg2={:#x} arg3={:#x} arg4={:#x} arg5={:#x}\n",
                option, arg2, arg3, arg4, arg5);
            0
        }
    }
}
