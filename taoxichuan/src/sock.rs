//! Stage 4: the AF_INET socket syscalls.
//!
//! Wiring, not implementation: every operation is one call into
//! `akuma_net::socket`, which owns the socket table, the smoltcp handles,
//! the blocking-wait policy (`wait_until` — whose laps drive the poll that
//! keeps this no-interrupt guest alive) and the backlog. The same shape as
//! `amd64/src/sock.rs`, minus everything a one-core, no-BKL, no-epoll
//! kernel does not have: no `uaccess` (the physmap copy pair in `main.rs`
//! is this kernel's crossing), no edge re-arming (nothing registers edges),
//! and one fd table — the same fixed-slot one the file syscalls use, with
//! a `Socket(idx)` kind.
//!
//! # Descriptors
//!
//! A socket occupies one of the task's fd slots as `OpenFile::Socket(idx)`,
//! carrying the index into `akuma_net`'s socket table. `read`/`write` on a
//! socket fd route here, which is what lets a program written against
//! `read(2)` work on a socket without knowing it has one.
//!
//! # Blocking
//!
//! sshd drives everything non-blocking (`try_accept`, `O_NONBLOCK` streams,
//! the 1 ms serve tick). Blocking calls are still correct: they land in
//! `wait_until`, which pumps the stack itself. There is no third path.

use akuma_net::socket::socket_const::{AF_INET, SOCK_DGRAM, SOCK_STREAM};
use akuma_net::socket::{SockAddrIn, SocketAddrV4};

/// `SOCK_STREAM`/`SOCK_DGRAM` are the low bits of `type`, which also
/// carries `SOCK_NONBLOCK`/`SOCK_CLOEXEC`.
const SOCK_TYPE_MASK: u64 = 0xf;
const SOCK_NONBLOCK: u64 = 0o4000;
const SOCK_CLOEXEC: u64 = 0o2000000;

/// Map `akuma-net`'s errno (a positive `i32`) to the kernel's negative
/// return — the same convention as every other arm.
fn net_err(e: i32) -> u64 {
    (-i64::from(e)) as u64
}

/// The fd's socket index, or `ENOTSOCK`.
fn fd_socket(fd: u64) -> Option<usize> {
    fd_table().with(fd as u32, |d| match d {
        crate::OpenFile::Socket(idx) => Some(*idx),
        _ => None,
    })
    .flatten()
}

use crate::fd_table;

/// The fd's nonblock flag, straight from the fd table.
fn fd_nonblock(fd: u64) -> bool {
    fd_table().is_nonblock(fd as u32)
}

/// Read a `struct sockaddr_in` from the task's memory.
fn sockaddr_in_from_user(ptr: u64, len: u64) -> Option<SocketAddrV4> {
    if len < core::mem::size_of::<SockAddrIn>() as u64 {
        return None;
    }
    let mut raw = [0u8; core::mem::size_of::<SockAddrIn>()];
    if !crate::copy_from_user(&mut raw, ptr) {
        return None;
    }
    // SAFETY: `SockAddrIn` is `repr(C)` plain-old-data — four integer fields
    // and a padding array, no pointers, no niches — so any 16 bytes are a
    // valid value of it.
    let sa: SockAddrIn = unsafe { core::ptr::read_unaligned(raw.as_ptr().cast()) };
    if sa.sin_family != AF_INET as u16 {
        return None;
    }
    Some(sa.to_addr())
}

/// Write a `struct sockaddr_in` to the task's memory; returns its length.
fn sockaddr_in_to_user(ptr: u64, addr: SocketAddrV4) -> usize {
    if ptr == 0 {
        return 0;
    }
    let sa = SockAddrIn::from_addr(&addr);
    // SAFETY: `repr(C)` plain-old-data; viewing it as its own bytes is well
    // defined.
    let raw: &[u8] = unsafe {
        core::slice::from_raw_parts(
            core::ptr::addr_of!(sa).cast::<u8>(),
            core::mem::size_of::<SockAddrIn>(),
        )
    };
    if !crate::copy_to_user(ptr, raw) {
        return 0;
    }
    raw.len()
}

/// `socket(domain, type, protocol)`. The flag bits in `type` are applied,
/// not masked off — musl opens its sockets `|SOCK_NONBLOCK|SOCK_CLOEXEC`
/// and expects the flags to stick (the DNS lesson `amd64/src/sock.rs`
/// records; the same lesson, one smaller kernel).
pub fn socket_arm(domain: u64, ty: u64, _protocol: u64) -> u64 {
    if domain != AF_INET as u64 {
        return crate::e(crate::err::EAFNOSUPPORT);
    }
    let kind = match (ty & SOCK_TYPE_MASK) as i32 {
        SOCK_STREAM => SOCK_STREAM,
        SOCK_DGRAM => SOCK_DGRAM,
        _ => return crate::e(crate::err::EINVAL),
    };
    let Some(idx) = akuma_net::socket::alloc_socket(kind) else {
        return crate::e(crate::err::EMFILE);
    };
    let mut table = crate::fd_table();
    let Some(_probe) = table.free_below(3, crate::SLOT_CEILING) else {
        akuma_net::socket::remove_socket(idx);
        return crate::e(crate::err::EMFILE);
    };
    let fd = table.alloc_fd_from(3, crate::OpenFile::Socket(idx));
    if ty & SOCK_NONBLOCK != 0 {
        table.set_nonblock(fd);
    }
    if ty & SOCK_CLOEXEC != 0 {
        table.set_cloexec(fd);
    }
    u64::from(fd)
}

/// `bind(fd, addr, addrlen)`.
pub fn bind_arm(fd: u64, addr: u64, addrlen: u64) -> u64 {
    let Some(idx) = fd_socket(fd) else {
        return crate::e(crate::err::ENOTSOCK);
    };
    let Some(sa) = sockaddr_in_from_user(addr, addrlen) else {
        return crate::e(crate::err::EINVAL);
    };
    match akuma_net::socket::socket_bind(idx, sa) {
        Ok(()) => 0,
        Err(e) => net_err(e),
    }
}

/// `listen(fd, backlog)`. A backlog of 0 is legal and means "one", clamped
/// as Linux clamps it.
pub fn listen_arm(fd: u64, backlog: u64) -> u64 {
    let Some(idx) = fd_socket(fd) else {
        return crate::e(crate::err::ENOTSOCK);
    };
    let backlog = (backlog as usize).clamp(1, 8);
    match akuma_net::socket::socket_listen(idx, backlog) {
        Ok(()) => 0,
        Err(e) => net_err(e),
    }
}

/// `accept(fd, addr, addrlen)` — blocking or not, by the fd's flag. The
/// accepted connection gets its own descriptor; the listener keeps
/// listening. A connection with no free fd slot is closed, not leaked.
pub fn accept_arm(fd: u64, addr: u64, addrlen_va: u64) -> u64 {
    let Some(idx) = fd_socket(fd) else {
        return crate::e(crate::err::ENOTSOCK);
    };
    match akuma_net::socket::socket_accept(idx, fd_nonblock(fd)) {
        Ok((new_idx, peer)) => {
            let mut table = crate::fd_table();
            let Some(_probe) = table.free_below(3, crate::SLOT_CEILING) else {
                akuma_net::socket::remove_socket(new_idx);
                return crate::e(crate::err::EMFILE);
            };
            let new_fd = table.alloc_fd_from(3, crate::OpenFile::Socket(new_idx));
            // The listener's nonblock flag should carry to the child — the
            // same inheritance Linux does with accept4 on a nonblocking
            // listener, and what sshd relies on not to set per-connection.
            if fd_nonblock(fd) {
                table.set_nonblock(new_fd);
            }
            if addrlen_va != 0 && crate::user_ok(addrlen_va, 4) {
                let _ = crate::copy_to_user(addrlen_va, &(16u32).to_le_bytes());
            }
            let written = sockaddr_in_to_user(addr, peer);
            if addr != 0 && written == 0 {
                // A bad address pointer loses the address, not the
                // connection — the lesson `amd64/src/sock.rs` records from
                // the SMAP sweep.
            }
            u64::from(new_fd)
        }
        Err(e) => net_err(e),
    }
}

/// `connect(fd, addr, addrlen)`. Outbound TCP leaves through the gateway's
/// `vm_outbound` relay: the guest dials the address it is given (the host's
/// LAN address is the proven path) and the host makes the real connection.
pub fn connect_arm(fd: u64, addr: u64, addrlen: u64) -> u64 {
    let Some(idx) = fd_socket(fd) else {
        return crate::e(crate::err::ENOTSOCK);
    };
    let Some(sa) = sockaddr_in_from_user(addr, addrlen) else {
        return crate::e(crate::err::EINVAL);
    };
    // Dial non-blocking, then wait HERE, in the kernel, with a visible trace.
    // The single-task guest's blocking wait (`wait_until` inside akuma-net)
    // is opaque from the console; this loop prints the smoltcp socket state
    // once a second until the handshake lands or dies. `EINPROGRESS` is the
    // x86_64 value akuma-net answers for a non-blocking dial in flight.
    const EINPROGRESS: i32 = 115;
    akuma_primitives::safe_print!(
        96,
        "  [cx] connect_arm fd={} ip={}.{}.{}.{}:{}\n",
        fd,
        sa.ip[0], sa.ip[1], sa.ip[2], sa.ip[3], sa.port
    );
    let r = akuma_net::socket::socket_connect(idx, sa, true);
    let rc = match r {
        Ok(()) => 0i64,
        Err(e) => -i64::from(e),
    };
    akuma_primitives::safe_print!(96, "  [cx] dial -> {}\n", rc as u64);
    match r {
        Ok(()) => 0,
        Err(EINPROGRESS) => connect_wait(idx),
        Err(e) => net_err(e),
    }
}

/// Poll a dial to completion, tracing the socket's smoltcp state once a
/// second: state, remote endpoint, queue depths, and how much `poll()`
/// actually moved (the poll counter's delta). A lap that never prints a
/// changing state with a rising counter means the SYN is being emitted into
/// a void; a frozen clock or a flat counter means the poll itself is stuck.
fn connect_wait(idx: usize) -> u64 {
    let start = crate::net::uptime_us();
    let mut printed = start;
    akuma_primitives::safe_print!(96, "  [cx] wait enter idx={} t0={}\n", idx, start);
    loop {
        crate::net::netpoll();
        // One line per lap: a missing follow-up pins the hang inside
        // `netpoll`; a frozen `t=` pins it in the clock; silence after one
        // `polled` pins it inside the peek's table/NETWORK locks.
        akuma_primitives::safe_print!(96, "  [cx] polled t={}\n", crate::net::uptime_us().saturating_sub(start));
        let now = crate::net::uptime_us();
        match akuma_net::socket::debug_stream_peek(idx) {
            Some(d) if d.completed => {
                akuma_primitives::safe_print!(96, "  [cx] established t={}\n", now.saturating_sub(start));
                return 0;
            }
            Some(d) if d.dead => {
                akuma_primitives::safe_print!(96, "  [cx] dead {} t={}\n", d.state, now.saturating_sub(start));
                return crate::e(crate::err::ECONNREFUSED);
            }
            Some(d) => {
                if now.saturating_sub(printed) >= 1_000_000 {
                    printed = now;
                    let polls = akuma_net::smoltcp_net::poll_count();
                    let (ip, port) = d.remote.unwrap_or(([0, 0, 0, 0], 0));
                    akuma_primitives::safe_print!(
                        96,
                        "  [cx] t={}s state={} remote={}.{}.{}.{}:{} q={}/{} polls={}\n",
                        now.saturating_sub(start) / 1_000_000,
                        d.state,
                        ip[0], ip[1], ip[2], ip[3],
                        port,
                        d.send_q, d.recv_q,
                        polls
                    );
                }
            }
            None => {
                akuma_primitives::safe_print!(96, "  [cx] peek None\n");
                return crate::e(crate::err::EBADF);
            }
        }
        if now.saturating_sub(start) >= 15_000_000 {
            return crate::e(crate::err::ETIMEDOUT);
        }
        core::hint::spin_loop();
    }
}

/// `sendto(fd, buf, len, flags, dest_addr, addrlen)`. TCP ignores the
/// destination; UDP addresses the datagram with it.
pub fn sendto_arm(fd: u64, buf: u64, len: u64, _flags: u64, dest: u64, dest_len: u64) -> u64 {
    const WRITE_LIMIT: u64 = crate::WRITE_LIMIT;
    let Some(idx) = fd_socket(fd) else {
        return crate::e(crate::err::ENOTSOCK);
    };
    if len > WRITE_LIMIT || !crate::user_ok(buf, len) {
        return crate::e(crate::err::EFAULT);
    }
    let mut data = alloc::vec![0u8; len as usize];
    if !crate::copy_from_user(&mut data, buf) {
        return crate::e(crate::err::EFAULT);
    }
    let nonblock = fd_nonblock(fd);
    // Stage-7 probe: a TCP send whose payload opens an HTTP request.
    if !akuma_net::socket::is_udp_socket(idx)
        && len >= 4
        && data[0] == b'G'
        && data[1] == b'E'
        && data[2] == b'T'
        && data[3] == b' '
    {
        akuma_primitives::safe_print!(96, "  [cx] GET len={} ->\n", len);
    }
    let r = if akuma_net::socket::is_udp_socket(idx) {
        let Some(sa) = sockaddr_in_from_user(dest, dest_len) else {
            return crate::e(crate::err::EINVAL);
        };
        akuma_net::socket::socket_send_udp(idx, &data, sa)
    } else {
        akuma_net::socket::socket_send(idx, &data, nonblock)
    };
    if data.starts_with(b"GET ") {
        akuma_primitives::safe_print!(
            96,
            "  [cx] GET <- {}\n",
            match r {
                Ok(n) => n as u64,
                Err(e) => -i64::from(e) as u64,
            }
        );
    }
    match r {
        Ok(n) => n as u64,
        Err(e) => net_err(e),
    }
}

/// `recvfrom(fd, buf, len, flags, src_addr, addrlen)`. The source address
/// is filled for UDP and skipped for TCP (which is what the caller asked
/// for by passing a null pointer).
pub fn recvfrom_arm(fd: u64, buf: u64, len: u64, _flags: u64, src: u64, src_len_va: u64) -> u64 {
    const WRITE_LIMIT: u64 = crate::WRITE_LIMIT;
    let Some(idx) = fd_socket(fd) else {
        return crate::e(crate::err::ENOTSOCK);
    };
    if len > WRITE_LIMIT || !crate::user_ok(buf, len) {
        return crate::e(crate::err::EFAULT);
    }
    let mut data = alloc::vec![0u8; len as usize];
    let nonblock = fd_nonblock(fd);
    let r = if akuma_net::socket::is_udp_socket(idx) {
        let mut sa = SocketAddrV4::new([0; 4], 0);
        match akuma_net::socket::socket_recv_udp(idx, &mut data, nonblock) {
            Ok((n, peer)) => {
                sa = peer;
                Ok(n)
            }
            Err(e) => Err(e),
        }
        .inspect(|_| {
            if src != 0 {
                sockaddr_in_to_user(src, sa);
            }
            if src_len_va != 0 && crate::user_ok(src_len_va, 4) {
                let _ = crate::copy_to_user(src_len_va, &(16u32).to_le_bytes());
            }
        })
    } else {
        akuma_net::socket::socket_recv(idx, &mut data, nonblock)
    };
    match r {
        Ok(n) => {
            if !crate::copy_to_user(buf, &data[..n]) {
                return crate::e(crate::err::EFAULT);
            }
            n as u64
        }
        Err(e) => net_err(e),
    }
}

/// `shutdown(fd, how)`.
pub fn shutdown_arm(fd: u64, how: u64) -> u64 {
    let Some(idx) = fd_socket(fd) else {
        return crate::e(crate::err::ENOTSOCK);
    };
    match akuma_net::socket::socket_shutdown(idx, how as i32) {
        Ok(()) => 0,
        Err(e) => net_err(e),
    }
}

/// `read(2)` on a socket fd — the same path `recvfrom` serves, flags 0.
pub fn read_arm(fd: u64, buf: u64, len: u64) -> u64 {
    const WRITE_LIMIT: u64 = crate::WRITE_LIMIT;
    let Some(idx) = fd_socket(fd) else {
        return crate::e(crate::err::ENOTSOCK);
    };
    if len > WRITE_LIMIT || !crate::user_ok(buf, len) {
        return crate::e(crate::err::EFAULT);
    }
    let mut data = alloc::vec![0u8; len as usize];
    match akuma_net::socket::socket_recv(idx, &mut data, fd_nonblock(fd)) {
        Ok(n) => {
            if !crate::copy_to_user(buf, &data[..n]) {
                return crate::e(crate::err::EFAULT);
            }
            n as u64
        }
        Err(e) => net_err(e),
    }
}

/// `write(2)` on a socket fd.
pub fn write_arm(fd: u64, buf: u64, len: u64) -> u64 {
    const WRITE_LIMIT: u64 = crate::WRITE_LIMIT;
    let Some(idx) = fd_socket(fd) else {
        return crate::e(crate::err::ENOTSOCK);
    };
    if len > WRITE_LIMIT || !crate::user_ok(buf, len) {
        return crate::e(crate::err::EFAULT);
    }
    let mut data = alloc::vec![0u8; len as usize];
    if !crate::copy_from_user(&mut data, buf) {
        return crate::e(crate::err::EFAULT);
    }
    // Stage-7 probe: the write(2) flavour of the HTTP request send.
    if data.starts_with(b"GET ") {
        akuma_primitives::safe_print!(96, "  [cx] GET(len={}) write ->\n", len);
    }
    let r = akuma_net::socket::socket_send(idx, &data, fd_nonblock(fd));
    if data.starts_with(b"GET ") {
        akuma_primitives::safe_print!(
            96,
            "  [cx] GET <- {}\n",
            match r {
                Ok(n) => n as u64,
                Err(e) => -i64::from(e) as u64,
            }
        );
    }
    match r {
        Ok(n) => n as u64,
        Err(e) => net_err(e),
    }
}

/// The fd-table close path for a socket: release the kernel side (the
/// socket table owns the smoltcp handle; `remove_socket` drops it), then
/// the slot itself.
pub fn close_arm(fd: u64) {
    let idx = fd_socket(fd);
    if let Some(idx) = idx {
        akuma_net::socket::remove_socket(idx);
    }
}
