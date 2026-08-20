use super::*;
#[cfg(feature = "smoltcp")]
use akuma_primitives::{GuardToggle, ToggledGuard};
// Socket types/errnos are only used by the smoltcp socket ops below; the copyin/
// copyout helpers are also used by the Tier-A bounce/socketpair paths and the
// rump-only UnixSocket `sendmsg` variant, so they stay ungated.
#[cfg(feature = "smoltcp")]
use akuma_net::socket::{self, SockAddrIn, libc_errno};

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
/// `panic = "abort"` (every profile) is a bare `brk #1`:
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
pub fn alloc_net_bounce(want: usize) -> Option<alloc::vec::Vec<u8>> {
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
pub fn net_bounce_size_plan(want: usize) -> [usize; 2] {
    let full = want.clamp(1, NET_BOUNCE_MAX);
    let single_page = 4096usize.min(full);
    [full, single_page]
}

/// The `no-bkl-network` carve-out (Phase 2 of
/// docs/archive/BKL_FINE_GRAINED_LOCKING_PLAN.md), as a [`GuardToggle`] marker —
/// the original of the five, and the template the other four were written from.
///
/// Correctness rests on the state these syscalls mutate already carrying its own
/// fine-grained locks — the per-process fd table (`SharedFdTable`'s spinlocks), the
/// socket descriptor table (`akuma_net::socket::SOCKET_TABLE`), and the network stack
/// (`akuma_net::smoltcp_net::NETWORK`, held under a `PreemptGuard`) — so the BKL is
/// redundant for them; dropping it lets non-network work on other cores proceed in
/// parallel. Blocking waits (`accept`/`connect`/`recv`/DNS) hold none of the inner
/// spinlocks across the wait, so two cores can block in network syscalls at once
/// without wedging (unlike wrapping the whole syscall in one coarse lock).
///
/// **The one with no runtime toggle.** [`enabled`](GuardToggle::enabled) is a
/// constant `true`: this phase shipped before the A/B toggles existed and never grew
/// one, so it is purely compile-time gated. The latching machinery costs it nothing —
/// `COMPILED_IN && true` folds to `COMPILED_IN`.
#[cfg(feature = "smoltcp")]
pub(super) struct NetBkl;

#[cfg(feature = "smoltcp")]
impl GuardToggle for NetBkl {
    const COMPILED_IN: bool = cfg!(all(kernel_smp_shared, kernel_no_bkl_network));
    #[inline]
    fn enabled() -> bool {
        true
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

/// RAII guard that runs a native (smoltcp) network syscall **without** the Big
/// Kernel Lock.
///
/// Constructed at the top of each net syscall: `new()` DROPS the BKL so this core
/// runs the syscall concurrently with peer cores, and `drop()` RE-ACQUIRES it on
/// every return path, keeping the syscall wrapper's single `leave_kernel`
/// (`rust_sync_el0_handler` in exceptions.rs) balanced. The pair registers the window
/// in the per-thread ledger so a nested IRQ, page fault, blocking wait, or context
/// switch RESTORES the dropped state on resume instead of silently re-holding the BKL
/// for the window's remainder (the `[BKL] stuck` conversion,
/// docs/archive/BKL_VFS_CARVE_OUT.md §8 — net's blocking recv/accept windows were
/// converted on every wake before the ledger existed).
#[cfg(feature = "smoltcp")]
pub(super) type NetBklGuard = ToggledGuard<NetBkl>;

#[cfg(feature = "smoltcp")]
pub(super) fn sys_socket(domain: i32, sock_type: i32, _proto: i32) -> u64 {
    let _net_bkl = NetBklGuard::new();
    let base_type = sock_type & 0xFF;
    let cloexec = sock_type & 0x80000 != 0;
    let nonblock = sock_type & 0x800 != 0;
    if domain != 2 || (base_type != 1 && base_type != 2) {
        crate::safe_print!(96, "[syscall] socket(domain={}, type=0x{:x}): unsupported\n", domain, sock_type);
        return EAFNOSUPPORT;
    }
    if let Some(idx) = socket::alloc_socket(base_type) {
        if let Some(proc) = akuma_exec::process::current_process_shared() {
            let fd = proc.alloc_fd(akuma_exec::process::FileDescriptor::Socket(idx));
            if cloexec {
                proc.set_cloexec(fd);
            }
            if nonblock {
                proc.set_nonblock(fd);
            }
            crate::safe_print!(96, "[syscall] socket(type={}) = fd {}\n", if base_type == 2 { "UDP" } else { "TCP" }, fd);
            return u64::from(fd);
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
    let proc = match akuma_exec::process::current_process_shared() {
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
    if write_user_val(sv_ptr, &fds).is_err() {
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

#[cfg(feature = "smoltcp")]
pub(super) fn sys_bind(fd: u32, addr_ptr: u64, len: usize) -> u64 {
    let _net_bkl = NetBklGuard::new();
    if len < 16 { return EINVAL; }
    if !validate_user_ptr(addr_ptr, len) { return EFAULT; }
    let mut sa = SockAddrIn::default();
    let copy_len = len.min(core::mem::size_of::<SockAddrIn>());
    if copy_from_user(
        &mut as_user_bytes_mut(core::slice::from_mut(&mut sa))[..copy_len],
        addr_ptr,
    )
    .is_err()
    {
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

#[cfg(feature = "smoltcp")]
pub(super) fn sys_listen(fd: u32, backlog: i32) -> u64 {
    let _net_bkl = NetBklGuard::new();
    let idx = match get_socket_from_fd(fd) {
        Some(i) => i,
        None => return EBADF,
    };
    match socket::socket_listen(idx, backlog as usize) {
        Ok(()) => 0,
        Err(e) => neg_errno(e),
    }
}

#[cfg(feature = "smoltcp")]
pub(super) fn sys_accept(fd: u32, addr_ptr: u64, len_ptr: u64) -> u64 {
    let _net_bkl = NetBklGuard::new();
    if addr_ptr != 0 && !validate_user_ptr(addr_ptr, 16) { return EFAULT; }
    if len_ptr != 0 && !validate_user_ptr(len_ptr, 4) { return EFAULT; }
    let idx = match get_socket_from_fd(fd) {
        Some(i) => i,
        None => return EBADF,
    };
    let nonblock = fd_is_nonblock(fd);
    match socket::socket_accept(idx, nonblock) {
        Ok((new_idx, addr)) => {
            let proc = match akuma_exec::process::current_process_shared() {
                Some(p) => p,
                None => return ESRCH,
            };
            if addr_ptr != 0 {
                let sa = SockAddrIn::from_addr(&addr);
                let _ = write_user_val(addr_ptr, &sa);
            }
            u64::from(proc.alloc_fd(akuma_exec::process::FileDescriptor::Socket(new_idx)))
        }
        Err(e) => neg_errno(e),
    }
}

#[cfg(feature = "smoltcp")]
pub(super) fn sys_accept4(fd: u32, addr_ptr: u64, len_ptr: u64, flags: u32) -> u64 {
    let _net_bkl = NetBklGuard::new();
    if addr_ptr != 0 && !validate_user_ptr(addr_ptr, 16) { return EFAULT; }
    if len_ptr != 0 && !validate_user_ptr(len_ptr, 4) { return EFAULT; }
    let idx = match get_socket_from_fd(fd) {
        Some(i) => i,
        None => return EBADF,
    };
    let nonblock = fd_is_nonblock(fd);
    match socket::socket_accept(idx, nonblock) {
        Ok((new_idx, addr)) => {
            let proc = match akuma_exec::process::current_process_shared() {
                Some(p) => p,
                None => return ESRCH,
            };
            if addr_ptr != 0 {
                let sa = SockAddrIn::from_addr(&addr);
                let _ = write_user_val(addr_ptr, &sa);
            }
            let new_fd = proc.alloc_fd(akuma_exec::process::FileDescriptor::Socket(new_idx));
            const SOCK_CLOEXEC: u32 = 0x80000;
            const SOCK_NONBLOCK: u32 = 0x800;
            if flags & SOCK_CLOEXEC != 0 { proc.set_cloexec(new_fd); }
            if flags & SOCK_NONBLOCK != 0 { proc.set_nonblock(new_fd); }
            u64::from(new_fd)
        }
        Err(e) => neg_errno(e),
    }
}

#[cfg(feature = "smoltcp")]
pub(super) fn sys_connect(fd: u32, addr_ptr: u64, len: usize) -> u64 {
    let _net_bkl = NetBklGuard::new();
    if len < 16 { return EINVAL; }
    if !validate_user_ptr(addr_ptr, len) { return EFAULT; }
    let mut sa = SockAddrIn::default();
    let copy_len = len.min(core::mem::size_of::<SockAddrIn>());
    if copy_from_user(
        &mut as_user_bytes_mut(core::slice::from_mut(&mut sa))[..copy_len],
        addr_ptr,
    )
    .is_err()
    {
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

#[cfg(feature = "smoltcp")]
pub(super) fn sys_getsockname(fd: u32, addr_ptr: u64, len_ptr: u64) -> u64 {
    let _net_bkl = NetBklGuard::new();
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
        if write_user_val(addr_ptr, &sa).is_err() {
            return EFAULT;
        }
        let out_len = core::mem::size_of::<SockAddrIn>() as u32;
        if write_user_val(len_ptr, &out_len).is_err() {
            return EFAULT;
        }
    }
    0
}

#[cfg(feature = "smoltcp")]
pub(super) fn sys_getpeername(fd: u32, addr_ptr: u64, len_ptr: u64) -> u64 {
    let _net_bkl = NetBklGuard::new();
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
                if write_user_val(addr_ptr, &sa).is_err() {
                    return EFAULT;
                }
                let out_len = core::mem::size_of::<SockAddrIn>() as u32;
                if write_user_val(len_ptr, &out_len).is_err() {
                    return EFAULT;
                }
            }
            0
        }
        None => neg_errno(libc_errno::ENOTCONN),
    }
}

#[cfg(feature = "smoltcp")]
pub(super) fn sys_sendto(fd: u32, buf_ptr: u64, len: usize, _flags: i32, dest_addr: u64, addr_len: usize) -> u64 {
    // AF_UNIX socketpair endpoint: send == write to the tx pipe. Checked BEFORE
    // dropping the BKL: the pipe/fs paths take spinlocks that BKL-holding peers
    // also take, which must not happen in the BKL-free window (AB-BA with a
    // nested IRQ's enter_kernel — see NetBklGuard).
    if fd_is_unix_socket(fd) {
        return super::fs::sys_write(u64::from(fd), buf_ptr, len);
    }
    let _net_bkl = NetBklGuard::new();
    if !validate_user_ptr(buf_ptr, len) { return EFAULT; }
    let mut kernel_buf = match alloc_net_bounce(len) {
        Some(b) => b,
        None => return ENOMEM,
    };
    let chunk_len = kernel_buf.len();
    if copy_from_user(&mut kernel_buf, buf_ptr).is_err() {
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
            if copy_from_user(
                &mut as_user_bytes_mut(core::slice::from_mut(&mut sa))[..sa_copy_len],
                dest_addr,
            )
            .is_err()
            {
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
            Ok(n) => {
                // A short write means the transmit buffer filled. Re-arm the
                // EPOLLET edge so the drain that follows counts as a fresh
                // EPOLLOUT — see `epoll_on_fd_write_blocked`.
                if n < buf.len() {
                    super::poll::epoll_on_fd_write_blocked(fd);
                }
                n as u64
            }
            Err(e) => {
                if e == libc_errno::EAGAIN {
                    super::poll::epoll_on_fd_write_blocked(fd);
                }
                neg_errno(e)
            }
        }
    }
}

#[cfg(feature = "smoltcp")]
pub(super) fn sys_recvfrom(fd: u32, buf_ptr: u64, len: usize, _flags: i32, src_addr: u64, addr_len_ptr: u64) -> u64 {
    // AF_UNIX socketpair endpoint: recv == read from the rx pipe. Checked BEFORE
    // dropping the BKL — see sys_sendto.
    if fd_is_unix_socket(fd) {
        return super::fs::sys_read(u64::from(fd), buf_ptr, len);
    }
    let _net_bkl = NetBklGuard::new();
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
                if copy_to_user(buf_ptr, &kernel_buf[..n]).is_err() {
                    return EFAULT;
                }
                if src_addr != 0 && addr_len_ptr != 0
                    && validate_user_ptr(src_addr, core::mem::size_of::<SockAddrIn>())
                        && validate_user_ptr(addr_len_ptr, core::mem::size_of::<u32>())
                    {
                        let sa = SockAddrIn::from_addr(&from);
                        let _ = write_user_val(src_addr, &sa);
                        let out_len = core::mem::size_of::<SockAddrIn>() as u32;
                        let _ = write_user_val(addr_len_ptr, &out_len);
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
                if copy_to_user(buf_ptr, &kernel_buf[..n]).is_err() {
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

// Rump-only build (no smoltcp): the native socket table is gone, but a UnixSocket
// (pipe-backed) fd still needs send/recv, which map to pipe write/read. This is
// how the box-0 `rump_server`'s fd-3 sysproxy channel works: rump_server is
// excluded from box interception, so its `send()`/`recv()` on fd 3 fall through to
// here — the sysproxy banner + reply frames flow through this path. Without it the
// handshake banner send fails and box 0's rump stack never comes up. A real AF_INET
// socket cannot exist without smoltcp, so anything else is EBADF.
#[cfg(not(feature = "smoltcp"))]
pub(super) fn sys_sendto(fd: u32, buf_ptr: u64, len: usize, _flags: i32, _dest_addr: u64, _addr_len: usize) -> u64 {
    if fd_is_unix_socket(fd) {
        return super::fs::sys_write(u64::from(fd), buf_ptr, len);
    }
    EBADF
}

#[cfg(not(feature = "smoltcp"))]
pub(super) fn sys_recvfrom(fd: u32, buf_ptr: u64, len: usize, _flags: i32, _src_addr: u64, _addr_len_ptr: u64) -> u64 {
    if fd_is_unix_socket(fd) {
        return super::fs::sys_read(u64::from(fd), buf_ptr, len);
    }
    EBADF
}

/// `shutdown(fd, how)` — half-close a TCP connection, keeping the fd.
///
/// A `return 0` stub until 2026-08-20, which made `SHUT_WR` a lie: no FIN ever
/// reached the peer. See [`akuma_net::socket::socket_shutdown`] for what that
/// cost (5 s per nginx request). Non-socket fds keep the old permissive
/// success — nothing in the tree half-closes a pipe, and failing them now would
/// be a new error where there has never been one.
#[cfg(feature = "smoltcp")]
pub(super) fn sys_shutdown(fd: u32, how: i32) -> u64 {
    let Some(idx) = get_socket_from_fd(fd) else { return 0 };
    match socket::socket_shutdown(idx, how) {
        Ok(()) => 0,
        Err(e) => neg_errno(e),
    }
}

#[cfg(not(feature = "smoltcp"))]
pub(super) fn sys_shutdown(_fd: u32, _how: i32) -> u64 { 0 }

/// AArch64 `struct timeval` — `{ time_t tv_sec; suseconds_t tv_usec; }`, both
/// 64-bit, 16 bytes total. musl passes this shape for `SO_RCVTIMEO`/`SO_SNDTIMEO`.
#[cfg(feature = "smoltcp")]
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Timeval {
    tv_sec: i64,
    tv_usec: i64,
}

/// Read a `struct timeval` option value and convert it to microseconds.
///
/// Returns `Ok(None)` for an all-zero timeval, because POSIX says zero means
/// "no timeout, block indefinitely" — NOT "time out immediately". Returns
/// `Err(())` for a malformed or unreadable value, which the caller reports as
/// `EINVAL`. The two nested "nothing"s mean different things, hence the
/// `Result` rather than an `Option<Option<_>>`.
#[cfg(feature = "smoltcp")]
#[allow(clippy::result_unit_err)]
fn read_timeval_us(optval: u64, optlen: u32) -> Result<Option<u64>, ()> {
    if optval == 0 || (optlen as usize) < core::mem::size_of::<Timeval>() {
        return Err(());
    }
    let mut tv = Timeval::default();
    if read_user_into(&mut tv, optval).is_err() {
        return Err(());
    }
    if tv.tv_sec < 0 || tv.tv_usec < 0 {
        return Err(());
    }
    let us = (tv.tv_sec as u64)
        .saturating_mul(1_000_000)
        .saturating_add(tv.tv_usec as u64);
    Ok(if us == 0 { None } else { Some(us) })
}

#[cfg(feature = "smoltcp")]
pub(super) fn sys_setsockopt(fd: u32, level: i32, optname: i32, optval: u64, optlen: u32) -> u64 {
    let _net_bkl = NetBklGuard::new();
    const SOL_SOCKET: i32 = 1;
    const IPPROTO_TCP: i32 = 6;
    const SO_REUSEADDR: i32 = 2;
    const SO_KEEPALIVE: i32 = 9;
    const SO_RCVBUF: i32 = 8;
    const SO_SNDBUF: i32 = 7;
    const SO_LINGER: i32 = 13;
    const SO_REUSEPORT: i32 = 15;
    const SO_RCVTIMEO: i32 = 20;
    const SO_SNDTIMEO: i32 = 21;
    const TCP_NODELAY: i32 = 1;
    const TCP_CORK: i32 = 3;
    const TCP_KEEPIDLE: i32 = 4;
    const TCP_KEEPINTVL: i32 = 5;
    const TCP_KEEPCNT: i32 = 6;

    // Read the value if provided
    let mut val: i32 = 0;
    if optval != 0 && optlen >= 4 && read_user_into(&mut val, optval).is_err() {
        return EFAULT;
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
                // Both used to fall through to the `_` arm below: accepted,
                // logged, and dropped. A client could not tell, because
                // `getsockopt` had no arm for them either — so a read that the
                // caller believed was bounded to 2 s was not bounded at all
                // (and then died at the kernel's own hidden 30 s cap).
                SO_RCVTIMEO | SO_SNDTIMEO => {
                    match read_timeval_us(optval, optlen) {
                        Ok(us) => {
                            socket::set_socket_timeout(idx, optname == SO_RCVTIMEO, us);
                            0
                        }
                        Err(()) => EINVAL,
                    }
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

#[cfg(feature = "smoltcp")]
pub(super) fn sys_getsockopt(fd: u32, level: i32, optname: i32, optval: u64, optlen: u64) -> u64 {
    let _net_bkl = NetBklGuard::new();
    const SOL_SOCKET: i32 = 1;
    const SO_ERROR: i32 = 4;
    const SO_SNDBUF: i32 = 7;
    const SO_RCVBUF: i32 = 8;
    const SO_KEEPALIVE: i32 = 9;
    const SO_TYPE: i32 = 3;
    const SO_RCVTIMEO: i32 = 20;
    const SO_SNDTIMEO: i32 = 21;

    if optval == 0 || optlen == 0 { return 0; }
    let mut len: u32 = 0;
    if read_user_into(&mut len, optlen).is_err() {
        return EFAULT;
    }

    // These two answer with a 16-byte `struct timeval`, not the 4-byte int
    // every other option here uses, so they are handled before the common
    // path. Without a readback a client cannot distinguish "honoured" from
    // "accepted and dropped" — which is how the missing SO_RCVTIMEO
    // implementation went unnoticed. Rust's `TcpStream::read_timeout()` is
    // exactly this call.
    if level == SOL_SOCKET && (optname == SO_RCVTIMEO || optname == SO_SNDTIMEO) {
        let tv_size = core::mem::size_of::<Timeval>();
        if (len as usize) < tv_size || !validate_user_ptr(optval, tv_size) {
            return EFAULT;
        }
        let us = get_socket_from_fd(fd)
            .and_then(|idx| socket::socket_timeout(idx, optname == SO_RCVTIMEO));
        // No timeout is reported as an all-zero timeval, matching how POSIX
        // spells "block indefinitely" on the way in.
        let tv = us.map_or_else(Timeval::default, |us| Timeval {
            tv_sec: (us / 1_000_000) as i64,
            tv_usec: (us % 1_000_000) as i64,
        });
        if write_user_val(optval, &tv).is_err() {
            return EFAULT;
        }
        let out_len = tv_size as u32;
        if write_user_val(optlen, &out_len).is_err() {
            return EFAULT;
        }
        return 0;
    }

    if (len as usize) < 4 || !validate_user_ptr(optval, 4) { return EFAULT; }

    let val: i32 = if level == SOL_SOCKET {
        match optname {
            SO_ERROR => {
                match get_socket_from_fd(fd) {
                    // A connect the kernel abandoned at `CONNECT_TIMEOUT_US`
                    // reports ETIMEDOUT, not the ECONNREFUSED that reading the
                    // bare `Closed` state below would produce. Checked first
                    // because it is the more specific answer for the same state.
                    Some(idx) if akuma_net::socket::take_connect_timed_out(idx) => {
                        libc_errno::ETIMEDOUT
                    }
                    Some(idx) => socket::with_socket(idx, |sock| {
                        if let socket::SocketType::Stream(h) = &sock.inner {
                            akuma_net::smoltcp_net::with_network(|net| {
                                let s = net.sockets.get::<smoltcp::socket::tcp::Socket>(*h);
                                if s.is_active() || s.may_send() { 0 }
                                else { libc_errno::ECONNREFUSED }
                            }).unwrap_or(0)
                        } else {
                            0
                        }
                    }).unwrap_or(0),
                    None => 0,
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

    // `SO_ERROR` is what a non-blocking connect's outcome is read through, so
    // its value and the socket state behind it are the two things any connect
    // investigation needs side by side.
    if crate::config::SYSCALL_DEBUG_NET_ENABLED && level == SOL_SOCKET && optname == SO_ERROR {
        let st = get_socket_from_fd(fd).map_or("no-fd", socket_tcp_state_str);
        crate::tprint!(96, "[soerr] fd={} val={} state={}\n", fd, val, st);
    }

    if write_user_val(optval, &val).is_err() {
        return EFAULT;
    }
    let out_len: u32 = 4;
    if write_user_val(optlen, &out_len).is_err() {
        return EFAULT;
    }
    0
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
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

#[cfg(feature = "smoltcp")]
pub(super) fn sys_sendmsg(fd: u32, msg_ptr: u64, _flags: i32) -> u64 {
    if !validate_user_ptr(msg_ptr, core::mem::size_of::<MsgHdr>()) { return EFAULT; }
    let mut msg = MsgHdr::default();
    if read_user_into(&mut msg, msg_ptr).is_err() {
        return EFAULT;
    }

    if msg.msg_iovlen == 0 { return 0; }
    let iov_size = msg.msg_iovlen as usize * core::mem::size_of::<super::fs::IoVec>();
    if !validate_user_ptr(msg.msg_iov, iov_size) { return EFAULT; }
    let mut iovs = alloc::vec![super::fs::IoVec { iov_base: 0, iov_len: 0 }; msg.msg_iovlen as usize];
    if copy_from_user(as_user_bytes_mut(&mut iovs), msg.msg_iov).is_err() {
        return EFAULT;
    }

    let iov = &iovs[0];
    if iov.iov_len == 0 { return 0; }
    if !validate_user_ptr(iov.iov_base, iov.iov_len as usize) { return EFAULT; }

    // AF_UNIX socketpair endpoint: sendmsg == write the first iovec to the tx
    // pipe. (libstd's handshake uses a single small message.) Handled BEFORE
    // dropping the BKL — the pipe paths must not run in the BKL-free window
    // (see sys_sendto).
    if fd_is_unix_socket(fd) {
        return super::fs::sys_write(u64::from(fd), iov.iov_base, iov.iov_len as usize);
    }

    let _net_bkl = NetBklGuard::new();

    let mut kernel_buf = match alloc_net_bounce(iov.iov_len) {
        Some(b) => b,
        None => return ENOMEM,
    };
    if copy_from_user(&mut kernel_buf, iov.iov_base).is_err() {
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
            let _ = copy_from_user(
                &mut as_user_bytes_mut(core::slice::from_mut(&mut sa))[..16],
                msg.msg_name,
            );
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
            Ok(n) => {
                // Short write == transmit buffer filled; re-arm the EPOLLOUT
                // edge (see `epoll_on_fd_write_blocked`).
                if n < kernel_buf.len() {
                    super::poll::epoll_on_fd_write_blocked(fd);
                }
                n as u64
            }
            Err(e) => {
                if e == libc_errno::EAGAIN {
                    super::poll::epoll_on_fd_write_blocked(fd);
                }
                neg_errno(e)
            }
        }
    }
}

// Rump-only build (no smoltcp): the box-0 `rump_server` replies to every sysproxy
// request via `dosend` → `sendmsg(MSG_NOSIGNAL)` on its fd-3 UnixSocket channel
// (only the initial banner uses plain `send`). rump_server is excluded from box
// interception, so those `sendmsg`s fall through to here. Write every iovec to the
// tx pipe so the full frame (header + payload) reaches the kernel; without this the
// handshake RESP — and all proxied-syscall replies — never arrive and box 0's rump
// stack never comes up. A real AF_INET socket cannot exist without smoltcp → EBADF.
#[cfg(not(feature = "smoltcp"))]
pub(super) fn sys_sendmsg(fd: u32, msg_ptr: u64, _flags: i32) -> u64 {
    if !fd_is_unix_socket(fd) {
        return EBADF;
    }
    if !validate_user_ptr(msg_ptr, core::mem::size_of::<MsgHdr>()) { return EFAULT; }
    let mut msg = MsgHdr::default();
    if read_user_into(&mut msg, msg_ptr).is_err() {
        return EFAULT;
    }
    if msg.msg_iovlen == 0 { return 0; }
    let iov_size = msg.msg_iovlen as usize * core::mem::size_of::<super::fs::IoVec>();
    if !validate_user_ptr(msg.msg_iov, iov_size) { return EFAULT; }
    let mut iovs = alloc::vec![super::fs::IoVec { iov_base: 0, iov_len: 0 }; msg.msg_iovlen as usize];
    if copy_from_user(as_user_bytes_mut(&mut iovs), msg.msg_iov).is_err() {
        return EFAULT;
    }
    // Coalesce ALL iovecs into a SINGLE pipe write so the reader (the rump
    // sysproxy client) is woken exactly once, with the complete reply frame.
    // Writing each iovec separately (header, then payload) makes `pipe_write`
    // fire a waker after the header; that SGI can preempt this server thread
    // mid-reply, so the client wakes on a partial frame, can't finish
    // `read_exact`, and blocks again — an extra ~10 ms-tick scheduler round trip
    // per reply (the `blk=.../2` seen in traces). One write = one wake = one
    // client block. The pipe is a byte stream and the client reads exactly
    // `rsp_len` bytes, so concatenation is wire-identical.
    // (See docs/archive/RUMP_SYSPROXY_LATENCY_FIX.md Phase 3q.)
    let tx = {
        let proc = match akuma_exec::process::current_process_shared() { Some(p) => p, None => return EBADF };
        match proc.get_fd(fd) {
            Some(akuma_exec::process::FileDescriptor::UnixSocket { tx, .. }) => tx,
            _ => return EBADF,
        }
    };
    let total_len: usize = iovs.iter().map(|v| v.iov_len).sum();
    if total_len == 0 { return 0; }
    let mut buf = alloc::vec![0u8; total_len];
    let mut off = 0usize;
    for iov in &iovs {
        let len = iov.iov_len;
        if len == 0 { continue; }
        if copy_from_user(&mut buf[off..off + len], iov.iov_base).is_err() {
            return EFAULT;
        }
        off += len;
    }
    // Write-all: the "one write = one complete frame" invariant above is what keeps the
    // client's `read_exact(rsp_len)` in sync, and pipes are capped at `PIPE_CAPACITY`, so
    // a plain `pipe_write` may accept only part of a large reply. Returning that short
    // count would leave the client waiting on bytes this server already discarded.
    match super::pipe::pipe_write_all_blocking(tx, &buf) {
        Ok(()) => total_len as u64,
        Err(e) => (-i64::from(e)) as u64,
    }
}

#[cfg(feature = "smoltcp")]
pub(super) fn sys_recvmsg(fd: u32, msg_ptr: u64, _flags: i32) -> u64 {
    if !validate_user_ptr(msg_ptr, core::mem::size_of::<MsgHdr>()) { return EFAULT; }
    let mut msg = MsgHdr::default();
    if read_user_into(&mut msg, msg_ptr).is_err() {
        return EFAULT;
    }

    if msg.msg_iovlen == 0 { return 0; }
    let iov_size = msg.msg_iovlen as usize * core::mem::size_of::<super::fs::IoVec>();
    if !validate_user_ptr(msg.msg_iov, iov_size) { return EFAULT; }
    let mut iovs = alloc::vec![super::fs::IoVec { iov_base: 0, iov_len: 0 }; msg.msg_iovlen as usize];
    if copy_from_user(as_user_bytes_mut(&mut iovs), msg.msg_iov).is_err() {
        return EFAULT;
    }

    let iov = &mut iovs[0];
    if iov.iov_len == 0 { return 0; }
    if !validate_user_ptr(iov.iov_base, iov.iov_len as usize) { return EFAULT; }

    // AF_UNIX socketpair endpoint: recvmsg == read into the first iovec from the
    // rx pipe. (libstd's handshake uses a single small message.) On success,
    // clear the ancillary/flags fields and write the header back.
    if fd_is_unix_socket(fd) {
        let n = super::fs::sys_read(u64::from(fd), iov.iov_base, iov.iov_len as usize);
        if (n as i64) >= 0 {
            msg.msg_controllen = 0;
            msg.msg_flags = 0;
            let _ = write_user_val(msg_ptr, &msg);
        }
        return n;
    }

    // Past the pipe-backed unix-socket branch: safe to drop the BKL (see sys_sendto).
    let _net_bkl = NetBklGuard::new();

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
                if copy_to_user(iov.iov_base, &kernel_buf[..n]).is_err() {
                    return EFAULT;
                }
                if msg.msg_name != 0 && msg.msg_namelen >= core::mem::size_of::<SockAddrIn>() as u32
                    && validate_user_ptr(msg.msg_name, core::mem::size_of::<SockAddrIn>()) {
                        let sa = SockAddrIn::from_addr(&from);
                        let _ = write_user_val(msg.msg_name, &sa);
                        msg.msg_namelen = core::mem::size_of::<SockAddrIn>() as u32;
                    }
                msg.msg_controllen = 0;
                msg.msg_flags = 0;
                // Copy msg back to user
                let _ = write_user_val(msg_ptr, &msg);
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
                if copy_to_user(iov.iov_base, &kernel_buf[..n]).is_err() {
                    return EFAULT;
                }
                msg.msg_controllen = 0;
                msg.msg_flags = 0;
                let _ = write_user_val(msg_ptr, &msg);
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
    let proc = akuma_exec::process::current_process_shared()?;
    if let Some(akuma_exec::process::FileDescriptor::Socket(idx)) = proc.get_fd(fd) { Some(idx) } else { None }
}

pub(super) fn fd_is_nonblock(fd: u32) -> bool {
    akuma_exec::process::current_process_shared().is_some_and(|p| p.is_nonblock(fd))
}

/// True if `fd` is one endpoint of an AF_UNIX socketpair (backed by two kernel
/// pipes, not a smoltcp `Socket`). The socket send/recv syscalls route these to
/// the backing pipes with plain read(2)/write(2) semantics — libstd's
/// `fork`+exec child-spawn handshake reads its `SOCK_SEQPACKET` socketpair via
/// `recvmsg`, which otherwise hit the `get_socket_from_fd` → `None` → `EBADF`
/// path and surfaced as `the CLOEXEC pipe failed: … Bad file descriptor`
/// (docs/RUST_TOOLCHAIN.md §4d).
pub(super) fn fd_is_unix_socket(fd: u32) -> bool {
    akuma_exec::process::current_process_shared().is_some_and(|p| {
        matches!(p.get_fd(fd), Some(akuma_exec::process::FileDescriptor::UnixSocket { .. }))
    })
}

#[cfg(feature = "smoltcp")]
pub(super) fn socket_get_udp_handle(idx: usize) -> Option<akuma_net::smoltcp_net::SocketHandle> {
    socket::with_socket(idx, |sock| {
        if let socket::SocketType::Datagram { handle, .. } = &sock.inner {
            Some(*handle)
        } else {
            None
        }
    }).flatten()
}

#[cfg(feature = "smoltcp")]
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

#[cfg(feature = "smoltcp")]
pub(super) fn socket_can_recv_tcp(idx: usize) -> bool {
    // A listening fd is readable when a connection is waiting to be accepted,
    // and asking that question is also what reaps backlog handles that died
    // before anyone accepted them — the maintenance that keeps the port
    // answering at all (`listener_refresh`). It has to run on the poller's path
    // and not just `accept`'s, because an event-driven server (nginx) never
    // calls `accept` until a poll tells it to. Called with no socket-table lock
    // held: `listener_ready` takes that lock itself.
    if let Some(ready) = akuma_net::socket::listener_ready(idx) {
        if crate::config::SYSCALL_DEBUG_EPOLL_EDGE {
            let (listening, pending, dead) =
                akuma_net::socket::listener_backlog_census(idx).unwrap_or((0, 0, 0));
            crate::tprint!(160, "[epoll-listener] idx={} ready={} backlog={}/{}/{}\n",
                idx, ready, listening, pending, dead);
        }
        return ready;
    }
    socket::with_socket(idx, |sock| {
        match &sock.inner {
            socket::SocketType::Stream(h) => {
                let was_connected = sock.was_connected;
                akuma_net::smoltcp_net::with_network(|net| {
                    let s = net.sockets.get::<smoltcp::socket::tcp::Socket>(*h);
                    // Report readable when:
                    //   - data is buffered (can_recv), OR
                    //   - the peer sent FIN on a connection that was UP, so that
                    //     recv() returns 0 (EOF) and the app can clean up.
                    //
                    // Do NOT use !is_active() here: a Closed smoltcp socket (e.g. after TCP
                    // timeout or RST) would permanently signal EPOLLIN even with no data,
                    // causing the caller to spin recv() → EAGAIN → epoll → EPOLLIN → ...
                    // Instead, a fully-dead socket is reported via EPOLLHUP in
                    // epoll_check_fd_readiness.
                    //
                    // The `is_active() && !may_recv()` pair this used to test was ALSO true
                    // in SynSent, so a socket mid-handshake was advertised as readable-at-EOF
                    // — see `tcp_reached_established`, which is what makes the difference.
                    akuma_net::socket::tcp_recv_ready(s.can_recv(), s.may_recv(), s.state(), was_connected)
                }).unwrap_or(false)
            }
            // Listeners are handled before the table lock is taken — see above.
            _ => false,
        }
    }).unwrap_or(false)
}

#[cfg(feature = "smoltcp")]
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

/// smoltcp TCP state as a static string, for `tprint!` — no allocation, so it is
/// safe on the console path (`docs/reference/subsystems/console.md`).
#[cfg(feature = "smoltcp")]
pub(super) fn socket_tcp_state_str(idx: usize) -> &'static str {
    socket::with_socket(idx, |sock| {
        if let socket::SocketType::Stream(h) = &sock.inner {
            akuma_net::smoltcp_net::with_network(|net| {
                use smoltcp::socket::tcp::State;
                match net.sockets.get::<smoltcp::socket::tcp::Socket>(*h).state() {
                    State::Closed => "Closed",
                    State::Listen => "Listen",
                    State::SynSent => "SynSent",
                    State::SynReceived => "SynReceived",
                    State::Established => "Established",
                    State::FinWait1 => "FinWait1",
                    State::FinWait2 => "FinWait2",
                    State::CloseWait => "CloseWait",
                    State::Closing => "Closing",
                    State::LastAck => "LastAck",
                    State::TimeWait => "TimeWait",
                }
            }).unwrap_or("no-net")
        } else {
            "not-stream"
        }
    }).unwrap_or("no-sock")
}

/// Returns true when the smoltcp socket is completely dead (Closed state).
/// Used to report EPOLLHUP so callers detect connection loss without spinning.
#[cfg(feature = "smoltcp")]
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
///
/// The `tcp_reached_established` guard is what keeps this from firing on a
/// socket that is still shaking hands: `may_recv()` is false in SynSent too, so
/// the bare `!may_recv()` this used to be announced "peer closed its write
/// half" on connections the peer had not yet accepted.
#[cfg(feature = "smoltcp")]
pub(super) fn socket_peer_closed_tcp(idx: usize) -> bool {
    socket::with_socket(idx, |sock| {
        if let socket::SocketType::Stream(h) = &sock.inner {
            akuma_net::smoltcp_net::with_network(|net| {
                let s = net.sockets.get::<smoltcp::socket::tcp::Socket>(*h);
                !s.may_recv() && akuma_net::socket::tcp_reached_established(s.state())
            }).unwrap_or(false)
        } else {
            false
        }
    }).unwrap_or(false)
}

#[cfg(feature = "smoltcp")]
pub(super) fn sys_resolve_host(path_ptr: u64, path_len: usize, res_ptr: u64) -> u64 {
    let _net_bkl = NetBklGuard::new();
    if !validate_user_ptr(path_ptr, path_len) { return EFAULT; }
    let mut kernel_path = alloc::vec![0u8; path_len];
    if copy_from_user(&mut kernel_path, path_ptr).is_err() {
        return EFAULT;
    }
    let host = core::str::from_utf8(&kernel_path).unwrap_or("");
    match akuma_net::dns::resolve_host_blocking(host) {
        Ok(ipv4) => {
            let octets = ipv4.octets();
            if copy_to_user(res_ptr, &octets).is_err() {
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

#[cfg(all(kernel_tests, feature = "smoltcp"))]
/// Regression for `SO_RCVTIMEO`/`SO_SNDTIMEO` being accepted and dropped.
///
/// Both used to fall through `sys_setsockopt`'s catch-all arm, which returns
/// success and does nothing, and `sys_getsockopt` had no arm for them at all —
/// so a client could not even detect the loss. Meanwhile the blocking paths
/// carried hardcoded caps (30 s recv, 5 s send), so a read the caller believed
/// was bounded to 2 s actually died at 30 s with ETIMEDOUT. Measured
/// 2026-08-17 with `nettest-std rcvtimeo`: readback NONE, fired at 30041 ms.
///
/// Checks the full round trip plus the two POSIX corners: the default is "no
/// timeout", and a zero timeval means "block indefinitely" — not "expire
/// immediately".
pub fn run_socket_timeout_tests() {
    use akuma_exec::process::{register_process, unregister_process, register_thread_pid, unregister_thread_pid, FileDescriptor};

    let _bypass = akuma_exec::mmu::user_access::BypassValidationGuard::new();

    const SOL_SOCKET: i32 = 1;
    const SO_RCVTIMEO: i32 = 20;
    const SO_SNDTIMEO: i32 = 21;

    #[repr(C)]
    #[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
    struct Timeval { tv_sec: i64, tv_usec: i64 }
    let tv_len = core::mem::size_of::<Timeval>() as u32;

    let tid = akuma_exec::threading::current_thread_id();
    let pid = 8032u32;
    register_process(pid, crate::process_tests::make_test_process(pid));
    register_thread_pid(tid, pid);

    let Some(idx) = socket::alloc_socket(socket::socket_const::SOCK_STREAM) else {
        unregister_thread_pid(tid);
        unregister_process(pid);
        crate::safe_print!(128, "[Test] socket_timeout_option_roundtrip SKIPPED (no socket slots)\n");
        return;
    };
    let proc = akuma_exec::process::current_process_shared().unwrap();
    let fd = proc.alloc_fd(FileDescriptor::Socket(idx));

    let mut readback = Timeval::default();
    let mut len_io: u32 = tv_len;

    // Default: no timeout, reported as an all-zero timeval.
    let rc_default = sys_getsockopt(
        fd, SOL_SOCKET, SO_RCVTIMEO,
        &raw mut readback as u64, &raw mut len_io as u64);
    let default_unset = rc_default == 0 && readback == Timeval::default()
        && socket::socket_timeout(idx, true).is_none();

    // 2.5 s round trip.
    let mut want = Timeval { tv_sec: 2, tv_usec: 500_000 };
    let rc_set = sys_setsockopt(
        fd, SOL_SOCKET, SO_RCVTIMEO, &raw mut want as u64, tv_len);
    readback = Timeval::default();
    len_io = tv_len;
    let rc_get = sys_getsockopt(
        fd, SOL_SOCKET, SO_RCVTIMEO, &raw mut readback as u64, &raw mut len_io as u64);
    let roundtrip = rc_set == 0 && rc_get == 0 && readback == want
        && socket::socket_timeout(idx, true) == Some(2_500_000);

    // A zero timeval means "block indefinitely" (POSIX), not "expire now".
    let mut zero = Timeval::default();
    let rc_zero = sys_setsockopt(
        fd, SOL_SOCKET, SO_RCVTIMEO, &raw mut zero as u64, tv_len);
    let zero_means_forever = rc_zero == 0 && socket::socket_timeout(idx, true).is_none();

    // Send side is stored independently of the receive side.
    let mut snd = Timeval { tv_sec: 7, tv_usec: 0 };
    let rc_snd = sys_setsockopt(
        fd, SOL_SOCKET, SO_SNDTIMEO, &raw mut snd as u64, tv_len);
    let send_independent = rc_snd == 0
        && socket::socket_timeout(idx, false) == Some(7_000_000)
        && socket::socket_timeout(idx, true).is_none();

    // `sys_close`, not a bare `remove_socket`: the fd must leave the table with
    // the socket reference it holds. Calling `remove_socket(idx)` directly and
    // leaving the fd behind drops the socket now and drops it AGAIN when the
    // test process is reaped — by which time the freed slot belongs to someone
    // else. That is not hypothetical: the first draft of this test did exactly
    // that and silently destroyed sshd's listener sockets, so every subsequent
    // connection was answered with a RST
    // (`kex_exchange_identification: read: Connection reset by peer`) while
    // sshd sat in a healthy accept loop. `KernelSocket::refs` exists for this
    // hazard; a test must not route around it.
    let _ = super::fs::sys_close(fd);
    unregister_thread_pid(tid);
    unregister_process(pid);

    if default_unset && roundtrip && zero_means_forever && send_independent {
        crate::safe_print!(
            160,
            "[Test] socket_timeout_option_roundtrip PASSED (SO_RCVTIMEO/SO_SNDTIMEO set+get, zero=forever, sides independent)\n"
        );
    } else {
        crate::safe_print!(
            192,
            "[Test] socket_timeout_option_roundtrip FAILED: default_unset={} roundtrip={} zero_forever={} send_independent={}\n",
            default_unset, roundtrip, zero_means_forever, send_independent
        );
        panic!("SO_RCVTIMEO/SO_SNDTIMEO regression");
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
#[cfg(kernel_tests)]
pub fn run_net_bounce_tests() {
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
