//! nettest-unix — does AF_UNIX actually work, and does it work the way Linux does?
//!
//! # Why this probe exists
//!
//! Until 2026-08-23 Akuma had no AF_UNIX socket object at all: `socket(AF_UNIX,
//! …)` returned `EAFNOSUPPORT`, and the only thing that worked was
//! `socketpair(2)` over two kernel pipes
//! (`docs/archive/UNIX_SOCKET_IMPROVEMENTS.md` §1). Two of the defects that
//! audit found were **silent**: `SOCK_SEQPACKET` merged messages, and `sendmsg`
//! sent only the first iovec. Neither produces an error, an errno, or a kernel
//! log line — the caller gets a plausible short count and carries on with
//! corrupt data.
//!
//! That is what this probe is shaped around. Every mode is written against a
//! specific way to lose or duplicate user data, and each prints a verdict that
//! says *which* way it failed rather than just "failed".
//!
//! # The Linux control arm — the whole reason this is a probe and not a test
//!
//! A unix-socket probe is entirely self-contained: no server, no network, no
//! peer to blame. So unlike the TCP probes in this directory, there is no
//! external reference to disagree with — which means **the only way to tell a
//! kernel bug from a probe bug is to run the identical binary on Linux.** The
//! build is static `aarch64-unknown-linux-musl`, so the same file runs under
//! Docker Linux (`userspace/nettest/README.md` § "The Linux control arm").
//!
//! Run the Linux arm FIRST. It is free, and a mode that fails there is a probe
//! bug — fixing it in the guest instead would be chasing your own tail.
//!
//! # Verdicts
//!
//! | verdict | meaning |
//! |---|---|
//! | `OK` | every assertion in the mode held |
//! | `UNSUPPORTED` | a syscall returned `EAFNOSUPPORT`/`ENOSYS`/`EOPNOTSUPP` — not built yet, not broken |
//! | `TRUNCATED` | data arrived short, or a message boundary was lost |
//! | `LEAK` | an fd or table count did not return to baseline |
//! | `READINESS` | poll/select/epoll disagreed about the same fd at the same moment |
//! | `FAIL` | a syscall failed where Linux succeeds |
//!
//! A verdict that differs between the Linux arm and the guest is a kernel
//! divergence. One that matches is not a bug.

use std::ffi::CString;
use std::process::ExitCode;

// ============================================================================
// Reporting
// ============================================================================

/// Every mode ends in exactly one of these, so a guest run diffs line-for-line
/// against the Linux arm.
enum Verdict {
    Ok,
    /// Not implemented yet. Distinguished from a failure on purpose: a phase
    /// that has not landed should not read as a regression.
    Unsupported(String),
    /// Data was lost or a boundary was merged — the silent class.
    Truncated(String),
    Leak(String),
    Readiness(String),
    Fail(String),
}

impl Verdict {
    fn tag(&self) -> &'static str {
        match self {
            Self::Ok => "OK",
            Self::Unsupported(_) => "UNSUPPORTED",
            Self::Truncated(_) => "TRUNCATED",
            Self::Leak(_) => "LEAK",
            Self::Readiness(_) => "READINESS",
            Self::Fail(_) => "FAIL",
        }
    }
    fn detail(&self) -> &str {
        match self {
            Self::Ok => "",
            Self::Unsupported(s)
            | Self::Truncated(s)
            | Self::Leak(s)
            | Self::Readiness(s)
            | Self::Fail(s) => s,
        }
    }
    /// Whether this verdict is an acceptable outcome for the run as a whole.
    /// `Unsupported` counts: a phase that has not landed is not a regression.
    fn is_acceptable(&self) -> bool {
        matches!(self, Self::Ok | Self::Unsupported(_))
    }

    /// Whether the mode actually *did* the thing, so a caller may build further
    /// assertions on top.
    ///
    /// Distinct from [`Verdict::is_acceptable`] on purpose, and the distinction
    /// is load-bearing: `mode_path` used the acceptable-check here and so
    /// continued past an `UNSUPPORTED` rendezvous — the socket was never
    /// created, `stat` on the absent path merely printed a note, and the mode
    /// returned **OK**. It reported a pass for a code path that had not run.
    /// Found by running on the rump-only build, where `socket(AF_UNIX)` is
    /// refused; the guest and Linux arms both hid it by succeeding.
    fn succeeded(&self) -> bool {
        matches!(self, Self::Ok)
    }
}

fn note(msg: &str) {
    println!("[probe] {msg}");
}

/// The last errno, with its name where we know it — a bare number in a probe
/// output is a lookup every reader has to do by hand.
fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

fn ename(e: i32) -> &'static str {
    match e {
        libc::EAGAIN => "EAGAIN",
        libc::EBADF => "EBADF",
        libc::ECONNREFUSED => "ECONNREFUSED",
        libc::EADDRINUSE => "EADDRINUSE",
        libc::EAFNOSUPPORT => "EAFNOSUPPORT",
        libc::EPROTOTYPE => "EPROTOTYPE",
        libc::ENOTSOCK => "ENOTSOCK",
        libc::EMSGSIZE => "EMSGSIZE",
        libc::EOPNOTSUPP => "EOPNOTSUPP",
        libc::ENOSYS => "ENOSYS",
        libc::EPIPE => "EPIPE",
        libc::EINVAL => "EINVAL",
        libc::ENOENT => "ENOENT",
        libc::ENOTCONN => "ENOTCONN",
        libc::EPERM => "EPERM",
        _ => "?",
    }
}

/// `EAFNOSUPPORT`/`ENOSYS`/`EOPNOTSUPP` mean "this phase has not landed", which
/// is a different thing from a broken implementation and must not read as one.
fn unsupported_errno(e: i32) -> bool {
    e == libc::EAFNOSUPPORT || e == libc::ENOSYS || e == libc::EOPNOTSUPP
}

fn fail(what: &str) -> Verdict {
    let e = errno();
    if unsupported_errno(e) {
        Verdict::Unsupported(format!("{what}={} {}", e, ename(e)))
    } else {
        Verdict::Fail(format!("{what}={} {}", e, ename(e)))
    }
}

// ============================================================================
// sockaddr_un, built by hand
// ============================================================================

/// A `sockaddr_un` plus the `addrlen` that must accompany it.
///
/// Hand-built rather than taken from a helper because **`addrlen` is the whole
/// subject**: it, not a NUL scan, is what delimits an abstract name, and the
/// three cases have three different lengths (unnamed 2; abstract counts its
/// leading NUL and has no terminator; a path counts its terminator). A wrapper
/// that computes it for you hides exactly the thing being tested.
struct Addr {
    raw: libc::sockaddr_un,
    len: libc::socklen_t,
}

impl Addr {
    /// Linux's abstract namespace: `sun_path[0] == '\0'`, the rest is the name
    /// verbatim — embedded NULs included, which is why the length comes from
    /// `name.len()` and not from `strlen`.
    fn abstract_name(name: &[u8]) -> Self {
        let mut raw: libc::sockaddr_un = unsafe { std::mem::zeroed() };
        raw.sun_family = libc::AF_UNIX as _;
        assert!(name.len() + 1 <= raw.sun_path.len(), "abstract name too long");
        for (i, b) in name.iter().enumerate() {
            raw.sun_path[i + 1] = *b as _; // [0] stays 0 = the abstract marker
        }
        let len = (std::mem::size_of::<libc::sa_family_t>() + 1 + name.len()) as libc::socklen_t;
        Self { raw, len }
    }

    fn path(p: &str) -> Self {
        let mut raw: libc::sockaddr_un = unsafe { std::mem::zeroed() };
        raw.sun_family = libc::AF_UNIX as _;
        let bytes = p.as_bytes();
        assert!(bytes.len() + 1 <= raw.sun_path.len(), "path too long");
        for (i, b) in bytes.iter().enumerate() {
            raw.sun_path[i] = *b as _;
        }
        let len =
            (std::mem::size_of::<libc::sa_family_t>() + bytes.len() + 1) as libc::socklen_t;
        Self { raw, len }
    }

    fn as_ptr(&self) -> *const libc::sockaddr {
        std::ptr::addr_of!(self.raw).cast()
    }
}

// ============================================================================
// Raw syscall wrappers
// ============================================================================

fn sock(ty: i32) -> i32 {
    unsafe { libc::socket(libc::AF_UNIX, ty, 0) }
}

fn close(fd: i32) {
    if fd >= 0 {
        unsafe { libc::close(fd) };
    }
}

fn bind(fd: i32, a: &Addr) -> i32 {
    unsafe { libc::bind(fd, a.as_ptr(), a.len) }
}

fn connect(fd: i32, a: &Addr) -> i32 {
    unsafe { libc::connect(fd, a.as_ptr(), a.len) }
}

fn listen(fd: i32, backlog: i32) -> i32 {
    unsafe { libc::listen(fd, backlog) }
}

fn accept(fd: i32) -> i32 {
    unsafe { libc::accept(fd, std::ptr::null_mut(), std::ptr::null_mut()) }
}

fn send(fd: i32, data: &[u8]) -> isize {
    unsafe { libc::send(fd, data.as_ptr().cast(), data.len(), 0) }
}

fn recv_into(fd: i32, buf: &mut [u8], flags: i32) -> isize {
    unsafe { libc::recv(fd, buf.as_mut_ptr().cast(), buf.len(), flags) }
}

fn pair(ty: i32) -> Option<(i32, i32)> {
    let mut sv = [0i32; 2];
    let r = unsafe { libc::socketpair(libc::AF_UNIX, ty, 0, sv.as_mut_ptr()) };
    if r == 0 { Some((sv[0], sv[1])) } else { None }
}

fn unlink_quiet(p: &str) {
    if let Ok(c) = CString::new(p) {
        unsafe { libc::unlink(c.as_ptr()) };
    }
}

/// Count this process's open descriptors, for the leak checks.
///
/// `/proc/self/fd` is the honest source but Akuma's `/proc/<pid>/` is empty
/// (`docs/archive/UNIX_SOCKET_IMPROVEMENTS.md` G10 and the redis
/// investigation), so fall back to probing fd numbers with `fcntl(F_GETFD)`.
/// The fallback is what actually runs in the guest; the `/proc` path exists so
/// the Linux arm reports the same number the same way where it can.
fn open_fd_count() -> usize {
    if let Ok(rd) = std::fs::read_dir("/proc/self/fd") {
        let n = rd.count();
        if n > 0 {
            return n;
        }
    }
    (0..256)
        .filter(|fd| unsafe { libc::fcntl(*fd, libc::F_GETFD) } >= 0)
        .count()
}

// ============================================================================
// Modes
// ============================================================================

/// `socketpair` + two messages each way.
///
/// Two messages, not one, is the point: one message passes on a byte stream
/// too, so a single round trip cannot see the boundary bug. For `seqpacket`
/// this reads both messages with a buffer large enough for BOTH — a byte-stream
/// implementation returns 20 and merges them.
fn mode_pair(ty_name: &str) -> Verdict {
    let (ty, framed) = match ty_name {
        "stream" => (libc::SOCK_STREAM, false),
        "seqpacket" => (libc::SOCK_SEQPACKET, true),
        other => return Verdict::Fail(format!("unknown pair type {other}")),
    };
    let Some((a, b)) = pair(ty) else {
        return fail("socketpair");
    };
    note(&format!("socketpair({ty_name}) = ({a}, {b})"));

    let m1 = b"0123456789";
    let m2 = b"abcdefghij";
    if send(a, m1) != 10 || send(a, m2) != 10 {
        let v = fail("send");
        close(a);
        close(b);
        return v;
    }

    // One read with room for both messages. This is the assertion.
    let mut buf = [0u8; 64];
    let n = recv_into(b, &mut buf[..20], 0);
    note(&format!("first recv (20-byte buffer, 20 bytes queued) = {n}"));
    if n < 0 {
        let v = fail("recv");
        close(a);
        close(b);
        return v;
    }
    let n = n as usize;
    if framed {
        if n != 10 {
            close(a);
            close(b);
            return Verdict::Truncated(format!(
                "seqpacket merged messages: one recv returned {n}, want 10 (message boundary lost)"
            ));
        }
        if &buf[..10] != m1 {
            close(a);
            close(b);
            return Verdict::Truncated("seqpacket first message corrupt".into());
        }
        let mut buf2 = [0u8; 64];
        let n2 = recv_into(b, &mut buf2[..20], 0);
        if n2 != 10 || &buf2[..10] != m2 {
            close(a);
            close(b);
            return Verdict::Truncated(format!(
                "seqpacket second message: n={n2}, want 10 — the first read consumed too much"
            ));
        }
    } else {
        if n != 20 {
            close(a);
            close(b);
            return Verdict::Truncated(format!(
                "stream did not coalesce: recv returned {n}, want 20"
            ));
        }
        if &buf[..20] != b"0123456789abcdefghij" {
            close(a);
            close(b);
            return Verdict::Truncated("stream data corrupt".into());
        }
    }

    // Reverse direction, so a one-way-only wiring bug cannot pass.
    if send(b, b"back") != 4 {
        let v = fail("reverse send");
        close(a);
        close(b);
        return v;
    }
    let mut rb = [0u8; 16];
    let rn = recv_into(a, &mut rb, 0);
    close(a);
    close(b);
    if rn != 4 || &rb[..4] != b"back" {
        return Verdict::Fail(format!("reverse direction: n={rn}, want 4"));
    }
    Verdict::Ok
}

/// `sendmsg` with three iovecs, `recvmsg` into two.
///
/// **The silent-loss regression check.** The AF_UNIX arm of `sendmsg` used
/// `iovs[0]` only and returned its length, so this exact call returned 3
/// instead of 11 — and the caller had no reason to distrust a short count from
/// a socket. `recvmsg` had the mirror bug: it filled `iov[0]` and left the rest
/// untouched, which is the normal shape for reading a fixed header plus a
/// payload.
fn mode_iovec() -> Verdict {
    let Some((a, b)) = pair(libc::SOCK_STREAM) else {
        return fail("socketpair");
    };

    let p0 = b"HDR";
    let p1 = b"payload";
    let p2 = b"!";
    let iov = [
        libc::iovec { iov_base: p0.as_ptr() as *mut _, iov_len: 3 },
        libc::iovec { iov_base: p1.as_ptr() as *mut _, iov_len: 7 },
        libc::iovec { iov_base: p2.as_ptr() as *mut _, iov_len: 1 },
    ];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = iov.as_ptr() as *mut _;
    msg.msg_iovlen = 3;
    let sent = unsafe { libc::sendmsg(a, &msg, 0) };
    note(&format!("sendmsg(3 iovecs, 11 bytes total) = {sent}"));
    if sent < 0 {
        let v = fail("sendmsg");
        close(a);
        close(b);
        return v;
    }
    if sent != 11 {
        close(a);
        close(b);
        return Verdict::Truncated(format!(
            "sendmsg sent {sent} of 11 bytes — iovecs past the first were dropped"
        ));
    }

    // Read it back split across two iovecs: header into one, rest into another.
    let mut r0 = [0u8; 3];
    let mut r1 = [0u8; 8];
    let riov = [
        libc::iovec { iov_base: r0.as_mut_ptr().cast(), iov_len: 3 },
        libc::iovec { iov_base: r1.as_mut_ptr().cast(), iov_len: 8 },
    ];
    let mut rmsg: libc::msghdr = unsafe { std::mem::zeroed() };
    rmsg.msg_iov = riov.as_ptr() as *mut _;
    rmsg.msg_iovlen = 2;
    let got = unsafe { libc::recvmsg(b, &mut rmsg, 0) };
    note(&format!("recvmsg(2 iovecs) = {got}"));
    close(a);
    close(b);
    if got < 0 {
        return fail("recvmsg");
    }
    if got != 11 {
        return Verdict::Truncated(format!(
            "recvmsg returned {got} of 11 — iovecs past the first were not filled"
        ));
    }
    if &r0[..] != b"HDR" || &r1[..8] != b"payload!" {
        return Verdict::Truncated(format!(
            "recvmsg scattered wrong: iov0={:?} iov1={:?}",
            String::from_utf8_lossy(&r0),
            String::from_utf8_lossy(&r1)
        ));
    }
    Verdict::Ok
}

/// The `shutdown` matrix, one line per cell.
///
/// `shutdown(2)` was a `return 0` stub for every non-AF_INET fd, so `SHUT_WR`
/// was a lie — no EOF ever reached the peer, and a protocol that ends a request
/// by half-closing hung until something else timed it out. The observable
/// consequence is the assertion: after `SHUT_WR` the peer's `recv` must return
/// **0**, promptly.
fn mode_shutdown() -> Verdict {
    // SHUT_WR: peer sees EOF.
    let Some((a, b)) = pair(libc::SOCK_STREAM) else {
        return fail("socketpair");
    };
    let sd = unsafe { libc::shutdown(a, libc::SHUT_WR) };
    note(&format!("shutdown(a, SHUT_WR) = {sd}"));
    if sd != 0 {
        let v = fail("shutdown SHUT_WR");
        close(a);
        close(b);
        return v;
    }
    // Non-blocking so a kernel that never delivers the EOF reports it as EAGAIN
    // instead of hanging the probe. A hang is the real symptom, but a hung probe
    // tells you nothing.
    set_nonblock(b);
    let mut buf = [0u8; 8];
    let n = recv_into(b, &mut buf, 0);
    let e = errno();
    note(&format!("peer recv after SHUT_WR = {n} (errno {} {})", e, ename(e)));
    close(a);
    close(b);
    if n != 0 {
        return Verdict::Fail(format!(
            "SHUT_WR did not deliver EOF: peer recv returned {n} ({} {}) — want 0",
            e,
            ename(e)
        ));
    }

    // SHUT_RD does NOT discard data that has already arrived.
    //
    // This assertion was wrong in the first version of this probe — it expected
    // an immediate 0 — and the Linux control arm is what corrected it: Linux
    // returns the 6 buffered bytes first and only then reads as EOF. Akuma
    // returned 0 immediately, silently destroying a message the peer had
    // already successfully sent, and that divergence was found by exactly this
    // comparison. It is the clearest case for why every mode here runs on Linux
    // first.
    let Some((a, b)) = pair(libc::SOCK_STREAM) else {
        return fail("socketpair");
    };
    if send(b, b"unread") != 6 {
        let v = fail("send before SHUT_RD");
        close(a);
        close(b);
        return v;
    }
    let sd = unsafe { libc::shutdown(a, libc::SHUT_RD) };
    set_nonblock(a);
    let mut buf = [0u8; 8];
    let buffered = recv_into(a, &mut buf, 0);
    // Whatever was already queued must still be readable.
    let drained = if buffered > 0 { recv_into(a, &mut buf, 0) } else { buffered };
    note(&format!(
        "shutdown(SHUT_RD)={sd}, buffered recv = {buffered} (want 6), then = {drained} (want 0)"
    ));
    close(a);
    close(b);
    if sd != 0 {
        return Verdict::Fail(format!("shutdown(SHUT_RD) = {sd}"));
    }
    if buffered != 6 {
        return Verdict::Truncated(format!(
            "SHUT_RD discarded {} of 6 already-received bytes — data the peer successfully sent was destroyed",
            6 - buffered.max(0)
        ));
    }
    if drained != 0 {
        return Verdict::Fail(format!(
            "after draining, SHUT_RD socket returned {drained}, want 0 (EOF)"
        ));
    }

    // An invalid `how` must be EINVAL, not silent success — the stub returned 0
    // for everything, so a caller passing a bad constant learned nothing.
    let Some((a, b)) = pair(libc::SOCK_STREAM) else {
        return fail("socketpair");
    };
    let r = unsafe { libc::shutdown(a, 99) };
    let e = errno();
    note(&format!("shutdown(how=99) = {r} (errno {} {})", e, ename(e)));
    close(a);
    close(b);
    if r == 0 {
        return Verdict::Fail("shutdown(how=99) reported success — invalid how accepted".into());
    }
    Verdict::Ok
}

fn set_nonblock(fd: i32) {
    unsafe {
        let f = libc::fcntl(fd, libc::F_GETFL);
        libc::fcntl(fd, libc::F_SETFL, f | libc::O_NONBLOCK);
    }
}

/// bind / listen / connect / accept over the **abstract** namespace, in one
/// process, then a round trip.
///
/// Abstract rather than a path because it needs no filesystem at all: it works
/// on a read-only or bare rootfs, and it isolates the socket family from the
/// VFS. If `path` fails and `abstract` passes, the fault is in the node
/// handling, not in AF_UNIX.
///
/// Single-process, non-blocking accept: `connect` queues the connection
/// synchronously before returning, so the backlog is already non-empty when
/// `accept` runs. No fork, no scheduling assumption, no way for this mode to
/// hang.
fn mode_abstract() -> Verdict {
    let name = b"akuma-nettest-unix";
    let addr = Addr::abstract_name(name);
    rendezvous(&addr, "abstract")
}

/// The same round trip over a filesystem path, plus the `S_ISSOCK` check.
fn mode_path(path: &str) -> Verdict {
    unlink_quiet(path);
    let addr = Addr::path(path);
    let v = rendezvous(&addr, "path");
    // `succeeded`, not `is_acceptable`: an UNSUPPORTED rendezvous never created
    // the socket, so there is nothing to stat and nothing to conclude.
    if !v.succeeded() {
        unlink_quiet(path);
        return v;
    }
    // Linux's `stat` reports S_IFSOCK for a bound socket path, and clients check
    // it before connecting. Akuma has no S_IFSOCK in its ext2 layer yet
    // (UNIX_SOCKET_IMPROVEMENTS.md G7 / Phase 2), so this is reported as a
    // divergence rather than a failure: the name table, not the node type, is
    // what `connect` resolves against, so AF_UNIX works either way.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    let c = CString::new(path).unwrap();
    let r = unsafe { libc::stat(c.as_ptr(), &mut st) };
    if r == 0 {
        let is_sock = st.st_mode & libc::S_IFMT == libc::S_IFSOCK;
        note(&format!(
            "stat({path}) mode=0o{:o} S_ISSOCK={}",
            st.st_mode, is_sock
        ));
        if !is_sock {
            unlink_quiet(path);
            return Verdict::Fail(format!(
                "node is not S_IFSOCK (mode=0o{:o}) — a client checking S_ISSOCK will refuse to connect (Phase 2 / G7)",
                st.st_mode
            ));
        }
    } else {
        note(&format!("stat({path}) failed: {} {}", errno(), ename(errno())));
    }
    unlink_quiet(path);
    Verdict::Ok
}

/// Shared bind/listen/connect/accept/round-trip body for both namespaces.
fn rendezvous(addr: &Addr, label: &str) -> Verdict {
    let srv = sock(libc::SOCK_STREAM);
    if srv < 0 {
        return fail("socket");
    }
    if bind(srv, addr) != 0 {
        let v = fail("bind");
        close(srv);
        return v;
    }
    note(&format!("bind({label}) ok on fd {srv}"));
    if listen(srv, 4) != 0 {
        let v = fail("listen");
        close(srv);
        return v;
    }

    let cli = sock(libc::SOCK_STREAM);
    if cli < 0 {
        let v = fail("client socket");
        close(srv);
        return v;
    }
    if connect(cli, addr) != 0 {
        let v = fail("connect");
        close(cli);
        close(srv);
        return v;
    }
    note("connect ok");

    let acc = accept(srv);
    if acc < 0 {
        let v = fail("accept");
        close(cli);
        close(srv);
        return v;
    }
    note(&format!("accept = fd {acc}"));

    // Round trip both ways. A wiring bug that crosses the pipes only one way
    // passes a single-direction test.
    let mut ok = send(cli, b"ping") == 4;
    let mut buf = [0u8; 16];
    let n = recv_into(acc, &mut buf, 0);
    ok &= n == 4 && &buf[..4] == b"ping";
    let n2 = send(acc, b"pong");
    let mut buf2 = [0u8; 16];
    let n3 = recv_into(cli, &mut buf2, 0);
    ok &= n2 == 4 && n3 == 4 && &buf2[..4] == b"pong";
    note(&format!("round trip: c->s {n}, s->c {n3}"));

    close(acc);
    close(cli);
    close(srv);
    if ok {
        Verdict::Ok
    } else {
        Verdict::Fail(format!("round trip failed: c->s={n} s->c={n3}"))
    }
}

/// A stale socket node with no live listener must be `ECONNREFUSED`, and
/// re-binding it must succeed.
///
/// **This is a daemon's restart path.** A service that dies without unlinking
/// leaves the node behind; if `connect` hangs instead of refusing, a client
/// cannot tell a dead service from a slow one, and if the re-`bind` fails the
/// service can never come back without a reboot.
fn mode_stale(path: &str) -> Verdict {
    unlink_quiet(path);
    let addr = Addr::path(path);

    let srv = sock(libc::SOCK_STREAM);
    if srv < 0 {
        return fail("socket");
    }
    if bind(srv, &addr) != 0 {
        let v = fail("bind");
        close(srv);
        return v;
    }
    if listen(srv, 4) != 0 {
        let v = fail("listen");
        close(srv);
        return v;
    }
    // "The daemon died": close the listener but leave the node.
    close(srv);
    note("listener closed, node left in place (simulating a crashed daemon)");

    // A client must be refused, not left hanging.
    let cli = sock(libc::SOCK_STREAM);
    set_nonblock(cli);
    let r = connect(cli, &addr);
    let e = errno();
    note(&format!("connect to stale node = {r} (errno {} {})", e, ename(e)));
    close(cli);
    if r == 0 {
        unlink_quiet(path);
        return Verdict::Fail("connect to a stale node SUCCEEDED — nothing is listening".into());
    }
    if e != libc::ECONNREFUSED {
        unlink_quiet(path);
        return Verdict::Fail(format!(
            "connect to stale node gave {} {} — want ECONNREFUSED so a client can tell dead from slow",
            e,
            ename(e)
        ));
    }

    // And the service must be able to restart. Linux requires an explicit
    // unlink first, which is what a real daemon does.
    unlink_quiet(path);
    let srv2 = sock(libc::SOCK_STREAM);
    let rb = bind(srv2, &addr);
    let e2 = errno();
    note(&format!("re-bind after unlink = {rb} (errno {} {})", rb, ename(e2)));
    close(srv2);
    unlink_quiet(path);
    if rb != 0 {
        return Verdict::Fail(format!(
            "re-bind after unlink failed with {} {} — the service can never restart",
            e2,
            ename(e2)
        ));
    }
    Verdict::Ok
}

/// `SOCK_DGRAM`: unconnected `sendto`, and a zero-length datagram.
fn mode_dgram(path: &str) -> Verdict {
    unlink_quiet(path);
    let addr = Addr::path(path);
    let srv = sock(libc::SOCK_DGRAM);
    if srv < 0 {
        return fail("socket(SOCK_DGRAM)");
    }
    if bind(srv, &addr) != 0 {
        let v = fail("bind");
        close(srv);
        unlink_quiet(path);
        return v;
    }
    let cli = sock(libc::SOCK_DGRAM);
    let sent = unsafe {
        libc::sendto(cli, b"dgram".as_ptr().cast(), 5, 0, addr.as_ptr(), addr.len)
    };
    note(&format!("sendto(unconnected, 5 bytes) = {sent}"));
    if sent < 0 {
        let v = fail("sendto");
        close(cli);
        close(srv);
        unlink_quiet(path);
        return v;
    }
    let mut buf = [0u8; 32];
    set_nonblock(srv);
    let got = recv_into(srv, &mut buf, 0);
    let mut verdict = if got == 5 && &buf[..5] == b"dgram" {
        Verdict::Ok
    } else {
        Verdict::Fail(format!("datagram round trip: got {got}, want 5"))
    };

    // A zero-length datagram is a real, deliverable message, and `recv`
    // returning 0 for it must be distinguishable from EOF. If the kernel drops
    // it, the receiver waits forever for a message that was sent.
    if verdict.succeeded() {
        let z = unsafe { libc::sendto(cli, [].as_ptr(), 0, 0, addr.as_ptr(), addr.len) };
        let n = recv_into(srv, &mut buf, 0);
        let e = errno();
        note(&format!("zero-length datagram: sendto={z} recv={n} (errno {} {})", e, ename(e)));
        if z != 0 || n != 0 {
            verdict = Verdict::Fail(format!(
                "zero-length datagram lost: sendto={z} recv={n} — an empty message is not EOF"
            ));
        }
    }
    close(cli);
    close(srv);
    unlink_quiet(path);
    verdict
}

/// `/dev/log` — the real-workload smoke test.
///
/// musl's `syslog(3)` is a `SOCK_DGRAM` connect to `/dev/log` and one `send`.
/// It is three lines of client code and it is what every daemon's logging goes
/// through, so it is the cheapest "does this actually work for software people
/// wrote" check available. A failure here is a missing feature, not corruption.
fn mode_syslog() -> Verdict {
    let addr = Addr::path("/dev/log");
    let fd = sock(libc::SOCK_DGRAM);
    if fd < 0 {
        return fail("socket(SOCK_DGRAM)");
    }
    let r = connect(fd, &addr);
    let e = errno();
    note(&format!("connect(/dev/log) = {r} (errno {} {})", e, ename(e)));
    if r != 0 {
        close(fd);
        if e == libc::ENOENT || e == libc::ECONNREFUSED {
            return Verdict::Unsupported(format!(
                "no syslog daemon bound /dev/log ({} {}) — AF_UNIX is fine, nothing is listening",
                e,
                ename(e)
            ));
        }
        return fail("connect(/dev/log)");
    }
    let line = b"<14>nettest-unix: AF_UNIX SOCK_DGRAM probe";
    let n = send(fd, line);
    close(fd);
    if n != line.len() as isize {
        return Verdict::Fail(format!("syslog send returned {n}, want {}", line.len()));
    }
    Verdict::Ok
}

/// `SCM_RIGHTS` fd passing.
fn mode_passfd() -> Verdict {
    let Some((a, b)) = pair(libc::SOCK_STREAM) else {
        return fail("socketpair");
    };
    // Pass one end of a pipe, then prove it works by writing through the
    // received copy. A test that only checks "an fd number came back" passes on
    // a kernel that hands over an unrelated descriptor.
    let mut pfd = [0i32; 2];
    if unsafe { libc::pipe(pfd.as_mut_ptr()) } != 0 {
        let v = fail("pipe");
        close(a);
        close(b);
        return v;
    }

    let payload = b"X";
    let iov = [libc::iovec { iov_base: payload.as_ptr() as *mut _, iov_len: 1 }];
    let mut cbuf = [0u8; 64];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = iov.as_ptr() as *mut _;
    msg.msg_iovlen = 1;
    msg.msg_control = cbuf.as_mut_ptr().cast();
    msg.msg_controllen = unsafe { libc::CMSG_SPACE(4) } as _;
    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(4) as _;
        std::ptr::copy_nonoverlapping(
            std::ptr::addr_of!(pfd[1]).cast::<u8>(),
            libc::CMSG_DATA(cmsg),
            4,
        );
    }
    let sent = unsafe { libc::sendmsg(a, &msg, 0) };
    note(&format!("sendmsg(SCM_RIGHTS, 1 byte) = {sent}"));
    if sent < 0 {
        let v = fail("sendmsg SCM_RIGHTS");
        for fd in [a, b, pfd[0], pfd[1]] {
            close(fd);
        }
        return v;
    }

    let mut rbuf = [0u8; 8];
    let riov = [libc::iovec { iov_base: rbuf.as_mut_ptr().cast(), iov_len: 8 }];
    let mut rcbuf = [0u8; 64];
    let mut rmsg: libc::msghdr = unsafe { std::mem::zeroed() };
    rmsg.msg_iov = riov.as_ptr() as *mut _;
    rmsg.msg_iovlen = 1;
    rmsg.msg_control = rcbuf.as_mut_ptr().cast();
    rmsg.msg_controllen = rcbuf.len() as _;
    let got = unsafe { libc::recvmsg(b, &mut rmsg, 0) };
    let mut passed = -1i32;
    if got > 0 {
        unsafe {
            let mut cmsg = libc::CMSG_FIRSTHDR(&rmsg);
            while !cmsg.is_null() {
                if (*cmsg).cmsg_level == libc::SOL_SOCKET
                    && (*cmsg).cmsg_type == libc::SCM_RIGHTS
                {
                    std::ptr::copy_nonoverlapping(
                        libc::CMSG_DATA(cmsg),
                        std::ptr::addr_of_mut!(passed).cast::<u8>(),
                        4,
                    );
                    break;
                }
                cmsg = libc::CMSG_NXTHDR(&rmsg, cmsg);
            }
        }
    }
    note(&format!("recvmsg = {got}, received fd = {passed}"));
    if passed < 0 {
        for fd in [a, b, pfd[0], pfd[1]] {
            close(fd);
        }
        return Verdict::Unsupported(
            "no SCM_RIGHTS descriptor in the reply — ancillary data not implemented (Phase 4)"
                .into(),
        );
    }

    // The sender closes ITS copy first. That is the real test: the descriptor
    // must have been referenced when it was queued, not merely copied by
    // number — otherwise the receiver holds a dangling fd.
    close(pfd[1]);
    let wrote = unsafe { libc::write(passed, b"through".as_ptr().cast(), 7) };
    let mut out = [0u8; 16];
    let read = unsafe { libc::read(pfd[0], out.as_mut_ptr().cast(), 16) };
    note(&format!("write via passed fd = {wrote}, read back = {read}"));
    for fd in [a, b, pfd[0], passed] {
        close(fd);
    }
    if wrote != 7 || read != 7 || &out[..7] != b"through" {
        return Verdict::Fail(format!(
            "passed fd does not work: write={wrote} read={read} — it is not a live reference"
        ));
    }
    Verdict::Ok
}

/// `SO_PEERCRED`.
fn mode_peercred() -> Verdict {
    let Some((a, b)) = pair(libc::SOCK_STREAM) else {
        return fail("socketpair");
    };
    #[repr(C)]
    #[derive(Default, Debug)]
    struct Ucred {
        pid: u32,
        uid: u32,
        gid: u32,
    }
    let mut cred = Ucred::default();
    let mut len = std::mem::size_of::<Ucred>() as libc::socklen_t;
    let r = unsafe {
        libc::getsockopt(
            a,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            std::ptr::addr_of_mut!(cred).cast(),
            &mut len,
        )
    };
    let e = errno();
    note(&format!("SO_PEERCRED = {r} {cred:?} (errno {} {})", e, ename(e)));
    let me = unsafe { libc::getpid() } as u32;
    close(a);
    close(b);
    if r != 0 {
        return fail("getsockopt(SO_PEERCRED)");
    }
    if cred.pid != me {
        return Verdict::Fail(format!(
            "SO_PEERCRED.pid = {} but our pid is {me} — both ends of a socketpair are this process",
            cred.pid
        ));
    }
    // uid is NOT asserted: Akuma has no per-process uid (getuid hardcodes 0),
    // so 0 is the only truthful answer it can give. Flagged, not failed —
    // and worth knowing, because a daemon authorising by peer uid would trust
    // everyone.
    if cred.uid == 0 && unsafe { libc::getuid() } != 0 {
        note("NOTE: SO_PEERCRED.uid is 0 but getuid() is not — per-process uids are absent; do not authorise on this field");
    }
    Verdict::Ok
}

/// Readiness through all four syscalls, at the same moment, on the same fd.
///
/// The four are not redundancy — they are a bisect of separate kernel paths
/// (`poll`/`ppoll`, `pselect6`, `epoll_pwait`). The AF_INET version of exactly
/// this disagreement was a real bug: `poll` said `CONNECTED` and `select` said
/// `HARDFAIL` for one socket at one instant, because `pselect6` never wrote
/// `exceptfds` (`docs/runbooks/cargo-cannot-reach-crates-io.md` §3).
///
/// A listener's `EPOLLIN` is a brand-new predicate — a listening unix socket has
/// no pipes at all — so it is the most likely place for the three paths to
/// diverge again.
fn mode_poll() -> Verdict {
    let addr = Addr::abstract_name(b"akuma-nettest-poll");
    let srv = sock(libc::SOCK_STREAM);
    if srv < 0 {
        return fail("socket");
    }
    if bind(srv, &addr) != 0 || listen(srv, 4) != 0 {
        let v = fail("bind/listen");
        close(srv);
        return v;
    }

    // An idle listener must NOT be readable. One that always reports readable
    // spins an event loop at 100% CPU on an accept that returns EAGAIN.
    let idle = readiness_triple(srv);
    note(&format!("idle listener: poll={} select={} epoll={}", idle.0, idle.1, idle.2));
    if idle.0 || idle.1 || idle.2 {
        close(srv);
        return Verdict::Readiness(format!(
            "idle listener reported readable (poll={} select={} epoll={}) — an event loop will spin",
            idle.0, idle.1, idle.2
        ));
    }

    let cli = sock(libc::SOCK_STREAM);
    if connect(cli, &addr) != 0 {
        let v = fail("connect");
        close(cli);
        close(srv);
        return v;
    }
    let ready = readiness_triple(srv);
    note(&format!(
        "listener with a pending connection: poll={} select={} epoll={}",
        ready.0, ready.1, ready.2
    ));
    close(cli);
    close(srv);

    if !(ready.0 && ready.1 && ready.2) {
        // All three false is a total hang; a split is the worse bug, because it
        // works under one event loop and not another.
        return Verdict::Readiness(format!(
            "accept-ready listener: poll={} select={} epoll={} — {}",
            ready.0,
            ready.1,
            ready.2,
            if !ready.0 && !ready.1 && !ready.2 {
                "no path reports it; every event-loop server hangs at startup"
            } else {
                "the three paths DISAGREE about one fd at one instant"
            }
        ));
    }
    Verdict::Ok
}

/// `(poll, select, epoll)` readability of one fd, right now, no blocking.
fn readiness_triple(fd: i32) -> (bool, bool, bool) {
    // poll with timeout 0 — pure level state, no wait.
    let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
    let p = unsafe { libc::poll(&mut pfd, 1, 0) } > 0 && pfd.revents & libc::POLLIN != 0;

    let s = unsafe {
        let mut rd: libc::fd_set = std::mem::zeroed();
        libc::FD_SET(fd, &mut rd);
        let mut tv = libc::timeval { tv_sec: 0, tv_usec: 0 };
        libc::select(fd + 1, &mut rd, std::ptr::null_mut(), std::ptr::null_mut(), &mut tv) > 0
            && libc::FD_ISSET(fd, &rd)
    };

    let e = unsafe {
        let ep = libc::epoll_create1(0);
        if ep < 0 {
            return (p, s, false);
        }
        let mut ev = libc::epoll_event { events: libc::EPOLLIN as u32, u64: fd as u64 };
        let added = libc::epoll_ctl(ep, libc::EPOLL_CTL_ADD, fd, &mut ev) == 0;
        let mut out = [libc::epoll_event { events: 0, u64: 0 }; 1];
        let n = if added {
            libc::epoll_wait(ep, out.as_mut_ptr(), 1, 0)
        } else {
            -1
        };
        libc::close(ep);
        n > 0 && out[0].events & libc::EPOLLIN as u32 != 0
    };
    (p, s, e)
}

/// `n` sequential connect/accept/echo/close cycles, then a leak check.
///
/// This mode exists because the leak classes in this design are all
/// *accumulating* and none is visible in a single round trip: a name left
/// behind by a closed listener, a server endpoint queued but never accepted, a
/// channel outliving its pipe. One cycle passes with any of them present.
fn mode_stress(n: usize) -> Verdict {
    let addr = Addr::abstract_name(b"akuma-nettest-stress");
    let before = open_fd_count();
    note(&format!("open fds before: {before}"));

    for round in 0..n {
        let srv = sock(libc::SOCK_STREAM);
        if srv < 0 {
            return Verdict::Fail(format!("round {round}: socket = {} {}", errno(), ename(errno())));
        }
        if bind(srv, &addr) != 0 {
            let e = errno();
            close(srv);
            return Verdict::Fail(format!(
                "round {round}: bind = {} {}{}",
                e,
                ename(e),
                if e == libc::EADDRINUSE {
                    " — the previous round's name outlived its socket, so this service can never restart"
                } else {
                    ""
                }
            ));
        }
        if listen(srv, 4) != 0 {
            close(srv);
            return Verdict::Fail(format!("round {round}: listen failed"));
        }
        let cli = sock(libc::SOCK_STREAM);
        if connect(cli, &addr) != 0 {
            let e = errno();
            close(cli);
            close(srv);
            return Verdict::Fail(format!("round {round}: connect = {} {}", e, ename(e)));
        }
        let acc = accept(srv);
        if acc < 0 {
            let e = errno();
            close(cli);
            close(srv);
            return Verdict::Fail(format!("round {round}: accept = {} {}", e, ename(e)));
        }
        if send(cli, b"x") != 1 {
            close(acc);
            close(cli);
            close(srv);
            return Verdict::Fail(format!("round {round}: send failed"));
        }
        let mut b = [0u8; 4];
        if recv_into(acc, &mut b, 0) != 1 {
            close(acc);
            close(cli);
            close(srv);
            return Verdict::Fail(format!("round {round}: recv failed"));
        }
        close(acc);
        close(cli);
        close(srv);
    }

    let after = open_fd_count();
    note(&format!("open fds after {n} cycles: {after}"));
    if after > before {
        return Verdict::Leak(format!(
            "{} descriptors leaked over {n} cycles ({before} -> {after})",
            after - before
        ));
    }
    Verdict::Ok
}

// ============================================================================
// main
// ============================================================================

fn usage() -> ExitCode {
    eprintln!(
        "usage: nettest-unix <mode> [args]

  pair stream|seqpacket   socketpair, TWO messages each way (boundary check)
  iovec                   sendmsg with 3 iovecs / recvmsg into 2
  shutdown                the SHUT_RD / SHUT_WR / invalid-how matrix
  abstract                bind/listen/connect/accept over \\0abstract, no VFS
  path <p>                the same over a filesystem path, plus S_ISSOCK
  stale <p>               crashed-daemon node: ECONNREFUSED, then re-bind
  dgram <p>               SOCK_DGRAM sendto + a zero-length datagram
  syslog                  connect(/dev/log) and send one line
  passfd                  SCM_RIGHTS, and the passed fd must actually work
  peercred                SO_PEERCRED reports our own pid
  poll                    readiness via poll AND select AND epoll, compared
  stress <n>              n connect/accept/close cycles + an fd leak check
  all                     every mode above that needs no argument

Run the Linux arm first: a mode that fails there is a probe bug."
    );
    ExitCode::from(2)
}

fn run(mode: &str, args: &[String]) -> Verdict {
    match mode {
        "pair" => mode_pair(args.first().map_or("stream", String::as_str)),
        "iovec" => mode_iovec(),
        "shutdown" => mode_shutdown(),
        "abstract" => mode_abstract(),
        "path" => mode_path(args.first().map_or("/tmp/nettest-unix.sock", String::as_str)),
        "stale" => mode_stale(args.first().map_or("/tmp/nettest-stale.sock", String::as_str)),
        "dgram" => mode_dgram(args.first().map_or("/tmp/nettest-dgram.sock", String::as_str)),
        "syslog" => mode_syslog(),
        "passfd" => mode_passfd(),
        "peercred" => mode_peercred(),
        "poll" => mode_poll(),
        "stress" => mode_stress(
            args.first().and_then(|s| s.parse().ok()).unwrap_or(100),
        ),
        _ => Verdict::Fail(format!("unknown mode {mode}")),
    }
}

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let Some(mode) = argv.first() else {
        return usage();
    };

    // `all` runs the argument-free modes in phase order, so the first
    // UNSUPPORTED line tells you how far the implementation has got.
    if mode == "all" {
        let modes: &[(&str, &[&str])] = &[
            ("pair", &["stream"]),
            ("pair", &["seqpacket"]),
            ("iovec", &[]),
            ("shutdown", &[]),
            ("abstract", &[]),
            ("poll", &[]),
            ("peercred", &[]),
            ("stress", &["50"]),
            ("passfd", &[]),
            ("syslog", &[]),
        ];
        let mut worst_ok = true;
        for (m, a) in modes {
            let args: Vec<String> = a.iter().map(|s| (*s).to_string()).collect();
            let v = run(m, &args);
            let label = if args.is_empty() {
                (*m).to_string()
            } else {
                format!("{m} {}", args.join(" "))
            };
            println!("[probe] RESULT {label} verdict={} {}", v.tag(), v.detail());
            worst_ok &= v.is_acceptable();
        }
        return if worst_ok { ExitCode::SUCCESS } else { ExitCode::FAILURE };
    }

    if mode == "--help" || mode == "-h" {
        return usage();
    }
    let v = run(mode, &argv[1..]);
    let label = if argv.len() > 1 { argv.join(" ") } else { mode.clone() };
    println!("[probe] RESULT {label} verdict={} {}", v.tag(), v.detail());
    if v.is_acceptable() { ExitCode::SUCCESS } else { ExitCode::FAILURE }
}
