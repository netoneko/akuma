use super::*;
use akuma_primitives::{GuardToggle, ToggledGuard};
// Both of these are only used by the smoltcp socket read/write arms: the module
// alias, and `libc_errno` for the positive-form error a socket call returns. The
// negated forms this file returns to userspace come from `super::*`
// (`akuma_primitives::errno::negated`).
#[cfg(feature = "smoltcp")]
use akuma_net::socket::{self, libc_errno};

/// The `no-bkl-vfs` carve-out (Phase 4 of
/// docs/archive/BKL_FINE_GRAINED_LOCKING_PLAN.md), as a [`GuardToggle`] marker.
///
/// Correctness rests on the state these syscalls mutate already carrying its own
/// fine-grained locks — the per-process fd table (`SharedFdTable`'s `Spinlock`, every
/// access already wrapped in `with_irqs_disabled`), the mount table (`MOUNT_TABLE`,
/// released before any I/O in `with_fs`), the ext2 superblock/BGD
/// (`Ext2Filesystem::state` RwSpinlock), the block cache
/// (`Ext2Filesystem::block_cache` Spinlock), and the block device (`BLOCK_DEVICE`
/// Spinlock) — so the BKL is redundant for them; dropping it lets non-fs work on
/// other cores proceed in parallel.
///
/// The runtime toggle lets a boot image with the feature compiled in still A/B
/// against the BKL-held path without a rebuild; [`ToggledGuard`] states the latching
/// discipline that makes such a flip safe mid-syscall.
pub(super) struct VfsBkl;

impl GuardToggle for VfsBkl {
    const COMPILED_IN: bool = cfg!(all(kernel_smp_shared, kernel_no_bkl_vfs));
    #[inline]
    fn enabled() -> bool {
        #[cfg(kernel_smp_shared)]
        {
            crate::smp_shared::vfs_bkl_drop_enabled()
        }
        #[cfg(not(kernel_smp_shared))]
        {
            false
        }
    }
    #[inline]
    fn enter() {
        akuma_exec::bkl::dropped_window_open();
    }
    #[inline]
    fn exit() {
        akuma_exec::bkl::dropped_window_close();
    }
}

/// RAII guard that runs a VFS syscall **without** the Big Kernel Lock.
///
/// Constructed at the top of each fs syscall: `new()` DROPS the BKL so this core
/// runs the syscall concurrently with peer cores, and `drop()` RE-ACQUIRES it on
/// every return path, keeping the syscall wrapper's single `leave_kernel`
/// (`rust_sync_el0_handler` in exceptions.rs) balanced.
///
/// `new_if(on_disk)` takes the guard only where the on-disk work spans a whole
/// function rather than sitting in one `match` arm (`sys_write`'s per-chunk loop,
/// `sys_lseek`'s `update_fd` closure). It mirrors how `sys_sendto` routes pipe-backed
/// fds *before* taking `NetBklGuard`: an fd that isn't a real file (tty, pipe, socket,
/// eventfd, `/dev/null`) must keep the BKL, since its path touches terminal/pipe state
/// this carve-out has not audited.
pub(super) type VfsBklGuard = ToggledGuard<VfsBkl>;

/// The `no-bkl-drivers` carve-out (Phase 6 of
/// docs/archive/BKL_FINE_GRAINED_LOCKING_PLAN.md), as a [`GuardToggle`] marker.
///
/// Covers the device-driver syscall paths — `sys_getrandom`, the `DevUrandom` arm of
/// `sys_read`/`sys_pread64`, the `DevDsp` arm of `sys_write`, and
/// `sys_fb_init`/`sys_fb_draw`/`sys_fb_info`. Correctness rests on each driver's state
/// already carrying its own fine-grained Spinlock — `RNG_DEVICE` (virtio-rng,
/// `src/rng.rs`), `SOUND_DEVICE` (virtio-sound, `src/audio.rs`), and `FB_STATE`
/// (ramfb, `src/ramfb.rs`) — so the BKL is redundant for them. The block device
/// (`BLOCK_DEVICE`) and network device (`NETWORK`) are already BKL-free via
/// `no-bkl-vfs` / `no-bkl-network`; this window covers the remaining drivers.
///
/// All virtio devices in this kernel are **polling-based** (no virtio IRQ handlers are
/// registered — only the timer IRQ 27), so device I/O is a synchronous busy-wait under
/// the driver's Spinlock, not an interrupt-driven completion. The BKL-drop lets peer
/// cores run while this core polls. Like [`MmBkl`](super::mem::MmBkl), none of the
/// driver Spinlocks need to know a BKL-free window is calling them — their acquisition
/// is already unconditional.
pub(super) struct DriverBkl;

impl GuardToggle for DriverBkl {
    const COMPILED_IN: bool = cfg!(all(kernel_smp_shared, kernel_no_bkl_drivers));
    #[inline]
    fn enabled() -> bool {
        #[cfg(kernel_smp_shared)]
        {
            crate::smp_shared::drivers_bkl_drop_enabled()
        }
        #[cfg(not(kernel_smp_shared))]
        {
            false
        }
    }
    #[inline]
    fn enter() {
        akuma_exec::bkl::dropped_window_open();
    }
    #[inline]
    fn exit() {
        akuma_exec::bkl::dropped_window_close();
    }
}

/// RAII guard that runs a device-driver syscall **without** the Big Kernel Lock.
/// See [`DriverBkl`] for what makes that safe.
pub(super) type DriverBklGuard = ToggledGuard<DriverBkl>;

/// Bounded, always-on diagnostic counter for `read()` returning EBADF.
///
/// A process calling `read()` on a descriptor that is *absent* from its fd
/// table is abnormal and is the exact symptom of a fork/CLOEXEC-handshake
/// fd-table bug — e.g. rustc's libstd `fork`+exec child-spawn, where the parent
/// reads its O_CLOEXEC error pipe and panics with
/// `the CLOEXEC pipe failed: ... Bad file descriptor`. Logging the first few
/// occurrences (with pid/tid/fd and the precise reason) localizes which fd
/// disappeared from which process without flooding the console on programs that
/// legitimately probe closed fds. See `docs/RUST_TOOLCHAIN.md`.
static READ_EBADF_TRACE: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

fn trace_read_ebadf(reason: &str, fd: u64, buf_ptr: u64) {
    use core::sync::atomic::Ordering;
    if READ_EBADF_TRACE.fetch_add(1, Ordering::Relaxed) < 32 {
        let pid = akuma_exec::process::read_current_pid().unwrap_or(0);
        let tid = akuma_exec::threading::current_thread_id();
        crate::safe_print!(
            192,
            "[read-ebadf] {} pid={} tid={} fd={} buf={:#x}\n",
            reason, pid, tid, fd, buf_ptr,
        );
    }
}

pub fn fs_error_to_errno(e: crate::vfs::FsError) -> u64 {
    use crate::vfs::FsError;
    match e {
        FsError::NotFound => ENOENT,
        // Linux uses EACCES for filesystem permission errors on open/access/etc.;
        // EPERM is reserved for capability-style "operation not permitted".
        FsError::PermissionDenied => EACCES,
        FsError::AlreadyExists => EEXIST,
        FsError::NotADirectory => ENOTDIR,
        FsError::NotAFile => EISDIR,
        FsError::DirectoryNotEmpty => ENOTEMPTY,
        FsError::NoSpace => ENOSPC,
        FsError::ReadOnly => EROFS,
        FsError::InvalidPath => EINVAL,
        FsError::IoError => EIO,
        FsError::Internal => EIO,
        FsError::TooManyOpenFiles => EMFILE,
        _ => EIO,
    }
}

pub(super) fn resolve_path_at(dirfd: i32, raw_path: &str) -> String {
    if raw_path.starts_with('/') {
        return crate::vfs::canonicalize_path(raw_path);
    }
    let base = if dirfd == -100 {
        if let Some(proc) = akuma_exec::process::current_process_shared() {
            proc.cwd.clone()
        } else {
            String::from("/")
        }
    } else if dirfd >= 0 {
        if let Some(proc) = akuma_exec::process::current_process_shared() {
            if let Some(akuma_exec::process::FileDescriptor::File(f)) = proc.get_fd(dirfd as u32) {
                f.path
            } else {
                String::from("/")
            }
        } else {
            String::from("/")
        }
    } else {
        String::from("/")
    };
    if raw_path == "." || raw_path.is_empty() {
        base
    } else {
        crate::vfs::resolve_path(&base, raw_path)
    }
}

// `struct iovec`, `struct stat`, `struct statx_timestamp`, `struct statx` and
// `makedev` moved to `akuma-syscalls-linux` on 2026-08-27, along with the
// offset assertions that used to sit right here — they are the same
// assertions, checked by `cargo test` on the host instead of only by whatever
// build happened to compile this file. Re-exported so
// `crate::syscall::fs::Stat` (boot tests) and `super::fs::IoVec` (net.rs)
// keep their spelling.
pub use super::{IoVec, Stat, Statx, StatxTimestamp, makedev};


pub fn sys_read(fd_num: u64, buf_ptr: u64, count: usize) -> u64 {
    // Per-stage fixed-cost attribution (`read-profile`; ZST otherwise). Created
    // before the first line of real work and committed only on the `File` arm —
    // see `crate::syscall::utils::read_profile`.
    let mut rec = crate::syscall::utils::read_profile::Rec::new();
    if !validate_user_ptr(buf_ptr, count) {
        if crate::config::SYSCALL_DEBUG_PIPE_READ {
            let pid = akuma_exec::process::read_current_pid().unwrap_or(0);
            crate::tprint!(
                192,
                "[pipe-read] EFAULT validate ptr pid={} fd={} buf={:#x} cnt={}\n",
                pid,
                fd_num,
                buf_ptr,
                count,
            );
        }
        return EFAULT;
    }
    rec.lap(crate::syscall::utils::read_profile::S_VALIDATE);
    // Deliberately scoped: `current_process_shared()` hands out `&'static Process`, but
    // that lifetime is not one the process table can honour across a blocking park. The
    // RETIRED + `PROCESS_RECLAIM_COOLDOWN_US` scheme bounds a *lookup-then-use* span of
    // microseconds; several arms below park in `schedule_blocking(u64::MAX)` for as long
    // as a pipe stays empty, and a peer core can retire and free the slot in between. So
    // bind the reference only long enough to clone the fd entry out of the table, keep
    // the pid as a scalar for the trace paths, and re-resolve at the few derefs that can
    // be reached after a park. See BKL_PHASE7F_OPTOUT_LIST.md §4.2.
    let (pid, fd) = {
        let Some(proc) = akuma_exec::process::current_process_shared() else {
            trace_read_ebadf("no-current-process", fd_num, buf_ptr);
            return EBADF;
        };
        let Some(fd) = proc.get_fd(fd_num as u32) else {
            trace_read_ebadf("fd-not-in-table", fd_num, buf_ptr);
            return EBADF;
        };
        (proc.pid, fd)
    };
    rec.lap(crate::syscall::utils::read_profile::S_FD);

    if crate::config::SYSCALL_DEBUG_INFO_ENABLED && fd_num == 0 {
        crate::safe_print!(128, "[syscall] read(stdin, count={})\n", count);
    }

    match fd {
        // `/dev/tty` reads take the exact Stdin path: same channel, same line
        // discipline, same canonical/echo handling. A pager holding both fd 0
        // (the content pipe) and a /dev/tty fd gets keyboard bytes here and
        // only here.
        akuma_exec::process::FileDescriptor::Stdin
        | akuma_exec::process::FileDescriptor::DevTty => {
            if akuma_exec::process::current_channel().is_none() {
                let mut temp = alloc::vec![0u8; count];
                let Some(proc) = akuma_exec::process::current_process_shared() else { return EBADF };
                let n = proc.read_stdin(&mut temp);
                if n > 0
                    && copy_to_user(buf_ptr, &temp[..n]).is_err() {
                        return EFAULT;
                    }
                if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
                    crate::safe_print!(128, "[syscall] read(stdin) fallback returned {}\n", n);
                }
                return n as u64;
            }

            let mut kernel_buf = alloc::vec![0u8; count];

            loop {
                // Re-resolve the channel every iteration rather than reusing the
                // `ch` captured before the loop: `box grab`/`sys_reattach` can
                // repoint this process's channel to a new one (e.g. a different
                // SSH session's) while a read is already parked here. Reusing the
                // stale `Arc` meant the wake fired correctly (the waker lives on
                // `terminal_state`, unaffected by reattach) but this loop kept
                // reading the abandoned old channel, which never receives the
                // reattached session's input — the reattached process looked
                // permanently hung despite `write_to_process_stdin` reporting
                // bytes accepted. See docs/archive/KNOWN_ISSUES.md #4.
                let ch = match akuma_exec::process::current_channel() {
                    Some(c) => c,
                    None => return 0,
                };

                // `is_pipe` selects raw pass-through (no canonical line
                // discipline, no echo) over cooked terminal input. A spawned
                // child whose stdin is a pipe (non-terminal channel — e.g. the
                // SSH-into-box bridge feeding commands to busybox) must NOT have
                // its input echoed/line-buffered as if it were a tty; that
                // corrupts the command stream. Treat non-terminal channels (and
                // closed stdin) as a raw pipe; only a real terminal is cooked.
                let is_pipe = ch.is_stdin_closed() || !ch.is_terminal();

                if !is_pipe {
                    let term_state_lock = akuma_exec::process::current_terminal_state();
                    if let Some(ref ts_lock) = term_state_lock {
                        let mut ts = ts_lock.lock();
                        if ts.is_canonical() && !ts.canon_ready.is_empty() {
                            let ready = ts.drain_canon_ready(count);
                            let to_read = ready.len();
                            kernel_buf[..to_read].copy_from_slice(&ready);
                            drop(ts);
                            if copy_to_user(buf_ptr, &kernel_buf[..to_read]).is_err() {
                                return EFAULT;
                            }
                            if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
                                crate::safe_print!(128, "[syscall] read(stdin) returned {} bytes from canon_ready\n", to_read);
                            }
                            return to_read as u64;
                        }
                    }
                }

                let n = ch.read_stdin(&mut kernel_buf);
                if n > 0 {
                    // Re-arm the `EPOLLET` `EPOLLIN` edge on every successful
                    // read, exactly like the `PipeRead`/`UnixSocket`/`Socket`
                    // arms above (see `epoll_on_fd_drained`'s doc comment and
                    // `docs/archive/TOKIO_PIPE_EPOLL_HANG.md`). This arm never
                    // called it before — an edge-triggered reader (mio, which
                    // is what crossterm's default backend uses to watch this
                    // exact fd for keyboard input) that drains this read and
                    // goes back to `epoll_wait` would see `new_bits == 0` for
                    // the *next* keystroke's edge and never wake for it.
                    super::poll::epoll_on_fd_drained(fd_num as u32);
                    if !is_pipe {
                        let term_state_lock = akuma_exec::process::current_terminal_state();
                        if let Some(ref ts_lock) = term_state_lock {
                            let mut ts = ts_lock.lock();

                            ts.map_cr_to_nl(&mut kernel_buf[..n]);

                            if ts.is_canonical() {
                                let result = ts.process_canon_input(&kernel_buf[..n]);
                                if !result.echo.is_empty() {
                                    ch.write(&result.echo);
                                }
                                if result.eof {
                                    drop(ts);
                                    return 0;
                                }

                                if !ts.canon_ready.is_empty() {
                                    let ready = ts.drain_canon_ready(count);
                                    let to_read = ready.len();
                                    kernel_buf[..to_read].copy_from_slice(&ready);
                                    drop(ts);
                                    if copy_to_user(buf_ptr, &kernel_buf[..to_read]).is_err() {
                                        return EFAULT;
                                    }
                                    if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
                                        crate::safe_print!(128, "[syscall] read(stdin) returned {} bytes (canonical)\n", to_read);
                                    }
                                    return to_read as u64;
                                }
                                continue;
                            } else if let Some(echo_buf) = ts.echo_noncanon(&kernel_buf[..n]) {
                                if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
                                    crate::safe_print!(128, "[syscall] read: echoing {} bytes\n", echo_buf.len());
                                }
                                ch.write(&echo_buf);
                            }
                        }
                    }

                    if copy_to_user(buf_ptr, &kernel_buf[..n]).is_err() {
                        return EFAULT;
                    }
                    if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
                        let mut snippet = [0u8; 32];
                        let sn_len = n.min(32);
                        snippet[..sn_len].copy_from_slice(&kernel_buf[..sn_len]);
                        for byte in &mut snippet[..sn_len] {
                            if *byte < 32 || *byte > 126 { *byte = b'.'; }
                        }
                        let snippet_str = core::str::from_utf8(&snippet[..sn_len]).unwrap_or("...");
                        crate::safe_print!(128, "[syscall] read(stdin) returned {} bytes \"{}\"\n", n, snippet_str);
                    }
                    return n as u64;
                }

                if ch.is_stdin_closed() {
                    if !is_pipe {
                        let term_state_lock = akuma_exec::process::current_terminal_state();
                        if let Some(ref ts_lock) = term_state_lock {
                            let mut ts = ts_lock.lock();
                            if ts.is_canonical() && !ts.canon_buffer.is_empty() {
                                ts.flush_canon_buffer();
                                let ready = ts.drain_canon_ready(count);
                                let to_read = ready.len();
                                kernel_buf[..to_read].copy_from_slice(&ready);
                                drop(ts);
                                if copy_to_user(buf_ptr, &kernel_buf[..to_read]).is_err() {
                                    return EFAULT;
                                }
                                return to_read as u64;
                            }
                        }
                    }
                    if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
                        crate::safe_print!(128, "[syscall] read(stdin) returned 0 (EOF)\n");
                    }
                    return 0;
                }

                if akuma_exec::process::should_interrupt_blocking_syscall() {
                    return EINTR;
                }

                // Every other read arm (`PipeRead`, `UnixSocket`, `Socket`)
                // honours `O_NONBLOCK` here; this one never did — a caller
                // that set it (mio, for the same reason crossterm needs
                // `EPOLLET` semantics to work at all: an edge-triggered
                // reader must be able to drain to `EAGAIN`) got parked in
                // `schedule_blocking(u64::MAX)` regardless, indistinguishable
                // from a real hang. Re-arm the edge before handing back
                // `EAGAIN`, exactly as `PipeRead` does.
                if super::net::fd_is_nonblock(fd_num as u32) {
                    super::poll::epoll_on_fd_drained(fd_num as u32);
                    return EAGAIN;
                }

                let term_state_lock = if let Some(state) = akuma_exec::process::current_terminal_state() { state } else {
                    if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
                        crate::safe_print!(128, "[syscall] read(stdin) no terminal state, EOF\n");
                    }
                    return 0;
                };

                // `lock_bounded` disables preemption only for a single `try_lock`
                // attempt, not across the whole (possibly contended, possibly
                // unbounded) wait — see docs/archive/TERM_POLL_INPUT_PREEMPTION_FIX.md
                // §9-§10.
                {
                    let thread_id = akuma_exec::threading::current_thread_id();
                    akuma_exec::sync::lock_bounded(&term_state_lock)
                        .set_input_waker(akuma_exec::threading::get_waker_for_thread(thread_id));
                }

                // Re-check AFTER registering the waker to close a lost-wakeup race:
                // stdin may have been closed (or data delivered) between the checks
                // above and registering the waker, in which case the wake already
                // fired to a not-yet-registered waker. Without this, a reader that
                // races `close_process_stdin` (e.g. `cat` over `ssh host cmd` when
                // the client closes stdin) parks forever. Re-loop instead of parking.
                if ch.is_stdin_closed() || ch.has_stdin_data() {
                    akuma_exec::sync::lock_bounded(&term_state_lock).input_waker.lock().take();
                    continue;
                }

                akuma_exec::threading::schedule_blocking(u64::MAX);

                akuma_exec::sync::lock_bounded(&term_state_lock).input_waker.lock().take();
            }
        }
        akuma_exec::process::FileDescriptor::File(ref f) => {
            // read(2) on a real file is the hot on-disk path — run it BKL-free, exactly
            // like the Socket arm below runs BKL-free under `no-bkl-network`. Scoped to
            // THIS arm on purpose: the Stdin arm parks in `schedule_blocking` while taking
            // non-IRQ-masked terminal-state locks, which this carve-out has not audited.
            let _vfs_bkl = VfsBklGuard::new();
            rec.lap(crate::syscall::utils::read_profile::S_BKL);
            let limit = 64 * 1024;
            let to_read = count.min(limit);
            let mut temp = alloc::vec![0u8; to_read];
            rec.lap(crate::syscall::utils::read_profile::S_ALLOC);

            let read_result = crate::fs::read_at_open_file(&f.path, f.mount_id(), f.inode(), f.position, &mut temp);
            rec.lap(crate::syscall::utils::read_profile::S_FS);
            match read_result {
                Ok(n) => {
                    if n > 0 {
                        if copy_to_user(buf_ptr, &temp[..n]).is_err() {
                            return EFAULT;
                        }
                        rec.lap(crate::syscall::utils::read_profile::S_COPY);
                        if let Some(proc) = akuma_exec::process::current_process_shared() {
                            proc.update_fd(fd_num as u32, |entry| if let akuma_exec::process::FileDescriptor::File(file) = entry { file.position += n; });
                        }
                        rec.lap(crate::syscall::utils::read_profile::S_POS);
                    }
                    if crate::config::SYSCALL_DEBUG_IO_ENABLED {
                        crate::safe_print!(256, "[syscall] read(fd={}, file={}, pos={}, req={}) = {}\n", fd_num, &f.path, f.position, to_read, n);
                    }
                    rec.commit(to_read);
                    n as u64
                }
                Err(e) => fs_error_to_errno(e)
            }
        }
        akuma_exec::process::FileDescriptor::BlockDev { idx, pos, .. } => {
            // Raw block read (`proposals/RAW_BLOCK_DEVICE_FD.md`) — same BKL
            // discipline as the `File` arm above, since it's the same
            // underlying disk I/O `crate::vfs::ext2` drives.
            let _vfs_bkl = VfsBklGuard::new();
            let capacity = crate::block::with_device_at(idx as usize, akuma_virtio::block::VirtioBlockDevice::capacity_bytes).unwrap_or(0);
            if pos >= capacity {
                0 // EOF: dd reading past the end of the device
            } else {
                let to_read = count.min((capacity - pos) as usize).min(64 * 1024);
                let mut temp = alloc::vec![0u8; to_read];
                match crate::block::read_bytes_at(idx as usize, pos, &mut temp) {
                    Ok(()) => {
                        if copy_to_user(buf_ptr, &temp).is_err() {
                            return EFAULT;
                        }
                        if let Some(proc) = akuma_exec::process::current_process_shared() {
                            proc.update_fd(fd_num as u32, |entry| if let akuma_exec::process::FileDescriptor::BlockDev { pos, .. } = entry { *pos += to_read as u64; });
                        }
                        to_read as u64
                    }
                    Err(_) => EIO,
                }
            }
        }
        #[cfg(feature = "smoltcp")]
        akuma_exec::process::FileDescriptor::Socket(idx) => {
            // read(2) on a TCP socket is sshd's hot recv path — run it BKL-free
            // like recvfrom (no-op guard unless no-bkl-network). `temp` is a
            // kernel bounce buffer; the copy_to_user below runs after the socket
            // ops, outside the socket locks.
            let _net_bkl = super::net::NetBklGuard::new();
            let limit = 64 * 1024;
            let to_read = count.min(limit);
            let mut temp = alloc::vec![0u8; to_read];
            let nonblock = super::net::fd_is_nonblock(fd_num as u32);
            let result = if socket::is_udp_socket(idx) {
                socket::socket_recv_udp(idx, &mut temp, nonblock).map(|(n, _)| n)
            } else {
                socket::socket_recv(idx, &mut temp, nonblock)
            };
            if crate::config::SYSCALL_DEBUG_NET_ENABLED {
                match &result {
                    Ok(n) => crate::tprint!(128, "[sock] read fd={} req={} got={}\n", fd_num, count, n),
                    Err(e) if *e == akuma_net::socket::libc_errno::EAGAIN => {
                        crate::tprint!(64, "[sock] read fd={} EAGAIN (drained)\n", fd_num);
                    }
                    Err(e) => crate::tprint!(128, "[sock] read fd={} err={}\n", fd_num, i64::from(*e)),
                }
            }
            match result {
                Ok(n) => {
                    if n > 0
                        && copy_to_user(buf_ptr, &temp[..n]).is_err() {
                            return EFAULT;
                        }
                    // Reset EPOLLET edge after every successful TCP read. Go (and other callers
                    // using read() rather than recvfrom/recvmsg) do not always drain to EAGAIN
                    // before going back to epoll. Without this reset the EPOLLET "last_ready"
                    // stays set to EPOLLIN so the next poll sees new_bits=0 and fires no event,
                    // leaving the socket unread even though more data is buffered.
                    if !socket::is_udp_socket(idx) {
                        super::poll::epoll_on_fd_drained(fd_num as u32);
                    }
                    n as u64
                }
                Err(e) => {
                    if e == akuma_net::socket::libc_errno::EAGAIN {
                        // Socket was drained — reset EPOLLET edge so next data arrival fires EPOLLIN.
                        super::poll::epoll_on_fd_drained(fd_num as u32);
                    }
                    (-i64::from(e)) as u64
                }
            }
        }
        akuma_exec::process::FileDescriptor::ChildStdout(child_pid) => {
            if let Some(ch) = akuma_exec::process::get_child_channel(child_pid) {
                let mut temp = alloc::vec![0u8; count];
                let nonblock = super::net::fd_is_nonblock(fd_num as u32);
                loop {
                    let n = ch.read(&mut temp);
                    if n > 0 {
                        if copy_to_user(buf_ptr, &temp[..n]).is_err() {
                            return EFAULT;
                        }
                        return n as u64;
                    }
                    if ch.has_exited() {
                        return 0; // EOF
                    }
                    if nonblock {
                        return EAGAIN;
                    }
                    if akuma_exec::process::should_interrupt_blocking_syscall() {
                        return EINTR;
                    }

                    // Block until data arrives or process exits
                    akuma_exec::threading::schedule_blocking(u64::MAX);
                }
            }
            EBADF
        }
        // PipeRead: forktest_parent drains child stdout; correlate **`[pipe-read]`** / EFAULT with
        // **`[sigsegv-syscall] x8=63`** (`read`) — **`GO_FORKTEST_DEBUG.md`** Pattern 2.
        akuma_exec::process::FileDescriptor::PipeRead(pipe_id) => {
            let mut temp = alloc::vec![0u8; count];
            if crate::config::SYSCALL_DEBUG_PIPE_READ {
                use core::sync::atomic::{AtomicU64, Ordering};
                static PIPE_READ_TRACE_SEQ: AtomicU64 = AtomicU64::new(0);
                let sample = crate::config::SYSCALL_DEBUG_PIPE_READ_SAMPLE.max(1);
                let seq = PIPE_READ_TRACE_SEQ.fetch_add(1, Ordering::Relaxed);
                if seq.is_multiple_of(sample) {
                    let tid = akuma_exec::threading::current_thread_id();
                    crate::tprint!(
                        224,
                        "[pipe-read] enter pid={} tid={} fd={} pipe={} buf={:#x} cnt={}\n",
                        pid,
                        tid,
                        fd_num,
                        pipe_id,
                        buf_ptr,
                        count,
                    );
                }
            }
            // O_NONBLOCK was ignored on this arm until 2026-08-17 — the sibling
            // `ChildStdout` and `UnixSocket` arms both honoured it, this one
            // parked in `schedule_blocking(u64::MAX)` instead. A reactor thread
            // that read a drained-but-still-open pipe therefore blocked *inside
            // the kernel* until the writer closed, stalling the whole runtime.
            // See `docs/archive/TOKIO_PIPE_EPOLL_HANG.md`.
            let nonblock = super::net::fd_is_nonblock(fd_num as u32);
            loop {
                let (n, eof) = super::pipe::pipe_read(pipe_id, &mut temp);
                if n > 0 {
                    if copy_to_user(buf_ptr, &temp[..n]).is_err() {
                        if crate::config::SYSCALL_DEBUG_PIPE_READ {
                            crate::tprint!(
                                224,
                                "[pipe-read] EFAULT copy_to_user pid={} fd={} pipe={} buf={:#x} copy_len={}\n",
                                pid,
                                fd_num,
                                pipe_id,
                                buf_ptr,
                                n,
                            );
                        }
                        return EFAULT;
                    }
                    // Re-arm the `EPOLLET` `EPOLLIN` edge, exactly as the socket
                    // read paths do (`epoll_on_fd_drained`'s own doc comment
                    // describes this hang for `EPOLLOUT`). Callers that do not
                    // drain to EAGAIN before returning to `epoll_pwait` — which
                    // is every `read_to_end` — would otherwise leave `last_ready`
                    // holding `EPOLLIN` and never see another edge.
                    super::poll::epoll_on_fd_drained(fd_num as u32);
                    return n as u64;
                }
                if eof {
                    return 0;
                }
                if nonblock {
                    // Drained with the writer still open: re-arm before handing
                    // back EAGAIN, or the arrival that follows fires no edge.
                    super::poll::epoll_on_fd_drained(fd_num as u32);
                    return EAGAIN;
                }
                if akuma_exec::process::should_interrupt_blocking_syscall() {
                    return EINTR;
                }
                let tid = akuma_exec::threading::current_thread_id();
                if !super::pipe::pipe_check_set_reader(pipe_id, tid) {
                    akuma_exec::threading::schedule_blocking(u64::MAX);
                }
            }
        }
        // AF_UNIX socketpair endpoint: read from this endpoint's `rx` pipe.
        akuma_exec::process::FileDescriptor::UnixSocket { rx, .. } => {
            let nonblock = super::net::fd_is_nonblock(fd_num as u32);
            let mut temp = alloc::vec![0u8; count];
            loop {
                let (n, eof) = super::pipe::pipe_read(rx, &mut temp);
                if n > 0 {
                    if copy_to_user(buf_ptr, &temp[..n]).is_err() {
                        return EFAULT;
                    }
                    // Same edge re-arm as the `PipeRead` arm — a socketpair is
                    // two pipes, and tokio's signal driver watches one of them
                    // edge-triggered.
                    super::poll::epoll_on_fd_drained(fd_num as u32);
                    return n as u64;
                }
                if eof {
                    return 0;
                }
                if nonblock {
                    super::poll::epoll_on_fd_drained(fd_num as u32);
                    return EAGAIN;
                }
                if akuma_exec::process::should_interrupt_blocking_syscall() {
                    return EINTR;
                }
                let tid = akuma_exec::threading::current_thread_id();
                if !super::pipe::pipe_check_set_reader(rx, tid) {
                    akuma_exec::threading::schedule_blocking(u64::MAX);
                }
            }
        }
        #[cfg(feature = "sc-eventfd")]
        akuma_exec::process::FileDescriptor::EventFd(efd_id) => {
            if count < 8 { return EINVAL; }
            let nonblock = super::eventfd::eventfd_is_nonblock(efd_id) || super::net::fd_is_nonblock(fd_num as u32);
            loop {
                if let Ok(val) = super::eventfd::eventfd_read(efd_id) {
                    let temp = val.to_ne_bytes();
                    if copy_to_user(buf_ptr, &temp).is_err() {
                        return EFAULT;
                    }
                    return 8;
                }
                if nonblock { return EAGAIN; }
                if akuma_exec::process::should_interrupt_blocking_syscall() { return EINTR; }
                let tid = akuma_exec::threading::current_thread_id();
                super::eventfd::eventfd_add_poller(efd_id, tid);
                akuma_exec::threading::schedule_blocking(u64::MAX);
            }
        }
        akuma_exec::process::FileDescriptor::DevNull => 0,
        akuma_exec::process::FileDescriptor::DevZero => {
            // /dev/zero: fill the user buffer with zero bytes and return count.
            let temp = alloc::vec![0u8; count];
            if copy_to_user(buf_ptr, &temp).is_err() {
                return EFAULT;
            }
            count as u64
        }
        akuma_exec::process::FileDescriptor::DevUrandom => {
            let mut temp = alloc::vec![0u8; count];
            let _drv_bkl = DriverBklGuard::new();
            if crate::rng::fill_bytes(&mut temp).is_ok() {
                if copy_to_user(buf_ptr, &temp).is_err() {
                    return EFAULT;
                }
                count as u64
            } else {
                EIO
            }
        }
        #[cfg(feature = "rump")]
        akuma_exec::process::FileDescriptor::Tap { nonblock } => {
            // Pull one L2 frame. O_NONBLOCK → EAGAIN when none ready; otherwise
            // BLOCK cooperatively (yield to the scheduler, re-poll the tap) until a
            // frame arrives — so the rump virtif RX thread does a plain blocking
            // read() with no busy-wait. Akuma's net is poll-based (no RX IRQ), so
            // this mirrors how socket recv blocks (akuma-net wait_until).
            let mut temp = alloc::vec![0u8; count];
            let got = if nonblock {
                akuma_net::rump_tap::read_frame(&mut temp)
            } else {
                // Block until a frame (None timeout); None only on interrupt → EAGAIN.
                akuma_net::rump_tap::read_frame_blocking(&mut temp, None)
            };
            match got {
                Some(n) => {
                    if n > 0
                        && copy_to_user(buf_ptr, &temp[..n]).is_err() {
                            return EFAULT;
                        }
                    n as u64
                }
                None => EAGAIN,
            }
        }
        #[cfg(feature = "sc-timerfd")]
        akuma_exec::process::FileDescriptor::TimerFd(timer_id) => {
            let result = super::timerfd::timerfd_read(timer_id);
            if result == EAGAIN { return EAGAIN; }
            if count >= 8 && validate_user_ptr(buf_ptr, 8) {
                let temp = result.to_ne_bytes();
                if copy_to_user(buf_ptr, &temp).is_err() {
                    return EFAULT;
                }
                8
            } else { EINVAL }
        }
        #[cfg(feature = "sc-epoll")]
        akuma_exec::process::FileDescriptor::EpollFd(_) => EINVAL,
        // Catch-all for fd types that don't support read(2) — Linux returns EBADF.
        // This fires when an fd *exists* in the table but its type can't be read
        // (e.g. a PipeWrite end, a raw Socket, Stderr). A libstd spawn parent
        // reading its CLOEXEC handshake pipe should never land here; if it does,
        // it points at a wrong-direction/wrong-type fd (see RUST_TOOLCHAIN.md §4).
        other => {
            trace_read_ebadf("fd-type-not-readable", fd_num, buf_ptr);
            let _ = other;
            EBADF
        }
    }
}

pub(super) fn sys_pread64(fd_num: u32, buf_ptr: u64, count: usize, offset: i64) -> u64 {
    if offset < 0 { return EINVAL; }
    if !validate_user_ptr(buf_ptr, count) { return EFAULT; }
    let proc = match akuma_exec::process::current_process_shared() { Some(p) => p, None => return EBADF };
    let fd = match proc.get_fd(fd_num) { Some(e) => e, None => return EBADF };

    match fd {
        akuma_exec::process::FileDescriptor::File(ref f) => {
            let _vfs_bkl = VfsBklGuard::new();
            let limit = 64 * 1024;
            let to_read = count.min(limit);
            let mut temp = alloc::vec![0u8; to_read];
            match crate::fs::read_at_open_file(&f.path, f.mount_id(), f.inode(), offset as usize, &mut temp) {
                Ok(n) => {
                    if n > 0
                        && copy_to_user(buf_ptr, &temp[..n]).is_err() {
                            return EFAULT;
                        }
                    if crate::config::SYSCALL_DEBUG_IO_ENABLED {
                        crate::safe_print!(256, "[syscall] pread64(fd={}, file={}, off={}, req={}) = {}\n", fd_num, &f.path, offset, to_read, n);
                    }
                    n as u64
                }
                Err(e) => fs_error_to_errno(e)
            }
        }
        akuma_exec::process::FileDescriptor::DevNull => 0,
        akuma_exec::process::FileDescriptor::DevZero => {
            let temp = alloc::vec![0u8; count];
            if copy_to_user(buf_ptr, &temp).is_err() {
                return EFAULT;
            }
            count as u64
        }
        akuma_exec::process::FileDescriptor::DevUrandom => {
            let mut temp = alloc::vec![0u8; count];
            let _drv_bkl = DriverBklGuard::new();
            if crate::rng::fill_bytes(&mut temp).is_ok() {
                if copy_to_user(buf_ptr, &temp).is_err() {
                    return EFAULT;
                }
                count as u64
            } else {
                EIO
            }
        }
        akuma_exec::process::FileDescriptor::TimerFd(_) => EAGAIN,
        akuma_exec::process::FileDescriptor::EpollFd(_) => EINVAL,
        _ => EBADF
    }
}

pub(super) fn sys_pwrite64(fd_num: u32, buf_ptr: u64, count: usize, offset: i64) -> u64 {
    if offset < 0 { return EINVAL; }
    if !validate_user_ptr(buf_ptr, count) { return EFAULT; }
    let proc = match akuma_exec::process::current_process_shared() { Some(p) => p, None => return EBADF };
    let fd = match proc.get_fd(fd_num) { Some(e) => e, None => return EBADF };

    match fd {
        akuma_exec::process::FileDescriptor::File(ref f) => {
            let _vfs_bkl = VfsBklGuard::new();
            let mut buf = alloc::vec![0u8; count];
            if copy_from_user(&mut buf, buf_ptr).is_err() {
                return EFAULT;
            }
            match crate::fs::write_at(&f.path, offset as usize, &buf) {
                Ok(n) => n as u64,
                Err(e) => fs_error_to_errno(e)
            }
        }
        akuma_exec::process::FileDescriptor::DevNull | akuma_exec::process::FileDescriptor::DevUrandom | akuma_exec::process::FileDescriptor::DevZero => count as u64,
        _ => EBADF
    }
}

pub(super) fn sys_write(fd_num: u64, buf_ptr: u64, count: usize) -> u64 {
    if !validate_user_ptr(buf_ptr, count) { return EFAULT; }
    // Scoped for the same reason as `sys_read`'s: the Stdout/pipe/unix arms below park in
    // `schedule_blocking(u64::MAX)` inside the per-chunk loop, and a `&'static Process`
    // may not outlive a park (BKL_PHASE7F_OPTOUT_LIST.md §4.2). Keep the pid as a scalar
    // for the trace paths; re-resolve at the derefs that can follow a park.
    let (pid, fd) = {
        let Some(proc) = akuma_exec::process::current_process_shared() else { return EBADF };
        match proc.get_fd(fd_num as u32) { Some(e) => (proc.pid, e), None => return EBADF }
    };

    // write(2) to a real file runs BKL-free. Unlike `sys_read`, the on-disk work here
    // isn't confined to one `match` arm — the match is *inside* the per-chunk loop, and
    // the O_APPEND `file_size` probe below already hits the VFS — so the guard spans the
    // function and is armed only for `File` fds (`new_if`). Every other fd kind (tty,
    // pipe, socket, /dev/*) keeps the BKL.
    let _vfs_bkl = VfsBklGuard::new_if(matches!(fd, akuma_exec::process::FileDescriptor::File(_) | akuma_exec::process::FileDescriptor::BlockDev { .. }));

    // For File descriptors, reserve the initial position now (before the loop) —
    // atomically read-and-advance, not a clone-then-write-back-later. `fd` is a
    // clone (`proc.get_fd()` above), so its own `.position` is only a snapshot;
    // two threads racing `write()` on the same fd (`CLONE_FILES` — any pair of
    // `clone_thread` siblings) could both read that same stale position and
    // corrupt each other's writes on disk. `reserve_write_pos` closes the gap by
    // reading and advancing the shared position in one lock hold, before any I/O
    // — see its doc comment for the reproduction. O_APPEND is unchanged: its
    // position is still derived from the live file size per write (a separate,
    // pre-existing race this does not address).
    let mut write_pos = if let akuma_exec::process::FileDescriptor::File(ref f) = fd {
        if f.flags & akuma_exec::process::open_flags::O_APPEND != 0 {
            crate::fs::file_size(&f.path).unwrap_or(0) as usize
        } else {
            match akuma_exec::process::current_process_shared() {
                Some(p) => p.reserve_write_pos(fd_num as u32, count).unwrap_or(f.position),
                None => f.position,
            }
        }
    } else if let akuma_exec::process::FileDescriptor::BlockDev { pos, .. } = fd {
        // No `reserve_write_pos`-style upfront reservation — the one intended
        // consumer (`dd` onto the drop-off drive, `proposals/RAW_BLOCK_DEVICE_FD.md`)
        // is a single sequential writer; two processes racing writable fds on
        // the same unmounted device is unguarded, same as Linux raw block I/O.
        pos as usize
    } else {
        0
    };

    let chunk_size = count.min(64 * 1024);
    let mut kernel_buf = alloc::vec![0u8; chunk_size];
    let mut total_written = 0;
    
    while total_written < count {
        let remaining = count - total_written;
        let this_chunk = remaining.min(chunk_size);
        
        if copy_from_user(&mut kernel_buf[..this_chunk], buf_ptr + total_written as u64).is_err() {
            if total_written > 0 { return total_written as u64; }
            return EFAULT;
        }
        
        let buf_slice = &kernel_buf[..this_chunk];
        
        let written = match fd {
            akuma_exec::process::FileDescriptor::Stdout | akuma_exec::process::FileDescriptor::Stderr
            | akuma_exec::process::FileDescriptor::DevTty => {
                if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
                    if total_written == 0 {
                      crate::safe_print!(96, "[OUT] pid={} fd={} len={}\n", pid, fd_num, count);
                    } else {
                    let display_len = this_chunk.min(64);
                    let mut snippet = [0u8; 64];
                    let n = display_len.min(snippet.len());
                    snippet[..n].copy_from_slice(&buf_slice[..n]);
                    for byte in &mut snippet[..n] {
                        if *byte < 32 || *byte > 126 { *byte = b'.'; }
                    }
                    let snippet_str = core::str::from_utf8(&snippet[..n]).unwrap_or("...");
                    crate::tprint!(192, "[OUT] pid={} fd={} len={} \"{}\"\n", pid, fd_num, count, snippet_str);
                    }
                }

                if let Some(ch) = akuma_exec::process::current_channel() {
                    let translated_buf;
                    let data_to_write: &[u8] = if ch.is_stdin_closed() {
                        buf_slice
                    } else if let Some(ts_lock) = akuma_exec::process::current_terminal_state() {
                        translated_buf = ts_lock.lock().translate_output(buf_slice);
                        &translated_buf
                    } else {
                        buf_slice
                    };

                    if ch.is_terminal() {
                        ch.write(data_to_write);
                    } else {
                        // Exec-channel (non-PTY) child: real backpressure instead of
                        // ProcessChannel::write's drop-oldest scrollback semantics — a
                        // producer that outruns the SSH bridge must block, not silently
                        // lose the middle of its output (see
                        // EXEC_CHANNEL_LARGE_OUTPUT_TRUNCATION.md). Write everything
                        // before returning rather than reporting a short write, since
                        // `data_to_write` may be a translated (expanded) copy that no
                        // longer lines up 1:1 with `this_chunk`, the raw byte count
                        // write(2) needs back — mirrors `pipe_write_all_blocking`.
                        let mut off = 0usize;
                        while off < data_to_write.len() {
                            let n = ch.write_bounded(&data_to_write[off..]);
                            if n > 0 {
                                off += n;
                                continue;
                            }
                            if akuma_exec::process::should_interrupt_blocking_syscall() {
                                if total_written > 0 { return total_written as u64; }
                                return EINTR;
                            }
                            let tid = akuma_exec::threading::current_thread_id();
                            if !ch.check_set_writer(tid) {
                                akuma_exec::threading::schedule_blocking(u64::MAX);
                            }
                        }
                    }
                }
                
                if crate::config::STDOUT_TO_KERNEL_LOG_COPY_ENABLED {
                    // Re-resolved: the backpressure loop just above can have parked.
                    if let Some(proc) = akuma_exec::process::current_process_shared() {
                        proc.write_stdout(buf_slice);
                    }
                }

                this_chunk as u64
            }
            akuma_exec::process::FileDescriptor::File(ref f) => {
                let is_append = f.flags & akuma_exec::process::open_flags::O_APPEND != 0;
                match crate::fs::write_at(&f.path, write_pos, buf_slice) {
                    Ok(0) => {
                        // The chunk was non-empty, so zero accepted means a bounded
                        // sink that is currently full. On-disk files never land
                        // here (they report `NoSpace` as an error instead); the one
                        // producer is `/proc/<pid>/fd/0`, whose stdin buffer stopped
                        // dropping bytes to make room. Report progress if there is
                        // any, else EAGAIN — never fall through, because `written`
                        // of 0 leaves `total_written < count` and spins this loop
                        // forever inside the kernel.
                        //
                        // EAGAIN rather than blocking even on a blocking fd: the
                        // only in-tree writer is sshd's `bridge_process`, one loop
                        // that must keep draining the child's stdout in the same
                        // iteration to make the space it is waiting for. Parking it
                        // here is precisely the deadlock its own "make BOTH ends
                        // non-blocking" comment was written to avoid.
                        if total_written > 0 { return total_written as u64; }
                        return EAGAIN;
                    }
                    Ok(n) => {
                        write_pos += n;
                        // O_APPEND still needs `.position` published per chunk (its
                        // start point is re-derived from the live file size on the
                        // *next* write, not from this field, but readers like
                        // `lseek(SEEK_CUR)`/`fstat` expect it current). The plain
                        // case does NOT write `.position` here: `reserve_write_pos`
                        // already advanced it by the FULL `count` up front, before
                        // this chunk loop started, so doing it again here would
                        // double-advance and skip bytes. A short chunk write (n <
                        // this_chunk) simply leaves the unused tail of the
                        // reservation as a sparse hole rather than rewinding the
                        // shared position — see `reserve_write_pos`'s doc comment
                        // for why rewinding it would reopen the race this closes.
                        if is_append {
                            let new_pos = write_pos;
                            if let Some(proc) = akuma_exec::process::current_process_shared() {
                                proc.update_fd(fd_num as u32, |entry| if let akuma_exec::process::FileDescriptor::File(file) = entry {
                                    file.position = new_pos;
                                });
                            }
                        }
                        n as u64
                    }
                    Err(e) => {
                        if total_written > 0 { return total_written as u64; }
                        return fs_error_to_errno(e);
                    }
                }
            }
            akuma_exec::process::FileDescriptor::BlockDev { idx, writable, .. } => {
                if !writable {
                    if total_written > 0 { return total_written as u64; }
                    return EBADF;
                }
                let capacity = crate::block::with_device_at(idx as usize, akuma_virtio::block::VirtioBlockDevice::capacity_bytes).unwrap_or(0);
                if write_pos as u64 >= capacity {
                    if total_written > 0 { return total_written as u64; }
                    return ENOSPC;
                }
                let clamped = this_chunk.min((capacity - write_pos as u64) as usize);
                if crate::block::write_bytes_at(idx as usize, write_pos as u64, &buf_slice[..clamped]).is_err() {
                    if total_written > 0 { return total_written as u64; }
                    return EIO;
                }
                write_pos += clamped;
                let new_pos = write_pos as u64;
                if let Some(proc) = akuma_exec::process::current_process_shared() {
                    proc.update_fd(fd_num as u32, |entry| if let akuma_exec::process::FileDescriptor::BlockDev { pos, .. } = entry { *pos = new_pos; });
                }
                clamped as u64
            }
            #[cfg(feature = "smoltcp")]
            akuma_exec::process::FileDescriptor::Socket(idx) => {
                // write(2) on a TCP socket is sshd's hot send path — run it
                // BKL-free like sendto (no-op guard unless no-bkl-network).
                // `buf_slice` is a kernel bounce buffer, so no user-memory
                // fault can occur inside the socket locks.
                let _net_bkl = super::net::NetBklGuard::new();
                let nonblock = super::net::fd_is_nonblock(fd_num as u32);
                let result = if socket::is_udp_socket(idx) {
                    match socket::udp_default_peer(idx) {
                        Some(peer) => socket::socket_send_udp(idx, buf_slice, peer),
                        None => Err(libc_errno::EDESTADDRREQ),
                    }
                } else {
                    socket::socket_send(idx, buf_slice, nonblock)
                };
                
                if crate::config::SYSCALL_DEBUG_NET_ENABLED && total_written == 0 {
                    match &result {
                        Ok(n) => crate::tprint!(96, "[TCP] write fd={} len={} sent={}\n", fd_num, count, n),
                        Err(e) => crate::tprint!(96, "[TCP] write fd={} len={} err={}\n", fd_num, count, i64::from(*e)),
                    }
                }
                
                match result {
                    Ok(n) => {
                        // Short write == transmit buffer filled. Re-arm the
                        // EPOLLET edge so the drain counts as a fresh EPOLLOUT
                        // (see `epoll_on_fd_write_blocked`). write(2) is the
                        // send path Go and hyper take, not just sendto/sendmsg.
                        if n < buf_slice.len() {
                            super::poll::epoll_on_fd_write_blocked(fd_num as u32);
                        }
                        n as u64
                    }
                    Err(e) => {
                        if e == libc_errno::EAGAIN {
                            super::poll::epoll_on_fd_write_blocked(fd_num as u32);
                        }
                        if total_written > 0 { return total_written as u64; }
                        return (-i64::from(e)) as u64;
                    }
                }
            }
            akuma_exec::process::FileDescriptor::PipeWrite(pipe_id) => {
                // Pipes are capped at `PIPE_CAPACITY`, so a write can now come up short
                // or accept nothing at all — block and retry like write(2), rather than
                // reporting a 0-byte "success" that userspace reads as EOF-on-write.
                let nonblock = super::net::fd_is_nonblock(fd_num as u32);
                loop {
                    match super::pipe::pipe_write(pipe_id, buf_slice) {
                        Ok(0) => {
                            // Pipe buffer full. If we already wrote data in
                            // previous chunks, return that rather than blocking.
                            if total_written > 0 { return total_written as u64; }
                            if nonblock { return EAGAIN; }
                            if akuma_exec::process::should_interrupt_blocking_syscall() {
                                return EINTR;
                            }
                            let tid = akuma_exec::threading::current_thread_id();
                            if !super::pipe::pipe_check_set_writer(pipe_id, tid) {
                                akuma_exec::threading::schedule_blocking(u64::MAX);
                            }
                            // After waking, retry pipe_write
                        }
                        Ok(n) => break n as u64,
                        Err(e) => {
                            crate::safe_print!(128, "[syscall] write: PipeWrite fd={} pipe_id={} EPIPE ({} bytes)\n", fd_num, pipe_id, buf_slice.len());
                            if total_written > 0 { return total_written as u64; }
                            return (-i64::from(e)) as u64;
                        }
                    }
                }
            }
            // AF_UNIX socketpair endpoint: write to this endpoint's `tx` pipe.
            akuma_exec::process::FileDescriptor::UnixSocket { tx, .. } => {
                let nonblock = super::net::fd_is_nonblock(fd_num as u32);
                loop {
                    match super::pipe::pipe_write(tx, buf_slice) {
                        Ok(0) => {
                            if total_written > 0 { return total_written as u64; }
                            if nonblock { return EAGAIN; }
                            if akuma_exec::process::should_interrupt_blocking_syscall() {
                                return EINTR;
                            }
                            let tid = akuma_exec::threading::current_thread_id();
                            if !super::pipe::pipe_check_set_writer(tx, tid) {
                                akuma_exec::threading::schedule_blocking(u64::MAX);
                            }
                        }
                        Ok(n) => break n as u64,
                        Err(e) => {
                            if total_written > 0 { return total_written as u64; }
                            return (-i64::from(e)) as u64;
                        }
                    }
                }
            }
            #[cfg(feature = "sc-eventfd")]
            akuma_exec::process::FileDescriptor::EventFd(efd_id) => {
                if this_chunk < 8 { return EINVAL; } // Should enforce 8 byte writes
                let val = unsafe { core::ptr::read(buf_slice.as_ptr().cast::<u64>()) };
                if val == u64::MAX { return EINVAL; }
                if crate::config::SYSCALL_DEBUG_NET_ENABLED {
                    crate::tprint!(96, "[eventfd] write via fd={} id={} val={}\n", fd_num, efd_id, val);
                }
                match super::eventfd::eventfd_write(efd_id, val) {
                    Ok(()) => 8,
                    Err(e) => (-i64::from(e)) as u64,
                }
            }
            akuma_exec::process::FileDescriptor::DevNull | akuma_exec::process::FileDescriptor::DevUrandom | akuma_exec::process::FileDescriptor::DevZero => this_chunk as u64,
            #[cfg(feature = "rump")]
            akuma_exec::process::FileDescriptor::Tap { .. } => {
                // One write() == one L2 frame. Ethernet frames are <2 KB, so a
                // frame never exceeds the 64 KB chunk (single iteration).
                if let Ok(n) = akuma_net::rump_tap::write_frame(buf_slice) {
                    n as u64
                } else {
                    if total_written > 0 { return total_written as u64; }
                    return EIO;
                }
            }
            akuma_exec::process::FileDescriptor::DevDsp => {
                // Blocking PCM playback. The audio driver re-chunks into bounded
                // periods internally; consumes the whole slice or errors.
                let _drv_bkl = DriverBklGuard::new();
                if let Ok(n) = crate::audio::play(buf_slice) { n as u64 } else {
                    if total_written > 0 { return total_written as u64; }
                    return EIO;
                }
            }
            _ => EBADF
        };

        // If write failed or returned error code (large positive u64)
        if (written as i64) < 0 {
            if total_written > 0 { return total_written as u64; }
            return written;
        }
        
        let written_usize = written as usize;
        total_written += written_usize;
        
        // If partial write, stop (short write)
        if written_usize < this_chunk {
            break;
        }
        
        // Special case: some FDs don't support chunking or offsets (like EventFd)
        // If we wrote something, checking FDs type to break might be complex.
        // Assuming file-like behavior.
    }
    
    total_written as u64
}

pub(super) fn sys_readv(fd_num: u64, iov_ptr: u64, iov_cnt: usize) -> u64 {
    let iov_size = iov_cnt * core::mem::size_of::<IoVec>();
    if !validate_user_ptr(iov_ptr, iov_size) { return EFAULT; }
    
    let mut kernel_iovs = alloc::vec![IoVec { iov_base: 0, iov_len: 0 }; iov_cnt];
    if copy_from_user(as_user_bytes_mut(&mut kernel_iovs), iov_ptr).is_err() {
        return EFAULT;
    }

    let mut total_read: u64 = 0;
    for iov in kernel_iovs.iter().take(iov_cnt) {
        if iov.iov_len == 0 { continue; }
        let n = sys_read(fd_num, iov.iov_base, iov.iov_len);
        if (n as i64) < 0 {
            if total_read == 0 { return n; }
            break;
        }
        total_read += n;
        if (n as usize) < iov.iov_len { break; }
    }
    if crate::config::SYSCALL_DEBUG_IO_ENABLED {
        crate::safe_print!(128, "[syscall] readv(fd={}, cnt={}) = {}\n", fd_num, iov_cnt, total_read);
    }
    total_read
}

/// Whether `writev` must stop after an iovec that wrote `written` of `want`.
///
/// **A short write ends the whole `writev`.** The tail of that iovec did not go
/// out; moving on to the next one splices it in directly after the truncated
/// bytes, and the caller — which learns only the total — resumes from there. The
/// result is a byte stream with a hole in the middle. On a socket that is silent
/// corruption of somebody else's protocol.
///
/// Short writes are routine on this kernel, not exceptional: `socket_send`
/// returns whatever fit in smoltcp's 16 KB TX buffer, and the net bounce buffer
/// degrades to a single page under memory pressure. Worse, `socket_send` ends
/// with a `poll()` that pushes the queued bytes onto the wire and frees TX
/// space — so the *next* iovec usually succeeds, which is what turns a dropped
/// tail into a splice rather than a harmless stall.
///
/// Redis writes replies through `writev` over many iovecs, so any reply larger
/// than the free TX window came out spliced; `redis-cli` reported it as
/// `Protocol error, got "\n" as reply type byte` — the byte it found where the
/// next reply should have started. `sys_readv` has always had the mirror guard;
/// `writev` was simply missed.
///
/// Pure so the rule is testable without an fd (`run_writev_short_write_tests`).
#[must_use]
pub const fn writev_stops_after(written: u64, want: usize) -> bool {
    written < want as u64
}

/// Boot-suite check for [`writev_stops_after`], in the same
/// pure-function-plus-boot-assert shape as `run_net_bounce_tests`.
///
/// It covers the decision, not the splice: staging a *real* short write that is
/// followed by an accepting one needs a peer draining the far end concurrently,
/// which the boot suite (single-threaded, no network) cannot do. The end-to-end
/// check is `scripts/redis_stream_integrity.py` against a live VM.
#[cfg(kernel_tests)]
pub fn run_writev_short_write_tests() {
    assert!(!writev_stops_after(4096, 4096), "a fully-written iovec must continue");
    assert!(writev_stops_after(10, 4096), "a short write must end the writev");
    assert!(writev_stops_after(0, 1), "writing nothing is short — do not skip ahead");
    assert!(!writev_stops_after(0, 0), "an empty iovec is complete, not short");
    // The exact shape that corrupted Redis replies: smoltcp's 16 KB TX window
    // truncating a larger iovec.
    assert!(writev_stops_after(16384, 65536), "a TX-window-bounded write must stop");
    crate::console::print("[Test] writev_stops_at_short_write PASSED\n");
}

pub(super) fn sys_writev(fd_num: u64, iov_ptr: u64, iov_cnt: usize) -> u64 {
    let iov_size = iov_cnt * core::mem::size_of::<IoVec>();
    if !validate_user_ptr(iov_ptr, iov_size) { return EFAULT; }
    
    let mut kernel_iovs = alloc::vec![IoVec { iov_base: 0, iov_len: 0 }; iov_cnt];
    if copy_from_user(as_user_bytes_mut(&mut kernel_iovs), iov_ptr).is_err() {
        return EFAULT;
    }

    let mut total_written: u64 = 0;
    for iov in kernel_iovs.iter().take(iov_cnt) {
        if iov.iov_len == 0 { continue; }
        let written = sys_write(fd_num, iov.iov_base, iov.iov_len);
        if (written as i64) < 0 {
            if total_written == 0 { return written; }
            break;
        }
        total_written += written;
        if writev_stops_after(written, iov.iov_len) {
            break;
        }
    }
    if crate::config::SYSCALL_DEBUG_IO_ENABLED {
        crate::safe_print!(128, "[syscall] writev(fd={}, cnt={}) = {}\n", fd_num, iov_cnt, total_written);
    }
    total_written
}

/// The one iovec walk behind `preadv`/`pwritev`/`preadv2`/`pwritev2`.
///
/// The same short-transfer rule as `sys_readv`/`sys_writev` applies — a partial
/// transfer ends the whole call, because continuing would splice the next
/// iovec's bytes in where the truncated tail belonged (see
/// [`writev_stops_after`]). The difference here is that the offset advances by
/// what was actually transferred rather than by the file position, so each
/// chunk lands where the caller asked.
fn pvec_at(fd_num: u64, iov_ptr: u64, iov_cnt: usize, offset: i64, write: bool) -> u64 {
    if offset < 0 {
        return EINVAL;
    }
    let Some(iov_size) = iov_cnt.checked_mul(core::mem::size_of::<IoVec>()) else {
        return EINVAL;
    };
    if !validate_user_ptr(iov_ptr, iov_size) {
        return EFAULT;
    }

    let mut kernel_iovs = alloc::vec![IoVec { iov_base: 0, iov_len: 0 }; iov_cnt];
    if copy_from_user(as_user_bytes_mut(&mut kernel_iovs), iov_ptr).is_err() {
        return EFAULT;
    }

    let mut total: u64 = 0;
    let mut at = offset;
    for iov in kernel_iovs.iter().take(iov_cnt) {
        if iov.iov_len == 0 {
            continue;
        }
        let n = if write {
            sys_pwrite64(fd_num as u32, iov.iov_base, iov.iov_len, at)
        } else {
            sys_pread64(fd_num as u32, iov.iov_base, iov.iov_len, at)
        };
        if (n as i64) < 0 {
            // Report the error only if nothing has moved yet; otherwise the
            // caller must learn how far the transfer got.
            if total == 0 {
                return n;
            }
            break;
        }
        total += n;
        // An offset past i64::MAX is not reachable through any file this kernel
        // serves, but wrapping here would silently rewrite the start of the file.
        let Some(next) = at.checked_add(n as i64) else {
            break;
        };
        at = next;
        if writev_stops_after(n, iov.iov_len) {
            break;
        }
    }
    total
}

/// `preadv`/`pwritev` (nr 69/70) and their `2` variants (nr 286/287).
///
/// musl only reaches `p{read,write}v2` when `flags` is nonzero — with no flags
/// it rewrites the call to `p{read,write}v`, or to plain `{read,write}v` when
/// the offset is -1 — so a build that lacked all four saw callers fall all the
/// way through to the dispatcher's `-ENOSYS` catch-all, which prints a line per
/// attempt. That is the `[ENOSYS] nr=287` console flood
/// (docs/archive/DEVBOX_ISSUES.md Issue 13): the write itself still succeeded
/// via a fallback, but every one of them paid a wasted syscall and a console
/// print, and console output is the expensive half under load.
///
/// `pos_h` is deliberately not a parameter. Linux reassembles the offset with
/// `pos_from_hilo(pos_h, pos_l)`, whose two 32-bit shifts of a 64-bit value make
/// `pos_h` contribute nothing on a 64-bit kernel; `pos_l` already carries the
/// whole offset. Naming it would invite someone to fold it in and break every
/// offset above 4 GB.
///
/// `flags` are the `RWF_*` set (`HIPRI`, `DSYNC`, `SYNC`, `NOWAIT`, `APPEND`).
/// None of them are implemented, and Linux's own answer for an unsupported
/// `RWF_*` bit is `EOPNOTSUPP` — not `EINVAL`, which would read as "bad
/// argument" and stop a caller from retrying without the flag.
pub(super) fn sys_pvec2(
    fd_num: u64,
    iov_ptr: u64,
    iov_cnt: usize,
    pos_l: u64,
    flags: u32,
    write: bool,
) -> u64 {
    if flags != 0 {
        return EOPNOTSUPP;
    }
    // -1 means "use the file position", which is exactly readv/writev.
    if pos_l as i64 == -1 {
        return if write {
            sys_writev(fd_num, iov_ptr, iov_cnt)
        } else {
            sys_readv(fd_num, iov_ptr, iov_cnt)
        };
    }
    pvec_at(fd_num, iov_ptr, iov_cnt, pos_l as i64, write)
}

pub(super) fn sys_fstatfs(fd: u32, buf_ptr: u64) -> u64 {
    // Resolve the fd's path to the mount it sits on and report that mount's
    // real statistics. Before 2026-08-24 this returned hardcoded fiction
    // (f_type=0xEF53, 65536 blocks) for every fd, so `df`-style tools sized
    // every filesystem identically (`docs/archive/MOUNT_MISSING_SYSCALLS.md`
    // §3.2). Non-file fds (pipes, sockets) have no path to resolve; Linux
    // reports their own pseudo-filesystem there, we report the root mount —
    // nothing in-tree branches on those.
    let Some(proc) = akuma_exec::process::current_process_shared() else { return ENOSYS; };
    let fd_path = match proc.get_fd(fd) {
        Some(akuma_exec::process::FileDescriptor::File(f)) => Some(f.path),
        Some(_) => None, // non-file fd: report the root mount (see doc comment)
        None => return EBADF,
    };
    let path = fd_path.unwrap_or_else(|| String::from("/"));
    match crate::vfs::stats_for_path(&path) {
        Ok(view) => statfs_into(&view, buf_ptr),
        Err(e) => fs_error_to_errno(e),
    }
}

/// `statfs` magic numbers, keyed by `Filesystem::name()`. `memfs` is the
/// implementation behind every `tmpfs` mount, so it reports `TMPFS_MAGIC`
/// like Linux does.
fn fs_magic(name: &str) -> i64 {
    match name {
        "ext2" => 0xEF53,
        "proc" => 0x9FA0,
        "memfs" | "tmpfs" => 0x0102_1994,
        "overlay" => 0x794C_7630,
        "subdirfs" => 0x794C_7630, // jail under an overlay: report overlay
        _ => 0xADF5,
    }
}

/// Shared `statfs`/`fstatfs` writer: resolve a path to its mount's real
/// statistics (the old `fstatfs` returned hardcoded fiction for every fd —
/// `docs/archive/MOUNT_MISSING_SYSCALLS.md` §3.2).
pub(super) fn statfs_into(view: &crate::vfs::FsView, buf_ptr: u64) -> u64 {
    // The `120` was a literal beside a function-local struct nothing could
    // check it against; it is `size_of::<Statfs>()` now, asserted in
    // `akuma-syscalls-linux`.
    if !validate_user_ptr(buf_ptr, core::mem::size_of::<Statfs>()) {
        return EFAULT;
    }
    let bs = i64::from(view.stats.block_size);
    let st = Statfs {
        f_type: fs_magic(&view.fs_name),
        f_bsize: bs,
        f_blocks: view.stats.total_blocks as i64,
        f_bfree: view.stats.free_blocks as i64,
        f_bavail: view.stats.free_blocks as i64,
        f_files: 0,
        f_ffree: 0,
        f_fsid: [0, 0],
        f_namelen: 255,
        f_frsize: bs,
        f_flags: view.flags as i64,
        f_spare: [0; 4],
    };
    if write_user_val(buf_ptr, &st).is_err() {
        return EFAULT;
    }
    0
}

/// `statfs(2)` (nr 43) — resolve `path` the way file operations do and report
/// the mount it lands on. Undispatched before 2026-08-24, which is what broke
/// busybox `df`.
pub(super) fn sys_statfs(path_ptr: u64, buf_ptr: u64) -> SysResult {
    let path = copy_from_user_str(path_ptr, 256)?;
    match crate::vfs::stats_for_path(&path) {
        Ok(view) => Ok(statfs_into(&view, buf_ptr)),
        Err(e) => Err(fs_error_to_errno(e)),
    }
}

pub(super) fn sys_dup(oldfd: u32) -> u64 {
    let proc = match akuma_exec::process::current_process_shared() { Some(p) => p, None => return ENOSYS };
    let entry = match proc.get_fd(oldfd) {
        Some(e) => e,
        None => return EBADF,
    };
    // One list, in `akuma_exec::process::clone_fd_refs`. This was a local
    // four-arm copy that had drifted: `EventFd` and `RumpSocket` are
    // refcounted but were handled only on the fork path, so a dup of
    // either produced an unreferenced alias and the first close
    // destroyed the object under the surviving fd.
    akuma_exec::process::clone_fd_refs(&entry);
    let newfd = proc.alloc_fd(entry);
    proc.clear_cloexec(newfd);
    if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
        crate::safe_print!(128, "[syscall] dup(oldfd={}) = {}\n", oldfd, newfd);
    }
    u64::from(newfd)
}

pub(super) fn sys_dup3(oldfd: u32, newfd: u32, flags: u32) -> u64 {
    if oldfd == newfd { return EINVAL; }
    let proc = match akuma_exec::process::current_process_shared() { Some(p) => p, None => return ENOSYS };
    let entry = match proc.get_fd(oldfd) {
        Some(e) => e,
        None => return EBADF,
    };

    if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
        let pid = proc.pid;
        crate::safe_print!(128, "[syscall] dup3(oldfd={}, newfd={}, flags=0x{:x}) PID {}\n", oldfd, newfd, flags, pid);
    }

    // Increment refcount for the new entry BEFORE atomically swapping it in.
    // This must happen before swap_fd so the pipe isn't prematurely destroyed
    // if another thread closes oldfd between these two steps.
    // One list, in `akuma_exec::process::clone_fd_refs`. This was a local
    // four-arm copy that had drifted: `EventFd` and `RumpSocket` are
    // refcounted but were handled only on the fork path, so a dup of
    // either produced an unreferenced alias and the first close
    // destroyed the object under the surviving fd.
    akuma_exec::process::clone_fd_refs(&entry);

    // Atomically replace newfd and retrieve the old entry in one operation.
    // This prevents a TOCTOU race on shared fd tables (CLONE_FILES goroutines).
    let old_entry = proc.swap_fd(newfd, entry);

    if flags & akuma_exec::process::open_flags::O_CLOEXEC != 0 {
        proc.set_cloexec(newfd);
    } else {
        proc.clear_cloexec(newfd);
    }
    // `nonblock`, unlike `cloexec`, is a property of the open file description
    // dup2/dup3 shares between oldfd and newfd (real Linux: both fds report
    // the same O_NONBLOCK status afterward), but this table tracks it per raw
    // fd *number* — copy it explicitly or `newfd` keeps whatever its own,
    // unrelated previous occupant left behind.
    if proc.is_nonblock(oldfd) {
        proc.set_nonblock(newfd);
    } else {
        proc.clear_nonblock(newfd);
    }

    // Close the old entry AFTER inserting the new one.
    if let Some(old) = old_entry {
        match old {
            akuma_exec::process::FileDescriptor::PipeWrite(id) => super::pipe::pipe_close_write(id),
            akuma_exec::process::FileDescriptor::PipeRead(id) => super::pipe::pipe_close_read(id),
            akuma_exec::process::FileDescriptor::UnixSocket { rx, tx, sock } => {
                super::pipe::pipe_close_read(rx);
                super::pipe::pipe_close_write(tx);
                super::unixsock::unix_sock_close(sock);
            }
            akuma_exec::process::FileDescriptor::Socket(idx) => { akuma_net::socket::remove_socket(idx); }
            #[cfg(feature = "sc-eventfd")]
            akuma_exec::process::FileDescriptor::EventFd(efd_id) => {
                super::eventfd::eventfd_close(efd_id);
            }
            _ => {}
        }
    }

    u64::from(newfd)
}

pub(super) fn sys_openat(dirfd: i32, path_ptr: u64, flags: u32, mode: u32) -> SysResult {
    let raw_path = copy_from_user_str(path_ptr, 1024)?;

    if crate::config::SYSCALL_DEBUG_IO_ENABLED {
        crate::tprint!(128, "[syscall] openat(dirfd={}, path={:?}, flags=0x{:x}, mode=0x{:x})\n", dirfd, raw_path, flags, mode);
    }

    // O_TMPFILE (an unnamed file linked in later via `linkat(/proc/self/fd/N)`)
    // is not implemented, and Linux kernels that predate it answer EINVAL —
    // portable callers (apk-tools 3 `__apk_ostream_to_file`) treat any failure
    // as "no tmpfiles here" and fall back to a named `.tmp` + `renameat`. What
    // this used to do instead was far worse: the flag was silently ignored, the
    // O_DIRECTORY bit it carries resolved to the directory itself, and the
    // write-mode-on-directory check below did not exist — so apk's
    // `openat(dirfd, ".", O_RDWR|O_TMPFILE)` "succeeded" with a writable fd ON
    // THE DIRECTORY, and every later write() failed EISDIR (apk: "failed to
    // write database: Is a directory" after a successful install).
    if flags & akuma_exec::process::open_flags::O_TMPFILE
        == akuma_exec::process::open_flags::O_TMPFILE
    {
        if crate::config::SYSCALL_DEBUG_IO_ENABLED {
            crate::safe_print!(128, "[syscall] openat({:?}): O_TMPFILE unsupported -> EINVAL\n", raw_path);
        }
        return Err(EINVAL);
    }

    let path = if raw_path.starts_with('/') {
        crate::vfs::canonicalize_path(&raw_path)
    } else {
        let base = if dirfd == -100 {
            if let Some(proc) = akuma_exec::process::current_process_shared() {
                proc.cwd.clone()
            } else {
                String::from("/")
            }
        } else if dirfd >= 0 {
            if let Some(proc) = akuma_exec::process::current_process_shared() {
                if let Some(akuma_exec::process::FileDescriptor::File(f)) = proc.get_fd(dirfd as u32) {
                    f.path
                } else {
                    if crate::config::SYSCALL_DEBUG_IO_ENABLED {
                        crate::safe_print!(128, "[syscall] openat: bad dirfd={}\n", dirfd);
                    }
                    return Err(EBADF);
                }
            } else {
                return Err(EBADF);
            }
        } else {
            String::from("/")
        };
        if raw_path == "." || raw_path.is_empty() {
            base
        } else {
            crate::vfs::resolve_path(&base, &raw_path)
        }
    };

    // Phase 7c (docs/archive/BKL_PHASE7C_OPENAT_RESIDUAL.md): opened HERE, before
    // `resolve_symlinks`, not after it. `resolve_symlinks` calls `read_symlink` ->
    // `with_fs`, a real on-disk ext2 lookup for any path that names a symlink — the
    // exact same class of I/O `sys_fchmodat` (fs.rs, `VfsBklGuard` before its own
    // `resolve_symlinks` call) and `sys_newfstatat` (guard before its conditional
    // `resolve_symlinks`) already run BKL-free. Moving the guard here brings `openat`
    // in line with those two and removes the prologue's ext2 work from the BKL-held
    // measurement window the audit flagged (`BKL_PHASE7_AUDIT.md` §5, 7c: "10.5% for
    // a converted syscall's prologue/epilogue is high enough that either the window
    // starts too late or the re-acquire costs more than expected"). This guard's only
    // live cfg is `kernel_smp_shared`+`kernel_no_bkl_vfs`; without both it's a no-op.
    let _vfs_bkl = VfsBklGuard::new();

    let path = crate::vfs::resolve_symlinks(&path);

    if path == "/dev/null" {
        if let Some(proc) = akuma_exec::process::current_process_shared() {
            let fd = proc.alloc_fd(akuma_exec::process::FileDescriptor::DevNull);
            if flags & akuma_exec::process::open_flags::O_CLOEXEC != 0 {
                proc.set_cloexec(fd);
            }
            if crate::config::SYSCALL_DEBUG_IO_ENABLED {
                crate::safe_print!(256, "[syscall] openat(/dev/null) = fd {} flags=0x{:x}\n", fd, flags);
            }
            return Ok(u64::from(fd));
        }
        return Err(ESRCH);
    }

    if path == "/dev/urandom" || path == "/dev/random" {
        if let Some(proc) = akuma_exec::process::current_process_shared() {
            let fd = proc.alloc_fd(akuma_exec::process::FileDescriptor::DevUrandom);
            if flags & akuma_exec::process::open_flags::O_CLOEXEC != 0 {
                proc.set_cloexec(fd);
            }
            if crate::config::SYSCALL_DEBUG_IO_ENABLED {
                crate::safe_print!(256, "[syscall] openat({}) = fd {} flags=0x{:x}\n", &path, fd, flags);
            }
            return Ok(u64::from(fd));
        }
        return Err(ESRCH);
    }

    if path == "/dev/zero" {
        if let Some(proc) = akuma_exec::process::current_process_shared() {
            let fd = proc.alloc_fd(akuma_exec::process::FileDescriptor::DevZero);
            if flags & akuma_exec::process::open_flags::O_CLOEXEC != 0 {
                proc.set_cloexec(fd);
            }
            if crate::config::SYSCALL_DEBUG_IO_ENABLED {
                crate::safe_print!(256, "[syscall] openat(/dev/zero) = fd {} flags=0x{:x}\n", fd, flags);
            }
            return Ok(u64::from(fd));
        }
        return Err(ESRCH);
    }

    // /dev/tty — the calling process's controlling terminal. Pagers (`less`,
    // `more`) read keyboard input from here, NEVER from stdin: stdin is the
    // pipe carrying the content being paged (`git log | less`). Handing less a
    // real fd here is what lets it see keys at all; before this node existed
    // the open failed and less fell back to reading stdin — the pipe — for
    // keystrokes, hanging forever while the typed bytes drained into the pipe.
    //
    // ENXIO when the process's channel is not a terminal (Linux's answer for
    // "no controlling terminal"): a non-terminal channel is the exec bridge's
    // command stream, and letting a pager read it would corrupt the stream.
    if path == "/dev/tty" {
        let Some(proc) = akuma_exec::process::current_process_shared() else { return Err(ESRCH) };
        if !akuma_exec::process::current_channel().is_some_and(|ch| ch.is_terminal()) {
            // Linux says ENXIO for "no controlling terminal"; this errno set
            // has no ENXIO (see akuma-primitives/src/errno.rs header note), and
            // ENODEV is the nearest "device not attached" answer.
            return Err(ENODEV);
        }
        let fd = proc.alloc_fd(akuma_exec::process::FileDescriptor::DevTty);
        if flags & akuma_exec::process::open_flags::O_CLOEXEC != 0 {
            proc.set_cloexec(fd);
        }
        if crate::config::SYSCALL_DEBUG_IO_ENABLED {
            crate::safe_print!(256, "[syscall] openat(/dev/tty) = fd {} flags=0x{:x}\n", fd, flags);
        }
        return Ok(u64::from(fd));
    }

    // /dev/dsp — virtio-sound output. Only opens when a sound device was found at
    // boot (audio::is_available()); otherwise falls through to the normal path
    // (→ ENOENT), so the node simply doesn't exist when the feature is off.
    if (path == "/dev/dsp" || path == "/dev/audio") && crate::audio::is_available() {
        if let Some(proc) = akuma_exec::process::current_process_shared() {
            let fd = proc.alloc_fd(akuma_exec::process::FileDescriptor::DevDsp);
            if flags & akuma_exec::process::open_flags::O_CLOEXEC != 0 {
                proc.set_cloexec(fd);
            }
            if crate::config::SYSCALL_DEBUG_IO_ENABLED {
                crate::safe_print!(256, "[syscall] openat({}) = fd {} flags=0x{:x}\n", &path, fd, flags);
            }
            return Ok(u64::from(fd));
        }
        return Err(ESRCH);
    }

    // /dev/net/tap0 — raw L2 packet device for the rump kernel feature. Only
    // opens when a second virtio-net NIC was bound at boot (rump_tap::is_ready);
    // otherwise returns ENODEV. Frame-granular read()/write() and a no-op
    // TUN/TAP ioctl let a userspace rump virtif drive the NetBSD stack over it.
    #[cfg(feature = "rump")]
    if path == "/dev/net/tap0" {
        if !akuma_net::rump_tap::is_ready() {
            return Err(ENODEV);
        }
        if let Some(proc) = akuma_exec::process::current_process_shared() {
            let fd = proc.alloc_fd(akuma_exec::process::FileDescriptor::Tap {
                nonblock: flags & akuma_exec::process::open_flags::O_NONBLOCK != 0,
            });
            if flags & akuma_exec::process::open_flags::O_CLOEXEC != 0 {
                proc.set_cloexec(fd);
            }
            if crate::config::SYSCALL_DEBUG_IO_ENABLED {
                crate::safe_print!(256, "[syscall] openat(/dev/net/tap0) = fd {} flags=0x{:x}\n", fd, flags);
            }
            return Ok(u64::from(fd));
        }
        return Err(ESRCH);
    }

    // Raw block device open (`/dev/vdX`) — `proposals/RAW_BLOCK_DEVICE_FD.md`.
    // `dev_node` only reports a block node here for the host (box_id == 0);
    // boxes get no synthetic /dev, so this never fires for a box process.
    if let Some(node) = crate::vfs::dev_node(&path)
        && node.is_block
    {
        let Some(idx) = crate::block::device_index_by_name(node.name) else {
            return Err(ENODEV);
        };
        let accmode = flags & 0x3; // O_RDONLY=0, O_WRONLY=1, O_RDWR=2
        let wants_write = accmode != akuma_exec::process::open_flags::O_RDONLY;
        // A raw write to a *mounted* device would bypass `Ext2Filesystem`'s
        // cache and corrupt it silently — refuse write-open, not write() itself,
        // so the failure lands at the syscall the caller can actually see
        // (§3's "Refuse write-open of a mounted device" option).
        if wants_write && crate::vfs::device_is_mounted(node.name) {
            if crate::config::SYSCALL_DEBUG_IO_ENABLED {
                crate::safe_print!(128, "[syscall] openat({}) EBUSY (mounted, write requested)\n", &path);
            }
            return Err(EBUSY);
        }
        if let Some(proc) = akuma_exec::process::current_process_shared() {
            let fd = proc.alloc_fd(akuma_exec::process::FileDescriptor::BlockDev {
                idx: idx as u32,
                pos: 0,
                writable: wants_write,
            });
            if flags & akuma_exec::process::open_flags::O_CLOEXEC != 0 {
                proc.set_cloexec(fd);
            }
            if crate::config::SYSCALL_DEBUG_IO_ENABLED {
                crate::safe_print!(256, "[syscall] openat({}) = fd {} flags=0x{:x}\n", &path, fd, flags);
            }
            return Ok(u64::from(fd));
        }
        return Err(ESRCH);
    }

    // Every device with `open()` behavior has returned by now. Anything still in
    // the table is a node this kernel can list and `stat` but not open — chardevs
    // with no fd behavior of their own. Refuse here: now that `crate::fs::exists`
    // knows about them, the generic path below would otherwise hand out a `File`
    // fd whose first `read()` fails against ext2 — a failure at the wrong
    // syscall. `ENODEV` matches `/dev/net/tap0` above.
    if crate::vfs::dev_node(&path).is_some() {
        if crate::config::SYSCALL_DEBUG_IO_ENABLED {
            crate::safe_print!(256, "[syscall] openat({}) ENODEV (no fd behavior)\n", &path);
        }
        return Err(ENODEV);
    }

    let path = if path == "/proc/self/exe" {
        if let Some(proc) = akuma_exec::process::current_process_shared() {
            proc.name.clone()
        } else {
            return Err(ENOENT);
        }
    } else {
        path
    };

    // From here on is the on-disk open work — existence probes, O_CREAT create /
    // O_TRUNC truncate (both `write_file`; truncating a large file frees its blocks
    // under the ext2 write guard, the same long hold §7.2 measured at ~40 s for an
    // unlink), and `chmod`. Already BKL-free: the guard opened above, before
    // `resolve_symlinks` (Phase 7c). `openat` (syscall 56) surfaced as 36.6% of all
    // cross-core BKL wait once §12 converted `unlinkat` —
    // docs/archive/BKL_VFS_CARVE_OUT.md §12.2 — making it Phase 2b's first target.
    if !crate::fs::exists(&path) {
        let is_creat = flags & akuma_exec::process::open_flags::O_CREAT != 0;
        if !is_creat {
            if crate::config::SYSCALL_DEBUG_IO_ENABLED {
                crate::safe_print!(256, "[syscall] openat({}) ENOENT flags=0x{:x}\n", &path, flags);
            }
            return Err(ENOENT);
        }

        let (parent_raw, _) = crate::vfs::split_path(&path);
        if !parent_raw.is_empty() {
            let parent_path = if parent_raw.starts_with('/') {
                String::from(parent_raw)
            } else {
                format!("/{parent_raw}")
            };
            if parent_path != "/" && !crate::fs::exists(&parent_path) {
                if crate::config::SYSCALL_DEBUG_IO_ENABLED {
                    crate::safe_print!(256, "[syscall] openat({}) parent {} not found flags=0x{:x}\n", &path, &parent_path, flags);
                }
                return Err(ENOENT);
            }
        }
    }

    if let Some(proc) = akuma_exec::process::current_process_shared() {
        let file_existed = crate::fs::exists(&path);

        // Linux (fs/namei.c `may_open`): write access to an existing directory
        // is EISDIR *at open()*. Handing out the fd anyway just moves the same
        // error to the first write() — a failure at the wrong syscall, which is
        // exactly how apk-tools 3's O_TMPFILE probe "succeeded" and then died
        // writing its database. Read-only directory opens (getdents/`ls`) are
        // unaffected.
        if file_existed
            && flags & (akuma_exec::process::open_flags::O_WRONLY
                | akuma_exec::process::open_flags::O_RDWR)
                != 0
            && crate::vfs::metadata(&path).is_ok_and(|m| m.is_dir)
        {
            if crate::config::SYSCALL_DEBUG_IO_ENABLED {
                crate::safe_print!(128, "[syscall] openat({:?}): write open of directory -> EISDIR\n", path);
            }
            return Err(EISDIR);
        }

        if !file_existed && (flags & akuma_exec::process::open_flags::O_CREAT != 0) {
            let _ = crate::fs::write_file(&path, &[]);
            if mode & 0o7777 != 0 {
                let _ = crate::vfs::chmod(&path, mode & 0o7777);
            }
        } else if file_existed && (flags & akuma_exec::process::open_flags::O_TRUNC != 0) {
            // Diagnostic (2026-08-15 hunt §14): O_TRUNC here zeroes the file IN
            // PLACE (write_file → truncate_inode, same inode, blocks freed,
            // size 0) — while other processes may still hold lazy
            // `LazySource::File` regions naming it (Linux pins the inode via
            // the mmap's fd; this kernel drops all references at mmap time).
            // Their later faults then fill `Ok(0)` → zero pages ([FILL-SHORT]),
            // or, after inode reuse, a different file's bytes → decode
            // garbage. Gated like the other syscall traces — flip
            // SYSCALL_DEBUG_IO_ENABLED to hunt the actor again.
            if crate::config::SYSCALL_DEBUG_IO_ENABLED
                && let Ok(sz) = crate::vfs::file_size(&path)
                && sz > 0
            {
                crate::safe_print!(224,
                    "[O_TRUNC-ZAP] pid={} path={} size={}\n",
                    akuma_exec::process::read_current_pid().unwrap_or(0), &path, sz);
            }
            let _ = crate::fs::write_file(&path, &[]);
        }
        // One path walk here replaces one per `read(2)` for the life of the fd
        // (`crate::vfs::open_file_inode` says which opens qualify), and the pin
        // it takes gives the descriptor "unlinked but still open" semantics —
        // without which reading by a number the filesystem may reissue would be
        // the `SELFHOST_ZERO_PAGE_HUNT` defect with an fd in place of an mmap.
        let file = akuma_exec::process::KernelFile::new(path.clone(), flags);
        let file = match crate::vfs::open_file_ids(&path) {
            Some((mount_id, inode)) => file.with_inode(mount_id, inode),
            None => file,
        };
        let fd = proc.alloc_fd(akuma_exec::process::FileDescriptor::File(file));
        if flags & akuma_exec::process::open_flags::O_CLOEXEC != 0 {
            proc.set_cloexec(fd);
        }
        if crate::config::SYSCALL_DEBUG_IO_ENABLED {
            crate::safe_print!(256, "[syscall] openat({}) = fd {} flags=0x{:x}\n", &path, fd, flags);
        }
        Ok(u64::from(fd))
    } else { Err(ESRCH) }
}

pub fn sys_close(fd: u32) -> u64 {
    if let Some(proc) = akuma_exec::process::current_process_shared() {
        if let Some(entry) = proc.remove_fd(fd) {
            // Both flag-clears must happen here, immediately after `remove_fd`
            // frees this fd number for reuse — not after the resource-cleanup
            // match below, which can run real syscalls (`pipe_close_*`,
            // `remove_socket`, ...) that give a concurrent thread on a shared
            // fd table (CLONE_THREAD/CLONE_FILES) time to `alloc_fd` the same
            // number for something new. `clear_cloexec` already lived here;
            // `clear_nonblock` didn't — it ran last, after that window, so a
            // fresh pipe reusing this fd number could transiently read this
            // fd's stale `nonblock` bit as its own (spurious `EAGAIN` on what
            // should be a blocking read — e.g. std's child CLOEXEC-pipe
            // handshake, "the CLOEXEC pipe failed: ... WouldBlock"), and this
            // call's late `clear_nonblock` could then wipe out whatever the
            // new owner had legitimately set in the meantime. See
            // docs/archive/NCA_MISSING_SYSCALLS.md §1 update — reproduced via
            // concurrent `cargo build` spawns from nca's multi-threaded runtime.
            proc.clear_cloexec(fd);
            proc.clear_nonblock(fd);
            match entry {
                akuma_exec::process::FileDescriptor::Socket(idx) => { akuma_net::socket::remove_socket(idx); }
                akuma_exec::process::FileDescriptor::ChildStdout(child_pid) => {
                    akuma_exec::process::remove_child_channel(child_pid);
                }
                akuma_exec::process::FileDescriptor::PipeWrite(pipe_id) => {
                    super::pipe::pipe_close_write(pipe_id);
                }
                akuma_exec::process::FileDescriptor::PipeRead(pipe_id) => {
                    super::pipe::pipe_close_read(pipe_id);
                }
                akuma_exec::process::FileDescriptor::UnixSocket { rx, tx, sock } => {
                    super::pipe::pipe_close_read(rx);
                    super::pipe::pipe_close_write(tx);
                    super::unixsock::unix_sock_close(sock);
                }
                #[cfg(feature = "sc-eventfd")]
                akuma_exec::process::FileDescriptor::EventFd(efd_id) => {
                    super::eventfd::eventfd_close(efd_id);
                }
                #[cfg(feature = "sc-epoll")]
                akuma_exec::process::FileDescriptor::EpollFd(epoll_id) => {
                    super::poll::epoll_destroy(epoll_id);
                }
                #[cfg(feature = "sc-pidfd")]
                akuma_exec::process::FileDescriptor::PidFd(pidfd_id) => {
                    super::pidfd::pidfd_close(pidfd_id);
                }
                akuma_exec::process::FileDescriptor::DevDsp => {
                    crate::audio::stop();
                }
                akuma_exec::process::FileDescriptor::File(f) => {
                    let holder = alloc::sync::Arc::as_ptr(&proc.fds) as usize;
                    super::flock::flock_release(&f.path, holder, fd);
                }
                _ => {}
            }
            0
        } else { EBADF }
    } else { ESRCH }
}

pub fn sys_close_range(first: u32, last: u32, flags: u32) -> u64 {
    const CLOSE_RANGE_CLOEXEC: u32 = 4;
    let proc = match akuma_exec::process::current_process_shared() {
        Some(p) => p,
        None => return EBADF,
    };

    let fds: Vec<u32> = crate::irq::with_irqs_disabled(|| {
        proc.fds.table.lock().range(first..=last).map(|(&fd, _)| fd).collect()
    });

    for fd in fds {
        if flags & CLOSE_RANGE_CLOEXEC != 0 {
            proc.set_cloexec(fd);
        } else if let Some(entry) = proc.remove_fd(fd) {
            // Same ordering fix as `sys_close`: clear both flags right after
            // `remove_fd`, before the resource-cleanup match can hand a
            // concurrent `alloc_fd` on this shared fd table time to reuse the
            // number while this fd's `nonblock` bit is still stale.
            proc.clear_cloexec(fd);
            proc.clear_nonblock(fd);
            match entry {
                akuma_exec::process::FileDescriptor::Socket(idx) => { akuma_net::socket::remove_socket(idx); }
                akuma_exec::process::FileDescriptor::ChildStdout(child_pid) => {
                    akuma_exec::process::remove_child_channel(child_pid);
                }
                akuma_exec::process::FileDescriptor::PipeWrite(pipe_id) => {
                    super::pipe::pipe_close_write(pipe_id);
                }
                akuma_exec::process::FileDescriptor::PipeRead(pipe_id) => {
                    super::pipe::pipe_close_read(pipe_id);
                }
                akuma_exec::process::FileDescriptor::UnixSocket { rx, tx, sock } => {
                    super::pipe::pipe_close_read(rx);
                    super::pipe::pipe_close_write(tx);
                    super::unixsock::unix_sock_close(sock);
                }
                #[cfg(feature = "sc-eventfd")]
                akuma_exec::process::FileDescriptor::EventFd(efd_id) => {
                    super::eventfd::eventfd_close(efd_id);
                }
                _ => {}
            }
        }
    }
    0
}

pub(super) fn sys_lseek(fd: u32, offset: i64, whence: i32) -> u64 {
    if let Some(proc) = akuma_exec::process::current_process_shared() {
        if matches!(proc.get_fd(fd), Some(akuma_exec::process::FileDescriptor::DevNull | akuma_exec::process::FileDescriptor::DevZero)) {
            return 0;
        }
        // SEEK_END needs the on-disk size, so lseek on a real file is a VFS call. It
        // happens *inside* the `update_fd` closure (i.e. under the fd table's
        // `with_irqs_disabled`), so the ext2 work below is already IRQ-masked — no nested
        // exception can wait on the BKL from inside it, which is what makes the drop safe.
        let _vfs_bkl = VfsBklGuard::new_if(matches!(
            proc.get_fd(fd),
            Some(akuma_exec::process::FileDescriptor::File(_) | akuma_exec::process::FileDescriptor::BlockDev { .. })
        ));

        let mut new_pos = 0i64;
        let mut success = false;
        let mut bad_fd = true;
        let mut is_seekable = false;
        proc.update_fd(fd, |entry| {
            bad_fd = false;
            match entry {
                akuma_exec::process::FileDescriptor::File(f) => {
                    is_seekable = true;
                    // By inode, like `read` — so `SEEK_END` still knows how big
                    // the file is after its name is gone, instead of silently
                    // treating an unlinked-but-open fd as a zero-length file and
                    // seeking to `offset`.
                    let size = crate::fs::metadata_open_file(&f.path, f.mount_id(), f.inode())
                        .map_or(0, |m| m.size) as i64;
                    new_pos = match whence { 0 => offset, 1 => f.position as i64 + offset, 2 => size + offset, _ => -1 };
                    if new_pos >= 0 {
                        f.position = new_pos as usize;
                        if new_pos == 0 { f.dir_cache = None; }
                        success = true;
                    }
                }
                akuma_exec::process::FileDescriptor::BlockDev { idx, pos, .. } => {
                    is_seekable = true;
                    // SEEK_END needs the device's byte capacity — what `dd
                    // seek=`/`skip=` (past the drop-off drive's known size)
                    // and a plain size probe both need.
                    let size = crate::block::with_device_at(*idx as usize, akuma_virtio::block::VirtioBlockDevice::capacity_bytes).unwrap_or(0) as i64;
                    new_pos = match whence { 0 => offset, 1 => *pos as i64 + offset, 2 => size + offset, _ => -1 };
                    if new_pos >= 0 {
                        *pos = new_pos as u64;
                        success = true;
                    }
                }
                _ => {}
            }
        });
        if success {
            new_pos as u64
        } else if bad_fd {
            EBADF
        } else if is_seekable {
            // The fd is a real, seekable file/block device but the requested
            // offset/whence is invalid (negative result or unknown `whence`) → EINVAL.
            EINVAL
        } else {
            // The fd exists but refers to a pipe, socket, terminal, eventfd, etc.
            // POSIX requires `lseek` on a non-seekable object to return ESPIPE,
            // not EINVAL. Rust/musl probe stderr with `lseek(fd, 0, SEEK_CUR)` to
            // test seekability and expect ESPIPE on a pipe/tty.
            ESPIPE
        }
    } else { ESRCH }
}

pub(super) fn sys_fstat(fd: u32, stat_ptr: u64) -> u64 {
    let stat_size = core::mem::size_of::<Stat>();
    if !validate_user_ptr(stat_ptr, stat_size) { return EFAULT; }
    let proc = match akuma_exec::process::current_process_shared() { Some(p) => p, None => return ESRCH };
    
    let mut stat = Stat::default();
    let res = match proc.get_fd(fd) {
        Some(akuma_exec::process::FileDescriptor::File(f)) => {
            // Only this arm reaches the on-disk VFS; every other arm below synthesizes a
            // Stat from constants (or forwards cross-core, which must keep the BKL).
            let _vfs_bkl = VfsBklGuard::new();
            if let Ok(meta) = crate::vfs::metadata_open_file(&f.path, f.mount_id(), f.inode()) {
                stat = Stat { st_dev: 1, st_ino: meta.inode, st_size: meta.size as i64, st_mode: meta.mode, st_nlink: if meta.is_dir { 2 } else { 1 }, st_blksize: 4096, st_blocks: ((meta.size as i64) + 511) / 512, st_atime: meta.accessed.unwrap_or(0) as i64, st_mtime: meta.modified.unwrap_or(0) as i64, st_ctime: meta.created.unwrap_or(0) as i64, ..Default::default() };
                if crate::config::SYSCALL_DEBUG_IO_ENABLED {
                    crate::safe_print!(256, "[syscall] fstat(fd={}, file={}) size={} mode=0o{:o}\n", fd, &f.path, meta.size, meta.mode);
                }
                0
            } else { ENOENT }
        }
        Some(akuma_exec::process::FileDescriptor::DevNull) => {
            stat = Stat { st_dev: 0, st_ino: 1, st_size: 0, st_mode: 0o20666, st_nlink: 1, st_rdev: makedev(1, 3), st_blksize: 4096, ..Default::default() };
            0
        }
        Some(akuma_exec::process::FileDescriptor::DevUrandom) => {
            stat = Stat { st_dev: 0, st_ino: 9, st_size: 0, st_mode: 0o20666, st_nlink: 1, st_rdev: makedev(1, 9), st_blksize: 4096, ..Default::default() };
            0
        }
        Some(akuma_exec::process::FileDescriptor::DevZero) => {
            stat = Stat { st_dev: 0, st_ino: 5, st_size: 0, st_mode: 0o20666, st_nlink: 1, st_rdev: makedev(1, 5), st_blksize: 4096, ..Default::default() };
            0
        }
        Some(akuma_exec::process::FileDescriptor::BlockDev { idx, .. }) => {
            // Mirrors `sys_newfstatat`'s `dev_node` lookup for mode/major/minor/ino;
            // `st_size` here is the one thing that lookup can't give (`DevNode` has
            // no size field) — the raw fd knows its device index, so ask the block
            // driver directly.
            let node = crate::block::device_name(idx as usize).and_then(crate::vfs::dev_node_named);
            let capacity = crate::block::with_device_at(idx as usize, akuma_virtio::block::VirtioBlockDevice::capacity_bytes).unwrap_or(0);
            if let Some(node) = node {
                stat = Stat {
                    st_dev: 0,
                    st_ino: node.ino,
                    st_size: capacity as i64,
                    st_mode: node.mode(),
                    st_nlink: 1,
                    st_rdev: makedev(u64::from(node.major), u64::from(node.minor)),
                    st_blksize: 512,
                    st_blocks: (capacity as i64) / 512,
                    ..Default::default()
                };
                0
            } else {
                ENODEV
            }
        }
        Some(akuma_exec::process::FileDescriptor::Tap { .. }) => {
            // /dev/net/tap0: char device. Linux's TUN/TAP is misc major 10, minor 200.
            stat = Stat { st_dev: 0, st_ino: 0, st_size: 0, st_mode: 0o20666, st_nlink: 1, st_rdev: makedev(10, 200), st_blksize: 4096, ..Default::default() };
            0
        }
        Some(akuma_exec::process::FileDescriptor::TimerFd(_) |
akuma_exec::process::FileDescriptor::EpollFd(_)) => {
            stat = Stat { st_dev: 0, st_ino: 0, st_size: 0, st_mode: 0o100600, st_nlink: 1, st_blksize: 4096, ..Default::default() };
            0
        }
        Some(akuma_exec::process::FileDescriptor::Stdin |
akuma_exec::process::FileDescriptor::Stdout |
akuma_exec::process::FileDescriptor::Stderr |
akuma_exec::process::FileDescriptor::DevTty) => {
            stat = Stat { st_dev: 0, st_ino: 0, st_size: 0, st_mode: 0o20620, st_nlink: 1, st_rdev: makedev(136, 0), st_blksize: 1024, ..Default::default() };
            0
        }
        Some(akuma_exec::process::FileDescriptor::PipeRead(_) |
akuma_exec::process::FileDescriptor::PipeWrite(_)) => {
            stat = Stat { st_dev: 0, st_ino: 0, st_size: 0, st_mode: 0o10600, st_nlink: 1, st_blksize: 4096, ..Default::default() };
            0
        }
        _ => EBADF,
    };
    
    if res == 0 && write_user_val(stat_ptr, &stat).is_err() {
        return EFAULT;
    }
    res
}

pub(super) fn sys_newfstatat(dirfd: i32, path_ptr: u64, stat_ptr: u64, _flags: u32) -> SysResult {
    let path = copy_from_user_str(path_ptr, 512)?;
    if !validate_user_ptr(stat_ptr, core::mem::size_of::<Stat>()) { return Err(EFAULT); }

    let resolved_path = if path.starts_with('/') {
         String::from(&path)
    } else {
        let base_path = if dirfd == -100 {
             if let Some(proc) = akuma_exec::process::current_process_shared() {
                 proc.cwd.clone()
             } else {
                 return Err(ESRCH);
             }
        } else if dirfd >= 0 {
             if let Some(proc) = akuma_exec::process::current_process_shared() {
                 if let Some(akuma_exec::process::FileDescriptor::File(f)) = proc.get_fd(dirfd as u32) {
                     f.path
                 } else {
                     return Err(EBADF);
                 }
             } else {
                 return Err(ESRCH);
             }
        } else {
            return Err(EBADF);
        };
        crate::vfs::resolve_path(&base_path, &path)
    };

    // Path resolution + `metadata` below are pure VFS work — run them BKL-free.
    let _vfs_bkl = VfsBklGuard::new();

    let mut stat = Stat::default();
    let res = (|| {
        // One table lookup where `/dev/null` and `/dev/zero` used to be two
        // hand-copied arms — which is why `/dev/random`, `/dev/urandom` and
        // `/dev/dsp` all `open()`ed fine and then `stat()`ed `ENOENT`
        // (`DEVFS_MISSING.md` §1.2). `Metadata` has no `rdev` field, so this
        // calls `dev_node` rather than going through `crate::vfs::metadata`.
        if let Some(node) = crate::vfs::dev_node(&resolved_path) {
            stat = Stat {
                st_dev: 0,
                st_ino: node.ino,
                st_size: 0,
                st_mode: node.mode(),
                st_nlink: 1,
                st_rdev: makedev(u64::from(node.major), u64::from(node.minor)),
                st_blksize: 4096,
                ..Default::default()
            };
            return 0;
        }

        let follow = _flags & akuma_syscalls_linux::flags::at::AT_SYMLINK_NOFOLLOW == 0;

        if !follow && crate::vfs::is_symlink(&resolved_path) {
            let target = crate::vfs::read_symlink(&resolved_path).unwrap_or_default();
            stat = Stat {
                st_dev: 1,
                st_ino: 1,
                st_size: target.len() as i64,
                st_mode: 0o120777,
                st_nlink: 1,
                st_blksize: 4096,
                ..Default::default()
            };
            return 0;
        }

        let final_path = if follow { crate::vfs::resolve_symlinks(&resolved_path) } else { resolved_path };

        if let Ok(meta) = crate::vfs::metadata(&final_path) {
            if crate::config::SYSCALL_DEBUG_IO_ENABLED {
                crate::safe_print!(128, "[syscall] newfstatat({}) mode=0o{:o} size={}\n", final_path, meta.mode, meta.size);
            }
            stat = Stat { 
                st_dev: 1,
                st_ino: meta.inode,
                st_size: meta.size as i64, 
                st_mode: meta.mode, 
                st_nlink: if meta.is_dir { 2 } else { 1 },
                st_blksize: 4096,
                st_blocks: ((meta.size as i64) + 511) / 512,
                st_atime: meta.accessed.unwrap_or(0) as i64,
                st_mtime: meta.modified.unwrap_or(0) as i64,
                st_ctime: meta.created.unwrap_or(0) as i64,
                ..Default::default() 
            };
            return 0;
        }

        if crate::vfs::is_symlink(&final_path) {
            let target = crate::vfs::read_symlink(&final_path).unwrap_or_default();
            stat = Stat {
                st_dev: 1,
                st_ino: 1,
                st_size: target.len() as i64,
                st_mode: 0o120777,
                st_nlink: 1,
                st_blksize: 4096,
                ..Default::default()
            };
            return 0;
        }
        
        if crate::config::SYSCALL_DEBUG_IO_ENABLED {
            crate::safe_print!(128, "[syscall] newfstatat: ENOENT {}\n", final_path);
        }
        ENOENT
    })();

    if res == 0 && write_user_val(stat_ptr, &stat).is_err() {
        return Err(EFAULT);
    }
    Ok(res)
}

pub(super) fn sys_fchmod(fd: u32, mode: u32) -> u64 {
    let proc = match akuma_exec::process::current_process_shared() { Some(p) => p, None => return EBADF };
    match proc.get_fd(fd) {
        Some(akuma_exec::process::FileDescriptor::File(f)) => {
            match crate::vfs::chmod(&f.path, mode) {
                Ok(()) => 0,
                Err(e) => fs_error_to_errno(e),
            }
        }
        Some(akuma_exec::process::FileDescriptor::DevNull) => 0,
        _ => 0,
    }
}

pub(super) fn sys_fchmodat(dirfd: i32, path_ptr: u64, mode: u32) -> SysResult {
    let raw_path = copy_from_user_str(path_ptr, 512)?;

    // Build the dirfd-relative base path (fd-table lookup only, no disk I/O) before
    // entering the VFS BKL window — mirrors sys_unlinkat/sys_mkdirat/sys_renameat.
    let base: Option<String> = if raw_path.starts_with('/') {
        None
    } else if dirfd == -100 {
        match akuma_exec::process::current_process_shared() {
            Some(proc) => Some(proc.cwd.clone()),
            None => return Err(EBADF),
        }
    } else if dirfd >= 0 {
        let proc = match akuma_exec::process::current_process_shared() {
            Some(p) => p,
            None => return Err(EBADF),
        };
        match proc.get_fd(dirfd as u32) {
            Some(akuma_exec::process::FileDescriptor::File(f)) => Some(f.path),
            _ => return Err(EBADF),
        }
    } else {
        return Err(EBADF);
    };

    // `resolve_symlinks` does a real on-disk symlink-target lookup (same class of
    // real lookup as renameat2's RENAME_NOREPLACE `exists` probe, §14.2), and
    // `crate::vfs::chmod` takes the ext2 write guard for the on-disk mode-bits
    // update — attribution named `fchmodat` (syscall 53) alongside `mkdirat` as the
    // next-largest untouched Phase 2c holders: docs/archive/BKL_VFS_CARVE_OUT.md
    // §14.6.
    let _vfs_bkl = VfsBklGuard::new();

    let path = match base {
        Some(b) => crate::vfs::resolve_path(&b, &raw_path),
        None => crate::vfs::canonicalize_path(&raw_path),
    };
    let path = crate::vfs::resolve_symlinks(&path);

    // chmod on a device node is a no-op that succeeds. Was `/dev/null` and
    // `/dev/zero` only; through the table it covers every node, so
    // `chmod /dev/urandom` stops returning `ENOENT` for a path that exists.
    if crate::vfs::dev_node(&path).is_some() {
        return Ok(0);
    }

    match crate::vfs::chmod(&path, mode) {
        Ok(()) => Ok(0),
        Err(e) => Err(fs_error_to_errno(e)),
    }
}

pub(super) fn sys_fallocate(fd: u32, mode: i32, offset: i64, len: i64) -> u64 {
    if offset < 0 || len <= 0 {
        return super::EINVAL;
    }
    let proc = match akuma_exec::process::current_process_shared() { Some(p) => p, None => return EBADF };
    match proc.get_fd(fd) {
        Some(akuma_exec::process::FileDescriptor::File(f)) => {
            match crate::vfs::fallocate(&f.path, mode, offset as u64, len as u64) {
                Ok(()) => 0,
                Err(e) => fs_error_to_errno(e),
            }
        }
        Some(akuma_exec::process::FileDescriptor::DevNull | akuma_exec::process::FileDescriptor::DevZero) => 0,
        _ => EBADF,
    }
}

pub(super) fn sys_ftruncate(fd: u32, length: i64) -> u64 {
    let proc = match akuma_exec::process::current_process_shared() { Some(p) => p, None => return EBADF };
    match proc.get_fd(fd) {
        Some(akuma_exec::process::FileDescriptor::File(f)) => {
            // Diagnostic (2026-08-15 hunt §14), gated: truncation of a live
            // artifact while dependents map it — see sys_openat's
            // [O_TRUNC-ZAP]. Never fired on this workload (the actor is
            // open(O_TRUNC)+unlink, not ftruncate).
            if crate::config::SYSCALL_DEBUG_IO_ENABLED
                && length == 0
                && f.path.contains("target")
            {
                crate::safe_print!(192, "[FTRUNC-0] pid={} path={}\n",
                    akuma_exec::process::read_current_pid().unwrap_or(0), &f.path);
            }
            match crate::vfs::truncate(&f.path, length as u64) {
                Ok(()) => 0,
                Err(e) => fs_error_to_errno(e),
            }
        }
        Some(akuma_exec::process::FileDescriptor::DevNull | akuma_exec::process::FileDescriptor::DevZero) => 0,
        _ => EBADF,
    }
}

pub(super) fn sys_truncate(path_ptr: u64, length: i64) -> SysResult {
    let path = copy_from_user_str(path_ptr, 512)?;
    let resolved = if path.starts_with('/') {
        path
    } else if let Some(proc) = akuma_exec::process::current_process_shared() {
        crate::vfs::resolve_path(&proc.cwd, &path)
    } else {
        return Err(EBADF);
    };
    match crate::vfs::truncate(&resolved, length as u64) {
        Ok(()) => Ok(0),
        Err(e) => Err(fs_error_to_errno(e)),
    }
}

pub(super) fn sys_statx(dirfd: i32, path_ptr: u64, flags: u32, _mask: u32, buf_ptr: u64) -> SysResult {
    // Early-out before any fs work; the copy at the end validates again for real. Sized
    // from the struct so the two can no longer disagree.
    if !validate_user_ptr(buf_ptr, core::mem::size_of::<Statx>()) { return Err(EFAULT); }

    let path = copy_from_user_str(path_ptr, 512)?;

    // `fd_inode` is non-zero only for the `AT_EMPTY_PATH` form — "stat the open
    // file description itself", i.e. `fstat` spelled as `statx`. That form gets
    // the same inode-first treatment `sys_fstat` does, so it too keeps answering
    // once the fd's name is gone. Every other form takes a *path* (the dirfd
    // below is only a base to join onto), which no inode number can stand in for.
    let mut fd_inode: u32 = 0;
    let mut fd_mount: u32 = 0;
    let resolved_path = if path.is_empty() {
        if flags & akuma_syscalls_linux::flags::at::AT_EMPTY_PATH != 0 && dirfd >= 0 {
            if let Some(proc) = akuma_exec::process::current_process_shared() {
                if let Some(akuma_exec::process::FileDescriptor::File(f)) = proc.get_fd(dirfd as u32) {
                    fd_inode = f.inode();
                    fd_mount = f.mount_id();
                    f.path
                } else {
                    return Err(EBADF);
                }
            } else {
                return Err(EBADF);
            }
        } else {
            return Err(ENOENT);
        }
    } else if path.starts_with('/') {
        String::from(&path)
    } else {
        let base_path = if dirfd == -100 {
            if let Some(proc) = akuma_exec::process::current_process_shared() {
                proc.cwd.clone()
            } else {
                return Err(EBADF);
            }
        } else if dirfd >= 0 {
            if let Some(proc) = akuma_exec::process::current_process_shared() {
                if let Some(akuma_exec::process::FileDescriptor::File(f)) = proc.get_fd(dirfd as u32) {
                    f.path
                } else {
                    return Err(EBADF);
                }
            } else {
                return Err(EBADF);
            }
        } else {
            return Err(EINVAL);
        };
        crate::vfs::resolve_path(&base_path, &path)
    };

    // Everything from here on is symlink resolution + `metadata` — pure VFS, BKL-free.
    // The user copies happen after, from `buf` (a kernel stack buffer), so no user access
    // is interleaved with the fs work.
    let _vfs_bkl = VfsBklGuard::new();

    let follow = flags & akuma_syscalls_linux::flags::at::AT_SYMLINK_NOFOLLOW == 0;

    let (mode, ino, size, nlink, atime, mtime, ctime, rdev_major, rdev_minor) =
        // The second copy of `sys_newfstatat`'s device arms, now the same
        // single lookup — see the comment there.
        if let Some(node) = crate::vfs::dev_node(&resolved_path) {
            (node.mode() as u16, node.ino, 0u64, 1u32, 0i64, 0i64, 0i64, node.major, node.minor)
        } else if !follow && crate::vfs::is_symlink(&resolved_path) {
            let target = crate::vfs::read_symlink(&resolved_path).unwrap_or_default();
            (0o120777u16, 1, target.len() as u64, 1, 0, 0, 0, 0, 0)
        } else {
            let final_path = if follow { crate::vfs::resolve_symlinks(&resolved_path) } else { resolved_path };
            if let Ok(meta) = crate::vfs::metadata_open_file(&final_path, fd_mount, fd_inode) {
                (meta.mode as u16, meta.inode, meta.size,
                 if meta.is_dir { 2 } else { 1 },
                 meta.accessed.unwrap_or(0) as i64,
                 meta.modified.unwrap_or(0) as i64,
                 meta.created.unwrap_or(0) as i64,
                 0, 0)
            } else if crate::vfs::is_symlink(&final_path) {
                let target = crate::vfs::read_symlink(&final_path).unwrap_or_default();
                (0o120777u16, 1, target.len() as u64, 1, 0, 0, 0, 0, 0)
            } else {
                return Err(ENOENT);
            }
        };

    let blksize: u32 = 4096;
    let blocks: u64 = size.div_ceil(512);

    // STATX_BASIC_STATS covers type/mode/nlink/uid/gid/ino/size/blocks/times
    const STATX_BASIC_STATS: u32 = 0x07ff;

    // Only the fields STATX_BASIC_STATS advertises are filled; the rest stay zero, as
    // they did when this was a zeroed byte buffer. `stx_uid`/`stx_gid` are 0 (root) and
    // `stx_attributes`/`stx_attributes_mask` are unsupported, so both stay zero too.
    let statx = Statx {
        stx_mask: STATX_BASIC_STATS,
        stx_blksize: blksize,
        stx_nlink: nlink,
        stx_mode: mode,
        stx_ino: ino,
        stx_size: size,
        stx_blocks: blocks,
        stx_atime: StatxTimestamp { tv_sec: atime, ..Default::default() },
        stx_ctime: StatxTimestamp { tv_sec: ctime, ..Default::default() },
        stx_mtime: StatxTimestamp { tv_sec: mtime, ..Default::default() },
        stx_rdev_major: rdev_major,
        stx_rdev_minor: rdev_minor,
        // The device the file lives on: 0:1, matching `sys_newfstatat`'s `st_dev`.
        stx_dev_major: 0,
        stx_dev_minor: 1,
        stx_mnt_id: 1,
        ..Default::default()
    };

    if write_user_val(buf_ptr, &statx).is_err() {
        return Err(EFAULT);
    }
    Ok(0)
}

pub(super) fn sys_faccessat2(dirfd: i32, path_ptr: u64, _mode: u32, _flags: u32) -> SysResult {
    let path = copy_from_user_str(path_ptr, 512)?;
    
    let resolved_path = if path.starts_with('/') {
         path
    } else {
        let base_path = if dirfd == -100 {
             if let Some(proc) = akuma_exec::process::current_process_shared() {
                 proc.cwd.clone()
             } else {
                 return Err(ESRCH);
             }
        } else if dirfd >= 0 {
             if let Some(proc) = akuma_exec::process::current_process_shared() {
                 if let Some(akuma_exec::process::FileDescriptor::File(f)) = proc.get_fd(dirfd as u32) {
                     f.path
                 } else {
                     return Err(EBADF);
                 }
             } else {
                 return Err(ESRCH);
             }
        } else {
            return Err(EBADF);
        };
        crate::vfs::resolve_path(&base_path, &path)
    };
    
    let final_path = crate::vfs::resolve_symlinks(&resolved_path);
    if crate::fs::exists(&final_path) || crate::vfs::is_symlink(&resolved_path) {
        Ok(0)
    } else {
        if crate::config::SYSCALL_DEBUG_IO_ENABLED {
            crate::safe_print!(128, "[syscall] faccessat: ENOENT {}\n", final_path);
        }
        Err(ENOENT)
    }
}

pub(super) fn sys_getcwd(buf_ptr: u64, size: usize) -> u64 {
    if !validate_user_ptr(buf_ptr, size) { return EFAULT; }
    if let Some(proc) = akuma_exec::process::current_process_shared() {
        let cwd_bytes = proc.cwd.as_bytes();
        if cwd_bytes.len() + 1 > size {
            return ERANGE;
        }
        let mut temp = alloc::vec![0u8; cwd_bytes.len() + 1];
        temp[..cwd_bytes.len()].copy_from_slice(cwd_bytes);
        temp[cwd_bytes.len()] = 0;
        
        if copy_to_user(buf_ptr, &temp).is_err() {
            return EFAULT;
        }
        return temp.len() as u64;
    }
    ENOENT
}

pub(super) fn sys_fcntl(fd: u32, cmd: u32, arg: u64) -> u64 {
    // The command table, and the two flag bits `fcntl` moves, from
    // `akuma-syscalls-linux`. `F_SETLK`/`F_SETLKW`/`F_GETLK` are advisory
    // record locking — no-op stubs, we have no lock state. `F_SETOWN`/
    // `F_GETOWN` set the SIGIO owner for an fd, paired with `ioctl(FIOASYNC)`
    // (src/syscall/term.rs); also a no-op, because Akuma delivers no SIGIO —
    // but nginx's `ngx_spawn_process` treats a failing `F_SETOWN` as fatal
    // before it ever calls fork(), so accepting it is what lets the worker
    // process actually spawn.
    use akuma_syscalls_linux::flags::fcntl::{
        F_DUPFD, F_DUPFD_CLOEXEC, F_GETFD, F_GETFL, F_GETLK, F_GETOWN, F_SETFD, F_SETFL, F_SETLK,
        F_SETLKW, F_SETOWN,
    };
    // `arg` and this function's return are the raw 64-bit syscall registers, so
    // the two bits tested against them are widened once, here.
    const FD_CLOEXEC: u64 = akuma_syscalls_linux::flags::fcntl::FD_CLOEXEC as u64;
    const O_NONBLOCK: u64 = akuma_syscalls_linux::flags::open::O_NONBLOCK as u64;

    let proc = match akuma_exec::process::current_process_shared() { Some(p) => p, None => return EBADF };

    if proc.get_fd(fd).is_none() {
        return EBADF;
    }

    match cmd {
        F_DUPFD | F_DUPFD_CLOEXEC => {
            let entry = match proc.get_fd(fd) { Some(e) => e, None => return EBADF };
            // Bump refcounts before inserting the duplicate entry. One list, in
            // `akuma_exec::process::clone_fd_refs`. This was a local four-arm
            // copy that had drifted: `EventFd` and `RumpSocket` are refcounted
            // but were handled only on the fork path, so a dup of either
            // produced an unreferenced alias and the first close destroyed the
            // object under the surviving fd.
            akuma_exec::process::clone_fd_refs(&entry);
            let new_fd = proc.alloc_fd_from(arg as u32, entry);
            if cmd == F_DUPFD_CLOEXEC {
                proc.set_cloexec(new_fd);
            }
            u64::from(new_fd)
        }
        F_GETFD => {
            if proc.is_cloexec(fd) { FD_CLOEXEC } else { 0 }
        }
        F_SETFD => {
            if arg & FD_CLOEXEC != 0 {
                proc.set_cloexec(fd);
            } else {
                proc.clear_cloexec(fd);
            }
            0
        }
        F_GETFL => {
            if proc.is_nonblock(fd) { O_NONBLOCK } else { 0 }
        }
        F_SETFL => {
            if arg & O_NONBLOCK != 0 {
                proc.set_nonblock(fd);
            } else {
                proc.clear_nonblock(fd);
            }
            0
        }
        // Advisory locks: no-op (we have no file locking state)
        F_GETLK | F_SETLK | F_SETLKW => 0,
        F_SETOWN | F_GETOWN => 0,
        _ => {
            crate::safe_print!(192, "[fcntl] UNSUPPORTED: pid={} fd={} cmd={} arg={:#x}\n",
                proc.pid, fd, cmd, arg);
            EINVAL
        },
    }
}

pub(super) fn sys_mkdirat(dirfd: i32, path_ptr: u64, _mode: u32) -> SysResult {
    let raw_path = copy_from_user_str(path_ptr, 512)?;

    // Build the dirfd-relative base path (fd-table lookup only, no disk I/O) before
    // entering the VFS BKL window — mirrors sys_unlinkat/sys_renameat: early-return
    // EBADF paths must not pay for a BKL drop/reacquire when no on-disk work will
    // happen.
    let base: Option<String> = if raw_path.starts_with('/') {
        None
    } else if dirfd == -100 {
        match akuma_exec::process::current_process_shared() {
            Some(proc) => Some(proc.cwd.clone()),
            None => return Err(EBADF),
        }
    } else if dirfd >= 0 {
        let proc = match akuma_exec::process::current_process_shared() {
            Some(p) => p,
            None => return Err(EBADF),
        };
        match proc.get_fd(dirfd as u32) {
            Some(akuma_exec::process::FileDescriptor::File(f)) => Some(f.path),
            _ => return Err(EBADF),
        }
    } else {
        return Err(EBADF);
    };

    // `crate::fs::create_dir` takes the ext2 write guard for the on-disk inode
    // allocation + directory-entry write — attribution named `mkdirat` (syscall 34)
    // the next-largest untouched Phase 2c holder after `renameat`:
    // docs/archive/BKL_VFS_CARVE_OUT.md §14.6.
    let _vfs_bkl = VfsBklGuard::new();

    let path = match base {
        Some(b) => crate::vfs::resolve_path(&b, &raw_path),
        None => crate::vfs::canonicalize_path(&raw_path),
    };

    if crate::config::SYSCALL_DEBUG_IO_ENABLED {
        crate::safe_print!(256, "[syscall] mkdirat({}) dirfd={}\n", &path, dirfd);
    }

    match crate::fs::create_dir(&path) {
        Ok(()) => Ok(0),
        Err(e) => Err(fs_error_to_errno(e)),
    }
}

pub(super) fn sys_unlinkat(dirfd: i32, path_ptr: u64, flags: u32) -> SysResult {
    let path = copy_from_user_str(path_ptr, 512)?;

    // Build the dirfd-relative base path (fd-table lookups only, no disk I/O) before
    // entering the VFS BKL window — matches sys_statx/sys_newfstatat: the early-return
    // EBADF paths must not pay for a BKL drop/reacquire when no on-disk work will happen.
    //
    // The path walk + inode deletion below is exactly the on-disk mutating work Phase 2c
    // targets: `unlinkat` (syscall 35) measured 72.6% of all cross-core BKL wait —
    // docs/archive/BKL_VFS_CARVE_OUT.md §11.6.
    let base: Option<String> = if path.starts_with('/') {
        None
    } else if dirfd == -100 {
        match akuma_exec::process::current_process_shared() {
            Some(proc) => Some(proc.cwd.clone()),
            None => return Err(EBADF),
        }
    } else if dirfd >= 0 {
        let proc = match akuma_exec::process::current_process_shared() {
            Some(p) => p,
            None => return Err(EBADF),
        };
        match proc.get_fd(dirfd as u32) {
            Some(akuma_exec::process::FileDescriptor::File(f)) => Some(f.path),
            _ => return Err(EBADF),
        }
    } else {
        return Err(EBADF);
    };

    // From here on is pure VFS work (directory walk + ext2 inode/block deallocation, which
    // takes the ext2 write guard) — run it BKL-free. `remove_file` of a large file can hold
    // the write guard for tens of seconds (§7.2: one 735 MB `rm` = ~40 s), which is the
    // hold this carve-out exists to remove from the peer core's view.
    let _vfs_bkl = VfsBklGuard::new();

    let resolved = match base {
        Some(b) => crate::vfs::resolve_path(&b, &path),
        None => crate::vfs::canonicalize_path(&path),
    };

    if crate::config::SYSCALL_DEBUG_IO_ENABLED {
        crate::safe_print!(256, "[syscall] unlinkat({}) flags=0x{:x}\n", &resolved, flags);
        // Diagnostic (2026-08-15 hunt §14): unlink frees the inode for reuse
        // while other processes may still hold lazy regions naming it — their
        // fills then read whatever file next claims the number (decode
        // garbage). This print caught the actor (cargo clean, 1000+/build).
        // Same gate as the trace above it; target-scoped to cut noise.
        if resolved.contains("target") {
            crate::safe_print!(192,
                "[UNLINK] pid={} path={}\n",
                akuma_exec::process::read_current_pid().unwrap_or(0), &resolved);
        }
    }

    if flags & akuma_syscalls_linux::flags::at::AT_REMOVEDIR != 0 {
        match crate::fs::remove_dir(&resolved) {
            Ok(()) => Ok(0),
            Err(e) => Err(fs_error_to_errno(e)),
        }
    } else {
        crate::vfs::remove_symlink(&resolved);
        match crate::fs::remove_file(&resolved) {
            Ok(()) => Ok(0),
            Err(e) => Err(fs_error_to_errno(e)),
        }
    }
}

pub(super) fn sys_renameat(olddirfd: i32, oldpath_ptr: u64, newdirfd: i32, newpath_ptr: u64) -> SysResult {
    let raw_old = copy_from_user_str(oldpath_ptr, 512)?;
    let raw_new = copy_from_user_str(newpath_ptr, 512)?;

    // From here on is pure VFS work (dirfd/cwd-relative path resolution — fd-table +
    // string ops only, no disk I/O — plus the on-disk directory-entry rewrite in
    // `crate::fs::rename`, which takes the ext2 write guard). Mirrors `sys_unlinkat`
    // (§12) — attribution named `renameat` (syscall 38) the largest untouched-syscall
    // BKL holder after `unlinkat`/`openat`: docs/archive/BKL_VFS_CARVE_OUT.md §14.
    let _vfs_bkl = VfsBklGuard::new();

    let oldpath = resolve_path_at(olddirfd, &raw_old);
    let newpath = resolve_path_at(newdirfd, &raw_new);
    crate::safe_print!(256, "[syscall] renameat: {} -> {}\n", oldpath, newpath);
    match crate::fs::rename(&oldpath, &newpath) {
        Ok(()) => Ok(0),
        Err(e) => Err(fs_error_to_errno(e)),
    }
}

const RENAME_NOREPLACE: u32 = 1;
const RENAME_EXCHANGE: u32 = 2;

pub(super) fn sys_renameat2(olddirfd: i32, oldpath_ptr: u64, newdirfd: i32, newpath_ptr: u64, flags: u32) -> SysResult {
    if flags & !(RENAME_NOREPLACE | RENAME_EXCHANGE) != 0 {
        return Err(super::EINVAL);
    }
    if flags & RENAME_NOREPLACE != 0 && flags & RENAME_EXCHANGE != 0 {
        return Err(super::EINVAL);
    }

    let raw_old = copy_from_user_str(oldpath_ptr, 512)?;
    let raw_new = copy_from_user_str(newpath_ptr, 512)?;

    // See sys_renameat above: path resolution is fd-table/string work only, and the
    // window also covers the `exists` probe (a real lookup) and `crate::fs::rename`
    // (the ext2-write-guarded directory-entry rewrite).
    let _vfs_bkl = VfsBklGuard::new();

    let oldpath = resolve_path_at(olddirfd, &raw_old);
    let newpath = resolve_path_at(newdirfd, &raw_new);

    if flags & RENAME_NOREPLACE != 0 && crate::vfs::exists(&newpath) {
        return Err(super::EEXIST);
    }

    if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
        crate::safe_print!(256, "[syscall] renameat2: {} -> {} flags=0x{:x}\n", oldpath, newpath, flags);
    }
    match crate::fs::rename(&oldpath, &newpath) {
        Ok(()) => Ok(0),
        Err(e) => Err(fs_error_to_errno(e)),
    }
}

pub(super) fn sys_symlinkat(target_ptr: u64, newdirfd: i32, linkpath_ptr: u64) -> SysResult {
    let target = copy_from_user_str(target_ptr, 1024)?;
    let raw_link = copy_from_user_str(linkpath_ptr, 1024)?;
    let link_path = resolve_path_at(newdirfd, &raw_link);
    if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
        crate::safe_print!(256, "[syscall] symlinkat: {} -> {}\n", link_path, target);
    }
    match crate::vfs::create_symlink(&link_path, &target) {
        Ok(()) => Ok(0),
        Err(e) => Err(fs_error_to_errno(e)),
    }
}

pub(super) fn sys_linkat(_olddirfd: i32, oldpath_ptr: u64, _newdirfd: i32, newpath_ptr: u64, _flags: u32) -> SysResult {
    let oldpath = copy_from_user_str(oldpath_ptr, 1024)?;
    let newpath = copy_from_user_str(newpath_ptr, 1024)?;
    let src = resolve_path_at(_olddirfd, &oldpath);
    let dst = resolve_path_at(_newdirfd, &newpath);
    match crate::fs::read_file(&src) {
        Ok(data) => match crate::fs::write_file(&dst, &data) {
            Ok(()) => Ok(0),
            Err(e) => Err(fs_error_to_errno(e)),
        },
        Err(e) => Err(fs_error_to_errno(e)),
    }
}

pub(super) fn sys_readlinkat(dirfd: i32, path_ptr: u64, buf_ptr: u64, bufsize: usize) -> SysResult {
    let raw_path = copy_from_user_str(path_ptr, 1024)?;
    let path = resolve_path_at(dirfd, &raw_path);

    if path == "/proc/self/exe" {
        if !validate_user_ptr(buf_ptr, bufsize) { return Err(EFAULT); }
        let exe = if let Some(proc) = akuma_exec::process::current_process_shared() {
            proc.name.clone()
        } else {
            String::from("/bin/unknown")
        };
        let bytes = exe.as_bytes();
        let copy_len = bytes.len().min(bufsize);
        if copy_to_user(buf_ptr, &bytes[..copy_len]).is_err() {
            return Err(EFAULT);
        }
        return Ok(copy_len as u64);
    }

    // Try filesystem symlinks first (includes File fds in procfs)
    let target = crate::vfs::read_symlink(&path)
        // Fall back to procfs fd description for non-file fds (pipes, sockets, etc.)
        .or_else(|| crate::vfs::proc::proc_fd_description(&path));

    if let Some(target) = target {
        if !validate_user_ptr(buf_ptr, bufsize) { return Err(EFAULT); }
        let bytes = target.as_bytes();
        let copy_len = bytes.len().min(bufsize);
        if copy_to_user(buf_ptr, &bytes[..copy_len]).is_err() {
            return Err(EFAULT);
        }
        return Ok(copy_len as u64);
    }

    if crate::vfs::exists(&path) {
        Err(EINVAL)
    } else {
        Err(ENOENT)
    }
}

pub(super) fn sys_getdents64(fd: u32, ptr: u64, size: usize) -> u64 {
    if !validate_user_ptr(ptr, size) { return EFAULT; }
    let proc = match akuma_exec::process::current_process_shared() { Some(p) => p, None => return ESRCH };
    let f = match proc.get_fd(fd) {
        Some(akuma_exec::process::FileDescriptor::File(f)) => f,
        _ => return EBADF,
    };

    // `fd` is already known to be a `File` (the match above returns EBADF otherwise), so
    // arm unconditionally: the cache-miss path below does a full directory read off disk.
    let _vfs_bkl = VfsBklGuard::new();

    let entries = if let Some(ref cached) = f.dir_cache {
        cached.clone()
    } else {
        let dir_entries = match crate::fs::list_dir(&f.path) {
            Ok(e) => e,
            Err(e) => return fs_error_to_errno(e),
        };
        // `DirEntry` carries only is_dir/is_symlink, so a device node would
        // otherwise report DT_REG. Only `/dev` can hold one, so the check is
        // hoisted out of the per-entry map: every other directory listing pays
        // one string compare total, not a table lookup per entry.
        let is_dev = crate::vfs::is_dev_dir(&f.path);
        let cache: alloc::vec::Vec<akuma_exec::process::types::DirCacheEntry> = dir_entries
            .iter()
            .map(|e| akuma_exec::process::types::DirCacheEntry {
                name: e.name.clone(),
                d_type: if e.is_dir {
                    4
                } else if e.is_symlink {
                    10
                } else {
                    match is_dev.then(|| crate::vfs::dev_node_named(&e.name)).flatten() {
                        Some(node) => node.d_type(),
                        None => 8,
                    }
                },
            })
            .collect();
        let snapshot = cache.clone();
        proc.update_fd(fd, |e| {
            if let akuma_exec::process::FileDescriptor::File(file) = e {
                file.dir_cache = Some(snapshot);
            }
        });
        cache
    };

    let position = f.position;
    if position >= entries.len() { return 0; }

    let mut kernel_buf = alloc::vec![0u8; size];
    let mut written = 0;
    let mut count = 0usize;
    for entry in entries.iter().skip(position) {
        let reclen = (19 + entry.name.len() + 1 + 7) & !7;
        if written + reclen > size { break; }
        let p = unsafe { kernel_buf.as_mut_ptr().add(written) };
        unsafe {
            core::ptr::write_unaligned(p.cast::<u64>(), 1);
            core::ptr::write_unaligned(p.add(8).cast::<u64>(), 1);
            core::ptr::write_unaligned(p.add(16).cast::<u16>(), reclen as u16);
            p.add(18).write(entry.d_type);
            core::ptr::copy_nonoverlapping(entry.name.as_ptr(), p.add(19), entry.name.len());
            p.add(19 + entry.name.len()).write(0);
        }
        written += reclen;
        count += 1;
    }
    if count > 0 {
        proc.update_fd(fd, |e| {
            if let akuma_exec::process::FileDescriptor::File(file) = e {
                file.position += count;
            }
        });
    }
    if written > 0 && copy_to_user(ptr, &kernel_buf[..written]).is_err() {
        return EFAULT;
    }
    written as u64
}

pub(super) fn sys_fchdir(fd: u32) -> u64 {
    let proc = match akuma_exec::process::current_process_shared() { Some(p) => p, None => return ESRCH };
    let entry = match proc.get_fd(fd) {
        Some(e) => e,
        None => return EBADF,
    };
    let path = match entry {
        akuma_exec::process::FileDescriptor::File(f) => f.path,
        _ => return ENOTDIR,
    };
    if let Ok(meta) = crate::vfs::metadata(&path)
        && meta.is_dir {
            // Move a pre-built String in; `with_current_process` runs IRQ-masked
            // and must not allocate (dropping the old cwd is fine).
            let new_cwd = path.clone();
            akuma_exec::process::with_current_process(|p| p.cwd = new_cwd);
            if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
                crate::safe_print!(128, "[syscall] fchdir(fd={}) -> \"{}\"\n", fd, path);
            }
            return 0;
        }
    ENOTDIR
}

pub(super) fn sys_chdir(ptr: u64) -> SysResult {
    let path = copy_from_user_str(ptr, 512)?;
    
    if let Some(proc) = akuma_exec::process::current_process_shared() {
        let new_cwd = crate::vfs::resolve_path(&proc.cwd, &path);
        
        if crate::fs::exists(&new_cwd)
            && let Ok(meta) = crate::vfs::metadata(&new_cwd)
                && meta.is_dir {
                    // Pre-built String moved into the IRQ-masked closure (no alloc inside).
                    akuma_exec::process::with_current_process(|p| p.cwd = new_cwd);
                    return Ok(0);
                }
        return Err(ENOENT);
    }
    Err(ESRCH)
}
