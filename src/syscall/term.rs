use super::*;
use akuma_net::socket::libc_errno;
use akuma_terminal::mode_flags;

pub(super) fn sys_ioctl(fd: u32, cmd: u32, arg: u64) -> u64 {
    const TCGETS: u32 = 0x5401;
    const TCSETS: u32 = 0x5402;
    const TCSETSW: u32 = 0x5403;
    const TCSETSF: u32 = 0x5404;
    const TIOCGWINSZ: u32 = 0x5413;
    const TIOCGPGRP: u32 = 0x540f;
    const TIOCSPGRP: u32 = 0x5410;

    if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
        crate::safe_print!(128, "[syscall] ioctl(fd={}, cmd=0x{:x}, arg=0x{:x})\n", fd, cmd, arg);
    }

    let _proc = match akuma_exec::process::current_process() { Some(p) => p, None => return !0u64 };
    
    if fd > 2 {
        return (-(25i64)) as u64; // ENOTTY
    }

    let result = match cmd {
        TCGETS => {
            if !validate_user_ptr(arg, 36) { return EFAULT; }
            let term_state_lock = match akuma_exec::process::current_terminal_state() {
                Some(state) => state,
                None => return (-(12i64)) as u64,
            };
            let ts = term_state_lock.lock();
            unsafe {
                let ptr = arg as *mut u32;
                *ptr.add(0) = ts.iflag;
                *ptr.add(1) = ts.oflag;
                *ptr.add(2) = ts.cflag;
                *ptr.add(3) = ts.lflag;
                
                let cc_ptr = ptr.add(4) as *mut u8;
                core::ptr::copy_nonoverlapping(ts.cc.as_ptr(), cc_ptr, 20);
            }
            0
        }
        TCSETS | TCSETSW | TCSETSF => {
            if !validate_user_ptr(arg, 36) { return EFAULT; }
            let term_state_lock = match akuma_exec::process::current_terminal_state() {
                Some(state) => state,
                None => return (-(12i64)) as u64,
            };
            let mut ts = term_state_lock.lock();
            unsafe {
                let ptr = arg as *const u32;
                ts.iflag = *ptr.add(0);
                ts.oflag = *ptr.add(1);
                ts.cflag = *ptr.add(2);
                ts.lflag = *ptr.add(3);
                
                if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
                    crate::safe_print!(128, "[syscall] TCSETS: iflag=0x{:x} oflag=0x{:x} cflag=0x{:x} lflag=0x{:x}\n",
                        ts.iflag, ts.oflag, ts.cflag, ts.lflag);
                }
                
                let cc_ptr = ptr.add(4) as *const u8;
                core::ptr::copy_nonoverlapping(cc_ptr, ts.cc.as_mut_ptr(), 20);
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
            unsafe {
                let ptr = arg as *mut u16;
                *ptr.add(0) = ts.term_height;
                *ptr.add(1) = ts.term_width;
                *ptr.add(2) = 0;
                *ptr.add(3) = 0;
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
            unsafe {
                let pgid = ts.foreground_pgid;
                if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
                    crate::safe_print!(128, "[syscall] TIOCGPGRP: returning foreground_pgid {}\n", pgid);
                }
                *(arg as *mut u32) = pgid;
            }
            0
        }
        TIOCSPGRP => {
            if !validate_user_ptr(arg, 4) { return EFAULT; }
            let term_state_lock = match akuma_exec::process::current_terminal_state() {
                Some(state) => state,
                None => return (-(12i64)) as u64,
            };
            let mut ts = term_state_lock.lock();
            unsafe {
                let pgid = *(arg as *const u32);
                if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
                    crate::safe_print!(128, "[syscall] TIOCSPGRP: setting foreground_pgid to {}\n", pgid);
                }
                ts.foreground_pgid = pgid;
            }
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

    unsafe {
        *(attr_ptr as *mut u64) = term_state.mode_flags;
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
        unsafe {
            core::ptr::copy_nonoverlapping(kernel_buf.as_ptr(), buf_ptr as *mut u8, bytes_read);
        }
        bytes_read as u64
    } else {
        0
    }
}

pub(super) fn sys_get_cpu_stats(ptr: u64, max: usize) -> u64 {
    if !validate_user_ptr(ptr, max * core::mem::size_of::<ThreadCpuStat>()) { return EFAULT; }
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

        unsafe { core::ptr::write_volatile((ptr as *mut ThreadCpuStat).add(i), stat); }
    }
    count as u64
}
