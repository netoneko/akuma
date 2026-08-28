use super::*;
use akuma_terminal::mode_flags;

pub(super) fn sys_ioctl(fd: u32, cmd: u32, arg: u64) -> u64 {
    const TCGETS: u32 = 0x5401;
    const TCSETS: u32 = 0x5402;
    const TCSETSW: u32 = 0x5403;
    const TCSETSF: u32 = 0x5404;
    const TIOCGWINSZ: u32 = 0x5413;
    const TIOCSWINSZ: u32 = 0x5414;
    const TIOCGPGRP: u32 = 0x540f;
    const TIOCSPGRP: u32 = 0x5410;
    const FIONBIO: u32 = 0x5421;
    const FIONREAD: u32 = 0x541B;
    const FIOCLEX: u32 = 0x5451;
    const FIONCLEX: u32 = 0x5450;
    // SIGIO-on-data-ready for a fd. Akuma delivers no such signal, but accepting
    // it as a no-op is enough for callers that only treat *failure* as fatal —
    // e.g. nginx's ngx_spawn_process bails out (never calls fork()) if this
    // ioctl on the master/worker channel socketpair fails, even though nothing
    // else in its worker lifecycle depends on the SIGIO actually arriving.
    const FIOASYNC: u32 = 0x5452;
    // TUN/TAP: _IOW('T', 202, int) — rump's Linux virtif uses this to bind the tap.
    #[cfg(feature = "rump")]
    const TUNSETIFF: u32 = 0x4004_54ca;
    // OSS audio ioctls for /dev/dsp (mirror crate::audio constants).
    const SNDCTL_DSP_SPEED: u32 = crate::audio::SNDCTL_DSP_SPEED;
    const SNDCTL_DSP_SETFMT: u32 = crate::audio::SNDCTL_DSP_SETFMT;
    const SNDCTL_DSP_CHANNELS: u32 = crate::audio::SNDCTL_DSP_CHANNELS;
    // Read-only network ioctls (mirror super::net constants) — `ifconfig`.
    #[cfg(feature = "smoltcp")]
    const SIOCGIFCONF: u32 = super::net::SIOCGIFCONF;
    #[cfg(feature = "smoltcp")]
    const SIOCGIFFLAGS: u32 = super::net::SIOCGIFFLAGS;
    #[cfg(feature = "smoltcp")]
    const SIOCGIFADDR: u32 = super::net::SIOCGIFADDR;
    #[cfg(feature = "smoltcp")]
    const SIOCGIFBRDADDR: u32 = super::net::SIOCGIFBRDADDR;
    #[cfg(feature = "smoltcp")]
    const SIOCGIFNETMASK: u32 = super::net::SIOCGIFNETMASK;
    #[cfg(feature = "smoltcp")]
    const SIOCGIFMTU: u32 = super::net::SIOCGIFMTU;
    #[cfg(feature = "smoltcp")]
    const SIOCGIFHWADDR: u32 = super::net::SIOCGIFHWADDR;

    if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
        crate::safe_print!(128, "[syscall] ioctl(fd={}, cmd=0x{:x}, arg=0x{:x})\n", fd, cmd, arg);
    }

    let proc = match akuma_exec::process::current_process_shared() { Some(p) => p, None => return ESRCH };

    match cmd {
        FIONBIO => {
            let mut val: i32 = 0;
            if read_user_into(&mut val, arg).is_err() {
                return EFAULT;
            }
            if val != 0 { proc.set_nonblock(fd); } else { proc.clear_nonblock(fd); }
            return 0;
        }
        FIONREAD => {
            let fd_entry = proc.get_fd(fd);
            let count: i32 = match fd_entry {
                // Restores the arm `b19ae838` ("more tty shenanigans") dropped when it
                // reworked the terminal-ioctl gate — see TTY_SHENANIGANS.md round 3's
                // "known follow-up". Without it, FIONREAD on stdin/`/dev/tty` always
                // answered 0, so a program that polls FIONREAD before a non-blocking
                // read to decide whether input is waiting never saw it go non-zero.
                // DevTty shares fd 0's channel (same reasoning as the ioctl gate and
                // `sys_read`/`sys_write`/`epoll_check_fd_readiness`), so it belongs here.
                Some(akuma_exec::process::FileDescriptor::Stdin
                | akuma_exec::process::FileDescriptor::DevTty) => {
                    akuma_exec::process::current_channel()
                        .map_or(0, |ch| ch.stdin_bytes_available() as i32)
                }
                Some(akuma_exec::process::FileDescriptor::PipeRead(pipe_id)) => {
                    super::pipe::pipe_bytes_available(pipe_id) as i32
                }
                #[cfg(feature = "smoltcp")]
                Some(akuma_exec::process::FileDescriptor::Socket(idx)) => {
                    super::net::socket_recv_queue_size(idx) as i32
                }
                #[cfg(feature = "sc-eventfd")]
                Some(akuma_exec::process::FileDescriptor::EventFd(efd_id)) => {
                    if super::eventfd::eventfd_can_read(efd_id) { 8 } else { 0 }
                }
                #[cfg(feature = "sc-timerfd")]
                Some(akuma_exec::process::FileDescriptor::TimerFd(timer_id)) => {
                    if super::timerfd::timerfd_can_read(timer_id) { 8 } else { 0 }
                }

                Some(akuma_exec::process::FileDescriptor::File(ref f)) => {
                    crate::fs::file_size(&f.path)
                        .map_or(0, |sz| (sz as usize).saturating_sub(f.position) as i32)
                }
                Some(akuma_exec::process::FileDescriptor::ChildStdout(_)) => 0,
                Some(akuma_exec::process::FileDescriptor::PipeWrite(_)) => 0,
                _ => 0,
            };
            if write_user_val(arg, &count).is_err() {
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
        FIOASYNC => {
            return 0;
        }
        TIOCSWINSZ => {
            // Set terminal window size (ws_row, ws_col). Unlike the TIOCGWINSZ
            // path below (which is gated to fd 0-2 and reads the CALLER's own
            // state), TIOCSWINSZ must also work on a `ChildStdout(child_pid)`
            // fd: userspace sshd holds the login shell it spawned under such an
            // fd, and a `pty` spawn gives that child a FRESH TerminalState
            // (spawn.rs — it deliberately does NOT inherit sshd's, so concurrent
            // sessions don't share one input_waker slot). So sshd cannot update
            // its own state; it must target the CHILD's. That child's state is
            // an Arc shared with all its descendants (shell → vi), so the update
            // reaches any full-screen app under the session via TIOCGWINSZ.
            let mut winsz = [0u16; 4];
            if copy_from_user(as_user_bytes_mut(&mut winsz), arg).is_err() {
                return EFAULT;
            }
            let height = winsz[0];
            let width = winsz[1];
            let child_pid = match proc.get_fd(fd) {
                Some(akuma_exec::process::FileDescriptor::ChildStdout(pid)) => Some(pid),
                _ => None,
            };
            let ts = match child_pid {
                Some(pid) => akuma_exec::process::lookup_process_shared(pid).map(|p| p.terminal_state.clone()),
                None => akuma_exec::process::current_terminal_state(),
            };
            match ts {
                Some(state) => {
                    let mut s = state.lock();
                    s.term_width = width;
                    s.term_height = height;
                    return 0;
                }
                // ENOMEM, which is what this arm has always returned — the
                // comment here read `ENXIO` and never matched the number. Linux
                // returns ENOTTY for an ioctl on something that is not a
                // terminal; changing it is a behaviour change, so it is recorded
                // in TRIM_FAT_EMBARASSING_DUPLICATIONS.md §5.7 and not made
                // here. The other five "no terminal state" arms below return the
                // same value.
                None => return ENOMEM,
            }
        }
        SNDCTL_DSP_SPEED | SNDCTL_DSP_SETFMT | SNDCTL_DSP_CHANNELS => {
            // OSS audio params on /dev/dsp. arg is *mut i32 (in/out): the desired
            // value in, the accepted value out (we accept what was requested).
            if !matches!(proc.get_fd(fd), Some(akuma_exec::process::FileDescriptor::DevDsp)) {
                return ENOTTY; // not a dsp fd
            }
            let mut val: i32 = 0;
            if read_user_into(&mut val, arg).is_err() {
                return EFAULT;
            }
            let res = match cmd {
                SNDCTL_DSP_SPEED => crate::audio::set_rate(val),
                SNDCTL_DSP_SETFMT => crate::audio::set_format_oss(val),
                SNDCTL_DSP_CHANNELS => crate::audio::set_channels(val),
                _ => unreachable!(),
            };
            if res.is_err() {
                return EINVAL;
            }
            // Echo the accepted value back (OSS contract).
            if write_user_val(arg, &val).is_err() {
                return EFAULT;
            }
            return 0;
        }
        #[cfg(feature = "smoltcp")]
        SIOCGIFCONF => {
            if !matches!(proc.get_fd(fd), Some(akuma_exec::process::FileDescriptor::Socket(_))) {
                return ENOTTY;
            }
            return super::net::sys_ioctl_siocgifconf(arg);
        }
        #[cfg(feature = "smoltcp")]
        SIOCGIFFLAGS | SIOCGIFADDR | SIOCGIFBRDADDR | SIOCGIFNETMASK | SIOCGIFMTU | SIOCGIFHWADDR => {
            if !matches!(proc.get_fd(fd), Some(akuma_exec::process::FileDescriptor::Socket(_))) {
                return ENOTTY;
            }
            return super::net::sys_ioctl_siocgifreq(cmd, arg);
        }
        #[cfg(feature = "rump")]
        TUNSETIFF => {
            // TUN/TAP interface bind. rump's stock Linux virtif backend issues
            // this on the tap fd to attach to an interface; /dev/net/tap0 is
            // already the (only) tap, so accept it as a no-op success. Reject
            // on any non-tap fd with ENOTTY.
            if !matches!(proc.get_fd(fd), Some(akuma_exec::process::FileDescriptor::Tap { .. })) {
                return ENOTTY; // not a tap fd
            }
            return 0;
        }
        _ => {}
    }

    // A spawned child's stdin/stdout is a pipe (ProcessChannel), not a real
    // terminal. Report ENOTTY for the terminal ioctls below (TCGETS is what
    // isatty() probes) so shells like busybox run non-interactively over the
    // SSH-into-box bridge instead of launching a line editor that hangs on an
    // ESC[6n cursor query. Console/boot processes keep is_terminal == true.
    //
    // The CHANNEL check alone is not enough: a fork+exec pipeline child
    // (`cat file | less`) has fd 0 dup2'd to a PipeRead, but still inherits the
    // shell's terminal channel — so isatty(0) returned true inside a pipeline
    // and busybox less (no FILE arg, stdin "a tty") printed its usage and
    // exited 1 instead of paging the pipe. The fd TABLE entry is the ground
    // truth: only Stdin/Stdout/Stderr/DevTty (the channel-backed console fds,
    // `/dev/tty` included) are a tty; anything else dup'd over them
    // (PipeRead, File, ...) is not.
    //
    // This replaces a plain `fd > 2` cutoff, which predates `DevTty` and
    // always rejected it: `/dev/tty` is opened as a fresh fd (never 0/1/2),
    // so every terminal ioctl issued directly on the `/dev/tty` fd itself —
    // exactly how programs that open it for raw-mode control (pagers,
    // crossterm's Unix event source) use it — returned `ENOTTY`
    // unconditionally. See `TTY_SHENANIGANS.md` round 3.
    match proc.get_fd(fd) {
        Some(akuma_exec::process::FileDescriptor::Stdin
        | akuma_exec::process::FileDescriptor::Stdout
        | akuma_exec::process::FileDescriptor::Stderr
        | akuma_exec::process::FileDescriptor::DevTty) => {}
        _ => return ENOTTY, // fd is a pipe/file/socket, not the console tty
    }

    if let Some(ch) = akuma_exec::process::current_channel()
        && !ch.is_terminal()
    {
        return ENOTTY; // fd 0/1/2 are a pipe, not a tty
    }

    let result = match cmd {
        TCGETS => {
            if !validate_user_ptr(arg, 36) { return EFAULT; }
            let term_state_lock = match akuma_exec::process::current_terminal_state() {
                Some(state) => state,
                None => return ENOMEM,
            };
            let ts = term_state_lock.lock();
            let mut kernel_buf = [0u32; 9]; // 4 flags + 5 u32 for 20 bytes CC
            kernel_buf[0] = ts.iflag;
            kernel_buf[1] = ts.oflag;
            kernel_buf[2] = ts.cflag;
            kernel_buf[3] = ts.lflag;
            unsafe {
                core::ptr::copy_nonoverlapping(ts.cc.as_ptr(), kernel_buf[4..].as_mut_ptr().cast::<u8>(), 20);
            }
            if copy_to_user_with(arg, as_user_bytes(&kernel_buf), Prefault::No).is_err() {
                return EFAULT;
            }
            0
        }
        TCSETS | TCSETSW | TCSETSF => {
            let term_state_lock = match akuma_exec::process::current_terminal_state() {
                Some(state) => state,
                None => return ENOMEM,
            };
            let mut kernel_buf = [0u32; 9];
            if copy_from_user(&mut as_user_bytes_mut(&mut kernel_buf)[..36], arg).is_err() {
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
                core::ptr::copy_nonoverlapping(kernel_buf[4..].as_ptr().cast::<u8>(), ts.cc.as_mut_ptr(), 20);
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
                None => return ENOMEM,
            };
            let ts = term_state_lock.lock();
            let kernel_winsz = [ts.term_height, ts.term_width, 0, 0];
            if copy_to_user_with(arg, as_user_bytes(&kernel_winsz), Prefault::No).is_err() {
                return EFAULT;
            }
            0
        }
        TIOCGPGRP => {
            if !validate_user_ptr(arg, 4) { return EFAULT; }
            let term_state_lock = match akuma_exec::process::current_terminal_state() {
                Some(state) => state,
                None => return ENOMEM,
            };
            let ts = term_state_lock.lock();
            let pgid = ts.foreground_pgid;
            if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
                crate::safe_print!(128, "[syscall] TIOCGPGRP: returning foreground_pgid {}\n", pgid);
            }
            if write_user_val_with(arg, &pgid, Prefault::No).is_err() {
                return EFAULT;
            }
            0
        }
        TIOCSPGRP => {
            let term_state_lock = match akuma_exec::process::current_terminal_state() {
                Some(state) => state,
                None => return ENOMEM,
            };
            let mut pgid: u32 = 0;
            if read_user_into(&mut pgid, arg).is_err() {
                return EFAULT;
            }
            let mut ts = term_state_lock.lock();
            if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
                crate::safe_print!(128, "[syscall] TIOCSPGRP: setting foreground_pgid to {}\n", pgid);
            }
            ts.foreground_pgid = pgid;
            0
        }
        _ => ENOTTY,
    };

    if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
        crate::safe_print!(128, "[syscall] ioctl result={}\n", result as i64);
    }
    result
}

fn write_to_process_channel(data: &[u8]) -> u64 {
    let proc_channel = match akuma_exec::process::current_channel() {
        Some(channel) => channel,
        None => return ENOMEM,
    };
    proc_channel.write(data);
    data.len() as u64
}

pub(super) fn sys_set_terminal_attributes(_fd: u64, action: u64, mode_flags_arg: u64) -> u64 {
    let term_state_lock = match akuma_exec::process::current_terminal_state() {
        Some(state) => state,
        None => return ENOMEM,
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
        None => return ENOMEM,
    };
    proc_channel.set_raw_mode(!term_state.is_canonical());

    if action == 2 {
        proc_channel.flush_stdin();
    }

    0
}

pub(super) fn sys_get_terminal_attributes(_fd: u64, attr_ptr: u64) -> u64 {
    if attr_ptr == 0 {
        return EINVAL;
    }
    if !validate_user_ptr(attr_ptr, 8) { return EFAULT; }

    let term_state_lock = match akuma_exec::process::current_terminal_state() {
        Some(state) => state,
        None => return ENOMEM,
    };

    let term_state = term_state_lock.lock();
    let val = term_state.mode_flags;
    if write_user_val_with(attr_ptr, &val, Prefault::No).is_err() {
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
    let sequence = format!("\x1b[{row_1};{col_1}H");
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
        return EINVAL;
    }
    if !validate_user_ptr(buf_ptr, buf_len) { return EFAULT; }


    let proc_channel = match akuma_exec::process::current_channel() {
        Some(channel) => channel,
        None => return ENOMEM,
    };

    let term_state_lock = match akuma_exec::process::current_terminal_state() {
        Some(state) => state,
        None => return EBADF,
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

        // Register the waker ONCE for the whole wait rather than once per iteration:
        // the register and its matching clear (after the loop) are the only two
        // `term_state_lock` touches this wait needs. `schedule_blocking`'s sticky wake
        // (`WOKEN_STATES`) already tolerates a wake landing against a still-registered
        // waker between iterations — it just re-enters the loop, drains nothing new,
        // and parks again — so re-registering every iteration bought nothing but extra
        // lock traffic on the exact lock the wedge below hangs off of. Each acquisition
        // uses `lock_bounded`, which disables preemption only for a single `try_lock`
        // attempt rather than across the whole (potentially contended, potentially
        // unbounded) wait — see docs/archive/TERM_POLL_INPUT_PREEMPTION_FIX.md §9-§10.
        {
            let thread_id = akuma_exec::threading::current_thread_id();
            akuma_exec::sync::lock_bounded(&term_state_lock)
                .set_input_waker(akuma_exec::threading::get_waker_for_thread(thread_id));
        }

        bytes_read = loop {
            // Re-resolve every iteration: `box grab`/`sys_reattach` can repoint
            // this process's channel to a new one while this wait is already
            // parked, and the waker (registered once above, on `terminal_state`,
            // which reattach never touches) fires correctly against the new
            // input either way. Reusing the `proc_channel` captured before the
            // loop would keep draining the abandoned old channel forever — same
            // bug as the `sys_read` stdin loop in `fs.rs`.
            let proc_channel = match akuma_exec::process::current_channel() {
                Some(c) => c,
                None => break 0,
            };
            let n = proc_channel.read_stdin(&mut kernel_buf);
            if n > 0 {
                break n;
            }

            if akuma_exec::process::should_interrupt_blocking_syscall() {
                akuma_exec::sync::lock_bounded(&term_state_lock).input_waker.lock().take();
                return EINTR;
            }

            if crate::timer::uptime_us() >= deadline {
                break 0;
            }

            akuma_exec::threading::schedule_blocking(deadline);
        };

        // Clear the waker once the wait is over — the counterpart to the single
        // register above, covering both remaining exit paths (data ready, deadline hit).
        akuma_exec::sync::lock_bounded(&term_state_lock).input_waker.lock().take();
    }

    if bytes_read > 0 {
        if copy_to_user(buf_ptr, &kernel_buf[..bytes_read]).is_err() {
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
            last_core: akuma_exec::threading::get_thread_last_core(i),
            ..Default::default()
        };

        if let Some(pid) = akuma_exec::process::find_pid_by_thread(i) {
            stat.pid = pid;
            if let Some(proc) = akuma_exec::process::lookup_process_shared(pid) {
                stat.box_id = proc.box_id;
                let name_bytes = proc.name.as_bytes();
                let to_copy = name_bytes.len().min(stat.name.len());
                stat.name[..to_copy].copy_from_slice(&name_bytes[..to_copy]);
                if to_copy < stat.name.len() {
                    for b in &mut stat.name[to_copy..] { *b = 0; }
                }
            }
        } else {
            // No owning userspace process → it's a kernel thread. Surface its role
            // (kernel/idle/network/system) instead of leaving the name blank.
            let kname = akuma_exec::threading::kernel_thread_name(i).as_bytes();
            let to_copy = kname.len().min(stat.name.len());
            stat.name[..to_copy].copy_from_slice(&kname[..to_copy]);
            for b in &mut stat.name[to_copy..] { *b = 0; }
        }

        if write_user_val(ptr + (i * stat_size) as u64, &stat).is_err() {
            return EFAULT;
        }
    }
    count as u64
}
