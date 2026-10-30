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
pub fn fd_socket(fd: u64) -> Option<usize> {
    fd_table().with(fd as u32, |d| match d {
        crate::OpenFile::Socket(idx) => Some(*idx),
        _ => None,
    })
    .flatten()
}

/// `writev` on a socket fd: one send of the concatenated iovecs. The syscall
/// return is already shaped (`net_err`'s negative errno on failure) because
/// the dispatcher returns it directly.
pub fn writev_socket(idx: usize, data: &[u8], nonblock: bool) -> Result<usize, u64> {
    if data.is_empty() {
        return Ok(0);
    }
    // A datagram socket's writev goes to its connect(2) peer — the same
    // contract as write(2) on a dgram fd. Routing it into `socket_send`
    // (TCP) answered a silent errno to any writev-on-UDP caller (stage 8,
    // session Q2; NOT yet proven to be c-ares's path — probes showed no
    // writev during the failing window — kept as a correctness fix).
    let r = if akuma_net::socket::is_udp_socket(idx) {
        match akuma_net::socket::socket_udp_peer(idx) {
            Some(sa) => akuma_net::socket::socket_send_udp(idx, data, sa),
            None => Err(crate::err::ENOTCONN as i32),
        }
    } else {
        akuma_net::socket::socket_send(idx, data, nonblock)
    };
    match r {
        Ok(n) => Ok(n),
        Err(e) => Err(net_err(e)),
    }
}

use crate::fd_table;

/// `poll(fds, nfds, timeout_ms)`. The readiness wait musl's resolver and the
/// busybox applets block on before their `recv`. Sockets ask akuma-net's
/// per-protocol probes (`socket_udp_recv_ready`, `socket_tcp_ready`); regular
/// files and directories are always ready both ways; the console and the
/// child pipe are write-only; an unbound number is POLLNVAL, the way Linux
/// answers a bad fd. Laps pump the NIC (`netpoll`) so a datagram that lands
/// while polling is visible to the next probe. (stage 8, M2)
pub fn poll_arm(fds_va: u64, nfds: u64, timeout_ms: i64) -> u64 {
    if nfds == 0 || nfds > 64 {
        return crate::e(crate::err::EINVAL);
    }
    let mut fds = alloc::vec![0u8; (nfds as usize) * 8];
    if !crate::copy_from_user(&mut fds, fds_va) {
        return crate::e(crate::err::EFAULT);
    }
    let now = crate::net::uptime_us();
    let deadline = if timeout_ms < 0 {
        now + 60_000_000 // unbounded asks cap at a minute; nobody waits longer
    } else {
        now.saturating_add(timeout_ms as u64 * 1000)
    };
    loop {
        let mut ready = 0u64;
        for i in 0..nfds as usize {
            let o = i * 8;
            let fd = i32::from_le_bytes(fds[o..o + 4].try_into().expect("8-byte pollfd"));
            let events = i16::from_le_bytes(fds[o + 4..o + 6].try_into().expect("8-byte pollfd"));
            let revents = fd_events(fd, events);
            if revents != 0 {
                ready += 1;
            }
            fds[o + 6..o + 8].copy_from_slice(&revents.to_le_bytes());
        }
        if ready > 0 || crate::net::uptime_us() >= deadline {
            if !crate::copy_to_user(fds_va, &fds) {
                return crate::e(crate::err::EFAULT);
            }
            return ready;
        }
        let slot = crate::exec_twin::current_slot();
        if slot < crate::child::NSLOTS && akuma_threading::take_thread_interrupted(slot) {
            return crate::e(crate::EINTR);
        }
        crate::net::sleep_until(crate::net::uptime_us() + 1000);
    }
}

/// The one readiness probe poll and select share: what `revents` does this fd
/// answer, given the events the caller asked about? Sockets ask akuma-net's
/// per-protocol probes (`socket_udp_recv_ready`, `socket_tcp_ready`); regular
/// files and directories are always ready both ways; the console and the
/// child pipe are write-only; an unbound number is POLLNVAL, the way Linux
/// answers a bad fd. Extracted for stage 8 (M3)'s `select` arm — same
/// answers, one table to keep true.
fn fd_events(fd: i32, events: i16) -> i16 {
    const POLLIN: i16 = 0x0001;
    const POLLOUT: i16 = 0x0004;
    const POLLNVAL: i16 = 0x0020;
    if fd < 0 {
        return 0;
    }
    let mut efd_idx: usize = usize::MAX;
    let mut pipe_rev: i16 = 0;
    let kind = crate::fd_table().with(fd as u32, |d| match d {
        crate::OpenFile::Socket(_) => 1,
        crate::OpenFile::File { .. } | crate::OpenFile::Dir { .. } => 2,
        crate::OpenFile::Null | crate::OpenFile::Random | crate::OpenFile::ChildOut(_) => 3,
        // A pipe end (M3): a read end is POLLIN while data or a finished
        // writer's EOF stands; a write end is POLLOUT while there is room.
        // The crate's own predicates — no second opinion, and no poller
        // registered (nothing on this kernel blocks).
        crate::OpenFile::Pipe(id, is_write) => {
            let ready = if *is_write {
                crate::pipes::writable(*id)
            } else {
                crate::pipes::readable(*id)
            };
            // The readiness rides OUT of the closure in `pipe_rev`: this arm
            // used to return POLLIN/POLLOUT as the KIND, and POLLOUT (4) is
            // the eventfd kind - polling a pipe's write end indexed the
            // eventfd table with usize::MAX and panicked the kernel (found
            // when rustc polled its jobserver pipe, 2026-10-30).
            pipe_rev = if !ready {
                0
            } else if *is_write {
                POLLOUT
            } else {
                POLLIN
            };
            5
        }
        crate::OpenFile::Pair(rd, wr) => {
            pipe_rev = (if crate::pipes::readable(*rd) { POLLIN } else { 0 })
                | (if crate::pipes::writable(*wr) { POLLOUT } else { 0 });
            5
        }
        crate::OpenFile::Session => 3, // the session stdio: always ready
        crate::OpenFile::EventFd(slot) => {
            efd_idx = *slot;
            4
        }
    });
    match kind {
        Some(1) => {
            if let Some(idx) = fd_socket(fd as u64) {
                let (rin, rout) = if akuma_net::socket::is_udp_socket(idx) {
                    (akuma_net::socket::socket_udp_recv_ready(idx), true)
                } else {
                    akuma_net::socket::socket_tcp_ready(idx)
                };
                let mut revents: i16 = 0;
                if rin && events & POLLIN != 0 {
                    revents |= POLLIN;
                }
                if rout && events & POLLOUT != 0 {
                    revents |= POLLOUT;
                }
                revents
            } else {
                POLLNVAL
            }
        }
        Some(2) => POLLIN | POLLOUT,
        Some(3) => events & POLLOUT,
        Some(5) => pipe_rev & events,
        Some(4) => {
            // An eventfd (session Q): readable while its counter is
            // non-zero, always writable — `FdState::EventFd`'s answers,
            // akuma-syscalls-poll's readiness vocabulary.
            let mut revents: i16 = 0;
            if crate::eventfd_can_read(efd_idx) && events & POLLIN != 0 {
                revents |= POLLIN;
            }
            if events & POLLOUT != 0 {
                revents |= POLLOUT;
            }
            revents
        }
        _ => {
            // Unbound: fd 1/2 are the console (write-only), the rest are bad.
            if fd <= 2 && events & POLLOUT != 0 {
                POLLOUT
            } else if fd > 2 {
                POLLNVAL
            } else {
                0
            }
        }
    }
}

/// `select(nfds, readfds, writefds, exceptfds, timeout)`. The readiness wait
/// apk's own fetcher blocks in — x86_64 musl's `select()` issues 23 directly
/// (`src/select/select.c`'s `#ifdef SYS_select` branch), where busybox wget
/// only ever polled. The fd-set bit arithmetic, the count-both-directions
/// return and the overwrite report are `akuma-syscalls-poll::fdset`'s
/// (host-tested; its crate doc records the received-but-never-written
/// exceptfds set that once killed every `cargo fetch` in this tree); this
/// arm only copies user memory, probes readiness through the same
/// `fd_events` the poll arm answers from, and laps the NIC on the same 1 ms
/// cadence. `exceptfds` comes back all-zero on purpose: this stack has no
/// out-of-band data, and leaving a received set unwritten is the exact bug
/// the crate doc warns about. Bad fds are silently skipped (Linux select has
/// no POLLNVAL to report); a NULL timeout is unbounded, capped at a minute
/// like poll's -1. (stage 8, M3)
pub fn select_arm(nfds: u64, read_va: u64, write_va: u64, except_va: u64, tv_va: u64) -> u64 {
    use akuma_syscalls_poll::fdset;
    const POLLNVAL: i16 = 0x0020;
    let nfds = nfds as usize;
    if !fdset::nfds_ok(nfds) {
        return crate::e(crate::err::EINVAL);
    }
    // Copy the caller's words in (whole words, the tail word partial —
    // fdset::bytes bounds the copy to what nfds covers). NULL set = empty.
    let mut ins = ([0u64; fdset::MAX_WORDS], [0u64; fdset::MAX_WORDS]);
    for (va, words) in [(read_va, &mut ins.0), (write_va, &mut ins.1)] {
        if va != 0 {
            let mut buf = [0u8; fdset::MAX_WORDS * 8];
            if !crate::copy_from_user(&mut buf[..fdset::bytes(nfds)], va) {
                return crate::e(crate::err::EFAULT);
            }
            for (w, word) in words.iter_mut().enumerate().take(fdset::words(nfds)) {
                *word = u64::from_le_bytes(buf[w * 8..w * 8 + 8].try_into().expect("word"));
            }
        }
    }
    let now = crate::net::uptime_us();
    let deadline = if tv_va == 0 {
        now + 60_000_000
    } else {
        // `struct timeval { i64 tv_sec, i64 tv_usec }` — 16 bytes.
        let mut tv = [0u8; 16];
        if !crate::copy_from_user(&mut tv, tv_va) {
            return crate::e(crate::err::EFAULT);
        }
        let sec = i64::from_le_bytes(tv[0..8].try_into().expect("tv_sec"));
        let usec = i64::from_le_bytes(tv[8..16].try_into().expect("tv_usec"));
        if sec < 0 || !(0..1_000_000).contains(&usec) {
            return crate::e(crate::err::EINVAL);
        }
        now.saturating_add(sec as u64 * 1_000_000 + usec as u64)
    };
    let mut outs = (
        [0u64; fdset::MAX_WORDS],
        [0u64; fdset::MAX_WORDS],
        [0u64; fdset::MAX_WORDS],
    );
    loop {
        let mut ready = 0u64;
        for i in fdset::interests(&ins.0, &ins.1, nfds) {
            // EPOLLIN/EPOLLOUT are POLLIN/POLLOUT's values, so the requested
            // mask passes straight into the poll-shaped probe.
            let rev = fd_events(i.fd as i32, i.requested() as i16);
            if rev != 0 && rev != POLLNVAL {
                ready += i.record(rev as u32, &mut outs.0, &mut outs.1);
            }
        }
        if ready > 0 || crate::net::uptime_us() >= deadline {
            let n = fdset::bytes(nfds);
            for (va, words) in [
                (read_va, &outs.0),
                (write_va, &outs.1),
                (except_va, &outs.2),
            ] {
                if va != 0 {
                    let mut buf = [0u8; fdset::MAX_WORDS * 8];
                    for (w, word) in words.iter().enumerate().take(fdset::words(nfds)) {
                        buf[w * 8..w * 8 + 8].copy_from_slice(&word.to_le_bytes());
                    }
                    if !crate::copy_to_user(va, &buf[..n]) {
                        return crate::e(crate::err::EFAULT);
                    }
                }
            }
            return ready;
        }
        crate::net::sleep_until(crate::net::uptime_us() + 1000);
    }
}

/// `setsockopt(fd, level, opt, optval, optlen)`. The timeouts are real (they
/// drive `KernelSocket`'s `rcvtimeo_us`/`sndtimeo_us`, the same values the
/// blocking `recv`/`send` waits already honor); the socket options this stack
/// has no behaviour behind (`SO_REUSEADDR`, `TCP_NODELAY`, buffer sizes) are
/// accepted and ignored so a libc's boilerplate setup cannot fail a session.
/// Anything else: ENOPROTOOPT. (stage 8, M2)
pub fn setsockopt_arm(fd: u64, level: u64, opt: u64, optval_va: u64, optlen: u64) -> u64 {
    const SOL_SOCKET: u64 = 1;
    const SO_RCVTIMEO: u64 = 20;
    const SO_SNDTIMEO: u64 = 21;
    const IGNORED: [u64; 9] = [2, 3, 4, 5, 6, 7, 8, 9, 10]; // REUSEADDR..OOBINLINE
    const IPPROTO_TCP: u64 = 6;
    let Some(idx) = fd_socket(fd) else {
        return crate::e(crate::err::ENOTSOCK);
    };
    if level == SOL_SOCKET && (opt == SO_RCVTIMEO || opt == SO_SNDTIMEO) {
        if optlen < 16 {
            return crate::e(crate::err::EINVAL);
        }
        let mut tv = [0u8; 16];
        if !crate::copy_from_user(&mut tv, optval_va) {
            return crate::e(crate::err::EFAULT);
        }
        let secs = u64::from_le_bytes(tv[0..8].try_into().expect("timeval"));
        let usec = u64::from_le_bytes(tv[8..16].try_into().expect("timeval"));
        let us = secs.saturating_mul(1_000_000).saturating_add(usec);
        akuma_net::socket::set_socket_timeout(idx, opt == SO_RCVTIMEO, (us != 0).then_some(us));
        return 0;
    }
    if (level == SOL_SOCKET && IGNORED.contains(&opt)) || level == IPPROTO_TCP {
        return 0;
    }
    crate::e(crate::err::ENOPROTOOPT)
}

/// `getsockopt(fd, level, optname, optval*, optlen*)` — the read side of
/// [`setsockopt_arm`], and the row the standard non-blocking connect idiom
/// cannot finish without: `curl` dials (the syscall answers EINPROGRESS),
/// `poll`s for writability, then reads `SOL_SOCKET/SO_ERROR` for the dial's
/// outcome. Without this row that read answered ENOSYS and curl threw away
/// an established connection (`curl: (7)` with the dial already answered —
/// stage 8 session Q). The answers mirror the glue's `sys_getsockopt`
/// (crates/akuma-syscalls-glue/src/net.rs, the full kernels' getsockopt);
/// the state behind them is `akuma_net::socket`'s — wiring, not a second
/// opinion.
pub fn getsockopt_arm(fd: u64, level: u64, opt: u64, optval_va: u64, optlen_va: u64) -> u64 {
    const SOL_SOCKET: u64 = 1;
    const SO_TYPE: u64 = 3;
    const SO_ERROR: u64 = 4;
    const SO_SNDBUF: u64 = 7;
    const SO_RCVBUF: u64 = 8;
    const SO_RCVTIMEO: u64 = 20;
    const SO_SNDTIMEO: u64 = 21;

    let Some(idx) = fd_socket(fd) else {
        return crate::e(crate::err::ENOTSOCK);
    };
    // NULL buffer or zero length: a length-only query — success, nothing
    // written back (the glue's answer for the same call shape).
    if optval_va == 0 || optlen_va == 0 {
        return 0;
    }

    // The timeouts read back as a 16-byte `struct timeval`, not the 4-byte
    // int below — the shape `setsockopt_arm` accepts on the way in, and the
    // readback that separates "honoured" from "accepted and dropped" (the
    // glue's 2026-08-17 lesson). All-zero = no timeout, POSIX's "forever".
    if level == SOL_SOCKET && (opt == SO_RCVTIMEO || opt == SO_SNDTIMEO) {
        if !crate::user_ok(optval_va, 16) || !crate::user_ok(optlen_va, 4) {
            return crate::e(crate::err::EFAULT);
        }
        let us = akuma_net::socket::socket_timeout(idx, opt == SO_RCVTIMEO).unwrap_or(0);
        let tv = akuma_syscalls_linux::time::Timeval::from_bits(us / 1_000_000, us % 1_000_000);
        if !crate::copy_to_user(optval_va, crate::struct_bytes(&tv))
            || !crate::copy_to_user(optlen_va, &16u32.to_le_bytes())
        {
            return crate::e(crate::err::EFAULT);
        }
        return 0;
    }

    // IP/TCP-level options have no state this stack records; Linux's
    // ENOPROTOOPT beats a lying zero for the ones callers actually check.
    if level != SOL_SOCKET {
        return crate::e(crate::err::ENOPROTOOPT);
    }
    if !crate::user_ok(optval_va, 4) || !crate::user_ok(optlen_va, 4) {
        return crate::e(crate::err::EFAULT);
    }
    let val: u32 = match opt {
        SO_ERROR => {
            // The dial's outcome. `connect_arm` waits the handshake out in
            // the kernel before returning, so by the time a caller reads
            // this the stream is established (0), dead (ECONNREFUSED — the
            // glue's answer for a refused dial; `connect_arm`'s own timeout
            // already reported ETIMEDOUT to its caller), or not a stream
            // (UDP/DNS: no error pending, 0).
            match akuma_net::socket::debug_stream_peek(idx) {
                Some(d) if d.dead => crate::err::ECONNREFUSED as u32,
                _ => 0,
            }
        }
        SO_TYPE => {
            if akuma_net::socket::is_udp_socket(idx) {
                2
            } else {
                1
            }
        }
        SO_SNDBUF | SO_RCVBUF => 131072,
        // SO_KEEPALIVE and the rest of the int-shaped SOL_SOCKET set: zero,
        // Linux's answer for options this stack defaults off.
        _ => 0,
    };
    if !crate::copy_to_user(optval_va, &val.to_le_bytes())
        || !crate::copy_to_user(optlen_va, &4u32.to_le_bytes())
    {
        return crate::e(crate::err::EFAULT);
    }
    0
}

/// The fd's nonblock flag, straight from the fd table.
pub fn fd_nonblock(fd: u64) -> bool {
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
        crate::net::wait_lap();
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
        // A null destination is the `send(2)` shape (musl implements send as
        // sendto(NULL, 0)): go to the connect(2) peer, ENOTCONN without one.
        // Anything else unparseable is EINVAL, as before. (stage 8, M2)
        let sa = match sockaddr_in_from_user(dest, dest_len) {
            Some(sa) => sa,
            None if dest == 0 || dest_len == 0 => match akuma_net::socket::socket_udp_peer(idx) {
                Some(sa) => sa,
                None => return crate::e(crate::err::ENOTCONN),
            },
            None => return crate::e(crate::err::EINVAL),
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
/// `getsockname(fd, addr*, addrlen*)` (stage 8 session Q): c-ares calls it
/// on its just-connected DNS socket and treats failure as "could not
/// contact DNS servers". The answer is the socket's bound/ephemeral port
/// with the interface's address standing in for an unbound 0.0.0.0 (a
/// connected socket's getsockname shows the routed source, Linux's rule).
pub fn getsockname_arm(fd: u64, addr_va: u64, addrlen_va: u64) -> u64 {
    const AF_INET: u16 = 2;
    let Some(idx) = fd_socket(fd) else {
        return crate::e(crate::err::ENOTSOCK);
    };
    let Some((bound_ip, port)) = akuma_net::socket::socket_local_addr(idx) else {
        return crate::e(crate::err::EINVAL);
    };
    let ip = if bound_ip == [0, 0, 0, 0] {
        akuma_net::smoltcp_net::iface::get_local_ip()
    } else {
        bound_ip
    };
    let mut sa = [0u8; 16];
    // sin_family is host byte order in Linux's sockaddr (only sin_port and
    // the address are network order); c-ares sanity-checks the raw u16 and
    // a big-endian answer reads 0x0200 != AF_INET on little-endian.
    sa[0..2].copy_from_slice(&AF_INET.to_ne_bytes());
    sa[2..4].copy_from_slice(&port.to_be_bytes());
    sa[4..8].copy_from_slice(&ip);
    if !crate::copy_to_user(addr_va, &sa) {
        return crate::e(crate::err::EFAULT);
    }
    if addrlen_va != 0 && !crate::copy_to_user(addrlen_va, &16u32.to_le_bytes()) {
        return crate::e(crate::err::EFAULT);
    }
    0
}

/// `getpeername(fd, addr*, addrlen*)` — the connect(2) peer of a dgram fd
/// (c-ares asks for it on the DNS socket; amd64 needed the same table row:
/// docs/archive/AKUMA_AMD64_DNS_CONNECTED_UDP.md, BUG 4). Stream sockets
/// answer their established remote.
pub fn getpeername_arm(fd: u64, addr_va: u64, addrlen_va: u64) -> u64 {
    const AF_INET: u16 = 2;
    let Some(idx) = fd_socket(fd) else {
        return crate::e(crate::err::ENOTSOCK);
    };
    let (ip, port) = if let Some(peer) = akuma_net::socket::socket_udp_peer(idx) {
        (peer.ip, peer.port)
    } else if let Some(d) = akuma_net::socket::debug_stream_peek(idx) {
        match d.remote {
            Some(remote) => remote,
            None => return crate::e(crate::err::ENOTCONN),
        }
    } else {
        return crate::e(crate::err::ENOTCONN);
    };
    let mut sa = [0u8; 16];
    // sin_family is host byte order (see getsockname_arm).
    sa[0..2].copy_from_slice(&AF_INET.to_ne_bytes());
    sa[2..4].copy_from_slice(&port.to_be_bytes());
    sa[4..8].copy_from_slice(&ip);
    if !crate::copy_to_user(addr_va, &sa) {
        return crate::e(crate::err::EFAULT);
    }
    if addrlen_va != 0 && !crate::copy_to_user(addrlen_va, &16u32.to_le_bytes()) {
        return crate::e(crate::err::EFAULT);
    }
    0
}

pub fn shutdown_arm(fd: u64, how: u64) -> u64 {    let Some(idx) = fd_socket(fd) else {
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
    // Stage-7 probe: socket reads are where a wedged wget would sit; print
    // the entry at most once a second (a monotone counter, not a clock).
    static LAST: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
    let n = LAST.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    if n.is_multiple_of(1000) {
        akuma_primitives::safe_print!(96, "  [cx] sockread fd={} len={}\n", fd, len);
    }
    let mut data = alloc::vec![0u8; len as usize];
    // A datagram socket's read(2) is the queued datagram (no source address
    // to report — that is what recvfrom(2) is for). (stage 8, M2)
    let r = if akuma_net::socket::is_udp_socket(idx) {
        akuma_net::socket::socket_recv_udp(idx, &mut data, fd_nonblock(fd))
            .map(|(n, _sa)| n)
    } else {
        akuma_net::socket::socket_recv(idx, &mut data, fd_nonblock(fd))
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
    // A datagram socket's write(2)/send(2) goes to its connect(2) peer —
    // the shape busybox `nslookup`, busybox `nc -u` and musl's resolver all
    // talk. Unconnected: ENOTCONN, the answer Linux gives. (stage 8, M2)
    let r = if akuma_net::socket::is_udp_socket(idx) {
        match akuma_net::socket::socket_udp_peer(idx) {
            Some(sa) => akuma_net::socket::socket_send_udp(idx, &data, sa),
            None => Err(crate::err::ENOTCONN as i32),
        }
    } else {
        akuma_net::socket::socket_send(idx, &data, fd_nonblock(fd))
    };    if data.starts_with(b"GET ") {
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
        // `dup`, `fork` and `CLONE_FILES` all copy the fd without the socket
        // behind it knowing; closing ONE of several fds must not destroy the
        // connection (git dups its socket, closes the copy, and its sideband
        // thread then read the closed handle: EBADF). The fd being closed is
        // still in the table here, so "one reference" is "the last".
        if socket_refs(idx) <= 1 {
            akuma_net::socket::remove_socket(idx);
        }
    }
}

/// How many open fds, across every task record, name socket `idx`.
pub fn socket_refs(idx: usize) -> usize {
    crate::task_slots()
        .iter()
        .map(|t| {
            t.fd_names
                .values()
                .filter(|d| matches!(d, crate::OpenFile::Socket(i) if *i == idx))
                .count()
        })
        .sum()
}

/// The kernel `struct msghdr` and `struct iovec` words `sendmsg`/`recvmsg`
/// read: `name`, `namelen`, `iov`, `iovlen`, then control (never used here).
const MSGHDR_LEN: usize = 56;
const MSG_MAX_IOV: u64 = 16;

fn read_msghdr(msg_va: u64) -> Option<([u8; MSGHDR_LEN], u64, u64)> {
    let mut h = [0u8; MSGHDR_LEN];
    if !crate::user_ok(msg_va, MSGHDR_LEN as u64) || !crate::copy_from_user(&mut h, msg_va) {
        return None;
    }
    let iov = u64::from_le_bytes(h[16..24].try_into().ok()?);
    let iovlen = u64::from_le_bytes(h[24..32].try_into().ok()?);
    Some((h, iov, iovlen))
}

fn read_iovec(iov_va: u64, i: u64) -> Option<(u64, u64)> {
    let mut v = [0u8; 16];
    if !crate::copy_from_user(&mut v, iov_va + i * 16) {
        return None;
    }
    Some((
        u64::from_le_bytes(v[0..8].try_into().ok()?),
        u64::from_le_bytes(v[8..16].try_into().ok()?),
    ))
}

/// `sendmsg(fd, &msghdr, flags)`: c-ares' and most resolvers' send. Each
/// iovec goes out through `sendto_arm` with the header's destination; control
/// data is ignored. A datagram socket with several iovecs sends several
/// datagrams (not Linux's one gathered one - nothing common relies on it).
pub fn sendmsg_arm(fd: u64, msg_va: u64, flags: u64) -> u64 {
    let Some((h, iov, iovlen)) = read_msghdr(msg_va) else {
        return crate::e(crate::err::EFAULT);
    };
    if iovlen > MSG_MAX_IOV {
        return crate::e(crate::err::EINVAL);
    }
    let name = u64::from_le_bytes(h[0..8].try_into().unwrap_or([0; 8]));
    let namelen = u64::from(u32::from_le_bytes(h[8..12].try_into().unwrap_or([0; 4])));
    let mut total = 0u64;
    for i in 0..iovlen {
        let Some((base, len)) = read_iovec(iov, i) else {
            return crate::e(crate::err::EFAULT);
        };
        if len == 0 {
            continue;
        }
        let r = sendto_arm(fd, base, len, flags, name, namelen);
        if (r as i64) < 0 {
            return if total > 0 { total } else { r };
        }
        total += r;
        if r < len {
            break;
        }
    }
    total
}

/// `recvmsg(fd, &msghdr, flags)`: one receive into the first non-empty iovec
/// through `recvfrom_arm` (the source address lands in `msg_name`, its length
/// in `msg_namelen`); `msg_controllen` and `msg_flags` are cleared. A
/// datagram is not split across several iovecs.
pub fn recvmsg_arm(fd: u64, msg_va: u64, flags: u64) -> u64 {
    let Some((h, iov, iovlen)) = read_msghdr(msg_va) else {
        return crate::e(crate::err::EFAULT);
    };
    if iovlen > MSG_MAX_IOV {
        return crate::e(crate::err::EINVAL);
    }
    let name = u64::from_le_bytes(h[0..8].try_into().unwrap_or([0; 8]));
    let mut target = None;
    for i in 0..iovlen {
        match read_iovec(iov, i) {
            Some((base, len)) if len > 0 => {
                target = Some((base, len));
                break;
            }
            Some(_) => {}
            None => return crate::e(crate::err::EFAULT),
        }
    }
    let Some((base, len)) = target else {
        return 0;
    };
    // `msg_namelen` is a u32 at offset 8 of the header: recvfrom's `src_len`.
    let name_len_va = if name != 0 { msg_va + 8 } else { 0 };
    let r = recvfrom_arm(fd, base, len, flags, name, name_len_va);
    if (r as i64) >= 0 {
        let _ = crate::copy_to_user(msg_va + 40, &0u64.to_le_bytes()); // msg_controllen
        let _ = crate::copy_to_user(msg_va + 48, &0u32.to_le_bytes()); // msg_flags
    }
    r
}
