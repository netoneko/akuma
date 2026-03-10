use super::*;
use akuma_net::socket::{self, SockAddrIn, libc_errno};

pub(super) fn sys_socket(domain: i32, sock_type: i32, _proto: i32) -> u64 {
    let base_type = sock_type & 0xFF;
    let cloexec = sock_type & 0x80000 != 0;
    let nonblock = sock_type & 0x800 != 0;
    if domain != 2 || (base_type != 1 && base_type != 2) {
        crate::safe_print!(96, "[syscall] socket(domain={}, type=0x{:x}): unsupported\n", domain, sock_type);
        return EAFNOSUPPORT;
    }
    if let Some(idx) = socket::alloc_socket(base_type) {
        if let Some(proc) = akuma_exec::process::current_process() {
            let fd = proc.alloc_fd(akuma_exec::process::FileDescriptor::Socket(idx));
            if cloexec {
                proc.set_cloexec(fd);
            }
            if nonblock {
                proc.set_nonblock(fd);
            }
            crate::safe_print!(96, "[syscall] socket(type={}) = fd {}\n", if base_type == 2 { "UDP" } else { "TCP" }, fd);
            return fd as u64;
        }
    }
    !0u64
}

pub(super) fn sys_bind(fd: u32, addr_ptr: u64, len: usize) -> u64 {
    if len < 16 { return !0u64; }
    if !validate_user_ptr(addr_ptr, len) { return EFAULT; }
    let addr = unsafe { core::ptr::read(addr_ptr as *const SockAddrIn) }.to_addr();
    crate::safe_print!(96, "[syscall] bind(fd={}, port={}, ip={}.{}.{}.{})\n", fd, addr.port, addr.ip[0], addr.ip[1], addr.ip[2], addr.ip[3]);
    if let Some(idx) = get_socket_from_fd(fd) {
        match socket::socket_bind(idx, addr) {
            Ok(()) => return 0,
            Err(e) => {
                crate::safe_print!(64, "[syscall] bind failed: {}\n", e);
                return !0u64;
            }
        }
    }
    !0u64
}

pub(super) fn sys_listen(fd: u32, backlog: i32) -> u64 {
    if let Some(idx) = get_socket_from_fd(fd) { if socket::socket_listen(idx, backlog as usize).is_ok() { return 0; } }
    !0u64
}

pub(super) fn sys_accept(fd: u32, addr_ptr: u64, len_ptr: u64) -> u64 {
    if addr_ptr != 0 && !validate_user_ptr(addr_ptr, 16) { return EFAULT; }
    if len_ptr != 0 && !validate_user_ptr(len_ptr, 4) { return EFAULT; }
    if let Some(idx) = get_socket_from_fd(fd) {
        let nonblock = fd_is_nonblock(fd);
        match socket::socket_accept(idx, nonblock) {
            Ok((new_idx, addr)) => {
                if let Some(proc) = akuma_exec::process::current_process() {
                    if addr_ptr != 0 { unsafe { core::ptr::write(addr_ptr as *mut SockAddrIn, SockAddrIn::from_addr(&addr)); } }
                    return proc.alloc_fd(akuma_exec::process::FileDescriptor::Socket(new_idx)) as u64;
                }
            }
            Err(e) => return (-e as i64) as u64,
        }
    }
    !0u64
}

pub(super) fn sys_accept4(fd: u32, addr_ptr: u64, len_ptr: u64, flags: u32) -> u64 {
    if addr_ptr != 0 && !validate_user_ptr(addr_ptr, 16) { return EFAULT; }
    if len_ptr != 0 && !validate_user_ptr(len_ptr, 4) { return EFAULT; }
    if let Some(idx) = get_socket_from_fd(fd) {
        let nonblock = fd_is_nonblock(fd);
        match socket::socket_accept(idx, nonblock) {
            Ok((new_idx, addr)) => {
                if let Some(proc) = akuma_exec::process::current_process() {
                    if addr_ptr != 0 {
                        unsafe { core::ptr::write(addr_ptr as *mut SockAddrIn, SockAddrIn::from_addr(&addr)); }
                    }
                    let new_fd = proc.alloc_fd(akuma_exec::process::FileDescriptor::Socket(new_idx));
                    const SOCK_CLOEXEC: u32 = 0x80000;
                    const SOCK_NONBLOCK: u32 = 0x800;
                    if flags & SOCK_CLOEXEC != 0 { proc.set_cloexec(new_fd); }
                    if flags & SOCK_NONBLOCK != 0 { proc.set_nonblock(new_fd); }
                    return new_fd as u64;
                }
            }
            Err(e) => return (-e as i64) as u64,
        }
    }
    !0u64
}

pub(super) fn sys_connect(fd: u32, addr_ptr: u64, len: usize) -> u64 {
    if len < 16 { return !0u64; }
    if !validate_user_ptr(addr_ptr, len) { return EFAULT; }
    let addr = unsafe { core::ptr::read(addr_ptr as *const SockAddrIn) }.to_addr();
    crate::safe_print!(96, "[syscall] connect(fd={}, ip={}.{}.{}.{}:{})\n", fd, addr.ip[0], addr.ip[1], addr.ip[2], addr.ip[3], addr.port);
    if let Some(idx) = get_socket_from_fd(fd) {
        let nonblock = fd_is_nonblock(fd);
        match socket::socket_connect(idx, addr, nonblock) {
            Ok(()) => {
                crate::safe_print!(64, "[syscall] connect(fd={}) = OK\n", fd);
                return 0;
            }
            Err(e) if e == libc_errno::EINPROGRESS => {
                crate::safe_print!(64, "[syscall] connect(fd={}) = EINPROGRESS\n", fd);
                return EINPROGRESS;
            }
            Err(e) => {
                crate::safe_print!(64, "[syscall] connect(fd={}) = err {}\n", fd, e);
                return (-e as i64) as u64;
            }
        }
    }
    !0u64
}

pub(super) fn sys_getsockname(fd: u32, addr_ptr: u64, len_ptr: u64) -> u64 {
    if addr_ptr == 0 || len_ptr == 0 { return (-libc_errno::EINVAL as i64) as u64; }
    if !validate_user_ptr(len_ptr, 4) { return EFAULT; }
    let idx = match get_socket_from_fd(fd) {
        Some(i) => i,
        None => return (-libc_errno::EBADF as i64) as u64,
    };
    let port = socket::with_socket(idx, |s| s.bind_port.unwrap_or(0)).unwrap_or(0);
    let local_ip = akuma_net::smoltcp_net::get_local_ip();
    let sa = SockAddrIn {
        sin_family: 2,
        sin_port: port.to_be(),
        sin_addr: u32::from_ne_bytes(local_ip),
        sin_zero: [0u8; 8],
    };
    if validate_user_ptr(addr_ptr, core::mem::size_of::<SockAddrIn>()) {
        unsafe {
            core::ptr::write(addr_ptr as *mut SockAddrIn, sa);
            core::ptr::write(len_ptr as *mut u32, core::mem::size_of::<SockAddrIn>() as u32);
        }
    }
    0
}

pub(super) fn sys_getpeername(fd: u32, addr_ptr: u64, len_ptr: u64) -> u64 {
    if addr_ptr == 0 || len_ptr == 0 { return (-libc_errno::EINVAL as i64) as u64; }
    if !validate_user_ptr(len_ptr, 4) { return EFAULT; }
    let idx = match get_socket_from_fd(fd) {
        Some(i) => i,
        None => return (-libc_errno::EBADF as i64) as u64,
    };

    let remote = socket::with_socket(idx, |sock| {
        match &sock.inner {
            socket::SocketType::Stream(h) => {
                akuma_net::smoltcp_net::with_network(|net| {
                    let s = net.sockets.get::<smoltcp::socket::tcp::Socket>(*h);
                    s.remote_endpoint().map(|ep| {
                        let ip = match ep.addr {
                            smoltcp::wire::IpAddress::Ipv4(addr) => addr.octets(),
                        };
                        (ip, ep.port)
                    })
                }).flatten()
            }
            socket::SocketType::Datagram { peer, .. } => {
                peer.map(|p| (p.ip, p.port))
            }
            _ => None,
        }
    }).flatten();

    match remote {
        Some((ip, port)) => {
            let sa = SockAddrIn {
                sin_family: 2,
                sin_port: port.to_be(),
                sin_addr: u32::from_ne_bytes(ip),
                sin_zero: [0u8; 8],
            };
            if validate_user_ptr(addr_ptr, core::mem::size_of::<SockAddrIn>()) {
                unsafe {
                    core::ptr::write(addr_ptr as *mut SockAddrIn, sa);
                    core::ptr::write(len_ptr as *mut u32, core::mem::size_of::<SockAddrIn>() as u32);
                }
            }
            0
        }
        None => (-libc_errno::ENOTCONN as i64) as u64,
    }
}

pub(super) fn sys_sendto(fd: u32, buf_ptr: u64, len: usize, _flags: i32, dest_addr: u64, addr_len: usize) -> u64 {
    if !validate_user_ptr(buf_ptr, len) { return EFAULT; }
    let buf = unsafe { core::slice::from_raw_parts(buf_ptr as *const u8, len) };
    let idx = match get_socket_from_fd(fd) {
        Some(i) => i,
        None => return (-libc_errno::EBADF as i64) as u64,
    };

    if socket::is_udp_socket(idx) {
        let dest = if dest_addr != 0 && addr_len >= 16 {
            if !validate_user_ptr(dest_addr, addr_len) { return EFAULT; }
            let a = unsafe { core::ptr::read(dest_addr as *const SockAddrIn) }.to_addr();
            crate::safe_print!(96, "[syscall] sendto(fd={}, len={}, dest={}.{}.{}.{}:{})\n", fd, len, a.ip[0], a.ip[1], a.ip[2], a.ip[3], a.port);
            a
        } else {
            match socket::udp_default_peer(idx) {
                Some(peer) => peer,
                None => return (-libc_errno::EINVAL as i64) as u64,
            }
        };
        match socket::socket_send_udp(idx, buf, dest) {
            Ok(n) => n as u64,
            Err(e) => (-e as i64) as u64,
        }
    } else {
        match socket::socket_send(idx, buf, fd_is_nonblock(fd)) {
            Ok(n) => n as u64,
            Err(e) => (-e as i64) as u64,
        }
    }
}

pub(super) fn sys_recvfrom(fd: u32, buf_ptr: u64, len: usize, _flags: i32, src_addr: u64, addr_len_ptr: u64) -> u64 {
    if !validate_user_ptr(buf_ptr, len) { return EFAULT; }
    let buf = unsafe { core::slice::from_raw_parts_mut(buf_ptr as *mut u8, len) };
    let idx = match get_socket_from_fd(fd) {
        Some(i) => i,
        None => return (-libc_errno::EBADF as i64) as u64,
    };
    let nonblock = fd_is_nonblock(fd);

    if socket::is_udp_socket(idx) {
        match socket::socket_recv_udp(idx, buf, nonblock) {
            Ok((n, from)) => {
                if src_addr != 0 && addr_len_ptr != 0 {
                    if validate_user_ptr(src_addr, core::mem::size_of::<SockAddrIn>())
                        && validate_user_ptr(addr_len_ptr, core::mem::size_of::<u32>())
                    {
                        let sa = SockAddrIn::from_addr(&from);
                        unsafe { core::ptr::write(src_addr as *mut SockAddrIn, sa); }
                        unsafe { core::ptr::write(addr_len_ptr as *mut u32, core::mem::size_of::<SockAddrIn>() as u32); }
                    }
                }
                n as u64
            }
            Err(e) => (-e as i64) as u64,
        }
    } else {
        match socket::socket_recv(idx, buf, nonblock) {
            Ok(n) => n as u64,
            Err(e) => (-e as i64) as u64,
        }
    }
}

pub(super) fn sys_shutdown(_fd: u32, _how: i32) -> u64 { 0 }

pub(super) fn sys_setsockopt(fd: u32, level: i32, optname: i32, optval: u64, optlen: u32) -> u64 {
    const SOL_SOCKET: i32 = 1;
    const IPPROTO_TCP: i32 = 6;
    const SO_REUSEADDR: i32 = 2;
    const SO_KEEPALIVE: i32 = 9;
    const SO_RCVBUF: i32 = 8;
    const SO_SNDBUF: i32 = 7;
    const SO_REUSEPORT: i32 = 15;
    const TCP_NODELAY: i32 = 1;
    const TCP_KEEPIDLE: i32 = 4;
    const TCP_KEEPINTVL: i32 = 5;
    const TCP_KEEPCNT: i32 = 6;

    // Read the value if provided
    let val: i32 = if optval != 0 && optlen >= 4 && validate_user_ptr(optval, 4) {
        unsafe { core::ptr::read(optval as *const i32) }
    } else {
        0
    };

    let idx = match get_socket_from_fd(fd) {
        Some(i) => i,
        None => return EBADF,
    };

    match level {
        SOL_SOCKET => {
            match optname {
                SO_REUSEADDR | SO_REUSEPORT => {
                    // We always allow address reuse - nothing to do
                    0
                }
                SO_KEEPALIVE => {
                    // Store keepalive setting (we don't actually use it yet)
                    socket::set_socket_keepalive(idx, val != 0);
                    0
                }
                SO_RCVBUF | SO_SNDBUF => {
                    // Buffer sizes are fixed at socket creation, ignore
                    0
                }
                _ => {
                    crate::tprint!(128, "[setsockopt] SOL_SOCKET optname={} ignored\n", optname);
                    0
                }
            }
        }
        IPPROTO_TCP => {
            match optname {
                TCP_NODELAY => {
                    // We already disable Nagle by default, but track the setting
                    socket::set_tcp_nodelay(idx, val != 0);
                    0
                }
                TCP_KEEPIDLE | TCP_KEEPINTVL | TCP_KEEPCNT => {
                    // Keepalive parameters - store but don't use yet
                    0
                }
                _ => {
                    crate::tprint!(128, "[setsockopt] IPPROTO_TCP optname={} ignored\n", optname);
                    0
                }
            }
        }
        _ => {
            crate::tprint!(128, "[setsockopt] level={} optname={} ignored\n", level, optname);
            0
        }
    }
}

pub(super) fn sys_getsockopt(fd: u32, level: i32, optname: i32, optval: u64, optlen: u64) -> u64 {
    const SOL_SOCKET: i32 = 1;
    const SO_ERROR: i32 = 4;
    const SO_SNDBUF: i32 = 7;
    const SO_RCVBUF: i32 = 8;
    const SO_KEEPALIVE: i32 = 9;
    const SO_TYPE: i32 = 3;

    if optval == 0 || optlen == 0 { return 0; }
    if !validate_user_ptr(optlen, 4) { return EFAULT; }
    let len = unsafe { core::ptr::read(optlen as *const u32) } as usize;
    if len < 4 || !validate_user_ptr(optval, 4) { return EFAULT; }

    let val: i32 = if level == SOL_SOCKET {
        match optname {
            SO_ERROR => {
                if let Some(idx) = get_socket_from_fd(fd) {
                    socket::with_socket(idx, |sock| {
                        if let socket::SocketType::Stream(h) = &sock.inner {
                            akuma_net::smoltcp_net::with_network(|net| {
                                let s = net.sockets.get::<smoltcp::socket::tcp::Socket>(*h);
                                if s.is_active() || s.may_send() { 0 }
                                else { libc_errno::ECONNREFUSED }
                            }).unwrap_or(0)
                        } else {
                            0
                        }
                    }).unwrap_or(0)
                } else {
                    0
                }
            }
            SO_TYPE => {
                if let Some(idx) = get_socket_from_fd(fd) {
                    if socket::is_udp_socket(idx) { 2 } else { 1 }
                } else {
                    1
                }
            }
            SO_SNDBUF => 131072,
            SO_RCVBUF => 131072,
            SO_KEEPALIVE => 0,
            _ => 0,
        }
    } else {
        0
    };

    unsafe {
        core::ptr::write(optval as *mut i32, val);
        core::ptr::write(optlen as *mut u32, 4);
    }
    0
}

#[repr(C)]
struct MsgHdr {
    msg_name: u64,
    msg_namelen: u32,
    _pad1: u32,
    msg_iov: u64,
    msg_iovlen: u32,
    _pad2: u32,
    msg_control: u64,
    msg_controllen: u64,
    msg_flags: i32,
}

pub(super) fn sys_sendmsg(fd: u32, msg_ptr: u64, _flags: i32) -> u64 {
    if !validate_user_ptr(msg_ptr, core::mem::size_of::<MsgHdr>()) { return EFAULT; }
    let msg = unsafe { &*(msg_ptr as *const MsgHdr) };

    if msg.msg_iovlen == 0 { return 0; }
    if !validate_user_ptr(msg.msg_iov, msg.msg_iovlen as usize * core::mem::size_of::<super::fs::IoVec>()) { return EFAULT; }
    let iovs = unsafe { core::slice::from_raw_parts(msg.msg_iov as *const super::fs::IoVec, msg.msg_iovlen as usize) };

    let iov = &iovs[0];
    if iov.iov_len == 0 { return 0; }
    if !validate_user_ptr(iov.iov_base, iov.iov_len as usize) { return EFAULT; }
    let buf = unsafe { core::slice::from_raw_parts(iov.iov_base as *const u8, iov.iov_len as usize) };

    let idx = match get_socket_from_fd(fd) {
        Some(i) => i,
        None => return (-libc_errno::EBADF as i64) as u64,
    };

    if socket::is_udp_socket(idx) {
        let dest = if msg.msg_name != 0 && msg.msg_namelen >= 16 {
            if !validate_user_ptr(msg.msg_name, msg.msg_namelen as usize) { return EFAULT; }
            unsafe { core::ptr::read(msg.msg_name as *const SockAddrIn) }.to_addr()
        } else {
            match socket::udp_default_peer(idx) {
                Some(peer) => peer,
                None => return (-libc_errno::EINVAL as i64) as u64,
            }
        };
        match socket::socket_send_udp(idx, buf, dest) {
            Ok(n) => n as u64,
            Err(e) => (-e as i64) as u64,
        }
    } else {
        match socket::socket_send(idx, buf, fd_is_nonblock(fd)) {
            Ok(n) => n as u64,
            Err(e) => (-e as i64) as u64,
        }
    }
}

pub(super) fn sys_recvmsg(fd: u32, msg_ptr: u64, _flags: i32) -> u64 {
    if !validate_user_ptr(msg_ptr, core::mem::size_of::<MsgHdr>()) { return EFAULT; }
    let msg = unsafe { &mut *(msg_ptr as *mut MsgHdr) };

    if msg.msg_iovlen == 0 { return 0; }
    if !validate_user_ptr(msg.msg_iov, msg.msg_iovlen as usize * core::mem::size_of::<super::fs::IoVec>()) { return EFAULT; }
    let iovs = unsafe { core::slice::from_raw_parts(msg.msg_iov as *const super::fs::IoVec, msg.msg_iovlen as usize) };

    let iov = &iovs[0];
    if iov.iov_len == 0 { return 0; }
    if !validate_user_ptr(iov.iov_base, iov.iov_len as usize) { return EFAULT; }
    let buf = unsafe { core::slice::from_raw_parts_mut(iov.iov_base as *mut u8, iov.iov_len as usize) };

    let idx = match get_socket_from_fd(fd) {
        Some(i) => i,
        None => return (-libc_errno::EBADF as i64) as u64,
    };
    let nonblock = fd_is_nonblock(fd);

    if socket::is_udp_socket(idx) {
        match socket::socket_recv_udp(idx, buf, nonblock) {
            Ok((n, from)) => {
                if msg.msg_name != 0 && msg.msg_namelen >= core::mem::size_of::<SockAddrIn>() as u32 {
                    if validate_user_ptr(msg.msg_name, core::mem::size_of::<SockAddrIn>()) {
                        let sa = SockAddrIn::from_addr(&from);
                        unsafe { core::ptr::write(msg.msg_name as *mut SockAddrIn, sa); }
                        msg.msg_namelen = core::mem::size_of::<SockAddrIn>() as u32;
                    }
                }
                msg.msg_controllen = 0;
                msg.msg_flags = 0;
                n as u64
            }
            Err(e) => (-e as i64) as u64,
        }
    } else {
        match socket::socket_recv(idx, buf, nonblock) {
            Ok(n) => {
                msg.msg_controllen = 0;
                msg.msg_flags = 0;
                n as u64
            }
            Err(e) => (-e as i64) as u64,
        }
    }
}

pub(super) fn get_socket_from_fd(fd: u32) -> Option<usize> {
    let proc = akuma_exec::process::current_process()?;
    if let Some(akuma_exec::process::FileDescriptor::Socket(idx)) = proc.get_fd(fd) { Some(idx) } else { None }
}

pub(super) fn fd_is_nonblock(fd: u32) -> bool {
    akuma_exec::process::current_process().map_or(false, |p| p.is_nonblock(fd))
}

pub(super) fn socket_get_udp_handle(idx: usize) -> Option<akuma_net::smoltcp_net::SocketHandle> {
    socket::with_socket(idx, |sock| {
        if let socket::SocketType::Datagram { handle, .. } = &sock.inner {
            Some(*handle)
        } else {
            None
        }
    }).flatten()
}

pub(super) fn socket_can_recv_tcp(idx: usize) -> bool {
    socket::with_socket(idx, |sock| {
        match &sock.inner {
            socket::SocketType::Stream(h) => {
                akuma_net::smoltcp_net::with_network(|net| {
                    let s = net.sockets.get::<smoltcp::socket::tcp::Socket>(*h);
                    s.can_recv() || !s.is_active() || !s.may_recv()
                }).unwrap_or(false)
            }
            socket::SocketType::Listener { handles, .. } => {
                // Report readable when any backlog handle has an established connection
                handles.iter().any(|&h| {
                    akuma_net::smoltcp_net::with_network(|net| {
                        net.sockets.get::<smoltcp::socket::tcp::Socket>(h).state()
                            == smoltcp::socket::tcp::State::Established
                    }).unwrap_or(false)
                })
            }
            _ => false,
        }
    }).unwrap_or(false)
}

pub(super) fn socket_can_send_tcp(idx: usize) -> bool {
    socket::with_socket(idx, |sock| {
        if let socket::SocketType::Stream(h) = &sock.inner {
            akuma_net::smoltcp_net::with_network(|net| {
                let s = net.sockets.get::<smoltcp::socket::tcp::Socket>(*h);
                s.can_send()
            }).unwrap_or(false)
        } else {
            false
        }
    }).unwrap_or(false)
}

/// Returns true when the remote peer has closed its write side (sent FIN).
/// Used to report EPOLLRDHUP — signals to libuv that recv() will return EOF.
pub(super) fn socket_peer_closed_tcp(idx: usize) -> bool {
    socket::with_socket(idx, |sock| {
        if let socket::SocketType::Stream(h) = &sock.inner {
            akuma_net::smoltcp_net::with_network(|net| {
                let s = net.sockets.get::<smoltcp::socket::tcp::Socket>(*h);
                !s.may_recv()
            }).unwrap_or(false)
        } else {
            false
        }
    }).unwrap_or(false)
}

pub(super) fn sys_resolve_host(path_ptr: u64, path_len: usize, res_ptr: u64) -> u64 {
    if !validate_user_ptr(path_ptr, path_len) { return EFAULT; }
    if !validate_user_ptr(res_ptr, 4) { return EFAULT; }
    let host = unsafe { core::str::from_utf8(core::slice::from_raw_parts(path_ptr as *const u8, path_len)).unwrap_or("") };
    match akuma_net::dns::resolve_host_blocking(host) {
        Ok(ipv4) => {
            unsafe { *(res_ptr as *mut [u8; 4]) = ipv4.octets(); }
            0
        }
        Err(_) => !0u64,
    }
}
