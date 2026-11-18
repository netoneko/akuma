//! `signalfd4(2)`: a file descriptor that reads pending signals.
//!
//! The fd carries only its mask (`FileDescriptor::SignalFd { mask }`); the
//! signals are the calling thread's own pending set, the same one
//! `rt_sigtimedwait` consumes (`take_pending_signal`). A `read` takes up to
//! `count / 128` of the pending signals in the mask, one `signalfd_siginfo`
//! each; with none pending it parks (or `EAGAIN` for a non-blocking fd) and
//! gives up with `EINTR` when an unmasked signal must interrupt it.
//!
//! The reason it exists: bubblewrap's parent blocks `SIGCHLD` and watches its
//! cloned child through one (`docs/archive/BWRAP_ON_BOXES.md`).
//!
//! Divergence, pinned: the siginfo is `ssi_signo` plus, for `SIGCHLD`, `ssi_pid`
//! and `ssi_status` from the per-thread SIGCHLD side channel. Every other field
//! is zero. `SFD_*` flags are the open flags' numbers (`O_NONBLOCK`,
//! `O_CLOEXEC`).
use super::*;

/// `sizeof(struct signalfd_siginfo)`.
const SSI_SIZE: usize = 128;
const SFD_NONBLOCK: u32 = 0o4000;
const SFD_CLOEXEC: u32 = 0o2_000_000;
const SIGCHLD: u32 = 17;
/// SIGKILL and SIGSTOP cannot be blocked, so a signalfd cannot take them.
const UNMASKABLE: u64 = (1 << (9 - 1)) | (1 << (19 - 1));

/// Whether a signal in `mask` is pending on the calling thread.
pub fn signalfd_can_read(mask: u64) -> bool {
    let tid = akuma_exec::threading::current_thread_id();
    akuma_exec::threading::pending_signals_raw(tid) & mask != 0
}

/// One `signalfd_siginfo` for `sig`, as bytes.
fn record(sig: u32) -> [u8; SSI_SIZE] {
    let mut r = [0u8; SSI_SIZE];
    r[0..4].copy_from_slice(&sig.to_ne_bytes());
    if sig == SIGCHLD {
        let tid = akuma_exec::threading::current_thread_id();
        if let Some((pid, status)) = akuma_exec::threading::peek_last_sigchld(tid) {
            r[12..16].copy_from_slice(&pid.to_ne_bytes()); // ssi_pid
            r[40..44].copy_from_slice(&status.to_ne_bytes()); // ssi_status
        }
    }
    r
}

/// `read` on a signalfd, in syscall-return form.
pub fn signalfd_read(mask: u64, buf_ptr: u64, count: usize, nonblock: bool) -> u64 {
    if count < SSI_SIZE {
        return EINVAL;
    }
    loop {
        let mut written = 0usize;
        while written + SSI_SIZE <= count {
            // `take_pending_signal` takes the set of signals to *leave*, so
            // everything outside `mask`.
            let Some(sig) = akuma_exec::threading::take_pending_signal(!mask) else { break };
            if copy_to_user(buf_ptr + written as u64, &record(sig)).is_err() {
                return EFAULT;
            }
            written += SSI_SIZE;
        }
        if written > 0 {
            return written as u64;
        }
        if nonblock {
            return EAGAIN;
        }
        if akuma_exec::process::should_interrupt_blocking_syscall() {
            return EINTR;
        }
        // A pend wakes this thread; the cap keeps a missed wake bounded.
        let now = akuma_primitives::clock::uptime_us();
        akuma_exec::threading::schedule_blocking(now + 10_000);
    }
}

/// `signalfd4(fd, mask, sizemask, flags)`: `fd == -1` makes a new descriptor,
/// otherwise the mask of an existing signalfd is replaced.
pub(super) fn sys_signalfd4(fd: i32, mask_ptr: UserAddr, sizemask: usize, flags: u32) -> u64 {
    if sizemask != 8 || flags & !(SFD_NONBLOCK | SFD_CLOEXEC) != 0 {
        return EINVAL;
    }
    let mut mask: u64 = 0;
    if read_user_into(&mut mask, mask_ptr.get()).is_err() {
        return EFAULT;
    }
    let mask = mask & !UNMASKABLE;
    let Some(proc) = akuma_exec::process::current_process_shared() else { return ENOSYS };
    if fd == -1 {
        let new = proc.alloc_fd(akuma_exec::process::FileDescriptor::SignalFd { mask });
        if flags & SFD_CLOEXEC != 0 {
            proc.set_cloexec(new);
        }
        if flags & SFD_NONBLOCK != 0 {
            proc.set_nonblock(new);
        }
        return u64::from(new);
    }
    match proc.get_fd(fd as u32) {
        Some(akuma_exec::process::FileDescriptor::SignalFd { .. }) => {
            proc.set_fd(fd as u32, akuma_exec::process::FileDescriptor::SignalFd { mask });
            0
        }
        Some(_) => EINVAL,
        None => EBADF,
    }
}
