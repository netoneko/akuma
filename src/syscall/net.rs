use super::*;
use akuma_net::socket::{self, SockAddrIn, libc_errno};
use akuma_exec::mmu::user_access::{copy_from_user_safe, copy_to_user_safe};

/// Largest bounce buffer a single net syscall will allocate (16 pages).
const NET_BOUNCE_MAX: usize = 64 * 1024;

/// Allocate a zeroed kernel bounce buffer of up to `want` bytes (capped at
/// [`NET_BOUNCE_MAX`]) for a net syscall, **without** risking a whole-kernel
/// abort under memory pressure.
///
/// `alloc::vec![0u8; N]` is an *infallible* allocation: when the kernel heap
/// can't grow — e.g. a process paged in a model larger than RAM and the PMM is
/// down to a fragmented handful of pages — Talc's `handle_oom` returns `Err`
/// and Rust routes through `handle_alloc_error`, which under
/// `panic = "immediate-abort"` (the size/extreme profiles) is a bare `brk #1`:
/// EC=0x3c, the whole kernel dies. A 64 KiB buffer needs 16 *physically
/// contiguous* pages — exactly the multi-page heap growth a fragmented pool
/// can't satisfy (single-page growth always can, by the `handle_oom` backoff).
///
/// So allocate *fallibly* (`try_reserve_exact` returns `Err` instead of
/// aborting) and degrade gracefully:
///   1. try the full size — throughput in the common, memory-ample case;
///   2. fall back to a single page (4 KiB needs only one free page, so it's
///      satisfiable whenever any page is free; the syscall returns a short
///      count and the caller loops — always-legal short read/write semantics);
///   3. if even one page can't be had, return `None` → the caller reports
///      ENOMEM instead of taking down the kernel.
pub(crate) fn alloc_net_bounce(want: usize) -> Option<alloc::vec::Vec<u8>> {
    for size in net_bounce_size_plan(want) {
        let mut v = alloc::vec::Vec::<u8>::new();
        if v.try_reserve_exact(size).is_ok() {
            v.resize(size, 0);
            return Some(v);
        }
    }
    None
}

/// The ordered sizes [`alloc_net_bounce`] attempts, largest first: the full
/// (capped) request, then a single-page fallback that only needs one free
/// page. Pure over its input so the degradation policy is unit-testable
/// without draining real RAM. Both entries are `>= 1` so an empty request
/// still yields a usable (zero-length-after-truncation) buffer rather than a
/// zero-capacity `try_reserve_exact` that the caller can't short-read into.
pub(crate) fn net_bounce_size_plan(want: usize) -> [usize; 2] {
    let full = want.min(NET_BOUNCE_MAX).max(1);
    let single_page = 4096usize.min(full);
    [full, single_page]
}

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
        // Process gone between alloc_socket and current_process.
        return ESRCH;
    }
    EMFILE
}

/// AF_UNIX `socketpair` (syscall 199).
///
/// Rust std uses this to build the IPC channel that relays a spawned child's
/// exec errno back to the parent. `rustc` calls it before exec'ing the linker,
/// so without it `rustc -C linker=...` fails with ENOSYS ("could not exec the
/// linker: Function not implemented").
///
/// Backed by two unidirectional kernel pipes (px carries endpoint0 -> endpoint1,
/// py carries endpoint1 -> endpoint0). Each endpoint reads from one pipe and
/// writes to the other. NOTE: this approximates SOCK_SEQPACKET with a byte
/// stream — message boundaries are not preserved. That is sufficient for
/// libstd's single fixed-size handshake (and EOF-on-success) but is not a fully
/// conformant SEQPACKET.
pub(super) fn sys_socketpair(domain: i32, sock_type: i32, _proto: i32, sv_ptr: u64) -> u64 {
    let base_type = sock_type & 0xFF;
    let cloexec = sock_type & 0x80000 != 0;
    let nonblock = sock_type & 0x800 != 0;
    // Only AF_UNIX (1); accept SOCK_STREAM (1) and SOCK_SEQPACKET (5).
    if domain != 1 || (base_type != 1 && base_type != 5) {
        crate::safe_print!(96, "[syscall] socketpair(domain={}, type=0x{:x}): unsupported\n", domain, sock_type);
        return EAFNOSUPPORT;
    }
    if !validate_user_ptr(sv_ptr, 8) {
        return EFAULT;
    }
    let proc = match akuma_exec::process::current_process() {
        Some(p) => p,
        None => return ESRCH,
    };

    // Two unidirectional pipes; each pipe_create() starts at write_count=1,
    // read_count=1, which is exactly one writer + one reader per direction.
    let px = super::pipe::pipe_create();
    let py = super::pipe::pipe_create();

    let fd0 = proc.alloc_fd(akuma_exec::process::FileDescriptor::UnixSocket { rx: px, tx: py });
    let fd1 = proc.alloc_fd(akuma_exec::process::FileDescriptor::UnixSocket { rx: py, tx: px });

    if cloexec {
        proc.set_cloexec(fd0);
        proc.set_cloexec(fd1);
    }
    if nonblock {
        proc.set_nonblock(fd0);
        proc.set_nonblock(fd1);
    }

    let fds = [fd0 as i32, fd1 as i32];
    if unsafe { copy_to_user_safe(sv_ptr as *mut u8, fds.as_ptr() as *const u8, 8).is_err() } {
        // Roll back so we don't leak fds or pipe slots. Closing both directions
        // of each pipe drives its ref counts to zero and destroys it.
        proc.remove_fd(fd0);
        proc.remove_fd(fd1);
        proc.clear_cloexec(fd0);
        proc.clear_cloexec(fd1);
        proc.clear_nonblock(fd0);
        proc.clear_nonblock(fd1);
        super::pipe::pipe_close_read(px);
        super::pipe::pipe_close_write(px);
        super::pipe::pipe_close_read(py);
        super::pipe::pipe_close_write(py);
        return EFAULT;
    }
    crate::safe_print!(96, "[syscall] socketpair(AF_UNIX) = ({}, {})\n", fd0, fd1);
    0
}

pub(super) fn sys_bind(fd: u32, addr_ptr: u64, len: usize) -> u64 {
    if len < 16 { return EINVAL; }
    if !validate_user_ptr(addr_ptr, len) { return EFAULT; }
    let mut sa = SockAddrIn::default();
    let copy_len = len.min(core::mem::size_of::<SockAddrIn>());
    if unsafe { copy_from_user_safe(&mut sa as *mut SockAddrIn as *mut u8, addr_ptr as *const u8, copy_len).is_err() } {
        return EFAULT;
    }
    let addr = sa.to_addr();
    crate::safe_print!(96, "[syscall] bind(fd={}, port={}, ip={}.{}.{}.{})\n", fd, addr.port, addr.ip[0], addr.ip[1], addr.ip[2], addr.ip[3]);
    let idx = match get_socket_from_fd(fd) {
        Some(i) => i,
        None => return EBADF,
    };
    match socket::socket_bind(idx, addr) {
        Ok(()) => 0,
        Err(e) => {
            crate::safe_print!(64, "[syscall] bind failed: {}\n", e);
            neg_errno(e)
        }
    }
}

pub(super) fn sys_listen(fd: u32, backlog: i32) -> u64 {
    let idx = match get_socket_from_fd(fd) {
        Some(i) => i,
        None => return EBADF,
    };
    match socket::socket_listen(idx, backlog as usize) {
        Ok(()) => 0,
        Err(e) => neg_errno(e),
    }
}

pub(super) fn sys_accept(fd: u32, addr_ptr: u64, len_ptr: u64) -> u64 {
    if addr_ptr != 0 && !validate_user_ptr(addr_ptr, 16) { return EFAULT; }
    if len_ptr != 0 && !validate_user_ptr(len_ptr, 4) { return EFAULT; }
    let idx = match get_socket_from_fd(fd) {
        Some(i) => i,
        None => return EBADF,
    };
    let nonblock = fd_is_nonblock(fd);
    match socket::socket_accept(idx, nonblock) {
        Ok((new_idx, addr)) => {
            let proc = match akuma_exec::process::current_process() {
                Some(p) => p,
                None => return ESRCH,
            };
            if addr_ptr != 0 {
                let sa = SockAddrIn::from_addr(&addr);
                let _ = unsafe { copy_to_user_safe(addr_ptr as *mut u8, &sa as *const SockAddrIn as *const u8, core::mem::size_of::<SockAddrIn>()) };
            }
            proc.alloc_fd(akuma_exec::process::FileDescriptor::Socket(new_idx)) as u64
        }
        Err(e) => neg_errno(e),
    }
}

pub(super) fn sys_accept4(fd: u32, addr_ptr: u64, len_ptr: u64, flags: u32) -> u64 {
    if addr_ptr != 0 && !validate_user_ptr(addr_ptr, 16) { return EFAULT; }
    if len_ptr != 0 && !validate_user_ptr(len_ptr, 4) { return EFAULT; }
    let idx = match get_socket_from_fd(fd) {
        Some(i) => i,
        None => return EBADF,
    };
    let nonblock = fd_is_nonblock(fd);
    match socket::socket_accept(idx, nonblock) {
        Ok((new_idx, addr)) => {
            let proc = match akuma_exec::process::current_process() {
                Some(p) => p,
                None => return ESRCH,
            };
            if addr_ptr != 0 {
                let sa = SockAddrIn::from_addr(&addr);
                let _ = unsafe { copy_to_user_safe(addr_ptr as *mut u8, &sa as *const SockAddrIn as *const u8, core::mem::size_of::<SockAddrIn>()) };
            }
            let new_fd = proc.alloc_fd(akuma_exec::process::FileDescriptor::Socket(new_idx));
            const SOCK_CLOEXEC: u32 = 0x80000;
            const SOCK_NONBLOCK: u32 = 0x800;
            if flags & SOCK_CLOEXEC != 0 { proc.set_cloexec(new_fd); }
            if flags & SOCK_NONBLOCK != 0 { proc.set_nonblock(new_fd); }
            new_fd as u64
        }
        Err(e) => neg_errno(e),
    }
}

pub(super) fn sys_connect(fd: u32, addr_ptr: u64, len: usize) -> u64 {
    if len < 16 { return EINVAL; }
    if !validate_user_ptr(addr_ptr, len) { return EFAULT; }
    let mut sa = SockAddrIn::default();
    let copy_len = len.min(core::mem::size_of::<SockAddrIn>());
    if unsafe { copy_from_user_safe(&mut sa as *mut SockAddrIn as *mut u8, addr_ptr as *const u8, copy_len).is_err() } {
        return EFAULT;
    }
    let addr = sa.to_addr();
    crate::safe_print!(96, "[syscall] connect(fd={}, ip={}.{}.{}.{}:{})\n", fd, addr.ip[0], addr.ip[1], addr.ip[2], addr.ip[3], addr.port);
    let idx = match get_socket_from_fd(fd) {
        Some(i) => i,
        None => return EBADF,
    };
    let nonblock = fd_is_nonblock(fd);
    match socket::socket_connect(idx, addr, nonblock) {
        Ok(()) => {
            crate::safe_print!(64, "[syscall] connect(fd={}) = OK\n", fd);
            0
        }
        Err(e) if e == libc_errno::EINPROGRESS => {
            crate::safe_print!(64, "[syscall] connect(fd={}) = EINPROGRESS\n", fd);
            EINPROGRESS
        }
        Err(e) => {
            crate::safe_print!(64, "[syscall] connect(fd={}) = err {}\n", fd, e);
            neg_errno(e)
        }
    }
}

pub(super) fn sys_getsockname(fd: u32, addr_ptr: u64, len_ptr: u64) -> u64 {
    if addr_ptr == 0 || len_ptr == 0 { return EINVAL; }
    if !validate_user_ptr(len_ptr, 4) { return EFAULT; }
    let idx = match get_socket_from_fd(fd) {
        Some(i) => i,
        None => return EBADF,
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
        if unsafe { copy_to_user_safe(addr_ptr as *mut u8, &sa as *const SockAddrIn as *const u8, core::mem::size_of::<SockAddrIn>()).is_err() } {
            return EFAULT;
        }
        let out_len = core::mem::size_of::<SockAddrIn>() as u32;
        if unsafe { copy_to_user_safe(len_ptr as *mut u8, &out_len as *const u32 as *const u8, 4).is_err() } {
            return EFAULT;
        }
    }
    0
}

pub(super) fn sys_getpeername(fd: u32, addr_ptr: u64, len_ptr: u64) -> u64 {
    if addr_ptr == 0 || len_ptr == 0 { return EINVAL; }
    if !validate_user_ptr(len_ptr, 4) { return EFAULT; }
    let idx = match get_socket_from_fd(fd) {
        Some(i) => i,
        None => return EBADF,
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
                if unsafe { copy_to_user_safe(addr_ptr as *mut u8, &sa as *const SockAddrIn as *const u8, core::mem::size_of::<SockAddrIn>()).is_err() } {
                    return EFAULT;
                }
                let out_len = core::mem::size_of::<SockAddrIn>() as u32;
                if unsafe { copy_to_user_safe(len_ptr as *mut u8, &out_len as *const u32 as *const u8, 4).is_err() } {
                    return EFAULT;
                }
            }
            0
        }
        None => neg_errno(libc_errno::ENOTCONN),
    }
}

pub(super) fn sys_sendto(fd: u32, buf_ptr: u64, len: usize, _flags: i32, dest_addr: u64, addr_len: usize) -> u64 {
    // AF_UNIX socketpair endpoint: send == write to the tx pipe.
    if fd_is_unix_socket(fd) {
        return super::fs::sys_write(fd as u64, buf_ptr, len);
    }
    if !validate_user_ptr(buf_ptr, len) { return EFAULT; }
    let mut kernel_buf = match alloc_net_bounce(len) {
        Some(b) => b,
        None => return ENOMEM,
    };
    let chunk_len = kernel_buf.len();
    if unsafe { copy_from_user_safe(kernel_buf.as_mut_ptr(), buf_ptr as *const u8, chunk_len).is_err() } {
        return EFAULT;
    }
    let buf = &kernel_buf[..chunk_len];
    
    let idx = match get_socket_from_fd(fd) {
        Some(i) => i,
        None => return EBADF,
    };

    if socket::is_udp_socket(idx) {
        let dest = if dest_addr != 0 && addr_len >= 16 {
            if !validate_user_ptr(dest_addr, addr_len) { return EFAULT; }
            let mut sa = SockAddrIn::default();
            let sa_copy_len = addr_len.min(core::mem::size_of::<SockAddrIn>());
            if unsafe { copy_from_user_safe(&mut sa as *mut SockAddrIn as *mut u8, dest_addr as *const u8, sa_copy_len).is_err() } {
                return EFAULT;
            }
            let a = sa.to_addr();
            crate::safe_print!(96, "[syscall] sendto(fd={}, len={}, dest={}.{}.{}.{}:{})\n", fd, len, a.ip[0], a.ip[1], a.ip[2], a.ip[3], a.port);
            // Extra debug for DNS traffic
            if crate::config::SYSCALL_DEBUG_NET_ENABLED && a.port == 53 {
                crate::tprint!(128, "[DNS] query sent: fd={} len={} to {}.{}.{}.{}:53\n", 
                    fd, len, a.ip[0], a.ip[1], a.ip[2], a.ip[3]);
            }
            a
        } else {
            match socket::udp_default_peer(idx) {
                Some(peer) => peer,
                None => return neg_errno(libc_errno::EDESTADDRREQ),
            }
        };
        match socket::socket_send_udp(idx, buf, dest) {
            Ok(n) => {
                if crate::config::SYSCALL_DEBUG_NET_ENABLED && dest.port == 53 {
                    crate::tprint!(64, "[DNS] query sent OK: {} bytes\n", n);
                }
                n as u64
            }
            Err(e) => {
                if crate::config::SYSCALL_DEBUG_NET_ENABLED && dest.port == 53 {
                    crate::tprint!(64, "[DNS] query send error: {}\n", e);
                }
                neg_errno(e)
            }
        }
    } else {
        match socket::socket_send(idx, buf, fd_is_nonblock(fd)) {
            Ok(n) => n as u64,
            Err(e) => neg_errno(e),
        }
    }
}

pub(super) fn sys_recvfrom(fd: u32, buf_ptr: u64, len: usize, _flags: i32, src_addr: u64, addr_len_ptr: u64) -> u64 {
    // AF_UNIX socketpair endpoint: recv == read from the rx pipe.
    if fd_is_unix_socket(fd) {
        return super::fs::sys_read(fd as u64, buf_ptr, len);
    }
    if !validate_user_ptr(buf_ptr, len) { return EFAULT; }
    let mut kernel_buf = match alloc_net_bounce(len) {
        Some(b) => b,
        None => return ENOMEM,
    };
    let idx = match get_socket_from_fd(fd) {
        Some(i) => i,
        None => return EBADF,
    };
    let nonblock = fd_is_nonblock(fd);

    if socket::is_udp_socket(idx) {
        if crate::config::SYSCALL_DEBUG_NET_ENABLED {
            crate::tprint!(96, "[UDP] recvfrom: fd={} len={} nonblock={}\n", fd, len, nonblock);
        }
        match socket::socket_recv_udp(idx, &mut kernel_buf, nonblock) {
            Ok((n, from)) => {
                if crate::config::SYSCALL_DEBUG_NET_ENABLED {
                    let ip = from.ip;
                    crate::tprint!(96, "[UDP] recvfrom OK: {} bytes from {}.{}.{}.{}:{}\n", 
                        n, ip[0], ip[1], ip[2], ip[3], from.port);
                }
                if unsafe { copy_to_user_safe(buf_ptr as *mut u8, kernel_buf.as_ptr(), n).is_err() } {
                    return EFAULT;
                }
                if src_addr != 0 && addr_len_ptr != 0 {
                    if validate_user_ptr(src_addr, core::mem::size_of::<SockAddrIn>())
                        && validate_user_ptr(addr_len_ptr, core::mem::size_of::<u32>())
                    {
                        let sa = SockAddrIn::from_addr(&from);
                        let _ = unsafe { copy_to_user_safe(src_addr as *mut u8, &sa as *const SockAddrIn as *const u8, core::mem::size_of::<SockAddrIn>()) };
                        let out_len = core::mem::size_of::<SockAddrIn>() as u32;
                        let _ = unsafe { copy_to_user_safe(addr_len_ptr as *mut u8, &out_len as *const u32 as *const u8, 4) };
                    }
                }
                n as u64
            }
            Err(e) => {
                if crate::config::SYSCALL_DEBUG_NET_ENABLED && e != libc_errno::EAGAIN {
                    crate::tprint!(64, "[UDP] recvfrom error: {}\n", e);
                }
                neg_errno(e)
            }
        }
    } else {
        match socket::socket_recv(idx, &mut kernel_buf, nonblock) {
            Ok(n) => {
                if crate::config::SYSCALL_DEBUG_NET_ENABLED {
                    crate::tprint!(96, "[TCP] recvfrom fd={} got={}\n", fd, n);
                }
                if unsafe { copy_to_user_safe(buf_ptr as *mut u8, kernel_buf.as_ptr(), n).is_err() } {
                    return EFAULT;
                }
                // Reset the EPOLLET edge so the next data arrival fires EPOLLIN.
                // BoringSSL/bun reads one TLS record at a time without draining to EAGAIN,
                // so we can't rely on EAGAIN to reset the edge.
                super::poll::epoll_on_fd_drained(fd);
                n as u64
            }
            Err(e) => {
                if crate::config::SYSCALL_DEBUG_NET_ENABLED {
                    crate::tprint!(64, "[TCP] recvfrom fd={} err={}\n", fd, e);
                }
                if e == libc_errno::EAGAIN {
                    super::poll::epoll_on_fd_drained(fd);
                }
                neg_errno(e)
            }
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
    const SO_LINGER: i32 = 13;
    const SO_REUSEPORT: i32 = 15;
    const TCP_NODELAY: i32 = 1;
    const TCP_CORK: i32 = 3;
    const TCP_KEEPIDLE: i32 = 4;
    const TCP_KEEPINTVL: i32 = 5;
    const TCP_KEEPCNT: i32 = 6;

    // Read the value if provided
    let mut val: i32 = 0;
    if optval != 0 && optlen >= 4 && validate_user_ptr(optval, 4) {
        if unsafe { copy_from_user_safe(&mut val as *mut i32 as *mut u8, optval as *const u8, 4).is_err() } {
            return EFAULT;
        }
    }

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
                    0
                }
                SO_LINGER => {
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
                TCP_CORK => {
                    0
                }
                TCP_KEEPIDLE | TCP_KEEPINTVL | TCP_KEEPCNT => {
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
    let mut len: u32 = 0;
    if unsafe { copy_from_user_safe(&mut len as *mut u32 as *mut u8, optlen as *const u8, 4).is_err() } {
        return EFAULT;
    }
    if (len as usize) < 4 || !validate_user_ptr(optval, 4) { return EFAULT; }

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

    if unsafe { copy_to_user_safe(optval as *mut u8, &val as *const i32 as *const u8, 4).is_err() } {
        return EFAULT;
    }
    let out_len: u32 = 4;
    if unsafe { copy_to_user_safe(optlen as *mut u8, &out_len as *const u32 as *const u8, 4).is_err() } {
        return EFAULT;
    }
    0
}

#[repr(C)]
#[derive(Default)]
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
    let mut msg = MsgHdr::default();
    if unsafe { copy_from_user_safe(&mut msg as *mut MsgHdr as *mut u8, msg_ptr as *const u8, core::mem::size_of::<MsgHdr>()).is_err() } {
        return EFAULT;
    }

    if msg.msg_iovlen == 0 { return 0; }
    let iov_size = msg.msg_iovlen as usize * core::mem::size_of::<super::fs::IoVec>();
    if !validate_user_ptr(msg.msg_iov, iov_size) { return EFAULT; }
    let mut iovs = alloc::vec![super::fs::IoVec { iov_base: 0, iov_len: 0 }; msg.msg_iovlen as usize];
    if unsafe { copy_from_user_safe(iovs.as_mut_ptr() as *mut u8, msg.msg_iov as *const u8, iov_size).is_err() } {
        return EFAULT;
    }

    let iov = &iovs[0];
    if iov.iov_len == 0 { return 0; }
    if !validate_user_ptr(iov.iov_base, iov.iov_len as usize) { return EFAULT; }

    // AF_UNIX socketpair endpoint: sendmsg == write the first iovec to the tx
    // pipe. (libstd's handshake uses a single small message.)
    if fd_is_unix_socket(fd) {
        return super::fs::sys_write(fd as u64, iov.iov_base, iov.iov_len as usize);
    }

    let mut kernel_buf = match alloc_net_bounce(iov.iov_len) {
        Some(b) => b,
        None => return ENOMEM,
    };
    if unsafe { copy_from_user_safe(kernel_buf.as_mut_ptr(), iov.iov_base as *const u8, kernel_buf.len()).is_err() } {
        return EFAULT;
    }

    let idx = match get_socket_from_fd(fd) {
        Some(i) => i,
        None => return EBADF,
    };

    if socket::is_udp_socket(idx) {
        let dest = if msg.msg_name != 0 && msg.msg_namelen >= 16 {
            if !validate_user_ptr(msg.msg_name, msg.msg_namelen as usize) { return EFAULT; }
            let mut sa = SockAddrIn::default();
            let _ = unsafe { copy_from_user_safe(&mut sa as *mut SockAddrIn as *mut u8, msg.msg_name as *const u8, 16) };
            sa.to_addr()
        } else {
            match socket::udp_default_peer(idx) {
                Some(peer) => peer,
                None => return neg_errno(libc_errno::EDESTADDRREQ),
            }
        };
        match socket::socket_send_udp(idx, &kernel_buf, dest) {
            Ok(n) => n as u64,
            Err(e) => neg_errno(e),
        }
    } else {
        let result = socket::socket_send(idx, &kernel_buf, fd_is_nonblock(fd));
        if crate::config::SYSCALL_DEBUG_NET_ENABLED {
            match &result {
                Ok(n) => crate::tprint!(96, "[TCP] sendmsg fd={} len={} sent={}\n", fd, kernel_buf.len(), n),
                Err(e) => crate::tprint!(64, "[TCP] sendmsg fd={} err={}\n", fd, e),
            }
        }
        match result {
            Ok(n) => n as u64,
            Err(e) => neg_errno(e),
        }
    }
}

pub(super) fn sys_recvmsg(fd: u32, msg_ptr: u64, _flags: i32) -> u64 {
    if !validate_user_ptr(msg_ptr, core::mem::size_of::<MsgHdr>()) { return EFAULT; }
    let mut msg = MsgHdr::default();
    if unsafe { copy_from_user_safe(&mut msg as *mut MsgHdr as *mut u8, msg_ptr as *const u8, core::mem::size_of::<MsgHdr>()).is_err() } {
        return EFAULT;
    }

    if msg.msg_iovlen == 0 { return 0; }
    let iov_size = msg.msg_iovlen as usize * core::mem::size_of::<super::fs::IoVec>();
    if !validate_user_ptr(msg.msg_iov, iov_size) { return EFAULT; }
    let mut iovs = alloc::vec![super::fs::IoVec { iov_base: 0, iov_len: 0 }; msg.msg_iovlen as usize];
    if unsafe { copy_from_user_safe(iovs.as_mut_ptr() as *mut u8, msg.msg_iov as *const u8, iov_size).is_err() } {
        return EFAULT;
    }

    let iov = &mut iovs[0];
    if iov.iov_len == 0 { return 0; }
    if !validate_user_ptr(iov.iov_base, iov.iov_len as usize) { return EFAULT; }

    // AF_UNIX socketpair endpoint: recvmsg == read into the first iovec from the
    // rx pipe. (libstd's handshake uses a single small message.) On success,
    // clear the ancillary/flags fields and write the header back.
    if fd_is_unix_socket(fd) {
        let n = super::fs::sys_read(fd as u64, iov.iov_base, iov.iov_len as usize);
        if (n as i64) >= 0 {
            msg.msg_controllen = 0;
            msg.msg_flags = 0;
            let _ = unsafe { copy_to_user_safe(msg_ptr as *mut u8, &msg as *const MsgHdr as *const u8, core::mem::size_of::<MsgHdr>()) };
        }
        return n;
    }

    let mut kernel_buf = match alloc_net_bounce(iov.iov_len) {
        Some(b) => b,
        None => return ENOMEM,
    };

    let idx = match get_socket_from_fd(fd) {
        Some(i) => i,
        None => return EBADF,
    };
    let nonblock = fd_is_nonblock(fd);

    if socket::is_udp_socket(idx) {
        if crate::config::SYSCALL_DEBUG_NET_ENABLED {
            crate::tprint!(96, "[UDP] recvmsg: fd={} buflen={} nonblock={}\n", fd, kernel_buf.len(), nonblock);
        }
        match socket::socket_recv_udp(idx, &mut kernel_buf, nonblock) {
            Ok((n, from)) => {
                if crate::config::SYSCALL_DEBUG_NET_ENABLED {
                    let ip = from.ip;
                    crate::tprint!(96, "[UDP] recvmsg OK: {} bytes from {}.{}.{}.{}:{}\n",
                        n, ip[0], ip[1], ip[2], ip[3], from.port);
                }
                if unsafe { copy_to_user_safe(iov.iov_base as *mut u8, kernel_buf.as_ptr(), n).is_err() } {
                    return EFAULT;
                }
                if msg.msg_name != 0 && msg.msg_namelen >= core::mem::size_of::<SockAddrIn>() as u32 {
                    if validate_user_ptr(msg.msg_name, core::mem::size_of::<SockAddrIn>()) {
                        let sa = SockAddrIn::from_addr(&from);
                        let _ = unsafe { copy_to_user_safe(msg.msg_name as *mut u8, &sa as *const SockAddrIn as *const u8, core::mem::size_of::<SockAddrIn>()) };
                        msg.msg_namelen = core::mem::size_of::<SockAddrIn>() as u32;
                    }
                }
                msg.msg_controllen = 0;
                msg.msg_flags = 0;
                // Copy msg back to user
                let _ = unsafe { copy_to_user_safe(msg_ptr as *mut u8, &msg as *const MsgHdr as *const u8, core::mem::size_of::<MsgHdr>()) };
                n as u64
            }
            Err(e) => {
                if crate::config::SYSCALL_DEBUG_NET_ENABLED && e != libc_errno::EAGAIN {
                    crate::tprint!(64, "[UDP] recvmsg error: {}\n", e);
                }
                neg_errno(e)
            }
        }
    } else {
        match socket::socket_recv(idx, &mut kernel_buf, nonblock) {
            Ok(n) => {
                if crate::config::SYSCALL_DEBUG_NET_ENABLED {
                    crate::tprint!(96, "[TCP] recvmsg fd={} got={}\n", fd, n);
                }
                if unsafe { copy_to_user_safe(iov.iov_base as *mut u8, kernel_buf.as_ptr(), n).is_err() } {
                    return EFAULT;
                }
                msg.msg_controllen = 0;
                msg.msg_flags = 0;
                let _ = unsafe { copy_to_user_safe(msg_ptr as *mut u8, &msg as *const MsgHdr as *const u8, core::mem::size_of::<MsgHdr>()) };
                // Reset EPOLLET edge — BoringSSL reads one TLS record at a time without
                // draining to EAGAIN, so we reset after every successful read.
                super::poll::epoll_on_fd_drained(fd);
                n as u64
            }
            Err(e) => {
                if crate::config::SYSCALL_DEBUG_NET_ENABLED {
                    crate::tprint!(64, "[TCP] recvmsg fd={} err={}\n", fd, e);
                }
                if e == libc_errno::EAGAIN {
                    super::poll::epoll_on_fd_drained(fd);
                }
                neg_errno(e)
            }
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

/// True if `fd` is one endpoint of an AF_UNIX socketpair (backed by two kernel
/// pipes, not a smoltcp `Socket`). The socket send/recv syscalls route these to
/// the backing pipes with plain read(2)/write(2) semantics — libstd's
/// `fork`+exec child-spawn handshake reads its `SOCK_SEQPACKET` socketpair via
/// `recvmsg`, which otherwise hit the `get_socket_from_fd` → `None` → `EBADF`
/// path and surfaced as `the CLOEXEC pipe failed: … Bad file descriptor`
/// (docs/RUST_TOOLCHAIN.md §4d).
pub(super) fn fd_is_unix_socket(fd: u32) -> bool {
    akuma_exec::process::current_process().map_or(false, |p| {
        matches!(p.get_fd(fd), Some(akuma_exec::process::FileDescriptor::UnixSocket { .. }))
    })
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

pub(super) fn socket_recv_queue_size(idx: usize) -> usize {
    socket::with_socket(idx, |sock| {
        match &sock.inner {
            socket::SocketType::Stream(h) => {
                akuma_net::smoltcp_net::with_network(|net| {
                    net.sockets.get::<smoltcp::socket::tcp::Socket>(*h).recv_queue()
                }).unwrap_or(0)
            }
            socket::SocketType::Datagram { handle, .. } => {
                akuma_net::smoltcp_net::with_network(|net| {
                    net.sockets.get::<smoltcp::socket::udp::Socket>(*handle).recv_queue()
                }).unwrap_or(0)
            }
            _ => 0,
        }
    }).unwrap_or(0)
}

pub(super) fn socket_can_recv_tcp(idx: usize) -> bool {
    socket::with_socket(idx, |sock| {
        match &sock.inner {
            socket::SocketType::Stream(h) => {
                akuma_net::smoltcp_net::with_network(|net| {
                    let s = net.sockets.get::<smoltcp::socket::tcp::Socket>(*h);
                    // Report readable when:
                    //   - data is buffered (can_recv), OR
                    //   - remote sent FIN and socket is still active (!may_recv && is_active):
                    //     this causes recv() to return 0 (EOF) so the app can clean up.
                    //
                    // Do NOT use !is_active() here: a Closed smoltcp socket (e.g. after TCP
                    // timeout or RST) would permanently signal EPOLLIN even with no data,
                    // causing the caller to spin recv() → EAGAIN → epoll → EPOLLIN → ...
                    // Instead, a fully-dead socket is reported via EPOLLHUP in
                    // epoll_check_fd_readiness.
                    s.can_recv() || (s.is_active() && !s.may_recv())
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

/// Returns true when the smoltcp socket is completely dead (Closed state).
/// Used to report EPOLLHUP so callers detect connection loss without spinning.
pub(super) fn socket_is_dead_tcp(idx: usize) -> bool {
    socket::with_socket(idx, |sock| {
        if let socket::SocketType::Stream(h) = &sock.inner {
            akuma_net::smoltcp_net::with_network(|net| {
                !net.sockets.get::<smoltcp::socket::tcp::Socket>(*h).is_active()
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
    let mut kernel_path = alloc::vec![0u8; path_len];
    if unsafe { copy_from_user_safe(kernel_path.as_mut_ptr(), path_ptr as *const u8, path_len).is_err() } {
        return EFAULT;
    }
    let host = core::str::from_utf8(&kernel_path).unwrap_or("");
    match akuma_net::dns::resolve_host_blocking(host) {
        Ok(ipv4) => {
            let octets = ipv4.octets();
            if unsafe { copy_to_user_safe(res_ptr as *mut u8, octets.as_ptr(), 4).is_err() } {
                return EFAULT;
            }
            0
        }
        // Custom Akuma syscall: report DNS resolution failure as ENOENT
        // (matches how getaddrinfo's EAI_NONAME maps to ENOENT-flavored errors
        // and is much more useful to userspace than a generic -EPERM).
        Err(_) => ENOENT,
    }
}

/// Boot self-test for the net bounce-buffer allocator. Verifies the
/// degradation policy that keeps an oversized socket send/recv from aborting
/// the whole kernel under PMM exhaustion (the EC=0x3c `brk #1` crash seen when
/// llama-server streamed HTTP while an 84 MB model had drained a 64 MB VM —
/// the 64 KiB bounce buffer needs 16 *contiguous* pages, which a fragmented
/// pool can't grow into, so the infallible `vec![]` routed through
/// `handle_alloc_error` → `brk #1`). The fix allocates *fallibly* and backs
/// off to a single page, then to ENOMEM — never aborting.
#[cfg(not(any(feature = "no-tests", kernel_profile_size)))]
pub(crate) fn run_net_bounce_tests() {
    // --- Pure size-plan boundaries (no RAM touched) ---
    // Empty request still yields a >=1-byte plan (never a zero-cap reserve).
    assert_eq!(net_bounce_size_plan(0), [1, 1],
        "empty request must still produce a usable 1-byte buffer");
    // Sub-page request: both attempts are the same small size.
    assert_eq!(net_bounce_size_plan(100), [100, 100],
        "sub-page request needs no single-page fallback distinct from itself");
    // Page-sized request: full == single-page.
    assert_eq!(net_bounce_size_plan(4096), [4096, 4096],
        "page-sized request's fallback equals the full size");
    // Multi-page request: full first, then a single-page (1-free-page) fallback.
    assert_eq!(net_bounce_size_plan(8192), [8192, 4096],
        "multi-page request must fall back to exactly one page");
    // 64 KiB (the dominant streaming case, 16 pages) — the exact size that
    // crashed the kernel; must fall back to a single page.
    assert_eq!(net_bounce_size_plan(NET_BOUNCE_MAX), [NET_BOUNCE_MAX, 4096],
        "the 16-page bounce buffer must offer a single-page fallback");
    // Over the cap: clamped to NET_BOUNCE_MAX, single-page fallback.
    assert_eq!(net_bounce_size_plan(1 << 20), [NET_BOUNCE_MAX, 4096],
        "oversized request is capped at the 64 KiB bounce maximum");

    // --- Real allocation under ample boot memory: correct size + zeroed ---
    let buf = alloc_net_bounce(8192).expect("8 KiB bounce alloc must succeed at boot");
    assert_eq!(buf.len(), 8192, "ample-memory alloc returns the full requested size");
    assert!(buf.iter().all(|&b| b == 0), "bounce buffer must be zero-initialised");

    // Oversized request is capped, not failed.
    let capped = alloc_net_bounce(1 << 20).expect("capped bounce alloc must succeed at boot");
    assert_eq!(capped.len(), NET_BOUNCE_MAX, "oversized request is served at the cap");

    crate::console::print("  [PASS] test_net_bounce_alloc_degradation\n");
}
