use super::*;
use akuma_net::socket;

struct EpollEntry {
    events: u32,
    data: u64,
}

struct EpollInstance {
    interest_list: BTreeMap<u32, EpollEntry>,
}

static EPOLL_TABLE: Spinlock<BTreeMap<u32, EpollInstance>> = Spinlock::new(BTreeMap::new());
static NEXT_EPOLL_ID: AtomicU32 = AtomicU32::new(1);

const EPOLLIN: u32 = 0x001;
const EPOLLOUT: u32 = 0x004;
const EPOLLERR: u32 = 0x008;
const EPOLLHUP: u32 = 0x010;

const EPOLL_CTL_ADD: i32 = 1;
const EPOLL_CTL_DEL: i32 = 2;
const EPOLL_CTL_MOD: i32 = 3;

#[repr(C)]
struct PollFd {
    fd: i32,
    events: i16,
    revents: i16,
}

#[repr(C, packed)]
#[derive(Clone, Copy)]
struct EpollEvent {
    events: u32,
    data: u64,
}

pub(super) fn sys_epoll_create1(_flags: u32) -> u64 {
    if let Some(proc) = akuma_exec::process::current_process() {
        let epoll_id = NEXT_EPOLL_ID.fetch_add(1, Ordering::SeqCst);
        EPOLL_TABLE.lock().insert(epoll_id, EpollInstance {
            interest_list: BTreeMap::new(),
        });
        let fd = proc.alloc_fd(akuma_exec::process::FileDescriptor::EpollFd(epoll_id));
        crate::tprint!(96, "[epoll] create1() id={} fd={}\n", epoll_id, fd);
        fd as u64
    } else {
        EBADF
    }
}

pub(super) fn sys_epoll_ctl(epfd: u32, op: i32, fd: u32, event_ptr: usize) -> u64 {
    let epoll_id = match akuma_exec::process::current_process().and_then(|p| p.get_fd(epfd)) {
        Some(akuma_exec::process::FileDescriptor::EpollFd(id)) => id,
        _ => return EBADF,
    };

    let mut table = EPOLL_TABLE.lock();
    let instance = match table.get_mut(&epoll_id) {
        Some(inst) => inst,
        None => return EBADF,
    };

    match op {
        EPOLL_CTL_ADD => {
            if !validate_user_ptr(event_ptr as u64, 12) { return EFAULT; }
            let ev = unsafe { core::ptr::read_unaligned(event_ptr as *const EpollEvent) };
            if instance.interest_list.contains_key(&fd) {
                return EEXIST;
            }
            let ev_events = { ev.events };
            let ev_data = { ev.data };
            instance.interest_list.insert(fd, EpollEntry {
                events: ev_events,
                data: ev_data,
            });
            crate::tprint!(96, "[epoll] ctl ADD epfd={} fd={} events=0x{:x}\n", epfd, fd, ev_events);
            0
        }
        EPOLL_CTL_MOD => {
            if !validate_user_ptr(event_ptr as u64, 12) { return EFAULT; }
            let ev = unsafe { core::ptr::read_unaligned(event_ptr as *const EpollEvent) };
            let ev_events = { ev.events };
            let ev_data = { ev.data };
            match instance.interest_list.get_mut(&fd) {
                Some(entry) => {
                    entry.events = ev_events;
                    entry.data = ev_data;
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

fn epoll_check_fd_readiness(fd_num: u32, requested: u32) -> u32 {
    let fd_entry = akuma_exec::process::current_process().and_then(|p| p.get_fd(fd_num));
    let fd_entry = match fd_entry {
        Some(e) => e,
        None => return EPOLLHUP | EPOLLERR,
    };

    let mut ready = 0u32;

    match fd_entry {
        akuma_exec::process::FileDescriptor::Socket(idx) => {
            if socket::is_udp_socket(idx) {
                if let Some(handle) = super::net::socket_get_udp_handle(idx) {
                    if requested & EPOLLIN != 0 && akuma_net::smoltcp_net::udp_can_recv(handle) {
                        ready |= EPOLLIN;
                    }
                    if requested & EPOLLOUT != 0 && akuma_net::smoltcp_net::udp_can_send(handle) {
                        ready |= EPOLLOUT;
                    }
                }
            } else {
                if requested & EPOLLIN != 0 && super::net::socket_can_recv_tcp(idx) {
                    ready |= EPOLLIN;
                }
                if requested & EPOLLOUT != 0 && super::net::socket_can_send_tcp(idx) {
                    ready |= EPOLLOUT;
                }
            }
        }
        akuma_exec::process::FileDescriptor::EventFd(efd_id) => {
            if requested & EPOLLIN != 0 && super::eventfd::eventfd_can_read(efd_id) {
                ready |= EPOLLIN;
            }
            if requested & EPOLLOUT != 0 {
                ready |= EPOLLOUT;
            }
        }
        akuma_exec::process::FileDescriptor::PipeRead(pipe_id) => {
            if requested & EPOLLIN != 0 && super::pipe::pipe_can_read(pipe_id) {
                ready |= EPOLLIN;
            }
        }
        akuma_exec::process::FileDescriptor::PipeWrite(pipe_id) => {
            if requested & EPOLLOUT != 0 && super::pipe::pipe_can_write(pipe_id) {
                ready |= EPOLLOUT;
            }
        }
        akuma_exec::process::FileDescriptor::TimerFd(timer_id) => {
            if requested & EPOLLIN != 0 && super::timerfd::timerfd_can_read(timer_id) {
                ready |= EPOLLIN;
            }
        }
        akuma_exec::process::FileDescriptor::Stdin => {
            if requested & EPOLLIN != 0 {
                if let Some(ch) = akuma_exec::process::current_channel() {
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

pub(super) fn sys_epoll_pwait(epfd: u32, events_ptr: usize, maxevents: i32, timeout: i32) -> u64 {
    if maxevents <= 0 { return EINVAL; }
    let maxevents = maxevents as usize;
    let out_size = maxevents * 12;
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

    loop {
        akuma_net::smoltcp_net::poll();

        let interest_snapshot: Vec<(u32, u32, u64)> = {
            let table = EPOLL_TABLE.lock();
            match table.get(&epoll_id) {
                Some(inst) => inst.interest_list.iter()
                    .map(|(&fd, entry)| (fd, entry.events, entry.data))
                    .collect(),
                None => return EBADF,
            }
        };

        let mut ready_count = 0usize;
        for &(fd, requested_events, data) in &interest_snapshot {
            if ready_count >= maxevents { break; }

            let revents = epoll_check_fd_readiness(fd, requested_events);
            if revents != 0 {
                let out_event = EpollEvent { events: revents, data };
                unsafe {
                    core::ptr::write_unaligned(
                        (events_ptr + ready_count * 12) as *mut EpollEvent,
                        out_event,
                    );
                }
                ready_count += 1;
            }
        }

        if ready_count > 0 {
            return ready_count as u64;
        }

        if timeout == 0 {
            return 0;
        }

        if timeout > 0 {
            let elapsed = crate::timer::uptime_us() - start_time;
            if elapsed >= timeout_us {
                return 0;
            }
        }

        if akuma_exec::process::is_current_interrupted() {
            return EINTR;
        }

        akuma_exec::threading::yield_now();
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
        unsafe { core::ptr::copy_nonoverlapping(readfds_ptr as *const u64, orig_read.as_mut_ptr(), nwords); }
    }
    if writefds_ptr != 0 {
        if !validate_user_ptr(writefds_ptr, fd_set_bytes) { return EFAULT; }
        unsafe { core::ptr::copy_nonoverlapping(writefds_ptr as *const u64, orig_write.as_mut_ptr(), nwords); }
    }

    let infinite = timeout_ptr == 0;
    let timeout_us = if !infinite {
        if !validate_user_ptr(timeout_ptr, 16) { return EFAULT; }
        let ts = unsafe { &*(timeout_ptr as *const Timespec) };
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

            let socket_idx = if fd > 2 {
                if let Some(proc) = akuma_exec::process::current_process() {
                    if let Some(akuma_exec::process::FileDescriptor::Socket(idx)) = proc.get_fd(fd as u32) {
                        Some(idx)
                    } else { None }
                } else { None }
            } else { None };

            if in_read {
                let readable = if fd == 0 {
                    akuma_exec::process::current_channel().map_or(false, |ch| ch.has_stdin_data())
                } else if let Some(idx) = socket_idx {
                    if socket::is_udp_socket(idx) {
                        super::net::socket_get_udp_handle(idx).map_or(false, |h| akuma_net::smoltcp_net::udp_can_recv(h))
                    } else {
                        super::net::socket_can_recv_tcp(idx)
                    }
                } else {
                    fd > 2
                };
                if readable { out_read[word] |= mask; ready_count += 1; }
            }

            if in_write {
                let writable = if let Some(idx) = socket_idx {
                    if socket::is_udp_socket(idx) {
                        super::net::socket_get_udp_handle(idx).map_or(false, |h| akuma_net::smoltcp_net::udp_can_send(h))
                    } else {
                        super::net::socket_can_send_tcp(idx)
                    }
                } else {
                    true
                };
                if writable { out_write[word] |= mask; ready_count += 1; }
            }
        }

        if ready_count > 0 {
            if readfds_ptr != 0 { unsafe { core::ptr::copy_nonoverlapping(out_read.as_ptr(), readfds_ptr as *mut u64, nwords); } }
            if writefds_ptr != 0 { unsafe { core::ptr::copy_nonoverlapping(out_write.as_ptr(), writefds_ptr as *mut u64, nwords); } }
            return ready_count;
        }

        if !infinite && (crate::timer::uptime_us() - start_time) >= timeout_us {
            if readfds_ptr != 0 { unsafe { core::ptr::write_bytes(readfds_ptr as *mut u8, 0, fd_set_bytes); } }
            if writefds_ptr != 0 { unsafe { core::ptr::write_bytes(writefds_ptr as *mut u8, 0, fd_set_bytes); } }
            return 0;
        }

        akuma_exec::threading::yield_now();
    }
}

pub(super) fn sys_ppoll(fds_ptr: u64, nfds: usize, timeout_ptr: u64, _sigmask: u64) -> u64 {
    if nfds == 0 { return 0; }
    if !validate_user_ptr(fds_ptr, nfds * 8) { return EFAULT; }

    let infinite = timeout_ptr == 0;
    let timeout_us = if !infinite {
        if !validate_user_ptr(timeout_ptr, 16) { return EFAULT; }
        let ts = unsafe { &*(timeout_ptr as *const Timespec) };
        (ts.tv_sec as u64) * 1000_000 + (ts.tv_nsec as u64) / 1000
    } else {
        0
    };

    let start_time = crate::timer::uptime_us();

    loop {
        akuma_net::smoltcp_net::poll();
        let mut ready_count = 0;
        unsafe {
            let fds = core::slice::from_raw_parts_mut(fds_ptr as *mut PollFd, nfds);
            for fd in fds.iter_mut() {
                fd.revents = 0;

                if fd.fd < 0 { continue; }

                let fd_entry = if fd.fd > 2 {
                    akuma_exec::process::current_process().and_then(|p| p.get_fd(fd.fd as u32))
                } else {
                    None
                };

                let socket_idx = match &fd_entry {
                    Some(akuma_exec::process::FileDescriptor::Socket(idx)) => Some(*idx),
                    _ => None,
                };
                let eventfd_id = match &fd_entry {
                    Some(akuma_exec::process::FileDescriptor::EventFd(id)) => Some(*id),
                    _ => None,
                };

                if fd.events & 1 != 0 {
                    if fd.fd == 0 {
                        if let Some(ch) = akuma_exec::process::current_channel() {
                            if ch.has_stdin_data() {
                                fd.revents |= 1;
                            }
                        }
                    } else if let Some(efd_id) = eventfd_id {
                        if super::eventfd::eventfd_can_read(efd_id) {
                            fd.revents |= 1;
                        }
                    } else if let Some(idx) = socket_idx {
                        if socket::is_udp_socket(idx) {
                            if let Some(handle) = super::net::socket_get_udp_handle(idx) {
                                if akuma_net::smoltcp_net::udp_can_recv(handle) {
                                    fd.revents |= 1;
                                }
                            }
                        } else {
                            if super::net::socket_can_recv_tcp(idx) {
                                fd.revents |= 1;
                            }
                        }
                    } else if fd.fd > 2 {
                        fd.revents |= 1;
                    }
                }

                if fd.events & 4 != 0 {
                    if eventfd_id.is_some() {
                        fd.revents |= 4;
                    } else if let Some(idx) = socket_idx {
                        if socket::is_udp_socket(idx) {
                            if let Some(handle) = super::net::socket_get_udp_handle(idx) {
                                if akuma_net::smoltcp_net::udp_can_send(handle) {
                                    fd.revents |= 4;
                                }
                            }
                        } else if super::net::socket_can_send_tcp(idx) {
                            fd.revents |= 4;
                        }
                    } else if fd.fd == 1 || fd.fd == 2 || fd.fd > 2 {
                        fd.revents |= 4;
                    }
                }

                if fd.revents != 0 {
                    ready_count += 1;
                }
            }
        }

        if ready_count > 0 {
            return ready_count as u64;
        }

        if !infinite && (crate::timer::uptime_us() - start_time) >= timeout_us {
            return 0;
        }

        akuma_exec::threading::yield_now();
    }
}
