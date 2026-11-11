//! The AF_INET socket descriptors' kernel-side leftovers.
//!
//! The socket **syscalls** are not here: every one of them — `socket`, `bind`,
//! `listen`, `accept`/`accept4`, `connect`, `sendto`/`recvfrom`,
//! `sendmsg`/`recvmsg`, `setsockopt`, and the `getsockopt`/`shutdown`/
//! `getsockname`/`getpeername` that never had a local arm — is
//! `akuma_syscalls_glue::net`'s, the AArch64 kernel's implementation, reached
//! through `usermode::dispatch`'s `to_glue`. This module held a second AF_INET
//! implementation over the same `akuma_net::socket` table until 2026-11-11;
//! folding it found three things glue lacked (`ENOTSOCK` for an open
//! non-socket descriptor, `accept4`'s flag check and `*addrlen` write, and the
//! accept `EAGAIN` re-arm) and moved them there, so both kernels have them
//! (`docs/archive/AKUMA_MULTIARCH_ATTEMPT_0.md`, "Slice 1: sockets").
//!
//! What stays is what is this kernel's: [`close`], the hook
//! `exec_runtime` hands `akuma-exec`; the `recvfrom` busy-loop tripwire,
//! which reads this kernel's task slots; and the boot self-test.

#[cfg(not(feature = "no-tests"))]
use akuma_selftest::Suite;

use crate::fd::{self, errno};
use crate::sched::MAX_TASKS;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

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

    // Through the dispatcher, by x86_64 number, so each check also proves the
    // arm routes to glue: there is no local implementation left to call.
    const NR_SOCKET: u64 = 41;
    const NR_ACCEPT: u64 = 43;
    const NR_BIND: u64 = 49;
    const NR_LISTEN: u64 = 50;
    const NR_GETSOCKNAME: u64 = 51;
    const NR_ACCEPT4: u64 = 288;
    const NR_PIPE2: u64 = 293;
    const AF_INET: u64 = 2;
    const SOCK_STREAM: u64 = 1;
    const SOCK_DGRAM: u64 = 2;
    const SOCK_NONBLOCK: u64 = 0o4000;
    const SOCK_CLOEXEC: u64 = 0o2_000_000;
    let call = |nr: u64, a1: u64, a2: u64, a3: u64, a4: u64| {
        crate::usermode::syscall_dispatch(nr, a1, a2, a3, a4, 0, 0)
    };

    let fd = call(NR_SOCKET, AF_INET, SOCK_STREAM, 0, 0);
    if !t.check("sock: socket() returns a descriptor", fd < 0x8000_0000) {
        return;
    }

    // A bad family must be refused rather than treated as IPv4, and a bad type
    // is `EINVAL`, not `EAFNOSUPPORT` — glue answered the second wrongly until
    // this kernel's own arm folded into it.
    t.check_eq("sock: a non-AF_INET family is refused", call(NR_SOCKET, 10, 1, 0, 0), errno::EAFNOSUPPORT);
    t.check_eq("sock: a bad socket type is EINVAL", call(NR_SOCKET, AF_INET, 99, 0, 0), errno::EINVAL);
    t.check_eq("sock: an unknown type flag is EINVAL", call(NR_SOCKET, AF_INET, SOCK_STREAM | 0x10, 0, 0), errno::EINVAL);

    // `SOCK_NONBLOCK`/`SOCK_CLOEXEC` must be APPLIED, not just masked off the
    // type. Dropping them made every socket blocking, which is what stopped
    // musl's resolver working — DNS failed for `curl` and `git` while busybox
    // `nslookup` succeeded, because only the libc resolver asks for a
    // non-blocking socket. A plain socket must NOT come back marked, or this
    // would pass on a kernel that marks everything.
    let plain = call(NR_SOCKET, AF_INET, SOCK_DGRAM, 0, 0);
    if plain < 0x8000_0000 {
        t.check("sock: a plain socket is not O_NONBLOCK", !fd::is_nonblocking(plain));
        t.check_eq("sock: close the plain socket", fd::sys_close(plain), 0);
    }
    let nb = call(NR_SOCKET, AF_INET, SOCK_DGRAM | SOCK_NONBLOCK, 0, 0);
    if t.check("sock: SOCK_NONBLOCK socket() returns a descriptor", nb < 0x8000_0000) {
        t.check("sock: SOCK_NONBLOCK is applied to the fd", fd::is_nonblocking(nb));
        t.check_eq("sock: close the SOCK_NONBLOCK socket", fd::sys_close(nb), 0);
    }

    // bind to a port, then listen. `sockaddr_in` built by hand: family 2,
    // then the port in network byte order — 0x08AE, where a byte-order slip
    // would bind 0xAE08.
    let mut sa = [0u8; 16];
    sa[0] = 2;
    sa[2..4].copy_from_slice(&2222u16.to_be_bytes());
    t.check_eq("sock: bind to port 2222", call(NR_BIND, fd, sa.as_ptr() as u64, 16, 0), 0);
    t.check_eq("sock: listen", call(NR_LISTEN, fd, 8, 0, 0), 0);

    // `getsockname` gives the port back in network byte order, and says how
    // long the address is.
    {
        let mut out = [0u8; 16];
        let mut len: u32 = 16;
        t.check_eq(
            "sock: getsockname",
            call(NR_GETSOCKNAME, fd, out.as_mut_ptr() as u64, core::ptr::from_mut(&mut len) as u64, 0),
            0,
        );
        t.check_eq(
            "sock: getsockname returns the port in network byte order",
            u64::from(u16::from_be_bytes([out[2], out[3]])),
            2222,
        );
    }

    // `accept4`: an unknown flag bit is refused before anything is dequeued,
    // and on a non-blocking listener with nothing pending the answer is
    // `EAGAIN`. Until 2026-09-23 x86_64 288 had no row at all, so every tokio
    // accept got `ENOSYS` while smoltcp completed the handshake underneath it.
    t.check_eq("sock: accept4 with an unknown flag is EINVAL", call(NR_ACCEPT4, fd, 0, 0, 1), errno::EINVAL);
    if let Some(proc) = akuma_exec::process::current_process_shared() {
        proc.set_nonblock(fd as u32);
        t.check_eq(
            "sock: accept4(SOCK_NONBLOCK|SOCK_CLOEXEC) with nothing pending is EAGAIN",
            call(NR_ACCEPT4, fd, 0, 0, SOCK_NONBLOCK | SOCK_CLOEXEC),
            errno::EAGAIN,
        );
        t.check_eq("sock: accept with nothing pending is EAGAIN", call(NR_ACCEPT, fd, 0, 0, 0), errno::EAGAIN);
        proc.clear_nonblock(fd as u32);
    }

    // A descriptor that is open and not a socket is `ENOTSOCK`; one that is
    // not open is `EBADF`. Glue said `EBADF` for both until this fold.
    {
        let mut pfd = [0u32; 2];
        if t.check_eq("sock: pipe2 for a non-socket fd", call(NR_PIPE2, pfd.as_mut_ptr() as u64, 0, 0, 0), 0) {
            t.check_eq(
                "sock: bind on a non-socket is ENOTSOCK",
                call(NR_BIND, u64::from(pfd[0]), sa.as_ptr() as u64, 16, 0),
                errno::ENOTSOCK,
            );
            let _ = fd::sys_close(u64::from(pfd[0]));
            let _ = fd::sys_close(u64::from(pfd[1]));
        }
        t.check_eq("sock: listen on an unopened fd is EBADF", call(NR_LISTEN, 900, 8, 0, 0), errno::EBADF);
    }

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
