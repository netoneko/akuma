//! nettest-connect — what does a non-blocking TCP connect actually report?
//!
//! # Why this probe exists
//!
//! `docs/archive/CARGO_CRATES_IO_CONNECT_FAIL.md` says nightly cargo's connects
//! to `index.crates.io:443` "never complete", on the strength of a kernel log
//! showing 110 × `socket()` + `connect() = EINPROGRESS` with zero completions.
//! Two things in that account do not fit each other:
//!
//! 1. libcurl gives up after **~353 ms**. A connect that genuinely hangs burns
//!    `CURLOPT_CONNECTTIMEOUT`, not a third of a second. Failing in roughly one
//!    round trip is the signature of an **error being reported**, not silence.
//! 2. The only reproducer is cargo itself. Every existing probe is four layers
//!    thick (cargo → `curl` crate → libcurl → OpenSSL), so nothing can say what
//!    the kernel answered — only that libcurl was unhappy.
//!
//! Reading the vendored libcurl this cargo is built from
//! (`curl-sys-0.4.90+curl-8.21.0`, the same crate + build script the nightly
//! toolchain links) settles what "unhappy" means. `curl/lib/cf-socket.c`,
//! `cf_tcp_connect()`:
//!
//! ```text
//!   rc = SOCKET_WRITABLE(ctx->sock, 0);          /* poll(POLLOUT), timeout 0 */
//!   if(rc == 0)                     -> "not connected yet", attempt stays ongoing
//!   else if(rc == CURL_CSELECT_OUT) -> verifyconnect(): getsockopt(SO_ERROR)
//!                                      == 0 ? connected : HARD FAIL
//!   else if(rc & CURL_CSELECT_ERR)  -> HARD FAIL
//! ```
//!
//! and `curl/lib/select.c`, `Curl_socket_check()`, for the write fd:
//!
//! ```text
//!   revents & (POLLWRNORM|POLLOUT)              -> CURL_CSELECT_OUT
//!   revents & (POLLERR|POLLHUP|POLLPRI|POLLNVAL)-> CURL_CSELECT_ERR
//! ```
//!
//! So there are exactly **two** ways for an attempt to die fast, and the `==`
//! (not `&`) in the `CURL_CSELECT_OUT` test means `POLLOUT|POLLHUP` takes the
//! error branch too:
//!
//! | what `poll` reports on the connecting fd | curl's verdict |
//! |---|---|
//! | `0`                                       | still connecting |
//! | `POLLOUT`, `SO_ERROR == 0`                | connected |
//! | `POLLOUT`, `SO_ERROR != 0`                | **hard fail** |
//! | anything with `POLLERR`/`POLLHUP`/`POLLNVAL`/`POLLPRI` | **hard fail** |
//!
//! Only when *every* attempt has hard-failed and the address list is exhausted
//! does `cf-ip-happy.c` emit `CURLE_COULDNT_CONNECT` — the observed `[7] Could
//! not connect to server`. An attempt that merely hangs keeps `ongoing > 0` and
//! cannot produce that message at 353 ms. **The failing connects are being
//! refused, not ignored** — this probe finds out by what.
//!
//! The kernel side of the same question (`src/syscall/poll.rs`,
//! `epoll_check_fd_readiness`) is worth knowing while reading the output:
//! `EPOLLHUP` is raised for a TCP socket whenever `socket_is_dead_tcp()`, i.e.
//! whenever smoltcp's `is_active()` is false — `Closed`, `TimeWait` or
//! `Listen` — and `sys_ppoll` passes `POLLHUP` through regardless of what the
//! caller asked for, exactly as Linux does. `POLLOUT` needs `can_send()`, which
//! is `Established`/`CloseWait` only. A socket sitting in `SynSent` therefore
//! polls as **0** and looks "still connecting" to curl, which is correct — so
//! any hard failure this probe records means the socket left `SynSent` for a
//! dead state, and the interesting question becomes *how fast* and *into what*.
//!
//! # Modes
//!
//! ```text
//! nettest-connect resolve <host>                  # getaddrinfo only, with timing
//! nettest-connect one     <host> <port>           # one attempt, full timeline
//! nettest-connect all     <host> <port>           # one attempt per resolved address
//! nettest-connect he      <host> <port>           # cargo's happy-eyeballs, emulated
//! nettest-connect churn   <host> <port> <n>       # n sequential attempts, histogram
//! ```
//!
//! Flags (any mode): `--wait poll0|poll|select|epoll` (default `poll0` — curl's
//! own zero-timeout query), `--timeout-ms N` (default 5000), `--sample-ms N`
//! (default 1), `--nonblock fcntl|sockflag` (default `fcntl`, what libcurl
//! does), `--soerr-every-sample` (see the hazard note below), `--quiet`.
//!
//! # Reading it
//!
//! Every mode ends in a `RESULT`/`SUMMARY` line carrying a verdict:
//!
//! | verdict | meaning |
//! |---|---|
//! | `CONNECTED`        | `POLLOUT` with `SO_ERROR == 0` — curl would proceed to TLS |
//! | `HARDFAIL_POLLERR` | `POLLERR`/`POLLHUP`/`POLLNVAL`/`POLLPRI` — curl aborts this address |
//! | `HARDFAIL_SOERROR` | `POLLOUT` but `SO_ERROR != 0` — curl aborts this address |
//! | `PENDING`          | still `SynSent` at `--timeout-ms`; this is what "never completes" would actually look like |
//!
//! `HARDFAIL_*` at ~one RTT reproduces the reported symptom and kills the
//! "connects never complete" wording. `PENDING` at 5 s confirms the wording and
//! moves the contradiction to libcurl's timing instead. Either answer closes a
//! gap; the probe is built so both are legible.
//!
//! # A/B against Linux
//!
//! This is a static `aarch64-unknown-linux-musl` binary, so the *same file*
//! runs under Docker Linux (`docs/archive/LINUX_AB_PROBE.md`, `scripts/probes/`).
//! Same argv, same output vocabulary: a verdict that differs between the two is
//! a kernel divergence, one that matches is libcurl's or the network's.
//!
//! # Hazard: SO_ERROR is consuming on Linux
//!
//! `getsockopt(SO_ERROR)` **clears** the pending socket error on Linux, so
//! sampling it on every poll iteration changes what a later reader sees and
//! would make the Linux control arm lie. Akuma computes `SO_ERROR` from the
//! smoltcp state on the fly (`src/syscall/net.rs`, `sys_getsockopt`) and clears
//! nothing, so the two kernels are not symmetric here. This probe therefore
//! reads `SO_ERROR` only where libcurl does — once, when the verdict is being
//! decided — unless `--soerr-every-sample` is passed to deliberately study that
//! axis.

use std::ffi::CString;
use std::os::raw::{c_int, c_short, c_void};

// Linux poll bits, spelled out rather than imported: the probe's whole output
// is these values, and `libc`'s aliases differ in width across targets.
const POLLIN: c_short = 0x001;
const POLLPRI: c_short = 0x002;
const POLLOUT: c_short = 0x004;
const POLLERR: c_short = 0x008;
const POLLHUP: c_short = 0x010;
const POLLNVAL: c_short = 0x020;
const POLLRDNORM: c_short = 0x040;
const POLLWRNORM: c_short = 0x100;

// curl's CURL_CSELECT_* (curl/lib/select.h).
const CSELECT_IN: u32 = 0x01;
const CSELECT_OUT: u32 = 0x02;
const CSELECT_ERR: u32 = 0x04;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Verdict {
    Pending,
    Connected,
    HardFailPollErr,
    HardFailSoError,
    ConnectFailed,
}

impl Verdict {
    fn name(self) -> &'static str {
        match self {
            Verdict::Pending => "PENDING",
            Verdict::Connected => "CONNECTED",
            Verdict::HardFailPollErr => "HARDFAIL_POLLERR",
            Verdict::HardFailSoError => "HARDFAIL_SOERROR",
            Verdict::ConnectFailed => "CONNECT_FAILED",
        }
    }
    fn is_hard_fail(self) -> bool {
        matches!(
            self,
            Verdict::HardFailPollErr | Verdict::HardFailSoError | Verdict::ConnectFailed
        )
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum WaitMode {
    Poll0,
    Poll,
    Select,
    Epoll,
}

impl WaitMode {
    fn parse(s: &str) -> Option<WaitMode> {
        match s {
            "poll0" => Some(WaitMode::Poll0),
            "poll" => Some(WaitMode::Poll),
            "select" => Some(WaitMode::Select),
            "epoll" => Some(WaitMode::Epoll),
            _ => None,
        }
    }
    fn name(self) -> &'static str {
        match self {
            WaitMode::Poll0 => "poll0",
            WaitMode::Poll => "poll",
            WaitMode::Select => "select",
            WaitMode::Epoll => "epoll",
        }
    }
}

struct Opts {
    wait: WaitMode,
    timeout_ms: f64,
    sample_ms: i32,
    nonblock_via_sockflag: bool,
    soerr_every_sample: bool,
    quiet: bool,
}

impl Default for Opts {
    fn default() -> Self {
        Opts {
            // curl's own query in cf_tcp_connect() is a zero-timeout poll, so
            // that is the default: it measures the kernel's LEVEL state without
            // ever asking it to block. `--wait poll` (a blocking poll) is the
            // discriminator for a lost wake-up: readiness that only appears
            // when someone actually waits is a very different bug from
            // readiness that never appears at all.
            wait: WaitMode::Poll0,
            timeout_ms: 5000.0,
            sample_ms: 1,
            nonblock_via_sockflag: false,
            soerr_every_sample: false,
            quiet: false,
        }
    }
}

fn now_ms() -> f64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as f64 * 1000.0 + ts.tv_nsec as f64 / 1_000_000.0
}

fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

fn errno_name(e: i32) -> &'static str {
    match e {
        0 => "0",
        libc::EINPROGRESS => "EINPROGRESS",
        libc::ECONNREFUSED => "ECONNREFUSED",
        libc::ECONNRESET => "ECONNRESET",
        libc::ETIMEDOUT => "ETIMEDOUT",
        libc::EHOSTUNREACH => "EHOSTUNREACH",
        libc::ENETUNREACH => "ENETUNREACH",
        libc::ENETDOWN => "ENETDOWN",
        libc::EADDRNOTAVAIL => "EADDRNOTAVAIL",
        libc::EISCONN => "EISCONN",
        libc::EALREADY => "EALREADY",
        libc::EAGAIN => "EAGAIN",
        libc::EBADF => "EBADF",
        libc::EMFILE => "EMFILE",
        libc::ENOMEM => "ENOMEM",
        libc::EINVAL => "EINVAL",
        libc::EAFNOSUPPORT => "EAFNOSUPPORT",
        libc::EOPNOTSUPP => "EOPNOTSUPP",
        _ => "?",
    }
}

fn decode_revents(r: c_short) -> String {
    if r == 0 {
        return "0".to_string();
    }
    let mut out = String::new();
    let bits = [
        (POLLIN, "IN"),
        (POLLPRI, "PRI"),
        (POLLOUT, "OUT"),
        (POLLERR, "ERR"),
        (POLLHUP, "HUP"),
        (POLLNVAL, "NVAL"),
        (POLLRDNORM, "RDNORM"),
        (POLLWRNORM, "WRNORM"),
    ];
    for (bit, name) in bits {
        if r & bit != 0 {
            if !out.is_empty() {
                out.push('|');
            }
            out.push_str(name);
        }
    }
    let known: c_short = bits.iter().fold(0, |a, (b, _)| a | b);
    if r & !known != 0 {
        out.push_str("|?");
    }
    out
}

/// `Curl_socket_check()`'s mapping for the **write** fd, verbatim
/// (curl/lib/select.c). Reproduced rather than approximated because the whole
/// point is to say what libcurl would have concluded from these exact bits.
fn curl_cselect_writefd(revents: c_short) -> u32 {
    let mut r = 0;
    if revents & (POLLWRNORM | POLLOUT) != 0 {
        r |= CSELECT_OUT;
    }
    if revents & (POLLERR | POLLHUP | POLLPRI | POLLNVAL) != 0 {
        r |= CSELECT_ERR;
    }
    r
}

fn cselect_name(r: u32) -> String {
    if r == 0 {
        return "0".to_string();
    }
    let mut s = String::new();
    for (bit, name) in [(CSELECT_IN, "IN"), (CSELECT_OUT, "OUT"), (CSELECT_ERR, "ERR")] {
        if r & bit != 0 {
            if !s.is_empty() {
                s.push('|');
            }
            s.push_str(name);
        }
    }
    s
}

/// Which kernel code path produced these `revents`.
///
/// The trace behind this table (see the README, Part 3) is what makes the bits
/// worth printing. In `epoll_check_fd_readiness` (`src/syscall/poll.rs`) a TCP
/// socket takes one of two mutually exclusive branches — `EPOLLHUP` when
/// `socket_is_dead_tcp()`, otherwise `EPOLLIN`/`EPOLLOUT`/`EPOLLRDHUP` — so
/// Akuma **cannot** report `POLLOUT` together with `POLLHUP` for a socket, and
/// the only way a socket fd gets `POLLERR` at all is the fd-lookup miss at the
/// top of that function. Each fingerprint therefore names a distinct origin:
///
/// * `HUP` alone — smoltcp reached a `!is_active()` state (`Closed`/`TimeWait`):
///   an RST arrived, or the kernel's `CONNECT_TIMEOUT_US` sweep gave up on a
///   `SynSent` socket. `SO_ERROR` tells those apart — `ECONNREFUSED` for the
///   former, `ETIMEDOUT` for the latter.
/// * `ERR|HUP` — `current_process_shared()`/`get_fd()` returned `None`; the poll
///   never looked at a socket at all. Thread-shaped, not network-shaped.
/// * `OUT` with `ERR`/`HUP` — the Linux control arm's shape for a refused
///   connect. Akuma cannot produce it; seeing it here means you are on Linux.
fn akuma_fingerprint(revents: c_short) -> Option<&'static str> {
    if revents <= 0 {
        return None;
    }
    let has_out = revents & (POLLOUT | POLLWRNORM) != 0;
    let has_err = revents & POLLERR != 0;
    let has_hup = revents & POLLHUP != 0;
    match (has_out, has_err, has_hup) {
        (false, false, true) => Some(
            "HUP-only: smoltcp socket left SynSent for a dead state - an RST arrived, or the kernel's CONNECT_TIMEOUT_US deadline fired (SO_ERROR then reads ETIMEDOUT)",
        ),
        (false, true, true) => Some(
            "ERR|HUP: fd lookup miss in epoll_check_fd_readiness (src/syscall/poll.rs) - no socket state was consulted",
        ),
        (true, _, true) | (true, true, _) => Some(
            "OUT+ERR/HUP: Linux-shaped refusal; Akuma's poll cannot emit OUT together with HUP for a socket",
        ),
        _ => None,
    }
}

fn so_error(fd: c_int) -> i32 {
    let mut val: c_int = 0;
    let mut len = std::mem::size_of::<c_int>() as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_ERROR,
            &mut val as *mut _ as *mut c_void,
            &mut len,
        )
    };
    // libcurl's verifyconnect(): a getsockopt that itself fails is treated as
    // "the error is in errno".
    if rc != 0 {
        errno()
    } else {
        val as i32
    }
}

// ---------------------------------------------------------------------------
// Address handling
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct V4 {
    ip: [u8; 4],
    port: u16,
}

impl V4 {
    fn to_string(self) -> String {
        format!(
            "{}.{}.{}.{}:{}",
            self.ip[0], self.ip[1], self.ip[2], self.ip[3], self.port
        )
    }
    fn sockaddr(self) -> libc::sockaddr_in {
        libc::sockaddr_in {
            sin_family: libc::AF_INET as libc::sa_family_t,
            sin_port: self.port.to_be(),
            sin_addr: libc::in_addr {
                s_addr: u32::from_be_bytes(self.ip).to_be(),
            },
            sin_zero: [0; 8],
        }
    }
}

/// Resolve through **musl's `getaddrinfo`** — the same resolver the nightly
/// toolchain's threaded-resolver libcurl calls (`USE_RESOLV_THREADED` in
/// curl-sys' build.rs), as opposed to apk libcurl's c-ares. Only `AF_INET` is
/// requested: `sys_socket` rejects `AF_INET6` outright
/// (`src/syscall/net.rs`, `domain != 2 => EAFNOSUPPORT`), so libcurl's
/// `Curl_ipv6works()` probe fails in the guest and no AAAA is ever tried.
fn resolve(host: &str, port: u16) -> Result<Vec<V4>, String> {
    // A literal address skips DNS entirely, which is how the DNS axis is held
    // fixed when the connect axis is under test.
    let parts: Vec<&str> = host.split('.').collect();
    if parts.len() == 4 {
        if let Ok(octets) = parts
            .iter()
            .map(|p| p.parse::<u8>())
            .collect::<Result<Vec<u8>, _>>()
        {
            return Ok(vec![V4 {
                ip: [octets[0], octets[1], octets[2], octets[3]],
                port,
            }]);
        }
    }

    let chost = CString::new(host).map_err(|_| "bad host".to_string())?;
    let cport = CString::new(port.to_string()).unwrap();
    let mut hints: libc::addrinfo = unsafe { std::mem::zeroed() };
    hints.ai_family = libc::AF_INET;
    hints.ai_socktype = libc::SOCK_STREAM;
    let mut res: *mut libc::addrinfo = std::ptr::null_mut();
    let rc = unsafe { libc::getaddrinfo(chost.as_ptr(), cport.as_ptr(), &hints, &mut res) };
    if rc != 0 {
        return Err(format!("getaddrinfo rc={rc}"));
    }
    let mut out = Vec::new();
    let mut p = res;
    while !p.is_null() {
        unsafe {
            if (*p).ai_family == libc::AF_INET && !(*p).ai_addr.is_null() {
                let sa = &*((*p).ai_addr as *const libc::sockaddr_in);
                let ip = u32::from_be(sa.sin_addr.s_addr).to_be_bytes();
                out.push(V4 {
                    ip,
                    port: u16::from_be(sa.sin_port),
                });
            }
            p = (*p).ai_next;
        }
    }
    unsafe { libc::freeaddrinfo(res) };
    if out.is_empty() {
        return Err("no A records".to_string());
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// One connect attempt, driven exactly like libcurl drives one
// ---------------------------------------------------------------------------

struct Attempt {
    fd: c_int,
    addr: V4,
    started: f64,
    verdict: Verdict,
    verdict_at: f64,
    /// `SO_ERROR` as read at the moment the verdict was decided.
    sockerr: i32,
    last_revents: c_short,
    samples: u64,
    /// Local port from `getsockname`, so port reuse across a churn run is
    /// visible in the log without a packet capture.
    local_port: u16,
}

impl Attempt {
    /// `socket()` + optional `fcntl(O_NONBLOCK)` + `connect()`. libcurl sets
    /// non-blocking with `fcntl` after `socket()` (`curlx_nonblock`), not with
    /// `SOCK_NONBLOCK`; `--nonblock sockflag` flips that so the two paths can be
    /// compared — akuma handles them in different places (`sys_socket`'s
    /// `sock_type & 0x800` vs `sys_fcntl`).
    fn start(addr: V4, opts: &Opts, trace: bool) -> Attempt {
        let started = now_ms();
        let ty = if opts.nonblock_via_sockflag {
            libc::SOCK_STREAM | libc::SOCK_NONBLOCK
        } else {
            libc::SOCK_STREAM
        };
        let fd = unsafe { libc::socket(libc::AF_INET, ty, 0) };
        let mut a = Attempt {
            fd,
            addr,
            started,
            verdict: Verdict::Pending,
            verdict_at: 0.0,
            sockerr: 0,
            last_revents: -1,
            samples: 0,
            local_port: 0,
        };
        if fd < 0 {
            let e = errno();
            if trace {
                println!(
                    "[probe]   t={:.1}ms socket() FAILED errno={} {}",
                    now_ms() - started,
                    e,
                    errno_name(e)
                );
            }
            a.verdict = Verdict::ConnectFailed;
            a.sockerr = e;
            a.verdict_at = now_ms() - started;
            return a;
        }
        if trace {
            println!("[probe]   t={:.1}ms socket() = fd {}", now_ms() - started, fd);
        }
        if !opts.nonblock_via_sockflag {
            unsafe {
                let fl = libc::fcntl(fd, libc::F_GETFL, 0);
                libc::fcntl(fd, libc::F_SETFL, fl | libc::O_NONBLOCK);
            }
        }

        let sa = addr.sockaddr();
        let rc = unsafe {
            libc::connect(
                fd,
                &sa as *const _ as *const libc::sockaddr,
                std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
            )
        };
        let e = if rc == 0 { 0 } else { errno() };
        if trace {
            println!(
                "[probe]   t={:.1}ms connect({}) rc={} errno={} {}",
                now_ms() - started,
                addr.to_string(),
                rc,
                e,
                errno_name(e)
            );
        }
        a.local_port = local_port(fd);
        if rc == 0 {
            // Immediate success: possible on loopback, never to Fastly.
            a.verdict = Verdict::Connected;
            a.verdict_at = now_ms() - started;
        } else if e != libc::EINPROGRESS {
            // libcurl's `socket_connect_result()` path — a hard failure before
            // any polling happens at all.
            a.verdict = Verdict::ConnectFailed;
            a.sockerr = e;
            a.verdict_at = now_ms() - started;
        }
        a
    }

    /// One readiness observation, mapped through libcurl's decision table. Returns
    /// true once a verdict is reached.
    fn step(&mut self, opts: &Opts, trace: bool) -> bool {
        if self.verdict != Verdict::Pending {
            return true;
        }
        let (revents, prc) = observe(self.fd, opts);
        self.samples += 1;

        let changed = revents != self.last_revents;
        self.last_revents = revents;

        let sample_err = if opts.soerr_every_sample {
            Some(so_error(self.fd))
        } else {
            None
        };

        if trace && (changed || opts.soerr_every_sample) {
            println!(
                "[probe]   t={:.1}ms {}() rc={} revents={} (0x{:x}){}",
                now_ms() - self.started,
                opts.wait.name(),
                prc,
                decode_revents(revents),
                revents,
                match sample_err {
                    Some(e) => format!(" so_error={} {}", e, errno_name(e)),
                    None => String::new(),
                }
            );
        }

        if prc < 0 {
            let e = errno();
            // A readiness syscall that itself errors is its own finding: it is
            // neither "connect hung" nor "connect refused".
            if trace {
                println!(
                    "[probe]   t={:.1}ms {}() FAILED errno={} {}",
                    now_ms() - self.started,
                    opts.wait.name(),
                    e,
                    errno_name(e)
                );
            }
            self.verdict = Verdict::ConnectFailed;
            self.sockerr = e;
            self.verdict_at = now_ms() - self.started;
            return true;
        }

        let cs = curl_cselect_writefd(revents);
        if prc == 0 || cs == 0 {
            // curl: "not connected yet" — the attempt stays ongoing.
            return false;
        }

        // From here on we are inside cf_tcp_connect()'s decision.
        if cs == CSELECT_OUT {
            let err = so_error(self.fd);
            self.sockerr = err;
            self.verdict_at = now_ms() - self.started;
            self.verdict = if err == 0 || err == libc::EISCONN {
                Verdict::Connected
            } else {
                Verdict::HardFailSoError
            };
        } else {
            // Includes OUT|ERR: cf_tcp_connect tests `rc == CURL_CSELECT_OUT`
            // by equality, so POLLOUT arriving together with POLLHUP is a
            // failure, not a success.
            self.sockerr = so_error(self.fd);
            self.verdict_at = now_ms() - self.started;
            self.verdict = Verdict::HardFailPollErr;
        }
        if trace {
            println!(
                "[probe]   t={:.1}ms curl_cselect={} so_error={} {} -> {}",
                self.verdict_at,
                cselect_name(cs),
                self.sockerr,
                errno_name(self.sockerr),
                self.verdict.name()
            );
        }
        true
    }

    fn close(&mut self) {
        if self.fd >= 0 {
            unsafe { libc::close(self.fd) };
            self.fd = -1;
        }
    }
}

fn local_port(fd: c_int) -> u16 {
    let mut sa: libc::sockaddr_in = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
    let rc = unsafe { libc::getsockname(fd, &mut sa as *mut _ as *mut libc::sockaddr, &mut len) };
    if rc == 0 {
        u16::from_be(sa.sin_port)
    } else {
        0
    }
}

/// The readiness observation itself. Four ways of asking the same question, so
/// a divergence between them localises the fault to one syscall:
/// `poll0`/`poll` → `sys_ppoll`, `select` → `sys_pselect6`, `epoll` →
/// `sys_epoll_pwait`. `sys_pselect6` in particular translates only `EPOLLIN`
/// and `EPOLLOUT` and drops `EPOLLERR`/`EPOLLHUP` on the floor, so a socket that
/// polls as `HUP` is expected to select as *nothing* — that difference is a
/// deliberate part of the matrix, not noise.
fn observe(fd: c_int, opts: &Opts) -> (c_short, c_int) {
    match opts.wait {
        WaitMode::Poll0 | WaitMode::Poll => {
            let timeout = if opts.wait == WaitMode::Poll0 {
                0
            } else {
                opts.sample_ms
            };
            // The exact event set libcurl asks for on a write fd.
            let mut pfd = libc::pollfd {
                fd,
                events: POLLWRNORM | POLLOUT | POLLPRI,
                revents: 0,
            };
            let rc = unsafe { libc::poll(&mut pfd, 1, timeout) };
            (pfd.revents, rc)
        }
        WaitMode::Select => {
            let mut wset: libc::fd_set = unsafe { std::mem::zeroed() };
            let mut eset: libc::fd_set = unsafe { std::mem::zeroed() };
            unsafe {
                libc::FD_SET(fd, &mut wset);
                libc::FD_SET(fd, &mut eset);
            }
            let mut tv = libc::timeval {
                tv_sec: 0,
                tv_usec: (opts.sample_ms.max(0) as i64) * 1000,
            };
            let rc = unsafe {
                libc::select(
                    fd + 1,
                    std::ptr::null_mut(),
                    &mut wset,
                    &mut eset,
                    &mut tv,
                )
            };
            let mut revents = 0;
            if rc > 0 {
                unsafe {
                    if libc::FD_ISSET(fd, &wset) {
                        revents |= POLLOUT;
                    }
                    if libc::FD_ISSET(fd, &eset) {
                        revents |= POLLPRI;
                    }
                }
            }
            (revents, rc)
        }
        WaitMode::Epoll => {
            let ep = unsafe { libc::epoll_create1(0) };
            if ep < 0 {
                return (0, -1);
            }
            let mut ev = libc::epoll_event {
                events: (POLLOUT as u32) | (POLLPRI as u32),
                u64: 0,
            };
            let rc_ctl = unsafe { libc::epoll_ctl(ep, libc::EPOLL_CTL_ADD, fd, &mut ev) };
            if rc_ctl < 0 {
                unsafe { libc::close(ep) };
                return (0, -1);
            }
            let mut out = libc::epoll_event { events: 0, u64: 0 };
            let rc = unsafe { libc::epoll_wait(ep, &mut out, 1, opts.sample_ms) };
            unsafe { libc::close(ep) };
            // EPOLL* and POLL* share numeric values for IN/OUT/ERR/HUP on Linux.
            let revents = if rc > 0 { out.events as c_short } else { 0 };
            (revents, rc)
        }
    }
}

fn sleep_ms(ms: i32) {
    if ms <= 0 {
        return;
    }
    let ts = libc::timespec {
        tv_sec: (ms / 1000) as _,
        tv_nsec: ((ms % 1000) as i64) * 1_000_000,
    };
    unsafe { libc::nanosleep(&ts, std::ptr::null_mut()) };
}

/// Run one attempt to a verdict (or to `--timeout-ms`). `trace` prints the
/// per-sample timeline; `print_result` prints the one-line verdict. They are
/// separate because `--quiet` should drop the timeline without ever dropping the
/// verdict — a run whose result line can be silenced by a flag is a run whose
/// output cannot be diffed against the Linux arm.
fn run_one(addr: V4, opts: &Opts, trace: bool, print_result: bool) -> Attempt {
    if trace {
        println!(
            "[probe] connect {} wait={} nonblock={}",
            addr.to_string(),
            opts.wait.name(),
            if opts.nonblock_via_sockflag { "SOCK_NONBLOCK" } else { "fcntl" }
        );
    }
    let mut a = Attempt::start(addr, opts, trace);
    while a.verdict == Verdict::Pending {
        if a.step(opts, trace) {
            break;
        }
        if now_ms() - a.started > opts.timeout_ms {
            a.verdict_at = now_ms() - a.started;
            // Read SO_ERROR once at give-up so a silent socket can be told from
            // one that was sitting on an unreported error the whole time.
            a.sockerr = so_error(a.fd);
            break;
        }
        if opts.wait == WaitMode::Poll0 {
            sleep_ms(opts.sample_ms);
        }
    }
    if print_result {
        println!(
            "[probe] RESULT {} verdict={} t={:.1}ms so_error={} {} revents={} samples={} local_port={}",
            addr.to_string(),
            a.verdict.name(),
            a.verdict_at,
            a.sockerr,
            errno_name(a.sockerr),
            decode_revents(a.last_revents.max(0)),
            a.samples,
            a.local_port
        );
        if let Some(h) = akuma_fingerprint(a.last_revents) {
            println!("[probe]   hint: {}", h);
        }
    }
    a
}

// ---------------------------------------------------------------------------
// Happy eyeballs — cargo's actual connect pattern, without libcurl
// ---------------------------------------------------------------------------

/// A faithful-enough re-implementation of `cf_ip_ballers_run()`
/// (curl/lib/cf-ip-happy.c) for one hostname:
///
/// * start one attempt per address, a new one every `attempt_delay_ms`
///   (`CURLOPT_HAPPY_EYEBALLS_TIMEOUT_MS`, default 200) while others are ongoing,
/// * at most `IP_HE_MAX_CONCURRENT_ATTEMPTS` (6) alive, oldest pruned to make
///   room — this is what produces the socket churn in the kernel log,
/// * finish the moment one attempt connects,
/// * report `CURLE_COULDNT_CONNECT` only when the address list is exhausted and
///   no attempt is still ongoing.
///
/// That last condition is the one the archive doc's account cannot satisfy: an
/// attempt that hangs is *ongoing*, and ongoing attempts prevent the error. If
/// this mode prints `COULDNT_CONNECT` at a few hundred ms, it has reproduced
/// cargo's message with no libcurl in the picture at all.
fn run_happy_eyeballs(addrs: &[V4], opts: &Opts) -> i32 {
    const MAX_CONCURRENT: usize = 6;
    let attempt_delay_ms = 200.0;

    let t0 = now_ms();
    let mut next_addr = 0usize;
    let mut running: Vec<Attempt> = Vec::new();
    let mut last_started = 0.0f64;
    let mut hard_failures = 0u32;

    println!(
        "[probe] he addresses={} max_concurrent={} attempt_delay={}ms wait={}",
        addrs.len(),
        MAX_CONCURRENT,
        attempt_delay_ms,
        opts.wait.name()
    );

    loop {
        // Evaluate every running attempt.
        let mut ongoing = 0;
        let mut i = 0;
        while i < running.len() {
            let done = running[i].step(opts, false);
            if done {
                let a = &mut running[i];
                let v = a.verdict;
                println!(
                    "[probe]   attempt {} -> {} t={:.1}ms so_error={} {} revents={}",
                    a.addr.to_string(),
                    v.name(),
                    a.verdict_at,
                    a.sockerr,
                    errno_name(a.sockerr),
                    decode_revents(a.last_revents.max(0))
                );
                if let Some(h) = akuma_fingerprint(a.last_revents) {
                    println!("[probe]     hint: {}", h);
                }
                if v == Verdict::Connected {
                    println!(
                        "[probe] SUMMARY he verdict=CONNECTED winner={} total={:.1}ms hard_failures={}",
                        a.addr.to_string(),
                        now_ms() - t0,
                        hard_failures
                    );
                    for a in running.iter_mut() {
                        a.close();
                    }
                    return 0;
                }
                if v.is_hard_fail() {
                    hard_failures += 1;
                }
                running[i].close();
                running.remove(i);
                continue;
            }
            ongoing += 1;
            i += 1;
        }

        let elapsed = now_ms() - t0;
        let more_addrs = next_addr < addrs.len();
        let do_more = if ongoing == 0 {
            true
        } else {
            more_addrs && (now_ms() - last_started) >= attempt_delay_ms
        };

        if do_more {
            if more_addrs {
                if running.len() >= MAX_CONCURRENT {
                    // "Discard oldest to make room for new attempt."
                    running[0].close();
                    running.remove(0);
                }
                let a = Attempt::start(addrs[next_addr], opts, false);
                println!(
                    "[probe]   start attempt #{} {} local_port={}",
                    next_addr,
                    addrs[next_addr].to_string(),
                    a.local_port
                );
                next_addr += 1;
                last_started = now_ms();
                running.push(a);
                continue;
            } else if ongoing == 0 {
                // cf_ip_ballers_run(): "no more attempts to try".
                println!(
                    "[probe] SUMMARY he verdict=COULDNT_CONNECT total={:.1}ms hard_failures={} \
                     (this is cargo's `[7] Could not connect to server` condition)",
                    elapsed, hard_failures
                );
                return 7;
            }
        }

        if elapsed > opts.timeout_ms {
            println!(
                "[probe] SUMMARY he verdict=PENDING total={:.1}ms ongoing={} hard_failures={} \
                 (attempts still connecting — this is what \"connects never complete\" looks like)",
                elapsed, ongoing, hard_failures
            );
            for a in running.iter_mut() {
                a.close();
            }
            return 1;
        }
        sleep_ms(opts.sample_ms.max(1));
    }
}

// ---------------------------------------------------------------------------
// Churn — is it the Nth connect that breaks, not the first?
// ---------------------------------------------------------------------------

/// `n` sequential attempts, each closed before the next starts. Catches the
/// resource-shaped explanations the one-shot modes cannot see: the socket table
/// (`MAX_SOCKETS = 128`), ephemeral ports (`alloc_ephemeral_port()` hands out
/// 49152..65535 monotonically and never reuses within a boot), and any smoltcp
/// handle that a `close()` fails to release. A run that succeeds early and
/// fails late — with `local_port` climbing in the log — is a very different bug
/// from one that fails from the first attempt.
fn run_churn(addrs: &[V4], n: u32, opts: &Opts) -> i32 {
    let t0 = now_ms();
    let mut counts: [u32; 5] = [0; 5];
    let mut first_fail: Option<(u32, Verdict, i32)> = None;
    for i in 0..n {
        let addr = addrs[(i as usize) % addrs.len()];
        let mut a = run_one(addr, opts, !opts.quiet && i == 0, false);
        let slot = match a.verdict {
            Verdict::Pending => 0,
            Verdict::Connected => 1,
            Verdict::HardFailPollErr => 2,
            Verdict::HardFailSoError => 3,
            Verdict::ConnectFailed => 4,
        };
        counts[slot] += 1;
        if a.verdict != Verdict::Connected && first_fail.is_none() {
            first_fail = Some((i, a.verdict, a.sockerr));
        }
        {
            // One line per attempt is the right granularity for a churn run:
            // the local port is the exhaustion signal.
            println!(
                "[probe]   #{} {} {} t={:.1}ms so_error={} local_port={}",
                i,
                addr.to_string(),
                a.verdict.name(),
                a.verdict_at,
                a.sockerr,
                a.local_port
            );
        }
        a.close();
    }
    println!(
        "[probe] SUMMARY churn n={} connected={} pending={} hardfail_pollerr={} \
         hardfail_soerror={} connect_failed={} total={:.1}ms",
        n, counts[1], counts[0], counts[2], counts[3], counts[4],
        now_ms() - t0
    );
    match first_fail {
        Some((i, v, e)) => {
            println!(
                "[probe] SUMMARY churn first_failure_at={} verdict={} so_error={} {}",
                i,
                v.name(),
                e,
                errno_name(e)
            );
            1
        }
        None => 0,
    }
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

fn usage() -> ! {
    eprintln!(
        "usage:
  nettest-connect resolve <host>
  nettest-connect one     <host> <port>
  nettest-connect all     <host> <port>
  nettest-connect he      <host> <port>
  nettest-connect churn   <host> <port> <n>

flags:
  --wait poll0|poll|select|epoll   readiness syscall (default poll0, libcurl's own)
  --timeout-ms N                   per-attempt give-up (default 5000)
  --sample-ms N                    poll cadence / blocking timeout (default 1)
  --nonblock fcntl|sockflag        how O_NONBLOCK is set (default fcntl, libcurl's way)
  --soerr-every-sample             read SO_ERROR every iteration (consuming on Linux!)
  --quiet                          one line per attempt instead of a timeline"
    );
    std::process::exit(2)
}

fn main() {
    let argv: Vec<String> = std::env::args().collect();
    if argv.len() < 3 {
        usage();
    }

    let mut opts = Opts::default();
    let mut positional: Vec<String> = Vec::new();
    let mut i = 1;
    while i < argv.len() {
        match argv[i].as_str() {
            "--wait" => {
                i += 1;
                opts.wait = argv.get(i).and_then(|s| WaitMode::parse(s)).unwrap_or_else(|| usage());
            }
            "--timeout-ms" => {
                i += 1;
                opts.timeout_ms = argv.get(i).and_then(|s| s.parse().ok()).unwrap_or_else(|| usage());
            }
            "--sample-ms" => {
                i += 1;
                opts.sample_ms = argv.get(i).and_then(|s| s.parse().ok()).unwrap_or_else(|| usage());
            }
            "--nonblock" => {
                i += 1;
                opts.nonblock_via_sockflag = match argv.get(i).map(|s| s.as_str()) {
                    Some("sockflag") => true,
                    Some("fcntl") => false,
                    _ => usage(),
                };
            }
            "--soerr-every-sample" => opts.soerr_every_sample = true,
            "--quiet" => opts.quiet = true,
            s if s.starts_with("--") => usage(),
            s => positional.push(s.to_string()),
        }
        i += 1;
    }

    if positional.len() < 2 {
        usage();
    }
    let mode = positional[0].as_str();
    let host = positional[1].clone();

    if mode == "resolve" {
        let t0 = now_ms();
        match resolve(&host, 443) {
            Ok(addrs) => {
                println!(
                    "[probe] resolve {} -> {} address(es) in {:.1}ms",
                    host,
                    addrs.len(),
                    now_ms() - t0
                );
                for a in &addrs {
                    println!("[probe]   {}", a.to_string());
                }
                std::process::exit(0);
            }
            Err(e) => {
                println!(
                    "[probe] resolve {} FAILED after {:.1}ms: {}",
                    host,
                    now_ms() - t0,
                    e
                );
                std::process::exit(1);
            }
        }
    }

    if positional.len() < 3 {
        usage();
    }
    let port: u16 = match positional[2].parse() {
        Ok(p) => p,
        Err(_) => usage(),
    };

    let t_dns = now_ms();
    let addrs = match resolve(&host, port) {
        Ok(a) => a,
        Err(e) => {
            println!("[probe] resolve {host} FAILED: {e}");
            std::process::exit(1);
        }
    };
    println!(
        "[probe] resolve {} -> {} address(es) in {:.1}ms",
        host,
        addrs.len(),
        now_ms() - t_dns
    );

    let rc = match mode {
        "one" => {
            let mut a = run_one(addrs[0], &opts, !opts.quiet, true);
            let v = a.verdict;
            a.close();
            if v == Verdict::Connected { 0 } else { 1 }
        }
        "all" => {
            let mut bad = 0;
            for addr in &addrs {
                let mut a = run_one(*addr, &opts, !opts.quiet, true);
                if a.verdict != Verdict::Connected {
                    bad += 1;
                }
                a.close();
            }
            println!(
                "[probe] SUMMARY all addresses={} failed={}",
                addrs.len(),
                bad
            );
            if bad == 0 { 0 } else { 1 }
        }
        "he" => run_happy_eyeballs(&addrs, &opts),
        "churn" => {
            if positional.len() < 4 {
                usage();
            }
            let n: u32 = positional[3].parse().unwrap_or_else(|_| usage());
            run_churn(&addrs, n, &opts)
        }
        _ => usage(),
    };
    std::process::exit(rc);
}
