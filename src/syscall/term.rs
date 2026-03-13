use super::*;
use akuma_net::socket::libc_errno;
use akuma_terminal::mode_flags;
use akuma_exec::mmu::user_access::{copy_from_user_safe, copy_to_user_safe};

pub(super) fn sys_ioctl(fd: u32, cmd: u32, arg: u64) -> u64 {
    const TCGETS: u32 = 0x5401;
    const TCSETS: u32 = 0x5402;
    const TCSETSW: u32 = 0x5403;
    const TCSETSF: u32 = 0x5404;
    const TIOCGWINSZ: u32 = 0x5413;
    const TIOCGPGRP: u32 = 0x540f;
    const TIOCSPGRP: u32 = 0x5410;
    const FIONBIO: u32 = 0x5421;
    const FIONREAD: u32 = 0x541B;
    const FIOCLEX: u32 = 0x5451;
    const FIONCLEX: u32 = 0x5450;

    if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
        crate::safe_print!(128, "[syscall] ioctl(fd={}, cmd=0x{:x}, arg=0x{:x})\n", fd, cmd, arg);
    }

    let proc = match akuma_exec::process::current_process() { Some(p) => p, None => return !0u64 };

    match cmd {
        FIONBIO => {
            if !validate_user_ptr(arg, 4) { return EFAULT; }
            let mut val: i32 = 0;
            if unsafe { copy_from_user_safe(&mut val as *mut i32 as *mut u8, arg as *const u8, 4).is_err() } {
                return EFAULT;
            }
            if val != 0 { proc.set_nonblock(fd); } else { proc.clear_nonblock(fd); }
            return 0;
        }
        FIONREAD => {
            if !validate_user_ptr(arg, 4) { return EFAULT; }
            let fd_entry = proc.get_fd(fd);
            let count: i32 = match fd_entry {
                Some(akuma_exec::process::FileDescriptor::PipeRead(pipe_id)) => {
                    super::pipe::pipe_bytes_available(pipe_id) as i32
                }
                Some(akuma_exec::process::FileDescriptor::Socket(idx)) => {
                    super::net::socket_recv_queue_size(idx) as i32
                }
                Some(akuma_exec::process::FileDescriptor::EventFd(efd_id)) => {
                    if super::eventfd::eventfd_can_read(efd_id) { 8 } else { 0 }
                }
                Some(akuma_exec::process::FileDescriptor::TimerFd(timer_id)) => {
                    if super::timerfd::timerfd_can_read(timer_id) { 8 } else { 0 }
                }
                Some(akuma_exec::process::FileDescriptor::Stdin) => {
                    akuma_exec::process::current_channel()
                        .map_or(0, |ch| ch.stdin_bytes_available() as i32)
                }
                Some(akuma_exec::process::FileDescriptor::File(ref f)) => {
                    crate::fs::file_size(&f.path)
                        .map(|sz| (sz as usize).saturating_sub(f.position) as i32)
                        .unwrap_or(0)
                }
                Some(akuma_exec::process::FileDescriptor::ChildStdout(_)) => 0,
                Some(akuma_exec::process::FileDescriptor::PipeWrite(_)) => 0,
                _ => 0,
            };
            if unsafe { copy_to_user_safe(arg as *mut u8, &count as *const i32 as *const u8, 4).is_err() } {
                return EFAULT;
            }
            return 0;
        }
        FIOCLEX => {
            proc.set_cloexec(fd);
            return 0;
        }
        FIONCLEX => {
            proc.clear_cloexec(fd);
            return 0;
        }
        _ => {}
    }

    if fd > 2 {
        return (-(25i64)) as u64; // ENOTTY for terminal ioctls on non-TTY fds
    }

    let result = match cmd {
        TCGETS => {
            if !validate_user_ptr(arg, 36) { return EFAULT; }
            let term_state_lock = match akuma_exec::process::current_terminal_state() {
                Some(state) => state,
                None => return (-(12i64)) as u64,
            };
            let ts = term_state_lock.lock();
            let mut kernel_buf = [0u32; 9]; // 4 flags + 5 u32 for 20 bytes CC
            kernel_buf[0] = ts.iflag;
            kernel_buf[1] = ts.oflag;
            kernel_buf[2] = ts.cflag;
            kernel_buf[3] = ts.lflag;
            unsafe {
                core::ptr::copy_nonoverlapping(ts.cc.as_ptr(), kernel_buf[4..].as_mut_ptr() as *mut u8, 20);
            }
            if unsafe { copy_to_user_safe(arg as *mut u8, kernel_buf.as_ptr() as *const u8, 36).is_err() } {
                return EFAULT;
            }
            0
        }
        TCSETS | TCSETSW | TCSETSF => {
            if !validate_user_ptr(arg, 36) { return EFAULT; }
            let term_state_lock = match akuma_exec::process::current_terminal_state() {
                Some(state) => state,
                None => return (-(12i64)) as u64,
            };
            let mut kernel_buf = [0u32; 9];
            if unsafe { copy_from_user_safe(kernel_buf.as_mut_ptr() as *mut u8, arg as *const u8, 36).is_err() } {
                return EFAULT;
            }
            let mut ts = term_state_lock.lock();
            ts.iflag = kernel_buf[0];
            ts.oflag = kernel_buf[1];
            ts.cflag = kernel_buf[2];
            ts.lflag = kernel_buf[3];
            
            if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
                crate::safe_print!(128, "[syscall] TCSETS: iflag=0x{:x} oflag=0x{:x} cflag=0x{:x} lflag=0x{:x}\n",
                    ts.iflag, ts.oflag, ts.cflag, ts.lflag);
            }
            
            unsafe {
                core::ptr::copy_nonoverlapping(kernel_buf[4..].as_ptr() as *const u8, ts.cc.as_mut_ptr(), 20);
            }

            if let Some(ch) = akuma_exec::process::current_channel() {
                ch.set_raw_mode(!ts.is_canonical());
                if cmd == TCSETSF {
                    ch.flush_stdin();
                }
            }
            0
        }
        TIOCGWINSZ => {
            if !validate_user_ptr(arg, 8) { return EFAULT; }
            let term_state_lock = match akuma_exec::process::current_terminal_state() {
                Some(state) => state,
                None => return (-(12i64)) as u64,
            };
            let ts = term_state_lock.lock();
            let kernel_winsz = [ts.term_height, ts.term_width, 0, 0];
            if unsafe { copy_to_user_safe(arg as *mut u8, kernel_winsz.as_ptr() as *const u8, 8).is_err() } {
                return EFAULT;
            }
            0
        }
        TIOCGPGRP => {
            if !validate_user_ptr(arg, 4) { return EFAULT; }
            let term_state_lock = match akuma_exec::process::current_terminal_state() {
                Some(state) => state,
                None => return (-(12i64)) as u64,
            };
            let ts = term_state_lock.lock();
            let pgid = ts.foreground_pgid;
            if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
                crate::safe_print!(128, "[syscall] TIOCGPGRP: returning foreground_pgid {}\n", pgid);
            }
            if unsafe { copy_to_user_safe(arg as *mut u8, &pgid as *const u32 as *const u8, 4).is_err() } {
                return EFAULT;
            }
            0
        }
        TIOCSPGRP => {
            if !validate_user_ptr(arg, 4) { return EFAULT; }
            let term_state_lock = match akuma_exec::process::current_terminal_state() {
                Some(state) => state,
                None => return (-(12i64)) as u64,
            };
            let mut pgid: u32 = 0;
            if unsafe { copy_from_user_safe(&mut pgid as *mut u32 as *mut u8, arg as *const u8, 4).is_err() } {
                return EFAULT;
            }
            let mut ts = term_state_lock.lock();
            if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
                crate::safe_print!(128, "[syscall] TIOCSPGRP: setting foreground_pgid to {}\n", pgid);
            }
            ts.foreground_pgid = pgid;
            0
        }
        _ => (-(25i64)) as u64,
    };

    if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
        crate::safe_print!(128, "[syscall] ioctl result={}\n", result as i64);
    }
    result
}

fn write_to_process_channel(data: &[u8]) -> u64 {
    let proc_channel = match akuma_exec::process::current_channel() {
        Some(channel) => channel,
        None => return (-libc_errno::ENOMEM as i64) as u64,
    };
    proc_channel.write(data);
    data.len() as u64
}

pub(super) fn sys_set_terminal_attributes(_fd: u64, action: u64, mode_flags_arg: u64) -> u64 {
    let term_state_lock = match akuma_exec::process::current_terminal_state() {
        Some(state) => state,
        None => return (-libc_errno::ENOMEM as i64) as u64,
    };

    let mut term_state = term_state_lock.lock();
    term_state.mode_flags = mode_flags_arg;

    if (mode_flags_arg & mode_flags::RAW_MODE_ENABLE) != 0 {
        term_state.enter_raw_mode();
    } else {
        term_state.exit_raw_mode();
    }

    let proc_channel = match akuma_exec::process::current_channel() {
        Some(channel) => channel,
        None => return (-libc_errno::ENOMEM as i64) as u64,
    };
    proc_channel.set_raw_mode(!term_state.is_canonical());

    if action == 2 {
        proc_channel.flush_stdin();
    }

    0
}

pub(super) fn sys_get_terminal_attributes(_fd: u64, attr_ptr: u64) -> u64 {
    if attr_ptr == 0 {
        return (-libc_errno::EINVAL as i64) as u64;
    }
    if !validate_user_ptr(attr_ptr, 8) { return EFAULT; }

    let term_state_lock = match akuma_exec::process::current_terminal_state() {
        Some(state) => state,
        None => return (-libc_errno::ENOMEM as i64) as u64,
    };

    let term_state = term_state_lock.lock();
    let val = term_state.mode_flags;
    if unsafe { copy_to_user_safe(attr_ptr as *mut u8, &val as *const u64 as *const u8, 8).is_err() } {
        return EFAULT;
    }

    0
}

pub(super) fn sys_set_cursor_position(col: u64, row: u64) -> u64 {
    if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
        crate::safe_print!(64, "[syscall] sys_set_cursor_position({}, {})\n", col, row);
    }
    let row_1 = row + 1;
    let col_1 = col + 1;
    let sequence = format!("\x1b[{};{}H", row_1, col_1);
    write_to_process_channel(sequence.as_bytes())
}

pub(super) fn sys_hide_cursor() -> u64 {
    if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
        crate::safe_print!(64, "[syscall] sys_hide_cursor()\n");
    }
    write_to_process_channel(b"\x1b[?25l")
}

pub(super) fn sys_show_cursor() -> u64 {
    if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
        crate::safe_print!(64, "[syscall] sys_show_cursor()\n");
    }
    write_to_process_channel(b"\x1b[?25h")
}

pub(super) fn sys_clear_screen() -> u64 {
    if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
        crate::safe_print!(64, "[syscall] sys_clear_screen()\n");
    }
    write_to_process_channel(b"\x1b[2J")
}

pub(super) fn sys_poll_input_event(buf_ptr: u64, buf_len: usize, timeout_us: u64) -> u64 {
    if buf_ptr == 0 || buf_len == 0 {
        return (-libc_errno::EINVAL as i64) as u64;
    }
    if !validate_user_ptr(buf_ptr, buf_len) { return EFAULT; }

    if crate::config::SYSCALL_DEBUG_INFO_ENABLED && timeout_us > 0 && timeout_us != u64::MAX {
    }

    let proc_channel = match akuma_exec::process::current_channel() {
        Some(channel) => channel,
        None => return (-libc_errno::ENOMEM as i64) as u64,
    };

    let term_state_lock = match akuma_exec::process::current_terminal_state() {
        Some(state) => state,
        None => return (-libc_errno::EBADF as i64) as u64,
    };

    let mut kernel_buf = alloc::vec![0u8; buf_len];
    let bytes_read;

    if timeout_us == 0 {
        bytes_read = proc_channel.read_stdin(&mut kernel_buf);
    } else {
        let deadline = if timeout_us == u64::MAX {
            u64::MAX
        } else {
            crate::timer::uptime_us().saturating_add(timeout_us)
        };

        loop {
            {
                akuma_exec::threading::disable_preemption();
                let term_state = term_state_lock.lock();
                let thread_id = akuma_exec::threading::current_thread_id();
                term_state.set_input_waker(akuma_exec::threading::get_waker_for_thread(thread_id));
                akuma_exec::threading::enable_preemption();
            }

            let n = proc_channel.read_stdin(&mut kernel_buf);
            if n > 0 {
                bytes_read = n;
                break;
            }

            if akuma_exec::process::is_current_interrupted() {
                return (-libc_errno::EINTR as i64) as u64;
            }

            if crate::timer::uptime_us() >= deadline {
                bytes_read = 0;
                break;
            }

            akuma_exec::threading::schedule_blocking(deadline);

            {
                akuma_exec::threading::disable_preemption();
                let term_state = term_state_lock.lock();
                term_state.input_waker.lock().take();
                akuma_exec::threading::enable_preemption();
            }
        }
    }

    if bytes_read > 0 {
        if unsafe { copy_to_user_safe(buf_ptr as *mut u8, kernel_buf.as_ptr(), bytes_read).is_err() } {
            return EFAULT;
        }
        bytes_read as u64
    } else {
        0
    }
}

pub(super) fn sys_get_cpu_stats(ptr: u64, max: usize) -> u64 {
    let stat_size = core::mem::size_of::<ThreadCpuStat>();
    if !validate_user_ptr(ptr, max * stat_size) { return EFAULT; }
    let count = max.min(crate::config::MAX_THREADS);
    for i in 0..count {
        let mut stat = ThreadCpuStat {
            tid: i as u32,
            total_time_us: akuma_exec::threading::get_thread_cpu_time(i),
            state: akuma_exec::threading::get_thread_state(i),
            ..Default::default()
        };

        if let Some(pid) = akuma_exec::process::find_pid_by_thread(i) {
            stat.pid = pid;
            if let Some(proc) = akuma_exec::process::lookup_process(pid) {
                stat.box_id = proc.box_id;
                let name_bytes = proc.name.as_bytes();
                let to_copy = name_bytes.len().min(stat.name.len());
                stat.name[..to_copy].copy_from_slice(&name_bytes[..to_copy]);
                if to_copy < stat.name.len() {
                    for b in &mut stat.name[to_copy..] { *b = 0; }
                }
            }
        } else if i == 0 {
            stat.name[..6].copy_from_slice(b"kernel");
            for b in &mut stat.name[6..] { *b = 0; }
        } else {
            for b in &mut stat.name { *b = 0; }
        }

        if unsafe { copy_to_user_safe((ptr as usize + i * stat_size) as *mut u8, &stat as *const ThreadCpuStat as *const u8, stat_size).is_err() } {
            return EFAULT;
        }
    }
    count as u64
}
