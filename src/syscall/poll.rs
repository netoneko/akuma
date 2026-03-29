use super::*;
use akuma_net::socket;
use akuma_exec::mmu::user_access::{copy_from_user_safe, copy_to_user_safe};
use core::sync::atomic::AtomicU64;
use core::task::Waker;
use alloc::collections::BTreeMap;

struct EpollEntry {
    events: u32,
    data: u64,
    last_ready: u32,
}

struct EpollInstance {
    interest_list: BTreeMap<u32, EpollEntry>,
}

static EPOLL_TABLE: Spinlock<BTreeMap<u32, EpollInstance>> = Spinlock::new(BTreeMap::new());
static NEXT_EPOLL_ID: AtomicU32 = AtomicU32::new(1);
/// Counts `epoll_pwait(timeout=0)` returns with `nready=0` for rate-limited logging.
static EPOLL_PWAIT_ZERO_ZERO_COUNT: AtomicU64 = AtomicU64::new(0);

const EPOLLIN: u32 = 0x001;
const EPOLLOUT: u32 = 0x004;
const EPOLLERR: u32 = 0x008;
const EPOLLHUP: u32 = 0x010;
const EPOLLRDHUP: u32 = 0x2000;
const EPOLLET: u32 = 1 << 31;
const EPOLL_EVENT_MASK: u32 = EPOLLIN | EPOLLOUT | EPOLLERR | EPOLLHUP | EPOLLRDHUP;

const EPOLL_CTL_ADD: i32 = 1;
const EPOLL_CTL_DEL: i32 = 2;
const EPOLL_CTL_MOD: i32 = 3;
const BLOCKING_POLL_INTERVAL_US: u64 = 10_000;

pub(crate) fn epoll_wait_deadline(timeout: i32, start_time: u64, timeout_us: u64, now: u64) -> u64 {
    if timeout > 0 {
        start_time + timeout_us
    } else if timeout == 0 {
        0
    } else {
        now + BLOCKING_POLL_INTERVAL_US
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct PollFd {
    fd: i32,
    events: i16,
    revents: i16,
}

// On ARM64, epoll_event is NOT packed (unlike x86_64).
// Layout: events (4 bytes) + padding (4 bytes) + data (8 bytes) = 16 bytes total
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct EpollEvent {
    pub(crate) events: u32,
    pub(crate) _pad: u32,  // ARM64 ABI padding
    pub(crate) data: u64,
}

/// One line per epoll_pwait return. Suppresses most `timeout=0, nready=0` returns (see config).
fn log_epoll_pwait_return(
    epfd: u32,
    timeout: i32,
    ready_count: usize,
    iterations: u64,
    start_time: u64,
    interest_fd_count: usize,
    kernel_events: &[EpollEvent],
    note: &'static str,
) {
    if !crate::config::SYSCALL_DEBUG_NET_ENABLED {
        return;
    }
    let pid = akuma_exec::process::read_current_pid().unwrap_or(0);
    let elapsed_us = crate::timer::uptime_us().saturating_sub(start_time);
    let nready = ready_count;
    let every = crate::config::EPOLL_ZERO_SAMPLE_INTERVAL.max(1);

    if timeout == 0 && nready == 0 && iterations == 1 && note.is_empty() {
        let n = EPOLL_PWAIT_ZERO_ZERO_COUNT.fetch_add(1, Ordering::Relaxed);
        if n % every != 0 {
            return;
        }
        crate::tprint!(
            224,
            "[epoll] pwait zero-sample#{} pid={} epfd={} nready=0 timeout=0ms (interval={} ~{} suppressed)\n",
            n / every,
            pid,
            epfd,
            every,
            every.saturating_sub(1),
        );
        return;
    }

    crate::tprint!(
        224,
        "[epoll] pwait ret pid={} epfd={} timeout_ms={} nready={} iters={} dur_us={} interest_fds={} {}\n",
        pid,
        epfd,
        timeout,
        nready,
        iterations,
        elapsed_us,
        interest_fd_count,
        note,
    );
    if nready == 0 || kernel_events.is_empty() {
        return;
    }
    for (i, ev) in kernel_events.iter().take(6).enumerate() {
        let in_flag = if ev.events & EPOLLIN != 0 { "IN" } else { "" };
        let out_flag = if ev.events & EPOLLOUT != 0 { "OUT" } else { "" };
        let hup_flag = if ev.events & EPOLLHUP != 0 { "HUP" } else { "" };
        let err_flag = if ev.events & EPOLLERR != 0 { "ERR" } else { "" };
        crate::tprint!(
            128,
            "[epoll]    ev[{}] data=0x{:x} {}{}{}{}\n",
            i,
            ev.data,
            in_flag,
            out_flag,
            hup_flag,
            err_flag
        );
    }
}

pub fn epoll_destroy(epoll_id: u32) {
    EPOLL_TABLE.lock().remove(&epoll_id);
}

/// Called when a non-blocking socket read returns EAGAIN (socket fully drained).
/// Resets the EPOLLET edge for this fd so the next data arrival fires a new EPOLLIN event.
/// Without this, if new data arrives within the same 10ms poll window as the drain,
/// the transition is missed and EPOLLIN never re-fires.
pub(super) fn epoll_on_fd_drained(fd: u32) {
    // Snapshot IDs to avoid holding EPOLL_TABLE lock during the entire iteration
    // (though not strictly necessary for this simple function yet, good practice)
    let ids: alloc::vec::Vec<u32> = {
        let table = EPOLL_TABLE.lock();
        table.keys().copied().collect()
    };

    for epoll_id in ids {
        let mut table = EPOLL_TABLE.lock();
        if let Some(inst) = table.get_mut(&epoll_id) {
            if let Some(entry) = inst.interest_list.get_mut(&fd) {
                if entry.events & EPOLLET != 0 {
                    entry.last_ready &= !EPOLLIN;
                }
            }
        }
    }
}

const EPOLL_CLOEXEC: u32 = 0o2000000;

pub(crate) fn sys_epoll_create1(flags: u32) -> u64 {
    if let Some(proc) = akuma_exec::process::current_process() {
        let epoll_id = NEXT_EPOLL_ID.fetch_add(1, Ordering::SeqCst);
        EPOLL_TABLE.lock().insert(epoll_id, EpollInstance {
            interest_list: BTreeMap::new(),
        });
        let fd = proc.alloc_fd(akuma_exec::process::FileDescriptor::EpollFd(epoll_id));
        if flags & EPOLL_CLOEXEC != 0 {
            proc.set_cloexec(fd);
        }
        crate::tprint!(96, "[epoll] create1() id={} fd={} cloexec={}\n", epoll_id, fd, flags & EPOLL_CLOEXEC != 0);
        fd as u64
    } else {
        EBADF
    }
}

pub(crate) fn sys_epoll_ctl(epfd: u32, op: i32, fd: u32, event_ptr: usize) -> u64 {
    let epoll_id = match akuma_exec::process::current_process().and_then(|p| p.get_fd(epfd)) {
        Some(akuma_exec::process::FileDescriptor::EpollFd(id)) => id,
        _ => return EBADF,
    };

    let mut table = EPOLL_TABLE.lock();
    let instance = match table.get_mut(&epoll_id) {
        Some(inst) => inst,
        None => return EBADF,
    };

    const EPOLL_EVENT_SIZE: usize = core::mem::size_of::<EpollEvent>();  // 16 on ARM64

    match op {
        EPOLL_CTL_ADD => {
            if !validate_user_ptr(event_ptr as u64, EPOLL_EVENT_SIZE) { return EFAULT; }
            let mut ev = EpollEvent { events: 0, _pad: 0, data: 0 };
            if unsafe { copy_from_user_safe(&mut ev as *mut EpollEvent as *mut u8, event_ptr as *const u8, EPOLL_EVENT_SIZE).is_err() } {
                return EFAULT;
            }
            let ev_events = { ev.events };
            let ev_data = { ev.data };
            if let Some(entry) = instance.interest_list.get_mut(&fd) {
                entry.events = ev_events;
                entry.data = ev_data;
                entry.last_ready = 0;
                crate::tprint!(96, "[epoll] ctl ADD->MOD epfd={} fd={} events=0x{:x}\n", epfd, fd, ev_events);
            } else {
                instance.interest_list.insert(fd, EpollEntry {
                    events: ev_events,
                    data: ev_data,
                    last_ready: 0,
                });
                crate::tprint!(96, "[epoll] ctl ADD epfd={} fd={} events=0x{:x}\n", epfd, fd, ev_events);
            }
            0
        }
        EPOLL_CTL_MOD => {
            if !validate_user_ptr(event_ptr as u64, EPOLL_EVENT_SIZE) { return EFAULT; }
            let mut ev = EpollEvent { events: 0, _pad: 0, data: 0 };
            if unsafe { copy_from_user_safe(&mut ev as *mut EpollEvent as *mut u8, event_ptr as *const u8, EPOLL_EVENT_SIZE).is_err() } {
                return EFAULT;
            }
            let ev_events = { ev.events };
            let ev_data = { ev.data };
            match instance.interest_list.get_mut(&fd) {
                Some(entry) => {
                    entry.events = ev_events;
                    entry.data = ev_data;
                    entry.last_ready = 0;
                    0
                }
                None => ENOENT,
            }
        }
        EPOLL_CTL_DEL => {
            match instance.interest_list.remove(&fd) {
                Some(_) => 0,
                None => ENOENT,
            }
        }
        _ => EINVAL,
    }
}

pub(crate) fn epoll_check_fd_readiness(fd_num: u32, requested: u32, waker: Option<&Waker>) -> u32 {
    let fd_entry = akuma_exec::process::current_process().and_then(|p| p.get_fd(fd_num));
    let fd_entry = match fd_entry {
        Some(e) => e,
        None => return EPOLLHUP | EPOLLERR,
    };

    let mut ready = 0u32;
    let tid = akuma_exec::threading::current_thread_id();

    match fd_entry {
        akuma_exec::process::FileDescriptor::Socket(idx) => {
            if let Some(w) = waker {
                socket::socket_add_waker(idx, w.clone());
            }

            if socket::is_udp_socket(idx) {
                if let Some(handle) = super::net::socket_get_udp_handle(idx) {
                    let can_recv = akuma_net::smoltcp_net::udp_can_recv(handle);
                    if crate::config::SYSCALL_DEBUG_NET_ENABLED {
                        crate::tprint!(96, "[epoll] check UDP fd={} can_recv={}\n", fd_num, can_recv);
                    }
                    if requested & EPOLLIN != 0 && can_recv {
                        ready |= EPOLLIN;
                    }
                    if requested & EPOLLOUT != 0 && akuma_net::smoltcp_net::udp_can_send(handle) {
                        ready |= EPOLLOUT;
                    }
                }
            } else {
                // EPOLLHUP: unconditionally set when the socket is fully dead (not
                // connected and not connecting).  This lets the caller detect a
                // timed-out or reset connection without spinning on EPOLLIN.
                if super::net::socket_is_dead_tcp(idx) {
                    ready |= EPOLLHUP;
                } else {
                    if requested & EPOLLIN != 0 && super::net::socket_can_recv_tcp(idx) {
                        ready |= EPOLLIN;
                    }
                    if requested & EPOLLOUT != 0 && super::net::socket_can_send_tcp(idx) {
                        ready |= EPOLLOUT;
                    }
                    if requested & EPOLLRDHUP != 0 && super::net::socket_peer_closed_tcp(idx) {
                        ready |= EPOLLRDHUP;
                    }
                }
            }
        }
        akuma_exec::process::FileDescriptor::EventFd(efd_id) => {
            if waker.is_some() {
                super::eventfd::eventfd_add_poller(efd_id, tid);
            }
            let can_read = super::eventfd::eventfd_can_read(efd_id);
            if requested & EPOLLIN != 0 && can_read {
                ready |= EPOLLIN;
            }
            if requested & EPOLLOUT != 0 {
                ready |= EPOLLOUT;
            }
        }
        akuma_exec::process::FileDescriptor::ChildStdout(child_pid) => {
            if requested & EPOLLIN != 0 {
                if let Some(ch) = akuma_exec::process::get_child_channel(child_pid) {
                    if waker.is_some() {
                        ch.add_poller(tid);
                    }
                    if ch.has_stdout_data() || ch.has_exited() {
                        ready |= EPOLLIN;
                    }
                } else {
                    ready |= EPOLLHUP;
                }
            }
        }
        akuma_exec::process::FileDescriptor::PipeRead(pipe_id) => {
            if requested & EPOLLIN != 0 {
                // Register for wakeup notifications
                if waker.is_some() {
                    super::pipe::pipe_add_poller(pipe_id, tid);
                }
                if super::pipe::pipe_can_read(pipe_id) {
                    ready |= EPOLLIN;
                }
            }
        }
        akuma_exec::process::FileDescriptor::PipeWrite(pipe_id) => {
            if requested & EPOLLOUT != 0 {
                super::pipe::pipe_add_poller(pipe_id, tid);
                if super::pipe::pipe_can_write(pipe_id) {
                    ready |= EPOLLOUT;
                }
            }
        }
        akuma_exec::process::FileDescriptor::TimerFd(timer_id) => {
            if requested & EPOLLIN != 0 {
                if waker.is_some() {
                    super::timerfd::timerfd_add_poller(timer_id, tid);
                }
                if super::timerfd::timerfd_can_read(timer_id) {
                    ready |= EPOLLIN;
                }
            }
        }
        akuma_exec::process::FileDescriptor::PidFd(pidfd_id) => {
            // A pidfd becomes readable (EPOLLIN) when the tracked process has exited.
            if requested & EPOLLIN != 0 {
                if let Some(target_pid) = super::pidfd::pidfd_get_pid(pidfd_id) {
                    if let Some(ch) = akuma_exec::process::get_child_channel(target_pid) {
                        if waker.is_some() {
                            ch.add_poller(tid);
                        }
                    }
                }
                if super::pidfd::pidfd_can_read(pidfd_id) {
                    ready |= EPOLLIN;
                }
            }
        }
        akuma_exec::process::FileDescriptor::Stdin => {
            if requested & EPOLLIN != 0 {
                if let Some(ch) = akuma_exec::process::current_channel() {
                    if waker.is_some() {
                        ch.add_poller(tid);
                    }
                    if ch.has_stdin_data() {
                        ready |= EPOLLIN;
                    }
                }
            }
        }
        akuma_exec::process::FileDescriptor::Stdout | akuma_exec::process::FileDescriptor::Stderr => {
            if requested & EPOLLOUT != 0 {
                ready |= EPOLLOUT;
            }
        }
        _ => {
            if requested & EPOLLIN != 0 { ready |= EPOLLIN; }
            if requested & EPOLLOUT != 0 { ready |= EPOLLOUT; }
        }
    }

    ready
}

pub(crate) fn sys_epoll_pwait(epfd: u32, events_ptr: usize, maxevents: i32, timeout: i32) -> u64 {
    const EPOLL_EVENT_SIZE: usize = core::mem::size_of::<EpollEvent>();  // 16 on ARM64
    
    if maxevents <= 0 { return EINVAL; }
    let maxevents = maxevents as usize;
    let out_size = maxevents * EPOLL_EVENT_SIZE;
    if !validate_user_ptr(events_ptr as u64, out_size) { return EFAULT; }

    let epoll_id = match akuma_exec::process::current_process().and_then(|p| p.get_fd(epfd)) {
        Some(akuma_exec::process::FileDescriptor::EpollFd(id)) => id,
        _ => return EBADF,
    };

    let timeout_us = if timeout > 0 {
        (timeout as u64) * 1000
    } else {
        0
    };
    let start_time = crate::timer::uptime_us();
    let waker = akuma_exec::threading::current_thread_waker();

    let mut iterations = 0u64;
    loop {
        iterations += 1;
        
        // Drive network stack (only once per loop)
        akuma_net::smoltcp_net::poll();

        let mut kernel_events = alloc::vec![];
        let mut ready_count = 0usize;

        // Snapshot interest list to avoid holding EPOLL_TABLE lock during readiness checks.
        // This prevents deadlock with PROCESS_TABLE lock (which readiness checks need).
        // We use a stack-allocated array for common small interest lists (up to 128).
        const STACK_SNAPSHOT_SIZE: usize = 128;
        let mut stack_snapshot = [0u32; STACK_SNAPSHOT_SIZE];
        let mut heap_snapshot = None;
        let snapshot_count;

        {
            let table = EPOLL_TABLE.lock();
            let instance = match table.get(&epoll_id) {
                Some(inst) => inst,
                None => return EBADF,
            };

            snapshot_count = instance.interest_list.len();
            if snapshot_count <= STACK_SNAPSHOT_SIZE {
                for (i, (&fd, _)) in instance.interest_list.iter().enumerate() {
                    stack_snapshot[i] = fd;
                }
            } else {
                heap_snapshot = Some(instance.interest_list.keys().copied().collect::<alloc::vec::Vec<u32>>());
            }
        }

        let fds: &[u32] = if let Some(ref h) = heap_snapshot { 
            h 
        } else { 
            &stack_snapshot[..snapshot_count] 
        };

        for &fd in fds {
            if ready_count >= maxevents { break; }

            // Re-acquire lock to get entry details (MUST NOT hold during readiness check)
            let entry_info = {
                let table = EPOLL_TABLE.lock();
                table.get(&epoll_id).and_then(|inst| inst.interest_list.get(&fd)).map(|e| (e.events, e.data, e.last_ready))
            };

            let (raw_events, data, last_ready) = match entry_info {
                Some(info) => info,
                None => continue, // FD removed from epoll interest during loop
            };

            let is_et = raw_events & EPOLLET != 0;
            let requested = raw_events & EPOLL_EVENT_MASK;
            
            // Pass waker to register interest for event-driven wakeups.
            // epoll_check_fd_readiness locks PROCESS_TABLE.
            let revents = epoll_check_fd_readiness(fd, requested, Some(&waker));

            if is_et {
                let new_bits = revents & !last_ready;
                // Update last_ready in the table
                {
                    let mut table = EPOLL_TABLE.lock();
                    if let Some(inst) = table.get_mut(&epoll_id) {
                        if let Some(entry) = inst.interest_list.get_mut(&fd) {
                            entry.last_ready = revents;
                        }
                    }
                }
                if new_bits != 0 {
                    kernel_events.push(EpollEvent { events: new_bits, _pad: 0, data });
                    ready_count += 1;
                }
            } else {
                if revents != 0 {
                    kernel_events.push(EpollEvent { events: revents, _pad: 0, data });
                    ready_count += 1;
                }
            }
        }

        if ready_count > 0 {
            if unsafe { copy_to_user_safe(events_ptr as *mut u8, kernel_events.as_ptr() as *const u8, ready_count * EPOLL_EVENT_SIZE).is_err() } {
                return EFAULT;
            }
            log_epoll_pwait_return(
                epfd,
                timeout,
                ready_count,
                iterations,
                start_time,
                0,
                &kernel_events,
                "",
            );
            return ready_count as u64;
        }

        if timeout == 0 {
            log_epoll_pwait_return(
                epfd,
                timeout,
                0,
                iterations,
                start_time,
                0,
                &[],
                "",
            );
            return 0;
        }

        if timeout > 0 {
            let elapsed = crate::timer::uptime_us() - start_time;
            if elapsed >= timeout_us {
                log_epoll_pwait_return(
                    epfd,
                    timeout,
                    0,
                    iterations,
                    start_time,
                    0,
                    &[],
                    "timeout_expired",
                );
                return 0;
            }
        }

        // Periodic log for long waits (every ~5 seconds = 500 iterations × 10ms)
        if crate::config::SYSCALL_DEBUG_NET_ENABLED && iterations % 500 == 0 {
            let elapsed = crate::timer::uptime_us() - start_time;
            let pid = akuma_exec::process::read_current_pid().unwrap_or(0);
            crate::tprint!(192, "[epoll] pwait still waiting: pid={} epfd={} {}us elapsed\n", 
                pid, epfd, elapsed);
        }

        if akuma_exec::process::is_current_interrupted() {
            log_epoll_pwait_return(
                epfd,
                timeout,
                0,
                iterations,
                start_time,
                0,
                &[],
                "EINTR",
            );
            return EINTR;
        }

        // With the waker mechanism, we can now block more efficiently.
        // We still use a 10ms cap for safety and for resources that don't
        // yet support wakers (like TimerFd), but network events will
        // now wake us up IMMEDIATELY.
        let abs_deadline = epoll_wait_deadline(timeout, start_time, timeout_us, crate::timer::uptime_us());
        if abs_deadline == 0 {
            log_epoll_pwait_return(
                epfd,
                timeout,
                0,
                iterations,
                start_time,
                0,
                &[],
                "deadline_abs0",
            );
            return 0;
        }
        let deadline = abs_deadline.min(crate::timer::uptime_us() + BLOCKING_POLL_INTERVAL_US);

        akuma_exec::threading::schedule_blocking(deadline);
    }
}

pub(super) fn sys_pselect6(nfds: usize, readfds_ptr: u64, writefds_ptr: u64, _exceptfds_ptr: u64, timeout_ptr: u64, _sigmask_ptr: u64) -> u64 {
    if nfds == 0 { return 0; }
    const MAX_FDS: usize = 1024;
    if nfds > MAX_FDS { return EINVAL; }
    let nwords = (nfds + 63) / 64;
    let fd_set_bytes = nwords * 8;

    let mut orig_read = [0u64; MAX_FDS / 64];
    let mut orig_write = [0u64; MAX_FDS / 64];

    if readfds_ptr != 0 {
        if !validate_user_ptr(readfds_ptr, fd_set_bytes) { return EFAULT; }
        if unsafe { copy_from_user_safe(orig_read.as_mut_ptr() as *mut u8, readfds_ptr as *const u8, fd_set_bytes).is_err() } {
            return EFAULT;
        }
    }
    if writefds_ptr != 0 {
        if !validate_user_ptr(writefds_ptr, fd_set_bytes) { return EFAULT; }
        if unsafe { copy_from_user_safe(orig_write.as_mut_ptr() as *mut u8, writefds_ptr as *const u8, fd_set_bytes).is_err() } {
            return EFAULT;
        }
    }

    let infinite = timeout_ptr == 0;
    let timeout_us = if !infinite {
        if !validate_user_ptr(timeout_ptr, 16) { return EFAULT; }
        let mut ts = Timespec { tv_sec: 0, tv_nsec: 0 };
        if unsafe { copy_from_user_safe(&mut ts as *mut Timespec as *mut u8, timeout_ptr as *const u8, 16).is_err() } {
            return EFAULT;
        }
        (ts.tv_sec as u64) * 1000_000 + (ts.tv_nsec as u64) / 1000
    } else {
        0
    };

    let start_time = crate::timer::uptime_us();

    loop {
        akuma_net::smoltcp_net::poll();
        let mut ready_count: u64 = 0;
        let mut out_read = [0u64; MAX_FDS / 64];
        let mut out_write = [0u64; MAX_FDS / 64];

        for fd in 0..nfds {
            let word = fd / 64;
            let bit = fd % 64;
            let mask = 1u64 << bit;

            let in_read = orig_read[word] & mask != 0;
            let in_write = orig_write[word] & mask != 0;
            if !in_read && !in_write { continue; }

            let _socket_idx = if fd > 2 {
                if let Some(proc) = akuma_exec::process::current_process() {
                    if let Some(akuma_exec::process::FileDescriptor::Socket(idx)) = proc.get_fd(fd as u32) {
                        Some(idx)
                    } else { None }
                } else { None }
            } else { None };

            let mut requested = 0u32;
            if in_read { requested |= EPOLLIN; }
            if in_write { requested |= EPOLLOUT; }

            let revents = epoll_check_fd_readiness(fd as u32, requested, None);
            if in_read && (revents & EPOLLIN != 0) { out_read[word] |= mask; ready_count += 1; }
            if in_write && (revents & EPOLLOUT != 0) { out_write[word] |= mask; ready_count += 1; }
        }

        if ready_count > 0 {
            if readfds_ptr != 0 { 
                if unsafe { copy_to_user_safe(readfds_ptr as *mut u8, out_read.as_ptr() as *const u8, fd_set_bytes).is_err() } {
                    return EFAULT;
                }
            }
            if writefds_ptr != 0 { 
                if unsafe { copy_to_user_safe(writefds_ptr as *mut u8, out_write.as_ptr() as *const u8, fd_set_bytes).is_err() } {
                    return EFAULT;
                }
            }
            return ready_count;
        }

        if !infinite && (crate::timer::uptime_us() - start_time) >= timeout_us {
            if readfds_ptr != 0 { 
                if unsafe { copy_to_user_safe(readfds_ptr as *mut u8, [0u8; MAX_FDS/8].as_ptr(), fd_set_bytes).is_err() } {
                    return EFAULT;
                }
            }
            if writefds_ptr != 0 { 
                if unsafe { copy_to_user_safe(writefds_ptr as *mut u8, [0u8; MAX_FDS/8].as_ptr(), fd_set_bytes).is_err() } {
                    return EFAULT;
                }
            }
            return 0;
        }

        let abs_deadline = if infinite { u64::MAX } else { start_time + timeout_us };
        let deadline = abs_deadline.min(crate::timer::uptime_us() + BLOCKING_POLL_INTERVAL_US);
        akuma_exec::threading::schedule_blocking(deadline);
    }
}

pub(super) fn sys_ppoll(fds_ptr: u64, nfds: usize, timeout_ptr: u64, _sigmask: u64) -> u64 {
    if nfds == 0 { return 0; }
    let fds_size = nfds * core::mem::size_of::<PollFd>();
    if !validate_user_ptr(fds_ptr, fds_size) { return EFAULT; }

    let infinite = timeout_ptr == 0;
    let timeout_us = if !infinite {
        if !validate_user_ptr(timeout_ptr, 16) { return EFAULT; }
        let mut ts = Timespec { tv_sec: 0, tv_nsec: 0 };
        if unsafe { copy_from_user_safe(&mut ts as *mut Timespec as *mut u8, timeout_ptr as *const u8, 16).is_err() } {
            return EFAULT;
        }
        (ts.tv_sec as u64) * 1000_000 + (ts.tv_nsec as u64) / 1000
    } else {
        0
    };

    if crate::config::SYSCALL_DEBUG_NET_ENABLED && nfds > 0 {
        let pid = akuma_exec::process::read_current_pid().unwrap_or(0);
        crate::tprint!(128, "[ppoll] enter: pid={} nfds={} timeout_us={}\n", pid, nfds, 
            if infinite { u64::MAX } else { timeout_us });
    }

    let start_time = crate::timer::uptime_us();
    let mut kernel_fds = alloc::vec![PollFd { fd: 0, events: 0, revents: 0 }; nfds];
    if unsafe { copy_from_user_safe(kernel_fds.as_mut_ptr() as *mut u8, fds_ptr as *const u8, fds_size).is_err() } {
        return EFAULT;
    }

    loop {
        akuma_net::smoltcp_net::poll();
        let mut ready_count = 0;
        
        for fd in kernel_fds.iter_mut() {
            fd.revents = 0;
            if fd.fd < 0 { continue; }

            let mut requested = 0u32;
            if fd.events & 1 != 0 { requested |= EPOLLIN; }
            if fd.events & 4 != 0 { requested |= EPOLLOUT; }

            let revents = epoll_check_fd_readiness(fd.fd as u32, requested, None);
            
            if (revents & EPOLLIN != 0) && (fd.events & 1 != 0) { fd.revents |= 1; }
            if (revents & EPOLLOUT != 0) && (fd.events & 4 != 0) { fd.revents |= 4; }
            if revents & EPOLLHUP != 0 { fd.revents |= 16; } // POLLHUP = 0x10
            if revents & EPOLLERR != 0 { fd.revents |= 8; }  // POLLERR = 0x08

            if fd.revents != 0 {
                ready_count += 1;
            }
        }

        if ready_count > 0 {
            if unsafe { copy_to_user_safe(fds_ptr as *mut u8, kernel_fds.as_ptr() as *const u8, fds_size).is_err() } {
                return EFAULT;
            }
            return ready_count as u64;
        }

        if !infinite && (crate::timer::uptime_us() - start_time) >= timeout_us {
            return 0;
        }

        let abs_deadline = if infinite { u64::MAX } else { start_time + timeout_us };
        let deadline = abs_deadline.min(crate::timer::uptime_us() + BLOCKING_POLL_INTERVAL_US);
        akuma_exec::threading::schedule_blocking(deadline);
    }
}
