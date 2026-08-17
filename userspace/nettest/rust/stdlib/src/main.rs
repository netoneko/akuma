//! nettest-std — the dependency-free half of the delayed-first-byte bisect.
//!
//! Companion to `../reqwest/` (nca's exact stack: tokio + hyper + reqwest +
//! rustls). Both probes speak the same command grammar and print the same
//! `[probe]` line vocabulary, so a run of each against the same URL diffs
//! directly.
//!
//! # Why two probes
//!
//! `docs/archive/SOCKET_DELAYED_FIRST_BYTE_HANG.md` records a guest TCP client
//! that blocks forever when the server's FIRST response byte is delayed past
//! ~5 s, while identical requests answered within ~1 s stream perfectly. The
//! open question that doc could not answer is *which layer* stalls, because
//! the only failing client (nca) is four layers thick. These probes cut the
//! stack into axes that can be tested one at a time:
//!
//! | probe / mode            | sockets                   | HTTP        | TLS           | isolates |
//! |-------------------------|---------------------------|-------------|---------------|----------|
//! | `nettest-std raw`       | blocking `std::net`       | hand-rolled | none          | the kernel's BLOCKING recv path (`socket_recv` → `wait_until`) |
//! | `nettest-std poll`      | nonblocking + `poll(2)`   | hand-rolled | none          | the readiness path WITHOUT epoll (`sys_ppoll`) |
//! | `nettest-std tls`       | blocking `std::net`       | hand-rolled | rustls (sync) | rustls WITHOUT an async runtime |
//! | `nettest-reqwest get`   | tokio/mio + `epoll_pwait` | hyper 1.x   | rustls (async)| nca's whole stack |
//!
//! Read the result matrix as a bisect: `raw` failing means the kernel's
//! blocking socket read is the fault; `raw` passing and `poll` failing moves it
//! to readiness reporting; both passing and `nettest-reqwest` failing moves it
//! to `epoll_pwait` / the reactor. See
//! `docs/runbooks/debug-delayed-first-byte.md` for the full table.
//!
//! # Kernel behaviour this probe is built to catch
//!
//! The akuma-net audit (see `docs/reference/subsystems/networking.md` § "The
//! native data path") turned up three divergences from Linux that a delay
//! sweep measures directly:
//!
//! 1. A **blocking** `read(2)` on a TCP socket returns `ETIMEDOUT` after 30 s
//!    (`socket_recv`'s `wait_until(..., Some(30_000_000))`). Linux blocks
//!    forever absent `SO_RCVTIMEO`. `raw` mode against `/delay/40` proves or
//!    disproves it in one run.
//! 2. A **blocking** `write(2)` returns `ETIMEDOUT` after 5 s
//!    (`socket_send`'s `Some(5_000_000)`) — the closest number in the kernel
//!    to the archive doc's observed "~5 s" threshold.
//! 3. `SO_RCVTIMEO` / `SO_SNDTIMEO` are accepted by `setsockopt` and silently
//!    dropped (`src/syscall/net.rs`, the `_ =>` arm returns 0). `rcvtimeo`
//!    mode asserts the option is honoured; on Akuma today it is not.
//!
//! # Usage
//!
//! ```text
//! nettest-std raw      <url>                     # http:// only, blocking
//! nettest-std poll     <url>                     # http:// only, O_NONBLOCK + poll(2)
//! nettest-std tls      <url>                     # https://, blocking rustls
//! nettest-std rcvtimeo <url> <secs>              # does SO_RCVTIMEO fire?
//! nettest-std sweep    <base-url> [secs,secs,…]  # GET <base>/delay/<n> per n
//! nettest-std gap      <base-url> <pre> <gap>    # GET <base>/gap/<pre>/<gap>
//! ```
//!
//! `<base-url>` is the host-side delay server: run
//! `scripts/net_delay_server.py` on the host and point the guest at
//! `http://10.0.2.2:18080` (SLIRP reaches the host at 10.0.2.2 with no
//! `hostfwd` rule needed — it is the guest→host direction).
//!
//! # Environment
//!
//! - `NETTEST_ALL_CHUNKS=1` — print a line per read() return. Off by default;
//!   normally only the first chunk, the last chunk, and any chunk that landed
//!   more than `NETTEST_GAP_MS` after its predecessor are printed.
//! - `NETTEST_GAP_MS=<n>` — the "interesting gap" threshold (default 200).
//! - `NETTEST_READ_TIMEOUT=<secs>` — `SO_RCVTIMEO` on the data socket in
//!   `raw`/`tls` mode, and the `poll(2)` timeout in `poll` mode. Unset means
//!   no timeout (which is what reproduces a hang; set it to bound a sweep).
//!
//! # Build
//!
//! `../build-musl.sh` — host cross-build for `aarch64-unknown-linux-musl` with
//! `aarch64-linux-musl-gcc`, the same toolchain `userspace/nca` uses. Output
//! lands in `bootstrap/bin/`, so `scripts/populate_disk.sh` ships it to `/bin`.

use std::env;
use std::io::{self, Read, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpStream, ToSocketAddrs};
use std::os::unix::io::{FromRawFd, RawFd};
use std::sync::Arc;
use std::time::{Duration, Instant};

// ============================================================================
// Shared line vocabulary — MUST stay identical to ../reqwest/src/main.rs
// ============================================================================

/// Where in the request a failure landed. Printed as `stage=…` so a failing
/// sweep row says whether the connect, the send, or the wait for the first
/// byte is what died — the distinction the archive doc could not make from
/// "the process sits in read".
#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage {
    Dns,
    Connect,
    Tls,
    Send,
    FirstByte,
    Body,
}

impl Stage {
    fn as_str(self) -> &'static str {
        match self {
            Stage::Dns => "dns",
            Stage::Connect => "connect",
            Stage::Tls => "tls",
            Stage::Send => "send",
            Stage::FirstByte => "first_byte",
            Stage::Body => "body",
        }
    }
}

struct ProbeError {
    stage: Stage,
    after: Duration,
    kind: String,
    msg: String,
}

impl ProbeError {
    fn io(stage: Stage, after: Duration, e: &io::Error) -> Self {
        ProbeError {
            stage,
            after,
            kind: format!("{:?}", e.kind()),
            msg: e.to_string(),
        }
    }
    fn other(stage: Stage, after: Duration, msg: impl Into<String>) -> Self {
        ProbeError {
            stage,
            after,
            kind: "Other".into(),
            msg: msg.into(),
        }
    }
}

/// Everything one request measured. `first_byte` is the number the whole
/// investigation turns on.
struct Timeline {
    t0: Instant,
    connect: Option<Duration>,
    tls: Option<Duration>,
    sent: Option<Duration>,
    first_byte: Option<Duration>,
    last_byte: Option<Duration>,
    status: u16,
    body_bytes: usize,
    chunks: usize,
}

impl Timeline {
    fn new() -> Self {
        Timeline {
            t0: Instant::now(),
            connect: None,
            tls: None,
            sent: None,
            first_byte: None,
            last_byte: None,
            status: 0,
            body_bytes: 0,
            chunks: 0,
        }
    }
    fn now(&self) -> Duration {
        self.t0.elapsed()
    }
    fn mark(&mut self, name: &str) -> Duration {
        let d = self.now();
        let slot = match name {
            "connect" => &mut self.connect,
            "tls" => &mut self.tls,
            "sent" => &mut self.sent,
            "first_byte" => &mut self.first_byte,
            "last_byte" => &mut self.last_byte,
            _ => unreachable!("unknown mark {name}"),
        };
        if slot.is_none() {
            *slot = Some(d);
            println!("[probe] mark {}={}ms", name, ms(d));
        }
        d
    }
}

fn ms(d: Duration) -> u128 {
    d.as_millis()
}

fn opt_ms(d: Option<Duration>) -> String {
    d.map_or_else(|| "-".to_string(), |v| ms(v).to_string())
}

fn print_ok(tl: &Timeline) {
    println!(
        "[probe] RESULT ok status={} body={} chunks={} connect_ms={} tls_ms={} sent_ms={} first_byte_ms={} total_ms={}",
        tl.status,
        tl.body_bytes,
        tl.chunks,
        opt_ms(tl.connect),
        opt_ms(tl.tls),
        opt_ms(tl.sent),
        opt_ms(tl.first_byte),
        opt_ms(tl.last_byte),
    );
}

fn print_fail(e: &ProbeError) {
    println!(
        "[probe] RESULT fail stage={} after_ms={} kind={} err=\"{}\"",
        e.stage.as_str(),
        ms(e.after),
        e.kind,
        e.msg.replace('"', "'"),
    );
}

// ============================================================================
// Minimal URL split — no `url` crate, because this probe's credibility rests
// on having nothing between it and the syscalls.
// ============================================================================

struct Url {
    tls: bool,
    host: String,
    port: u16,
    path: String,
}

fn parse_url(raw: &str) -> Result<Url, String> {
    let (tls, rest) = if let Some(r) = raw.strip_prefix("https://") {
        (true, r)
    } else if let Some(r) = raw.strip_prefix("http://") {
        (false, r)
    } else {
        return Err(format!("url must start with http:// or https:// — got {raw}"));
    };

    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    // No IPv6 literals: Akuma's smoltcp is proto-ipv4 only and `sys_socket`
    // returns EAFNOSUPPORT for AF_INET6, so a `[::1]`-style authority could
    // never work here anyway.
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (
            h.to_string(),
            p.parse::<u16>().map_err(|_| format!("bad port in {raw}"))?,
        ),
        None => (authority.to_string(), if tls { 443 } else { 80 }),
    };
    if host.is_empty() {
        return Err(format!("empty host in {raw}"));
    }
    Ok(Url {
        tls,
        host,
        port,
        path: path.to_string(),
    })
}

fn request_bytes(u: &Url) -> Vec<u8> {
    // `Connection: close` on purpose: the response then ends at EOF, so this
    // probe needs no chunked-transfer decoder and no Content-Length parser.
    // Byte counts and timings are what we are measuring, not HTTP semantics.
    format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: nettest-std/0.1 (akuma probe)\r\nAccept: */*\r\nConnection: close\r\n\r\n",
        u.path,
        if (u.tls && u.port == 443) || (!u.tls && u.port == 80) {
            u.host.clone()
        } else {
            format!("{}:{}", u.host, u.port)
        }
    )
    .into_bytes()
}

fn parse_status(head: &[u8]) -> u16 {
    // "HTTP/1.1 200 OK" — the three digits after the first space.
    let s = String::from_utf8_lossy(&head[..head.len().min(64)]);
    s.split(' ')
        .nth(1)
        .and_then(|c| c.parse::<u16>().ok())
        .unwrap_or(0)
}

// ============================================================================
// Chunk accounting — the inter-chunk deltas are what tell "one delayed burst"
// apart from "steady sub-second streaming", the exact distinction the archive
// doc drew between the gemma run and the proxy run.
// ============================================================================

struct ChunkLog {
    all: bool,
    gap_ms: u128,
    last_at: Option<Duration>,
    /// The most recent chunk that was NOT worth a line of its own. Held back so
    /// `finish` can still print the last chunk of the body — a bulk transfer
    /// otherwise ends with no timestamp at all.
    held: Option<(usize, Duration, usize, Duration)>,
}

impl ChunkLog {
    fn new() -> Self {
        ChunkLog {
            all: env::var("NETTEST_ALL_CHUNKS").is_ok(),
            gap_ms: env::var("NETTEST_GAP_MS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(200),
            last_at: None,
            held: None,
        }
    }

    fn line(n: usize, at: Duration, len: usize, delta: Duration, suffix: &str) {
        println!(
            "[probe] chunk n={n} at={}ms len={len} gap={}ms{suffix}",
            ms(at),
            ms(delta)
        );
    }

    fn record(&mut self, n: usize, at: Duration, len: usize) {
        let delta = at.saturating_sub(self.last_at.unwrap_or(at));
        self.last_at = Some(at);
        if self.all || n == 1 || ms(delta) >= self.gap_ms {
            Self::line(n, at, len, delta, "");
            self.held = None;
        } else {
            self.held = Some((n, at, len, delta));
        }
    }

    fn finish(&mut self) {
        if let Some((n, at, len, delta)) = self.held.take() {
            Self::line(n, at, len, delta, " (last)");
        }
    }
}

// ============================================================================
// The read loop, shared by every blocking mode
// ============================================================================

fn drain<S: Read>(s: &mut S, tl: &mut Timeline) -> Result<(), ProbeError> {
    let mut buf = vec![0u8; 16 * 1024];
    let mut log = ChunkLog::new();
    let mut head = Vec::new();
    loop {
        match s.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let at = tl.now();
                if tl.first_byte.is_none() {
                    tl.mark("first_byte");
                }
                if head.len() < 64 {
                    head.extend_from_slice(&buf[..n.min(64)]);
                }
                tl.chunks += 1;
                tl.body_bytes += n;
                log.record(tl.chunks, at, n);
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => {
                let stage = if tl.first_byte.is_none() {
                    Stage::FirstByte
                } else {
                    Stage::Body
                };
                log.finish();
                return Err(ProbeError::io(stage, tl.now(), &e));
            }
        }
    }
    log.finish();
    tl.mark("last_byte");
    tl.status = parse_status(&head);
    Ok(())
}

fn resolve(u: &Url, tl: &mut Timeline) -> Result<SocketAddr, ProbeError> {
    let mut addrs = (u.host.as_str(), u.port)
        .to_socket_addrs()
        .map_err(|e| ProbeError::io(Stage::Dns, tl.now(), &e))?;
    // IPv4 only — see the note in `parse_url`.
    addrs
        .find(|a| matches!(a.ip(), IpAddr::V4(_)))
        .ok_or_else(|| ProbeError::other(Stage::Dns, tl.now(), "no IPv4 address for host"))
}

fn read_timeout_from_env() -> Option<Duration> {
    env::var("NETTEST_READ_TIMEOUT")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(Duration::from_secs)
}

// ============================================================================
// Mode: raw — blocking std::net, plaintext HTTP
// ============================================================================

fn mode_raw(url: &str) -> Result<Timeline, ProbeError> {
    let u = parse_url(url).map_err(|e| ProbeError::other(Stage::Dns, Duration::ZERO, e))?;
    if u.tls {
        return Err(ProbeError::other(
            Stage::Tls,
            Duration::ZERO,
            "raw mode is plaintext only — use `tls` for https://",
        ));
    }
    let mut tl = Timeline::new();
    let addr = resolve(&u, &mut tl)?;

    let mut sock = TcpStream::connect(addr).map_err(|e| ProbeError::io(Stage::Connect, tl.now(), &e))?;
    tl.mark("connect");
    // Nagle off, matching what the kernel does for us anyway
    // (`KernelSocket::new_stream` sets `tcp_nodelay: true`) — set it explicitly
    // so the probe does not depend on that default.
    let _ = sock.set_nodelay(true);
    if let Some(t) = read_timeout_from_env() {
        let _ = sock.set_read_timeout(Some(t));
    }

    sock.write_all(&request_bytes(&u))
        .map_err(|e| ProbeError::io(Stage::Send, tl.now(), &e))?;
    sock.flush()
        .map_err(|e| ProbeError::io(Stage::Send, tl.now(), &e))?;
    tl.mark("sent");

    drain(&mut sock, &mut tl)?;
    let _ = sock.shutdown(Shutdown::Both);
    Ok(tl)
}

// ============================================================================
// Mode: poll — nonblocking connect + poll(2), plaintext HTTP
//
// This is the mode that exercises the kernel paths akuma-net documents as
// historically fragile: `connect` → EINPROGRESS → poll → `connect` again to
// collect the result (the "redial" that `ConnectStep::InProgress` exists for,
// and that used to be reported as ECONNREFUSED), and `sys_ppoll`'s readiness
// reporting instead of `sys_epoll_pwait`'s.
// ============================================================================

fn sockaddr_in(addr: &SocketAddr) -> libc::sockaddr_in {
    let ip = match addr.ip() {
        IpAddr::V4(v4) => v4,
        IpAddr::V6(_) => unreachable!("resolve() filtered to IPv4"),
    };
    // Built field-by-field from a zeroed struct rather than with a literal:
    // BSD/macOS `sockaddr_in` carries a `sin_len` field that Linux does not, so
    // a struct literal only compiles on one of the two hosts this probe builds
    // for (see `new_nonblocking_socket` for why the host build matters).
    let mut sa: libc::sockaddr_in = unsafe { std::mem::zeroed() };
    sa.sin_family = libc::AF_INET as libc::sa_family_t;
    sa.sin_port = addr.port().to_be();
    sa.sin_addr.s_addr = u32::from_ne_bytes(ip.octets());
    #[cfg(not(target_os = "linux"))]
    {
        sa.sin_len = std::mem::size_of::<libc::sockaddr_in>() as u8;
    }
    sa
}

/// A fresh nonblocking, close-on-exec AF_INET stream socket.
///
/// The Linux arm is the one that matters — `SOCK_NONBLOCK` (type & 0x800) and
/// `SOCK_CLOEXEC` (type & 0x80000) are both honoured by Akuma's `sys_socket`,
/// so the fd is nonblocking from birth with no `fcntl` round trip, exactly as
/// mio would create it.
///
/// The non-Linux arm exists so this probe also builds and runs on the
/// **development host**. That is not incidental: running the identical probe
/// against the identical delay server on macOS is the control measurement. A
/// sweep that fails on Akuma and passes on the host localises the fault to the
/// kernel; one that fails on both is a probe or a server bug.
#[cfg(target_os = "linux")]
fn new_nonblocking_socket() -> io::Result<RawFd> {
    let fd = unsafe {
        libc::socket(
            libc::AF_INET,
            libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
            0,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(fd)
}

#[cfg(not(target_os = "linux"))]
fn new_nonblocking_socket() -> io::Result<RawFd> {
    // BSD/macOS has no SOCK_NONBLOCK/SOCK_CLOEXEC type bits — two fcntls.
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    unsafe {
        let fl = libc::fcntl(fd, libc::F_GETFL);
        if fl < 0 || libc::fcntl(fd, libc::F_SETFL, fl | libc::O_NONBLOCK) < 0 {
            let e = io::Error::last_os_error();
            libc::close(fd);
            return Err(e);
        }
        let _ = libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
    }
    Ok(fd)
}

fn poll_fd(fd: RawFd, events: libc::c_short, timeout_ms: libc::c_int) -> io::Result<libc::c_short> {
    let mut pfd = libc::pollfd {
        fd,
        events,
        revents: 0,
    };
    loop {
        let r = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
        if r < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        if r == 0 {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "poll(2) timed out"));
        }
        return Ok(pfd.revents);
    }
}

fn mode_poll(url: &str) -> Result<Timeline, ProbeError> {
    let u = parse_url(url).map_err(|e| ProbeError::other(Stage::Dns, Duration::ZERO, e))?;
    if u.tls {
        return Err(ProbeError::other(
            Stage::Tls,
            Duration::ZERO,
            "poll mode is plaintext only — use `tls` for https://",
        ));
    }
    let mut tl = Timeline::new();
    let addr = resolve(&u, &mut tl)?;

    // Raw socket(2) so the CONNECT itself is nonblocking, not just the reads —
    // that is what puts this mode through the EINPROGRESS → poll → redial
    // idiom `ConnectStep::InProgress` exists for.
    let fd = new_nonblocking_socket()
        .map_err(|e| ProbeError::io(Stage::Connect, tl.now(), &e))?;
    // Owned from here on, so every early return closes it.
    let mut sock = unsafe { TcpStream::from_raw_fd(fd) };

    let sa = sockaddr_in(&addr);
    let rc = unsafe {
        libc::connect(
            fd,
            (&sa as *const libc::sockaddr_in).cast::<libc::sockaddr>(),
            std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
        )
    };
    if rc < 0 {
        let e = io::Error::last_os_error();
        let raw = e.raw_os_error().unwrap_or(0);
        if raw != libc::EINPROGRESS {
            return Err(ProbeError::io(Stage::Connect, tl.now(), &e));
        }
        println!("[probe] connect EINPROGRESS, polling POLLOUT");
        let timeout_ms = read_timeout_from_env()
            .map_or(-1, |t| t.as_millis() as libc::c_int);
        poll_fd(fd, libc::POLLOUT, timeout_ms)
            .map_err(|e| ProbeError::io(Stage::Connect, tl.now(), &e))?;
        // The redial idiom: ask connect(2) again for the verdict. On Akuma an
        // established socket answers 0 here rather than EISCONN (deliberate —
        // see `connect_step` in crates/akuma-net/src/socket.rs).
        let rc2 = unsafe {
            libc::connect(
                fd,
                (&sa as *const libc::sockaddr_in).cast::<libc::sockaddr>(),
                std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
            )
        };
        let redial = if rc2 == 0 {
            0
        } else {
            io::Error::last_os_error().raw_os_error().unwrap_or(0)
        };
        println!("[probe] connect redial errno={redial} (0 or EISCONN=106 both mean connected)");
        if redial != 0 && redial != libc::EISCONN {
            return Err(ProbeError::other(
                Stage::Connect,
                tl.now(),
                format!("redial errno {redial}"),
            ));
        }
    }
    tl.mark("connect");
    let _ = sock.set_nodelay(true);

    // Write with the same WouldBlock+poll loop a reactor would use.
    let req = request_bytes(&u);
    let mut off = 0;
    while off < req.len() {
        match sock.write(&req[off..]) {
            Ok(0) => {
                return Err(ProbeError::other(Stage::Send, tl.now(), "write returned 0"));
            }
            Ok(n) => off += n,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                poll_fd(fd, libc::POLLOUT, -1)
                    .map_err(|e| ProbeError::io(Stage::Send, tl.now(), &e))?;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(ProbeError::io(Stage::Send, tl.now(), &e)),
        }
    }
    tl.mark("sent");

    // Read loop: EAGAIN → poll(POLLIN) → read. This is the shape that must NOT
    // stall across a long silent window; `sys_ppoll` re-polls the smoltcp stack
    // itself every iteration (≤10 ms cap), so a stall here is a readiness bug,
    // not a lost wakeup.
    let timeout_ms = read_timeout_from_env().map_or(-1, |t| t.as_millis() as libc::c_int);
    let mut buf = vec![0u8; 16 * 1024];
    let mut log = ChunkLog::new();
    let mut head = Vec::new();
    loop {
        match sock.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let at = tl.now();
                if tl.first_byte.is_none() {
                    tl.mark("first_byte");
                }
                if head.len() < 64 {
                    head.extend_from_slice(&buf[..n.min(64)]);
                }
                tl.chunks += 1;
                tl.body_bytes += n;
                log.record(tl.chunks, at, n);
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                let stage = if tl.first_byte.is_none() {
                    Stage::FirstByte
                } else {
                    Stage::Body
                };
                let revents = poll_fd(fd, libc::POLLIN, timeout_ms)
                    .map_err(|e| ProbeError::io(stage, tl.now(), &e))?;
                // POLLHUP with no POLLIN means the kernel decided the socket is
                // dead (`socket_is_dead_tcp` → EPOLLHUP). Report it rather than
                // spinning: that is exactly the "spurious readable" loop the
                // kernel's `socket_can_recv_tcp` comment warns about.
                if revents & libc::POLLIN == 0 && revents & (libc::POLLHUP | libc::POLLERR) != 0 {
                    log.finish();
                    return Err(ProbeError::other(
                        stage,
                        tl.now(),
                        format!("poll returned revents=0x{revents:x} (HUP/ERR, no data)"),
                    ));
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => {
                let stage = if tl.first_byte.is_none() {
                    Stage::FirstByte
                } else {
                    Stage::Body
                };
                log.finish();
                return Err(ProbeError::io(stage, tl.now(), &e));
            }
        }
    }
    log.finish();
    tl.mark("last_byte");
    tl.status = parse_status(&head);
    let _ = sock.shutdown(Shutdown::Both);
    Ok(tl)
}

// ============================================================================
// Mode: tls — blocking rustls over a blocking std::net::TcpStream
// ============================================================================

fn mode_tls(url: &str) -> Result<Timeline, ProbeError> {
    let u = parse_url(url).map_err(|e| ProbeError::other(Stage::Dns, Duration::ZERO, e))?;
    if !u.tls {
        return Err(ProbeError::other(
            Stage::Tls,
            Duration::ZERO,
            "tls mode needs an https:// url — use `raw` for http://",
        ));
    }
    let mut tl = Timeline::new();
    let addr = resolve(&u, &mut tl)?;

    let mut root_store = rustls::RootCertStore::empty();
    root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();
    let server_name = rustls::pki_types::ServerName::try_from(u.host.clone())
        .map_err(|e| ProbeError::other(Stage::Tls, tl.now(), format!("bad server name: {e}")))?;
    let conn = rustls::ClientConnection::new(Arc::new(config), server_name)
        .map_err(|e| ProbeError::other(Stage::Tls, tl.now(), format!("rustls: {e}")))?;

    let sock = TcpStream::connect(addr).map_err(|e| ProbeError::io(Stage::Connect, tl.now(), &e))?;
    tl.mark("connect");
    let _ = sock.set_nodelay(true);
    if let Some(t) = read_timeout_from_env() {
        let _ = sock.set_read_timeout(Some(t));
    }

    let mut stream = rustls::StreamOwned::new(conn, sock);
    // Force the handshake now so `tls` is a real mark and not folded into the
    // first write.
    stream
        .flush()
        .map_err(|e| ProbeError::io(Stage::Tls, tl.now(), &e))?;
    while stream.conn.is_handshaking() {
        stream
            .conn
            .complete_io(&mut stream.sock)
            .map_err(|e| ProbeError::io(Stage::Tls, tl.now(), &e))?;
    }
    tl.mark("tls");
    if let Some(proto) = stream.conn.protocol_version() {
        println!("[probe] tls version={proto:?}");
    }

    stream
        .write_all(&request_bytes(&u))
        .map_err(|e| ProbeError::io(Stage::Send, tl.now(), &e))?;
    stream
        .flush()
        .map_err(|e| ProbeError::io(Stage::Send, tl.now(), &e))?;
    tl.mark("sent");

    match drain(&mut stream, &mut tl) {
        Ok(()) => Ok(tl),
        // A server that closes without close_notify surfaces as UnexpectedEof
        // from rustls. That is not a probe failure if we already have a body —
        // `Connection: close` servers do it routinely.
        Err(e) if e.stage == Stage::Body && e.kind == "UnexpectedEof" => {
            println!("[probe] note: peer closed without close_notify (benign)");
            tl.mark("last_byte");
            Ok(tl)
        }
        Err(e) => Err(e),
    }
}

// ============================================================================
// Mode: rcvtimeo — is SO_RCVTIMEO honoured?
//
// `src/syscall/net.rs`'s setsockopt SOL_SOCKET arm falls through to `_ => 0`
// for SO_RCVTIMEO, i.e. it reports success and does nothing. A client that
// relies on the option to bound a read gets an unbounded read instead — and on
// Akuma, an unbounded blocking read is capped at the kernel's own hidden 30 s
// (`socket_recv`'s `wait_until(..., Some(30_000_000))`), so the observable
// failure is "my 2 s timeout fired at 30 s, as ETIMEDOUT".
// ============================================================================

/// Returns a shell exit code, not a `Timeline`: a fired timeout is the SUCCESS
/// outcome of this mode, so mapping it onto the ordinary "request failed" exit
/// would make "the option works" and "the probe broke" indistinguishable to a
/// script. 0 = verdict reached, 1 = could not get far enough to judge.
fn mode_rcvtimeo(url: &str, secs: u64) -> i32 {
    // "Could not get far enough to judge" is exit 1; every path that reaches a
    // VERDICT is exit 0, including the one where the timeout fires.
    macro_rules! bail {
        ($e:expr) => {{
            print_fail(&$e);
            return 1;
        }};
    }
    let u = match parse_url(url) {
        Ok(u) => u,
        Err(e) => bail!(ProbeError::other(Stage::Dns, Duration::ZERO, e)),
    };
    let mut tl = Timeline::new();
    let addr = match resolve(&u, &mut tl) {
        Ok(a) => a,
        Err(e) => bail!(e),
    };
    let mut sock = match TcpStream::connect(addr) {
        Ok(s) => s,
        Err(e) => bail!(ProbeError::io(Stage::Connect, tl.now(), &e)),
    };
    tl.mark("connect");
    if let Err(e) = sock.set_read_timeout(Some(Duration::from_secs(secs))) {
        bail!(ProbeError::io(Stage::Connect, tl.now(), &e));
    }
    match sock.read_timeout() {
        Ok(Some(t)) => println!("[probe] SO_RCVTIMEO readback={}ms (requested {}s)", t.as_millis(), secs),
        Ok(None) => println!("[probe] SO_RCVTIMEO readback=NONE (requested {secs}s) — option dropped"),
        Err(e) => println!("[probe] SO_RCVTIMEO readback failed: {e}"),
    }
    if let Err(e) = sock.write_all(&request_bytes(&u)) {
        bail!(ProbeError::io(Stage::Send, tl.now(), &e));
    }
    tl.mark("sent");

    let mut buf = [0u8; 1024];
    let started = tl.now();
    match sock.read(&mut buf) {
        Ok(n) => {
            tl.mark("first_byte");
            tl.chunks = 1;
            tl.body_bytes = n;
            tl.status = parse_status(&buf[..n]);
            tl.mark("last_byte");
            println!("[probe] read returned {n} bytes before the timeout could fire");
            println!("[probe] VERDICT inconclusive — the server answered inside the timeout");
            print_ok(&tl);
            0
        }
        Err(e) => {
            let waited = tl.now() - started;
            println!(
                "[probe] read failed after {}ms (SO_RCVTIMEO was {}s) kind={:?}",
                ms(waited),
                secs,
                e.kind()
            );
            let slack = Duration::from_millis(500);
            if waited > Duration::from_secs(secs) + slack {
                println!(
                    "[probe] VERDICT SO_RCVTIMEO NOT honoured — waited {}ms for a {}s timeout",
                    ms(waited),
                    secs
                );
            } else {
                println!("[probe] VERDICT SO_RCVTIMEO honoured");
            }
            0
        }
    }
}

// ============================================================================
// Sweeps
// ============================================================================

fn default_delays() -> Vec<u64> {
    // Straddles every candidate threshold at once: under the archive doc's
    // observed ~1 s "always works" band, across its ~5 s "always hangs" band,
    // past the kernel's 5 s blocking-send cap, and past its 30 s blocking-recv
    // cap. One run says which number the failure is keyed to.
    vec![0, 1, 3, 5, 8, 12, 20, 35]
}

fn run_once(mode: &str, url: &str) -> Result<Timeline, ProbeError> {
    match mode {
        "raw" => mode_raw(url),
        "poll" => mode_poll(url),
        "tls" => mode_tls(url),
        _ => unreachable!(),
    }
}

fn mode_sweep(base: &str, delays: &[u64], inner: &str) -> i32 {
    let base = base.trim_end_matches('/');
    let mut worst_ok = 0u64;
    let mut first_fail: Option<u64> = None;
    println!("[probe] sweep base={base} mode={inner} delays={delays:?}");
    for &d in delays {
        let url = format!("{base}/delay/{d}");
        println!("[probe] --- delay={d}s url={url}");
        match run_once(inner, &url) {
            Ok(tl) => {
                let fb = tl.first_byte.map_or(0, ms);
                // The server slept `d` seconds; anything much past that is the
                // stack adding latency of its own.
                let overhead = fb.saturating_sub(u128::from(d) * 1000);
                println!(
                    "[probe] SWEEP delay={d}s OK first_byte_ms={fb} overhead_ms={overhead} body={} chunks={}",
                    tl.body_bytes, tl.chunks
                );
                worst_ok = worst_ok.max(d);
            }
            Err(e) => {
                print_fail(&e);
                println!(
                    "[probe] SWEEP delay={d}s FAIL stage={} after_ms={} kind={}",
                    e.stage.as_str(),
                    ms(e.after),
                    e.kind
                );
                if first_fail.is_none() {
                    first_fail = Some(d);
                }
            }
        }
    }
    match first_fail {
        None => {
            println!("[probe] SWEEP SUMMARY all {} delays passed (max {worst_ok}s)", delays.len());
            0
        }
        Some(d) => {
            println!("[probe] SWEEP SUMMARY threshold: last OK={worst_ok}s, first FAIL={d}s");
            1
        }
    }
}

fn mode_gap(base: &str, pre: u64, gap: u64, inner: &str) -> i32 {
    let base = base.trim_end_matches('/');
    let url = format!("{base}/gap/{pre}/{gap}");
    println!("[probe] gap pre={pre}s gap={gap}s url={url}");
    // A `gap` run answers the question the archive doc explicitly left open:
    // is the failure keyed to a delayed FIRST byte, or to ANY long idle window
    // on an established connection? First chunk arrives at `pre`, second at
    // `pre + gap`. If the first arrives and the second never does, it is "any
    // long idle" and the delayed-first-byte framing is wrong.
    match run_once(inner, &url) {
        Ok(tl) => {
            println!(
                "[probe] GAP OK first_byte_ms={} last_byte_ms={} chunks={} body={}",
                opt_ms(tl.first_byte),
                opt_ms(tl.last_byte),
                tl.chunks,
                tl.body_bytes
            );
            print_ok(&tl);
            0
        }
        Err(e) => {
            print_fail(&e);
            println!(
                "[probe] GAP FAIL stage={} after_ms={} — {}",
                e.stage.as_str(),
                ms(e.after),
                if e.stage == Stage::FirstByte {
                    "died before the FIRST byte: delayed-first-byte class"
                } else {
                    "died MID-STREAM: any-long-idle class, not delayed-first-byte"
                }
            );
            1
        }
    }
}

// ============================================================================
// CLI
// ============================================================================

fn usage() -> ! {
    eprintln!("nettest-std <mode> [args]");
    eprintln!();
    eprintln!("  raw      <url>                     blocking std::net, http:// only");
    eprintln!("  poll     <url>                     O_NONBLOCK + poll(2), http:// only");
    eprintln!("  tls      <url>                     blocking rustls, https:// only");
    eprintln!("  rcvtimeo <url> <secs>              is SO_RCVTIMEO honoured?");
    eprintln!("  sweep    <base> [secs,secs,…] [raw|poll]   GET <base>/delay/<n> per n");
    eprintln!("  gap      <base> <pre> <gap> [raw|poll]     GET <base>/gap/<pre>/<gap>");
    eprintln!();
    eprintln!("<base> is scripts/net_delay_server.py, e.g. http://10.0.2.2:18080");
    eprintln!("env: NETTEST_READ_TIMEOUT=<s> NETTEST_ALL_CHUNKS=1 NETTEST_GAP_MS=<n>");
    std::process::exit(2);
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 3 {
        usage();
    }
    let mode = args[1].as_str();

    let code = match mode {
        "raw" | "poll" | "tls" => {
            println!("[probe] impl=std mode={mode} url={}", args[2]);
            match run_once(mode, &args[2]) {
                Ok(tl) => {
                    print_ok(&tl);
                    0
                }
                Err(e) => {
                    print_fail(&e);
                    1
                }
            }
        }
        "rcvtimeo" => {
            let secs = args.get(3).and_then(|v| v.parse().ok()).unwrap_or(2);
            println!("[probe] impl=std mode=rcvtimeo url={} secs={secs}", args[2]);
            mode_rcvtimeo(&args[2], secs)
        }
        "sweep" => {
            let delays: Vec<u64> = match args.get(3) {
                Some(s) if s.contains(',') || s.parse::<u64>().is_ok() => {
                    s.split(',').filter_map(|v| v.trim().parse().ok()).collect()
                }
                _ => default_delays(),
            };
            let inner = args
                .iter()
                .skip(3)
                .find(|a| a.as_str() == "raw" || a.as_str() == "poll")
                .map_or("raw", |s| s.as_str());
            println!("[probe] impl=std mode=sweep");
            mode_sweep(&args[2], &delays, inner)
        }
        "gap" => {
            let pre = args.get(3).and_then(|v| v.parse().ok()).unwrap_or(0);
            let gap = args.get(4).and_then(|v| v.parse().ok()).unwrap_or(10);
            let inner = args
                .iter()
                .skip(5)
                .find(|a| a.as_str() == "raw" || a.as_str() == "poll")
                .map_or("raw", |s| s.as_str());
            println!("[probe] impl=std mode=gap");
            mode_gap(&args[2], pre, gap, inner)
        }
        _ => usage(),
    };
    std::process::exit(code);
}
