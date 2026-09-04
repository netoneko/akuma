//! The AF_INET socket syscalls.
//!
//! Wiring, not implementation: every operation below is one call into
//! `akuma_net::socket`, which owns the socket table, the smoltcp handles, the
//! blocking-wait policy and the backlog. That crate builds for
//! `x86_64-unknown-none` unchanged.
//!
//! # Descriptors
//!
//! A socket occupies a descriptor in [`crate::fd`]'s table, as
//! `FileDescriptor::Socket(idx)` — the same variant the AArch64 kernel uses,
//! carrying the same index into the same socket table. `read`/`write` on a
//! socket descriptor route here, which is what lets a program that was written
//! against `read(2)` work on a socket without knowing it has one.
//!
//! # Blocking
//!
//! Every call is made in **blocking** mode. `akuma-net`'s wait loop
//! (`akuma-net-yarn`) drives the poll from inside, and this target's
//! `NetRuntime::blocking_relax` is a `yield_now`, so a blocked socket hands the
//! CPU to the round-robin rather than spinning. `O_NONBLOCK` is not plumbed
//! through yet: there is no `fcntl`, so nothing can ask for it.

use akuma_net::socket::socket_const::{AF_INET, SOCK_DGRAM, SOCK_STREAM};
use akuma_net::socket::{SockAddrIn, SocketAddrV4};
use akuma_selftest::Suite;

use crate::fd::{self, errno};

/// `SOCK_STREAM`/`SOCK_DGRAM` are the low bits of `type`, which also carries
/// `SOCK_NONBLOCK` and `SOCK_CLOEXEC`.
const SOCK_TYPE_MASK: u64 = 0xf;

/// `socket(domain, type, protocol)`.
pub fn sys_socket(domain: u64, ty: u64, _protocol: u64) -> u64 {
    if domain != AF_INET as u64 {
        // AF_UNIX would be `akuma-net-unix`, which is a separate crate and a
        // separate table; refusing is honest rather than pretending.
        return errno::EAFNOSUPPORT;
    }
    let kind = match (ty & SOCK_TYPE_MASK) as i32 {
        SOCK_STREAM => SOCK_STREAM,
        SOCK_DGRAM => SOCK_DGRAM,
        _ => return errno::EINVAL,
    };
    let Some(idx) = akuma_net::socket::alloc_socket(kind) else {
        return errno::EMFILE;
    };
    let Some(fd) = fd::alloc_socket_fd(idx) else {
        akuma_net::socket::remove_socket(idx);
        return errno::EMFILE;
    };
    fd
}

/// Read a `struct sockaddr_in` from user memory.
///
/// The decode is `akuma_net::socket::SockAddrIn::to_addr` — the wire struct and
/// its byte-order handling already exist in the crate that owns sockets, and
/// this module hand-rolled both before checking. Getting the byte order wrong is
/// the classic way a bind lands on port 8080 instead of 80 (0x1F90 vs 0x901F),
/// and the existing version has been right about it for as long as the AArch64
/// kernel has served connections.
///
/// What is local is only the *copy* across the privilege boundary, which is
/// per-architecture (`akuma-user-access` is AArch64 asm).
fn sockaddr_in_from_user(ptr: u64, len: u64) -> Option<SocketAddrV4> {
    if len < core::mem::size_of::<SockAddrIn>() as u64 {
        return None;
    }
    let mut raw = [0u8; core::mem::size_of::<SockAddrIn>()];
    for (i, slot) in raw.iter_mut().enumerate() {
        // SAFETY: a user pointer, bounded to the struct's size, checked above.
        // Same contract as every other user access in this kernel — see `fd`.
        *slot = unsafe { (ptr as *const u8).add(i).read_volatile() };
    }
    // SAFETY: `SockAddrIn` is `repr(C)` and plain-old-data — four integer
    // fields and a padding array, no pointers and no niches — so any 16 bytes
    // are a valid value of it.
    let sa: SockAddrIn = unsafe { core::ptr::read_unaligned(raw.as_ptr().cast()) };
    if sa.sin_family != AF_INET as u16 {
        return None;
    }
    Some(sa.to_addr())
}

/// Write a `struct sockaddr_in` to user memory, returning its length.
fn sockaddr_in_to_user(ptr: u64, addr: SocketAddrV4) -> usize {
    if ptr == 0 {
        return 0;
    }
    let sa = SockAddrIn::from_addr(&addr);
    // SAFETY: `SockAddrIn` is `repr(C)` plain-old-data; viewing it as its own
    // bytes is well defined.
    let raw: &[u8] = unsafe {
        core::slice::from_raw_parts((&raw const sa).cast::<u8>(), core::mem::size_of::<SockAddrIn>())
    };
    for (i, b) in raw.iter().enumerate() {
        // SAFETY: a user pointer, as above.
        unsafe { (ptr as *mut u8).add(i).write_volatile(*b) };
    }
    raw.len()
}

/// Map `akuma-net`'s errno (a positive `i32`) to the kernel's negative return.
fn net_err(e: i32) -> u64 {
    (-i64::from(e)) as u64
}

/// `bind(fd, addr, addrlen)`.
pub fn sys_bind(fd: u64, addr: u64, addrlen: u64) -> u64 {
    let Some(idx) = fd::socket_index(fd) else {
        return errno::ENOTSOCK;
    };
    let Some(sa) = sockaddr_in_from_user(addr, addrlen) else {
        return errno::EINVAL;
    };
    match akuma_net::socket::socket_bind(idx, sa) {
        Ok(()) => 0,
        Err(e) => net_err(e),
    }
}

/// `listen(fd, backlog)`.
pub fn sys_listen(fd: u64, backlog: u64) -> u64 {
    let Some(idx) = fd::socket_index(fd) else {
        return errno::ENOTSOCK;
    };
    // A backlog of 0 is legal and means "one". Clamped rather than rejected,
    // which is what Linux does.
    let backlog = (backlog as usize).clamp(1, 128);
    match akuma_net::socket::socket_listen(idx, backlog) {
        Ok(()) => 0,
        Err(e) => net_err(e),
    }
}

/// `accept(fd, addr, addrlen)`.
///
/// Blocking. The new connection gets its own descriptor; the listener keeps
/// listening.
pub fn sys_accept(fd: u64, addr: u64, addrlen: u64) -> u64 {
    let Some(idx) = fd::socket_index(fd) else {
        return errno::ENOTSOCK;
    };
    match akuma_net::socket::socket_accept(idx, false) {
        Ok((new_idx, peer)) => {
            let Some(new_fd) = fd::alloc_socket_fd(new_idx) else {
                // No descriptor for the accepted connection. Closing it is the
                // only correct move: leaving it in the table would leak a
                // connection the caller can never reach or close.
                akuma_net::socket::remove_socket(new_idx);
                return errno::EMFILE;
            };
            let written = sockaddr_in_to_user(addr, peer);
            if addrlen != 0 {
                // SAFETY: a user pointer to a socklen_t, as the ABI defines it.
                unsafe { (addrlen as *mut u32).write_volatile(written as u32) };
            }
            new_fd
        }
        Err(e) => net_err(e),
    }
}

/// `connect(fd, addr, addrlen)`.
pub fn sys_connect(fd: u64, addr: u64, addrlen: u64) -> u64 {
    let Some(idx) = fd::socket_index(fd) else {
        return errno::ENOTSOCK;
    };
    let Some(sa) = sockaddr_in_from_user(addr, addrlen) else {
        return errno::EINVAL;
    };
    match akuma_net::socket::socket_connect(idx, sa, false) {
        Ok(()) => 0,
        Err(e) => net_err(e),
    }
}

/// Send on a socket descriptor. Reached from `write` as well as `sendto`.
pub fn send(idx: usize, buf: u64, len: u64) -> u64 {
    let data = fd::copy_in(buf, len);
    match akuma_net::socket::socket_send(idx, &data, false) {
        Ok(n) => n as u64,
        Err(e) => net_err(e),
    }
}

/// Receive on a socket descriptor. Reached from `read` as well as `recvfrom`.
pub fn recv(idx: usize, buf: u64, len: u64) -> u64 {
    let mut data = alloc::vec![0u8; len as usize];
    match akuma_net::socket::socket_recv(idx, &mut data, false) {
        Ok(n) => fd::copy_out(buf, &data[..n]) as u64,
        Err(e) => net_err(e),
    }
}

/// `sendto` / `recvfrom`, TCP-only: the address arguments are ignored, which is
/// what Linux does for a connected socket.
pub fn sys_sendto(fd: u64, buf: u64, len: u64) -> u64 {
    match fd::socket_index(fd) {
        Some(idx) => send(idx, buf, len),
        None => errno::ENOTSOCK,
    }
}

pub fn sys_recvfrom(fd: u64, buf: u64, len: u64) -> u64 {
    match fd::socket_index(fd) {
        Some(idx) => recv(idx, buf, len),
        None => errno::ENOTSOCK,
    }
}

/// `setsockopt` — accepted and mostly ignored.
///
/// Returning success for an option this kernel does not implement is the
/// deliberate choice: `SO_REUSEADDR` is set by every server before `bind`, and a
/// failure there makes `sshd` exit before it ever listens. The two that are
/// honoured are the two that change behaviour a caller can observe.
pub fn sys_setsockopt(fd: u64, level: u64, optname: u64, optval: u64, optlen: u64) -> u64 {
    const SOL_SOCKET: u64 = 1;
    const IPPROTO_TCP: u64 = 6;
    const SO_KEEPALIVE: u64 = 9;
    const TCP_NODELAY: u64 = 1;

    let Some(idx) = fd::socket_index(fd) else {
        return errno::ENOTSOCK;
    };
    let on = if optlen >= 4 && optval != 0 {
        // SAFETY: a user pointer to an int, bounded by the length check.
        unsafe { (optval as *const u32).read_volatile() != 0 }
    } else {
        false
    };
    match (level, optname) {
        (IPPROTO_TCP, TCP_NODELAY) => akuma_net::socket::set_tcp_nodelay(idx, on),
        (SOL_SOCKET, SO_KEEPALIVE) => akuma_net::socket::set_socket_keepalive(idx, on),
        _ => {}
    }
    0
}

/// Close a socket descriptor's underlying socket.
pub fn close(idx: usize) {
    akuma_net::socket::remove_socket(idx);
}

/// Prove the socket table works without needing a peer.
///
/// A loopback connection would be the better test and needs the netpoll loop to
/// be driven from somewhere; these check the operations that are pure table
/// work, which is where a wiring mistake would be.
pub fn smoke_test(t: &mut Suite, up: bool) {
    if !up {
        t.note("sock: no network stack; skipped", 0);
        return;
    }

    let fd = sys_socket(AF_INET as u64, SOCK_STREAM as u64, 0);
    if !t.check("sock: socket() returns a descriptor", fd < 0x8000_0000) {
        return;
    }

    // A bad family must be refused rather than treated as IPv4.
    t.check_eq("sock: a non-AF_INET family is refused", sys_socket(10, 1, 0), errno::EAFNOSUPPORT);
    t.check_eq("sock: a bad socket type is EINVAL", sys_socket(AF_INET as u64, 99, 0), errno::EINVAL);

    // bind to a port, then listen. The sockaddr goes through the same
    // user-memory path a real caller uses, byte order included.
    let want = SocketAddrV4::new([0, 0, 0, 0], 2222);
    let encoded = SockAddrIn::from_addr(&want);
    // SAFETY: `repr(C)` plain-old-data viewed as its own bytes.
    let sa: [u8; 16] = unsafe { core::mem::transmute(encoded) };
    let r = sys_bind(fd, sa.as_ptr() as u64, 16);
    t.check_eq("sock: bind to port 2222", r, 0);
    t.check_eq("sock: listen", sys_listen(fd, 8), 0);

    // The round trip through the sockaddr encoder must give the port back.
    let mut out = [0u8; 16];
    sockaddr_in_to_user(out.as_mut_ptr() as u64, SocketAddrV4::new([10, 0, 2, 15], 2222));
    t.check_eq(
        "sock: sockaddr_in round-trips the port in network byte order",
        u64::from(u16::from_be_bytes([out[2], out[3]])),
        2222,
    );

    // Operations on a descriptor that is not a socket must say so.
    t.check_eq("sock: bind on a non-socket is ENOTSOCK", sys_bind(1, sa.as_ptr() as u64, 16), errno::ENOTSOCK);

    t.check_eq("sock: close", fd::sys_close(fd), 0);
}
