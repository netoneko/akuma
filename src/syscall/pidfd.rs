use super::*;

struct KernelPidFd {
    target_pid: u32,
}

static PIDFD_TABLE: Spinlock<BTreeMap<u32, KernelPidFd>> = Spinlock::new(BTreeMap::new());
static NEXT_PIDFD_ID: AtomicU32 = AtomicU32::new(1);

fn pidfd_create(target_pid: u32) -> u32 {
    let id = NEXT_PIDFD_ID.fetch_add(1, Ordering::SeqCst);
    crate::irq::with_irqs_disabled(|| {
        PIDFD_TABLE.lock().insert(id, KernelPidFd { target_pid });
    });
    id
}

pub(super) fn pidfd_get_pid(id: u32) -> Option<u32> {
    crate::irq::with_irqs_disabled(|| {
        PIDFD_TABLE.lock().get(&id).map(|pf| pf.target_pid)
    })
}

/// Returns true when the tracked process has exited (pidfd becomes EPOLLIN-readable).
pub(super) fn pidfd_can_read(id: u32) -> bool {
    let pid = match pidfd_get_pid(id) {
        Some(p) => p,
        None => return true, // stale pidfd — report as readable so callers unblock
    };
    akuma_exec::process::get_child_channel(pid)
        .map_or(true, |ch| ch.has_exited())
}

pub(super) fn pidfd_close(id: u32) {
    crate::irq::with_irqs_disabled(|| {
        PIDFD_TABLE.lock().remove(&id);
    });
}

pub(super) fn sys_pidfd_open(pid: u32, flags: u32) -> u64 {
    const O_NONBLOCK: u32 = 0x800;
    const O_CLOEXEC: u32 = 0x80000;

    // Only flags 0, O_NONBLOCK, and O_CLOEXEC are valid.
    if flags & !(O_NONBLOCK | O_CLOEXEC) != 0 {
        return EINVAL;
    }

    // The process must exist and be a child of the caller.
    if akuma_exec::process::get_child_channel(pid).is_none() {
        // Linux returns ESRCH for non-existent pid; EINVAL if it exists but
        // isn't a child. We return ESRCH since we can't distinguish here.
        return ESRCH;
    }

    let proc = match akuma_exec::process::current_process() {
        Some(p) => p,
        None => return ENOSYS,
    };

    let pidfd_id = pidfd_create(pid);
    let fd = proc.alloc_fd(akuma_exec::process::FileDescriptor::PidFd(pidfd_id));
    if flags & O_CLOEXEC != 0 {
        proc.set_cloexec(fd);
    }
    if flags & O_NONBLOCK != 0 {
        proc.set_nonblock(fd);
    }

    crate::tprint!(96, "[pidfd] open pid={} → fd={} (pidfd_id={})\n", pid, fd, pidfd_id);
    fd as u64
}
