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
//! `NetRuntime::blocking_relax` **drops the BKL across a `hlt`**
//! (`net::net_blocking_relax`), so a waiting socket is off the lock while it
//! waits and the netpoll daemon can run. It was a plain `yield_now` until
//! 2026-09-19, which holds the lock across the wait and wedged any request
//! with a multi-second silent window — see that function's comment. `O_NONBLOCK` is not plumbed
//! through yet: there is no `fcntl`, so nothing can ask for it.

use akuma_net::socket::socket_const::{AF_INET, SOCK_DGRAM, SOCK_STREAM};
use akuma_net::socket::{SockAddrIn, SocketAddrV4};
#[cfg(not(feature = "no-tests"))]
use akuma_selftest::Suite;

use crate::fd::{self, errno};
use crate::sched::MAX_TASKS;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// `SOCK_STREAM`/`SOCK_DGRAM` are the low bits of `type`, which also carries
/// `SOCK_NONBLOCK` and `SOCK_CLOEXEC`.
const SOCK_TYPE_MASK: u64 = 0xf;

/// `SOCK_NONBLOCK` / `SOCK_CLOEXEC`, the two flag bits `type` carries above
/// [`SOCK_TYPE_MASK`]. asm-generic values, identical on both architectures.
const SOCK_NONBLOCK: u64 = 0o4000; // 0x800
const SOCK_CLOEXEC: u64 = 0o2000000; // 0x80000

/// `socket(domain, type, protocol)`.
///
/// **The flag bits in `type` are applied, not merely masked off.** They were
/// parsed away by [`SOCK_TYPE_MASK`] and dropped until 2026-09-18, which made
/// every socket blocking however it was asked for — and that is not a cosmetic
/// divergence from Linux, it is why **DNS did not work for anything linked
/// against musl** on this target.
///
/// musl's resolver (`__res_msend`) opens its UDP socket with
/// `SOCK_DGRAM|SOCK_NONBLOCK|SOCK_CLOEXEC`, sends to each nameserver, and then
/// relies on the socket being non-blocking. Handed a blocking one it parks in
/// `recvfrom` forever, so `git clone` and `curl` hung or reported "could not
/// contact DNS servers" — while busybox `nslookup`, which builds its own query
/// and does not use the libc resolver, resolved the same name perfectly. That
/// split is the diagnostic: **`nslookup` working while `curl` does not means the
/// resolver path, not the network.**
///
/// The AArch64 side has applied both flags since it was written
/// (`akuma_syscalls_glue::net::sys_socket`); this is the same code, and the
/// setters are the shared `Process` ones, so the two cannot drift again.
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
    // Both sets are keyed by fd number and read back through `fd::cur_table`;
    // `Process` owns the store so `fcntl` and this agree by construction.
    if let Some(proc) = akuma_exec::process::current_process_shared() {
        if ty & SOCK_CLOEXEC != 0 {
            proc.set_cloexec(fd as u32);
        }
        if ty & SOCK_NONBLOCK != 0 {
            proc.set_nonblock(fd as u32);
        }
    }
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
    if !crate::uaccess::read_bytes(ptr, &mut raw) {
        return None;
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
    if !crate::uaccess::write_bytes(ptr, raw) {
        return 0;
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
    sys_accept4(fd, addr, addrlen, 0)
}

/// `accept4(fd, addr, addrlen, flags)`: `accept` plus `SOCK_NONBLOCK` /
/// `SOCK_CLOEXEC` on the *new* descriptor. Any other bit is `EINVAL`, as on
/// Linux. The nonblock bit matters more than it looks: tokio registers the
/// accepted fd with epoll and reads it until `EAGAIN`, so a blocking fd parks
/// a worker thread inside `read` instead.
pub fn sys_accept4(fd: u64, addr: u64, addrlen: u64, flags: u64) -> u64 {
    if flags & !(SOCK_NONBLOCK | SOCK_CLOEXEC) != 0 {
        return errno::EINVAL;
    }
    let Some(idx) = fd::socket_index(fd) else {
        return errno::ENOTSOCK;
    };
    match akuma_net::socket::socket_accept(idx, fd::is_nonblocking(fd)) {
        Ok((new_idx, peer)) => {
            let Some(new_fd) = fd::alloc_socket_fd(new_idx) else {
                // No descriptor for the accepted connection. Closing it is the
                // only correct move: leaving it in the table would leak a
                // connection the caller can never reach or close.
                akuma_net::socket::remove_socket(new_idx);
                return errno::EMFILE;
            };
            let written = sockaddr_in_to_user(addr, peer);
            // Through `uaccess`, like every other user write: this was a raw
            // `write_volatile` the 2026-09-05 SMAP sweep missed, and the first
            // `accept` with a non-null `addrlen` after `CR4.SMAP` went on —
            // sshd's, under `SMP=4` — faulted in ring 0 on the client's stack
            // (`#PF err=3, cr2=0x7fffffffdba8`) and took the BKL down with it.
            // A bad pointer loses the length, not the connection.
            if addrlen != 0 {
                let _ = crate::uaccess::write_val::<u32>(addrlen, written as u32);
            }
            if flags != 0
                && let Some(proc) = akuma_exec::process::current_process_shared()
            {
                if flags & SOCK_CLOEXEC != 0 {
                    proc.set_cloexec(new_fd as u32);
                }
                if flags & SOCK_NONBLOCK != 0 {
                    proc.set_nonblock(new_fd as u32);
                }
            }
            new_fd
        }
        Err(e) => {
            // Nothing pending: the next connection is a fresh `EPOLLIN` edge
            // on the listener, and an `EPOLLET` waiter (tokio's accept loop)
            // must be told about it.
            let r = net_err(e);
            if r == errno::EAGAIN {
                akuma_syscalls_glue::poll::epoll_on_fd_drained(fd as u32);
            }
            r
        }
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
    match akuma_net::socket::socket_connect(idx, sa, fd::is_nonblocking(fd)) {
        Ok(()) => 0,
        Err(e) => net_err(e),
    }
}

/// Re-arm `fd`'s `EPOLLOUT` edge after a TCP send that could not take
/// everything: an `EAGAIN` or a short write. Glue's `sendto`/`sendmsg` do this
/// (`poll::epoll_on_fd_write_blocked` says why it hangs without it); these
/// arms are this target's own, so they have to do it too.
fn tcp_sent(fd: u64, want: usize, r: Result<usize, i32>) -> u64 {
    match r {
        Ok(n) => {
            if n < want {
                akuma_syscalls_glue::poll::epoll_on_fd_write_blocked(fd as u32);
            }
            n as u64
        }
        Err(e) => {
            let r = net_err(e);
            if r == errno::EAGAIN {
                akuma_syscalls_glue::poll::epoll_on_fd_write_blocked(fd as u32);
            }
            r
        }
    }
}

/// Re-arm `fd`'s `EPOLLIN` edge after a TCP receive: after **every** successful
/// read, and on `EAGAIN`, exactly as glue's `recvfrom`/`recvmsg` do.
///
/// Missing until 2026-09-23, and it was the kot wedge. tokio registers every
/// socket `EPOLLET` and reads through `recv(2)` — `recvfrom`, which this target
/// serves here rather than in glue — so the first `EPOLLIN` an accepted
/// connection reported was the last: the edge stayed "already reported"
/// forever, its task never woke again, the peer's FIN went unread
/// (`CLOSE_WAIT` piling up in `/proc/net/tcp`), and the listener's backlog
/// filled with connections nobody accepted until new ones were refused.
/// `read(2)` goes through glue and always re-armed, which is why `busybox`
/// and `sshd` never saw it.
fn tcp_received(fd: u64) {
    akuma_syscalls_glue::poll::epoll_on_fd_drained(fd as u32);
}

/// Send on a TCP socket descriptor, for `sendto`.
fn send(fd: u64, idx: usize, buf: u64, len: u64, nonblock: bool) -> u64 {
    let Some(data) = fd::copy_in(buf, len) else {
        return errno::EFAULT;
    };
    tcp_sent(fd, data.len(), akuma_net::socket::socket_send(idx, &data, nonblock))
}

/// Receive on a TCP socket descriptor, for `recvfrom`.
fn recv(fd: u64, idx: usize, buf: u64, len: u64, nonblock: bool) -> u64 {
    let mut data = alloc::vec![0u8; len as usize];
    match akuma_net::socket::socket_recv(idx, &mut data, nonblock) {
        Ok(n) => {
            tcp_received(fd);
            fd::copy_out(buf, &data[..n])
        }
        Err(e) => {
            let r = net_err(e);
            if r == errno::EAGAIN {
                tcp_received(fd);
            }
            r
        }
    }
}

/// `sendto(fd, buf, len, flags, dest_addr, addrlen)`.
///
/// TCP: `dest_addr` is ignored, matching what Linux does on a connected
/// socket — `send`/`sys_write` reach the same [`send`] this falls through to.
///
/// UDP is where this used to be wrong. A UDP socket has no peer to fall back
/// on unless it was `connect()`ed, and musl's stub DNS resolver never does
/// that — `__res_msend` opens one `SOCK_DGRAM` socket and addresses each
/// nameserver by hand on every `sendto`. Until now `dest_addr` was dropped at
/// the syscall boundary entirely (this function took three arguments), so
/// every UDP `sendto` on an unconnected socket had nowhere to go — the
/// `EIO`/timeout a DNS query saw was this, not a smoltcp or wiring problem one
/// layer down. `akuma_net::socket::socket_send_udp` (used unmodified, same as
/// the AArch64 kernel) is what actually addresses a datagram.
///
/// `addrlen` (Linux's 6th argument) is not available — `syscall_entry`'s
/// register shuffle keeps only five — so the decode assumes a `sockaddr_in`'s
/// fixed 16 bytes, which is safe because [`sys_socket`] refuses every family
/// but `AF_INET`.
pub fn sys_sendto(fd: u64, buf: u64, len: u64, dest_addr: u64) -> u64 {
    use core::mem::size_of;

    let Some(idx) = fd::socket_index(fd) else {
        return errno::ENOTSOCK;
    };
    if akuma_net::socket::is_udp_socket(idx) {
        // **A null `dest_addr` on a UDP socket means "the peer `connect(2)`
        // recorded", not "fall through to the TCP path".**
        //
        // Until 2026-09-18 the guard here was `dest_addr != 0 && is_udp`, so
        // `send(2)` — which musl compiles to `sendto(fd, .., NULL, 0)` — fell
        // into `send()` below, the *stream* path, and came back `EBADF` on a
        // perfectly good connected UDP socket. `write(2)` on the same fd
        // worked, because that goes through `akuma-syscalls-glue`, which has
        // always consulted `udp_default_peer`. One fd, two answers.
        //
        // The dispatcher's comment in `usermode.rs` explains why it was never
        // noticed: musl's resolver "never `connect()`s its query socket — it
        // addresses every nameserver by hand on each `sendto`". True of musl,
        // and **false of c-ares**, which is what `curl` and `git` resolve
        // through — so DNS failed for them while `nslookup` and `getaddrinfo`
        // both worked. That divergence is the whole bug.
        //
        // `EDESTADDRREQ` when there is no peer, which is what Linux answers and
        // what glue already answered; `EBADF` sent c-ares looking at its own
        // descriptor bookkeeping.
        let dest = if dest_addr != 0 {
            let Some(d) = sockaddr_in_from_user(dest_addr, size_of::<SockAddrIn>() as u64) else {
                return errno::EINVAL;
            };
            d
        } else {
            let Some(peer) = akuma_net::socket::udp_default_peer(idx) else {
                return errno::EDESTADDRREQ;
            };
            peer
        };
        let Some(data) = fd::copy_in(buf, len) else {
            return errno::EFAULT;
        };
        return match akuma_net::socket::socket_send_udp(idx, &data, dest) {
            Ok(n) => n as u64,
            Err(e) => net_err(e),
        };
    }
    send(fd, idx, buf, len, fd::is_nonblocking(fd))
}

/// `recvfrom(fd, buf, len, flags, src_addr, addrlen)`.
///
/// TCP: `src_addr` is left untouched, as for [`sys_sendto`]. UDP: the sender's
/// address is written back through `akuma_net::socket::socket_recv_udp`'s
/// returned peer — a DNS response arriving from the wrong source would
/// otherwise be indistinguishable from a real answer, though this target's
/// resolver support does not check it yet either. Same `addrlen`-is-unavailable
/// note as `sys_sendto`: a non-null `src_addr` is always written the full 16
/// bytes of a `sockaddr_in`.
pub fn sys_recvfrom(fd: u64, buf: u64, len: u64, src_addr: u64) -> u64 {
    let Some(idx) = fd::socket_index(fd) else {
        return errno::ENOTSOCK;
    };
    if akuma_net::socket::is_udp_socket(idx) {
        let mut data = alloc::vec![0u8; len as usize];
        return match akuma_net::socket::socket_recv_udp(idx, &mut data, fd::is_nonblocking(fd)) {
            Ok((n, from)) => {
                sockaddr_in_to_user(src_addr, from);
                fd::copy_out(buf, &data[..n])
            }
            Err(e) => net_err(e),
        };
    }
    recv(fd, idx, buf, len, fd::is_nonblocking(fd))
}

/// A thread whose `recvfrom` keeps answering the same non-positive result this
/// fast is looping on it — no caller that parks does this.
const RECV_SPIN_CALLS: u32 = 50_000;
/// …within this long of the streak's first call. A healthy tokio socket
/// returns `EAGAIN` once per readiness event, and an idle connection can
/// collect 100k of those over a day; the window is what separates that from
/// the 2026-09-25 spin (~720k/s).
const RECV_SPIN_WINDOW_US: u64 = 1_000_000;
/// At most one report per task per this long, so a spin that never ends costs
/// one console line a minute rather than the console.
const RECV_SPIN_REPORT_EVERY_US: u64 = 60_000_000;

// Per-task streak state, each row touched only by the task in that slot, so
// `Relaxed` throughout. A recycled slot inherits a streak at worst, and the
// fd/result/window checks restart it on the new occupant's first call.
static SPIN_FD: [AtomicU64; MAX_TASKS] = [const { AtomicU64::new(u64::MAX) }; MAX_TASKS];
static SPIN_RET: [AtomicU64; MAX_TASKS] = [const { AtomicU64::new(0) }; MAX_TASKS];
static SPIN_START_US: [AtomicU64; MAX_TASKS] = [const { AtomicU64::new(0) }; MAX_TASKS];
static SPIN_COUNT: [AtomicU32; MAX_TASKS] = [const { AtomicU32::new(0) }; MAX_TASKS];
static SPIN_REPORTED_US: [AtomicU64; MAX_TASKS] = [const { AtomicU64::new(0) }; MAX_TASKS];

/// Busy-loop tripwire for `recvfrom`, called by the dispatcher after either
/// family has answered `r`.
///
/// Built for the 2026-09-25 kot wedge on the Ryzen Firecracker guest: one
/// tokio worker went from ~126 syscalls/s to ~720k `recvfrom`/s and never
/// parked again, and nothing in the log said which socket or which answer.
/// This prints both, plus the state `socket_recv` decides from
/// ([`akuma_net::socket::TcpRecvSnapshot`]). Positive results are data and
/// never count — a bulk transfer is not a spin. Heap-free.
pub fn recv_spin_tripwire(fd: u64, len: u64, flags: u64, r: u64, unix: bool) {
    let t = crate::sched::current_task();
    if t >= MAX_TASKS {
        return;
    }
    if r != 0 && !errno::is_err(r) {
        SPIN_COUNT[t].store(0, Ordering::Relaxed);
        return;
    }
    let now = crate::net::uptime_us();
    let same = SPIN_FD[t].load(Ordering::Relaxed) == fd
        && SPIN_RET[t].load(Ordering::Relaxed) == r
        && now.saturating_sub(SPIN_START_US[t].load(Ordering::Relaxed)) <= RECV_SPIN_WINDOW_US;
    if !same {
        SPIN_FD[t].store(fd, Ordering::Relaxed);
        SPIN_RET[t].store(r, Ordering::Relaxed);
        SPIN_START_US[t].store(now, Ordering::Relaxed);
        SPIN_COUNT[t].store(1, Ordering::Relaxed);
        return;
    }
    let n = SPIN_COUNT[t].load(Ordering::Relaxed).saturating_add(1);
    SPIN_COUNT[t].store(n, Ordering::Relaxed);
    if n != RECV_SPIN_CALLS {
        return;
    }
    let last = SPIN_REPORTED_US[t].load(Ordering::Relaxed);
    if last != 0 && now.saturating_sub(last) < RECV_SPIN_REPORT_EVERY_US {
        return;
    }
    SPIN_REPORTED_US[t].store(now, Ordering::Relaxed);
    let elapsed = now.saturating_sub(SPIN_START_US[t].load(Ordering::Relaxed));
    akuma_primitives::tprint!(192,
        "[recv-spin] pid={} task={} fd={} ret={} len={} flags={:#x} calls={} in {}us family={}\n",
        crate::usermode::current_pid(), t, fd, r.cast_signed(), len, flags, n, elapsed,
        if unix { "unix" } else { "inet" });
    if unix {
        return;
    }
    let Some(idx) = fd::socket_index(fd) else {
        akuma_primitives::safe_print!(64, "[recv-spin]   fd {} is not a socket\n", fd);
        return;
    };
    if akuma_net::socket::is_udp_socket(idx) {
        akuma_primitives::safe_print!(96,
            "[recv-spin]   udp idx={} nonblock={}\n", idx, fd::is_nonblocking(fd));
        return;
    }
    if let Some(s) = akuma_net::socket::tcp_recv_snapshot(idx) {
        akuma_primitives::safe_print!(256,
            "[recv-spin]   tcp idx={} {} {}.{}.{}.{}:{} local={} can_recv={} may_recv={} \
rxq={} was_connected={} recv_shutdown={} handle_live={} nonblock={}\n",
            idx, s.state, s.remote_ip[0], s.remote_ip[1], s.remote_ip[2], s.remote_ip[3],
            s.remote_port, s.local_port, s.can_recv, s.may_recv, s.recv_queue,
            s.was_connected, s.recv_shutdown, s.handle_live, fd::is_nonblocking(fd));
    } else {
        akuma_primitives::safe_print!(64, "[recv-spin]   idx={} is a listener\n", idx);
    }
}

/// Byte offsets into the x86_64 `struct msghdr` — 56 bytes, `{ void
/// *msg_name; socklen_t msg_namelen; struct iovec *msg_iov; size_t
/// msg_iovlen; void *msg_control; size_t msg_controllen; int msg_flags; }`.
/// `msg_namelen` (a 4-byte `socklen_t` at offset 8) leaves 4 bytes of padding
/// before the next pointer-sized field, which is why `IOV` is 16, not 12.
mod msghdr_off {
    pub const NAME: u64 = 0;
    pub const IOV: u64 = 16;
    pub const IOVLEN: u64 = 24;
}

/// One `struct iovec`: `{ void *iov_base; size_t iov_len; }`, 16 bytes.
const IOVEC_SIZE: u64 = 16;

/// Scatter/gather entries [`sys_sendmsg`]/[`sys_recvmsg`] will walk. A bound
/// rather than trust, like every other length this kernel takes from ring 3 —
/// musl's DNS resolver (the reason these two exist) uses exactly one.
const MAX_MSG_IOV: u64 = 8;

/// Read one `u64` field out of user memory at `base + off`.
///
/// A bad pointer reads as 0, which every caller here treats as "no such
/// field" (a NULL `iov`, a zero length, a NULL name) — the fault is recovered
/// by `crate::uaccess`, not reported, because these are optional fields of a
/// `msghdr` and the syscall proceeds without them exactly as it would for a
/// caller that zeroed them.
fn read_u64(base: u64, off: u64) -> u64 {
    crate::uaccess::read_val::<u64>(base + off).unwrap_or(0)
}

/// `sendmsg(fd, msghdr*, flags)` / `recvmsg(fd, msghdr*, flags)` — x86_64
/// 46/47. musl's DNS resolver on Alpine's musl build uses these, not
/// `sendto`/`recvfrom`: `apk`'s own name resolution (before it can even reach
/// `dl-cdn.alpinelinux.org`) hung in a `poll`+`recvmsg` loop forever —
/// `poll(2)` correctly reported the UDP socket readable (a real DNS reply had
/// arrived; see `sys_sendto`'s fix), but `recvmsg` did not exist at all on
/// this target, so the data sitting in the socket was never reachable and the
/// loop spun retrying the same read forever.
///
/// Only what a UDP query round-trip needs: `msg_name`/`msg_namelen` (the
/// `sendto`/`recvfrom` address argument's equivalent) and the scatter/gather
/// buffer, built by concatenating up to `MAX_MSG_IOV` `iovec` entries into one
/// contiguous copy — `msg_control`/`msg_controllen` (ancillary data:
/// `SCM_RIGHTS` fd-passing and the like) are read as present and never
/// interpreted, and `msg_flags` on the way back is always `0`, matching what
/// no caller on this target needs yet.
pub fn sys_sendmsg(fd: u64, msg: u64, _flags: u64) -> u64 {
    let Some(idx) = fd::socket_index(fd) else {
        return errno::ENOTSOCK;
    };
    let name = read_u64(msg, msghdr_off::NAME);
    let iov = read_u64(msg, msghdr_off::IOV);
    let iovlen = read_u64(msg, msghdr_off::IOVLEN).min(MAX_MSG_IOV);

    let mut data = alloc::vec::Vec::new();
    for i in 0..iovlen {
        let entry = iov + i * IOVEC_SIZE;
        let base = read_u64(entry, 0);
        let len = read_u64(entry, 8);
        let Some(chunk) = fd::copy_in(base, len) else {
            return errno::EFAULT;
        };
        data.extend_from_slice(&chunk);
    }

    if name != 0 && akuma_net::socket::is_udp_socket(idx) {
        let Some(dest) = sockaddr_in_from_user(name, core::mem::size_of::<SockAddrIn>() as u64) else {
            return errno::EINVAL;
        };
        return match akuma_net::socket::socket_send_udp(idx, &data, dest) {
            Ok(n) => n as u64,
            Err(e) => net_err(e),
        };
    }
    tcp_sent(fd, data.len(), akuma_net::socket::socket_send(idx, &data, fd::is_nonblocking(fd)))
}

/// See [`sys_sendmsg`]. Scatters the received bytes across `msg_iov` in
/// order, filling each entry before moving to the next — the shape a caller
/// asking for more `iovlen` entries than one datagram needs is written to
/// expect. Writes the sender's address back through `msg_name` (fixed 16
/// bytes, the `sockaddr_in` size — same "no real `addrlen`" note as
/// `sys_recvfrom`, except here `msg_namelen` *is* available, so it is set to
/// match rather than left for the caller to guess).
pub fn sys_recvmsg(fd: u64, msg: u64, _flags: u64) -> u64 {
    let Some(idx) = fd::socket_index(fd) else {
        return errno::ENOTSOCK;
    };
    let name = read_u64(msg, msghdr_off::NAME);
    let iov = read_u64(msg, msghdr_off::IOV);
    let iovlen = read_u64(msg, msghdr_off::IOVLEN).min(MAX_MSG_IOV);
    let total_cap: u64 = (0..iovlen).map(|i| read_u64(iov + i * IOVEC_SIZE, 8)).sum();

    let is_udp = akuma_net::socket::is_udp_socket(idx);
    let mut data = alloc::vec![0u8; total_cap as usize];
    let n = if is_udp {
        match akuma_net::socket::socket_recv_udp(idx, &mut data, fd::is_nonblocking(fd)) {
            Ok((n, from)) => {
                if name != 0 {
                    sockaddr_in_to_user(name, from);
                    // `msg_namelen` sits 8 bytes into `msghdr`, a plain
                    // `socklen_t` (u32). Through `uaccess` — the other raw user
                    // write the SMAP sweep missed; see `sys_accept`.
                    let _ = crate::uaccess::write_val::<u32>(
                        msg + 8,
                        core::mem::size_of::<SockAddrIn>() as u32,
                    );
                }
                n
            }
            Err(e) => return net_err(e),
        }
    } else {
        match akuma_net::socket::socket_recv(idx, &mut data, fd::is_nonblocking(fd)) {
            Ok(n) => {
                tcp_received(fd);
                n
            }
            Err(e) => {
                let r = net_err(e);
                if r == errno::EAGAIN {
                    tcp_received(fd);
                }
                return r;
            }
        }
    };

    let mut written = 0usize;
    for i in 0..iovlen {
        if written >= n {
            break;
        }
        let entry = iov + i * IOVEC_SIZE;
        let base = read_u64(entry, 0);
        let cap = read_u64(entry, 8) as usize;
        let chunk = (n - written).min(cap);
        if errno::is_err(fd::copy_out(base, &data[written..written + chunk])) {
            return if written == 0 { errno::EFAULT } else { written as u64 };
        }
        written += chunk;
    }
    written as u64
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
        // Through `uaccess`, like every other user read on this target. This
        // was a raw `read_volatile` the same 2026-09-05 SMAP sweep that fixed
        // `accept`'s `addrlen` (two functions up) missed, and `apk update`'s
        // `setsockopt(TCP_NODELAY)` — the option value living on the caller's
        // stack — faulted in ring 0 on it (`#PF err=1, cr2=0x7fffffffc58c`)
        // the first time anyone ran apk against this rig since `CR4.SMAP` went
        // on. A bad pointer is `EFAULT`, not a dead machine.
        crate::uaccess::read_val::<u32>(optval).is_some_and(|v| v != 0)
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

#[cfg(not(feature = "no-tests"))]
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

    // A descriptor identity for the closes below: `close` is glue's arm since
    // 4b batch 2b, and it answers `ESRCH` without a registered process. This
    // check is what found that — `sock: close` returned `-ESRCH` where it
    // wanted 0.
    let boot_tid = crate::fd::boot_row_register();

    let fd = sys_socket(AF_INET as u64, SOCK_STREAM as u64, 0);
    if !t.check("sock: socket() returns a descriptor", fd < 0x8000_0000) {
        return;
    }

    // A bad family must be refused rather than treated as IPv4.
    t.check_eq("sock: a non-AF_INET family is refused", sys_socket(10, 1, 0), errno::EAFNOSUPPORT);
    t.check_eq("sock: a bad socket type is EINVAL", sys_socket(AF_INET as u64, 99, 0), errno::EINVAL);

    // `SOCK_NONBLOCK`/`SOCK_CLOEXEC` must be APPLIED, not just masked off the
    // type. Dropping them made every socket blocking, which is what stopped
    // musl's resolver working (see `sys_socket`) — DNS failed for `curl` and
    // `git` while busybox `nslookup` succeeded, because only the libc resolver
    // asks for a non-blocking socket. A plain socket must NOT come back marked,
    // or this would pass on a kernel that marks everything.
    let plain = sys_socket(AF_INET as u64, SOCK_DGRAM as u64, 0);
    if plain < 0x8000_0000 {
        t.check("sock: a plain socket is not O_NONBLOCK", !fd::is_nonblocking(plain));
    }
    let nb = sys_socket(AF_INET as u64, SOCK_DGRAM as u64 | SOCK_NONBLOCK, 0);
    if t.check("sock: SOCK_NONBLOCK socket() returns a descriptor", nb < 0x8000_0000) {
        t.check("sock: SOCK_NONBLOCK is applied to the fd", fd::is_nonblocking(nb));
    }

    // bind to a port, then listen. The sockaddr goes through the same
    // user-memory path a real caller uses, byte order included.
    let want = SocketAddrV4::new([0, 0, 0, 0], 2222);
    let encoded = SockAddrIn::from_addr(&want);
    // SAFETY: `repr(C)` plain-old-data viewed as its own bytes.
    let sa: [u8; 16] = unsafe { core::mem::transmute(encoded) };
    let r = sys_bind(fd, sa.as_ptr() as u64, 16);
    t.check_eq("sock: bind to port 2222", r, 0);
    t.check_eq("sock: listen", sys_listen(fd, 8), 0);

    // `accept4`: an unknown flag bit is refused before anything is dequeued,
    // and on a non-blocking listener with nothing pending the answer is
    // `EAGAIN`. Until 2026-09-23 x86_64 288 had no row at all, so every tokio
    // accept got `ENOSYS` while smoltcp completed the handshake underneath it.
    t.check_eq("sock: accept4 with an unknown flag is EINVAL", sys_accept4(fd, 0, 0, 1), errno::EINVAL);
    if let Some(proc) = akuma_exec::process::current_process_shared() {
        proc.set_nonblock(fd as u32);
        t.check_eq(
            "sock: accept4(SOCK_NONBLOCK|SOCK_CLOEXEC) with nothing pending is EAGAIN",
            sys_accept4(fd, 0, 0, SOCK_NONBLOCK | SOCK_CLOEXEC),
            errno::EAGAIN,
        );
        proc.clear_nonblock(fd as u32);
    }

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

    // `fstat` on a socket fd is `S_IFSOCK`, not `EBADF`. Glue's `fstat_fill`
    // had no `Socket` arm and fell to `_ => EBADF` — so `fstat(socket)` told a
    // caller its descriptor was closed. The arm (and this check) landed with
    // the amd64 `fstat` fold, 4b batch 3b, and the fix is in glue so both
    // kernels get it.
    {
        let mut st = [0u8; 144];
        t.check_eq("sock: fstat on a socket succeeds", crate::fd::sys_fstat(fd, st.as_mut_ptr() as u64), 0);
        t.check_eq(
            "sock: and reports S_IFSOCK",
            u64::from(u32::from_le_bytes(st[24..28].try_into().unwrap_or([0; 4])) & 0o170_000),
            0o140_000,
        );
    }

    // **`ifconfig`'s read-only `SIOCGIF*` ioctls**, on a kernel-stack `struct
    // ifreq` (the self-tests run inside the user-pointer bypass). Here rather
    // than in `fd::smoke_test` since 4b batch 4b: they are **socket** ioctls,
    // and glue's arm — which `fd::sys_ioctl` now delegates to — gates them on
    // the descriptor being a `FileDescriptor::Socket(_)`, as Linux does. The
    // checks used to pass an unopened fd 3 and only worked because this target
    // answered them regardless of the fd. `fd` above is a real one.
    {
        const SIOCGIFADDR: u64 = 0x8915;
        const SIOCGIFFLAGS: u64 = 0x8913;
        let mut ifr = [0u8; 40];
        ifr[..2].copy_from_slice(b"lo");
        t.check_eq(
            "sock: SIOCGIFADDR(lo) succeeds",
            crate::fd::sys_ioctl(fd, SIOCGIFADDR, ifr.as_mut_ptr() as u64),
            0,
        );
        t.check("sock: SIOCGIFADDR(lo) returns 127.0.0.1", ifr[20..24] == [127, 0, 0, 1]);
        ifr = [0u8; 40];
        ifr[..4].copy_from_slice(b"eth0");
        t.check_eq(
            "sock: SIOCGIFFLAGS(eth0) succeeds",
            crate::fd::sys_ioctl(fd, SIOCGIFFLAGS, ifr.as_mut_ptr() as u64),
            0,
        );
        t.check(
            "sock: eth0 is UP|BROADCAST|RUNNING|MULTICAST",
            i16::from_le_bytes([ifr[16], ifr[17]]) == akuma_syscalls_net::iff::ETHERNET,
        );
        ifr = [0u8; 40];
        ifr[..3].copy_from_slice(b"zz9");
        t.check_eq(
            "sock: SIOCGIFADDR on an unknown interface is ENODEV",
            crate::fd::sys_ioctl(fd, SIOCGIFADDR, ifr.as_mut_ptr() as u64),
            errno::ENODEV,
        );
        // A `SIOCGIF*` on a descriptor that is **not** a socket is `ENOTTY` —
        // the gate itself, which is the half of this fold that is a behaviour
        // change rather than a move.
        ifr = [0u8; 40];
        ifr[..2].copy_from_slice(b"lo");
        t.check_eq(
            "sock: SIOCGIFADDR on a non-socket fd is ENOTTY",
            crate::fd::sys_ioctl(1, SIOCGIFADDR, ifr.as_mut_ptr() as u64),
            errno::ENOTTY,
        );
    }

    t.check_eq("sock: close", fd::sys_close(fd), 0);

    let drained = crate::fd::boot_row_release(boot_tid);
    t.check("sock: the borrowed identity was reclaimed", drained >= 1);
}
