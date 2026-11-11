use super::*;
use core::sync::atomic::AtomicU32;

struct KernelPidFd {
    target_pid: u32,
}

static PIDFD_TABLE: Spinlock<BTreeMap<u32, KernelPidFd>> = Spinlock::new(BTreeMap::new());
static NEXT_PIDFD_ID: AtomicU32 = AtomicU32::new(1);

pub fn pidfd_create(target_pid: u32) -> u32 {
    let id = NEXT_PIDFD_ID.fetch_add(1, Ordering::SeqCst);
    akuma_primitives::irq::with_irqs_disabled(|| {
        PIDFD_TABLE.lock().insert(id, KernelPidFd { target_pid });
    });
    id
}

pub(super) fn pidfd_get_pid(id: u32) -> Option<u32> {
    akuma_primitives::irq::with_irqs_disabled(|| {
        PIDFD_TABLE.lock().get(&id).map(|pf| pf.target_pid)
    })
}

/// Returns true when the tracked process has exited (pidfd becomes EPOLLIN-readable).
pub fn pidfd_can_read(id: u32) -> bool {
    let pid = match pidfd_get_pid(id) {
        Some(p) => p,
        None => return true, // stale pidfd — report as readable so callers unblock
    };
    akuma_exec::process::get_child_channel(pid)
        .is_none_or(|ch| ch.has_exited())
}

pub fn pidfd_close(id: u32) {
    akuma_primitives::irq::with_irqs_disabled(|| {
        PIDFD_TABLE.lock().remove(&id);
    });
}

pub(super) fn sys_pidfd_open(pid: u32, flags: u32) -> u64 {
    use akuma_syscalls_linux::flags::open::O_NONBLOCK;

    // `PIDFD_NONBLOCK` (= `O_NONBLOCK`) is the one flag; anything else, `O_CLOEXEC`
    // included, is `EINVAL`, as on Linux (checked against 6.17 by
    // `userspace/forktest/c_stress/pidfdprobe.c`). `PIDFD_THREAD` (6.9) is refused
    // like a kernel older than it: this pidfd only ever names a process.
    if flags & !O_NONBLOCK != 0 {
        return EINVAL;
    }

    // The process must exist and be a child of the caller.
    if akuma_exec::process::get_child_channel(pid).is_none() {
        // Linux returns ESRCH for non-existent pid; EINVAL if it exists but
        // isn't a child. We return ESRCH since we can't distinguish here.
        return ESRCH;
    }

    let proc = match akuma_exec::process::current_process_shared() {
        Some(p) => p,
        None => return ENOSYS,
    };

    let pidfd_id = pidfd_create(pid);
    let fd = proc.alloc_fd(akuma_exec::process::FileDescriptor::PidFd(pidfd_id));
    // A pidfd is close-on-exec whether or not anyone asked.
    proc.set_cloexec(fd);
    if flags & O_NONBLOCK != 0 {
        proc.set_nonblock(fd);
    }

    if akuma_config::SYSCALL_DEBUG_INFO_ENABLED {
        akuma_primitives::tprint!(96, "[pidfd] open pid={} → fd={} (pidfd_id={})\n", pid, fd, pidfd_id);
    }
    u64::from(fd)
}

/// `pidfd_send_signal(pidfd, sig, info, flags)`. `info` is not read: this
/// kernel's signal delivery (`pend_signal_to_group`, the same one `kill(2)`
/// uses) has no siginfo payload to carry it in, the same simplification
/// `sys_kill` already makes. `flags` has exactly one
/// defined value (0) as of this writing; anything else is `EINVAL` rather
/// than silently ignored.
pub(super) fn sys_pidfd_send_signal(pidfd: u32, sig: u32, _info: u64, flags: u32) -> u64 {
    if flags != 0 || sig as usize > akuma_exec::process::MAX_SIGNALS {
        return EINVAL;
    }
    let target = match akuma_exec::process::current_process_shared().and_then(|p| p.get_fd(pidfd)) {
        Some(akuma_exec::process::FileDescriptor::PidFd(id)) => id,
        _ => return EBADF,
    };
    let Some(pid) = pidfd_get_pid(target) else {
        return ESRCH;
    };
    // A pidfd can be inherited across a box boundary or passed over AF_UNIX, so
    // holding one is not permission to signal through it.
    if let Err(e) = akuma_exec::process::check_signal_pid(pid) {
        return super::signal::deny_errno(e);
    }
    // `sig == 0` probes existence without delivering anything — same
    // exemption `sys_kill` makes, for the same reason: `deliver_signal(pid,
    // 0)` would have nothing to deliver and no defined meaning.
    if sig == 0 {
        return if akuma_exec::process::lookup_process_shared(pid).is_some() { 0 } else { ESRCH };
    }
    if akuma_config::SYSCALL_DEBUG_INFO_ENABLED {
        akuma_primitives::tprint!(96, "[pidfd] send_signal pidfd={} → pid={} sig={}\n", pidfd, pid, sig);
    }
    // Pended like `kill(2)`, SIGKILL included (2026-11-11): this was
    // `deliver_signal`, whose SIGKILL marks the victim's slots `TERMINATED` from
    // the caller's core — the path amd64 cannot survive, reached here by Go's
    // `os.Process.Kill` (`super::signal::sys_kill` has the history).
    if akuma_exec::process::pend_signal_to_group(pid, sig) { 0 } else { ESRCH }
}
