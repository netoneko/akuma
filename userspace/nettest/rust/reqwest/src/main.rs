//! nettest-reqwest — nca's exact network stack, reduced to a stopwatch.
//!
//! Companion to `../stdlib/` (no runtime, no reactor, `std::net` + `poll(2)`).
//! Both probes take the same subcommands and print the same `[probe]` line
//! vocabulary, so a run of each against the same URL diffs directly.
//!
//! # What this is for
//!
//! `docs/archive/SOCKET_DELAYED_FIRST_BYTE_HANG.md`: nca talking to host Ollama
//! over SLIRP completes every round trip when the model prefills in ~1 s, and
//! blocks forever in read when the model takes ~10 s before its first response
//! byte. The same binary succeeds through a host-side proxy that answers
//! instantly and then pipes chunks — so the discriminator is *timing*, not
//! topology. What that doc could not say is which layer stalls, because nca is
//! tokio + hyper + reqwest + rustls + an agent loop, and only the whole thing
//! was ever run.
//!
//! This probe is nca's four network layers with the agent loop deleted:
//!
//! - `tokio` 1.x multi-thread runtime (nca's `#[tokio::main]` default), so the
//!   reactor is mio's `epoll_pwait` on `O_NONBLOCK` sockets — the readiness
//!   path, not the blocking-recv path.
//! - `hyper` 1.x via `reqwest` 0.12 with `default-features = false` +
//!   `rustls-tls`, byte-for-byte nca's dependency line.
//!
//! Run it beside `nettest-std` and the failure localises. See
//! `docs/runbooks/debug-delayed-first-byte.md` for the result matrix.
//!
//! # Usage
//!
//! ```text
//! nettest-reqwest get    <url>                     one GET, full timeline
//! nettest-reqwest stream <url>                     GET, one line per body chunk
//! nettest-reqwest post   <url> <kb>                POST <kb> KiB of JSON-ish body
//! nettest-reqwest sweep  <base> [secs,secs,…]      GET <base>/delay/<n> per n
//! nettest-reqwest gap    <base> <pre> <gap>        GET <base>/gap/<pre>/<gap>
//! nettest-reqwest reuse  <base> [idle]             GET <base>/keepalive/<idle> twice, across
//!                                                   the server's own idle-close of the pool
//! ```
//!
//! `<base>` is the host-side delay server: run `scripts/net_delay_server.py` on
//! the host and point the guest at `http://10.0.2.2:18080` (guest→host over
//! SLIRP needs no `hostfwd` rule).
//!
//! # Environment
//!
//! - `NETTEST_RT=current|multi` — tokio runtime flavour (default `multi`, what
//!   nca uses). `current` collapses reactor and application onto one thread,
//!   which is the configuration where a blocking syscall inside the reactor
//!   stalls everything — worth testing separately.
//! - `NETTEST_TIMEOUT=<secs>` — `Client::timeout`. Unset means no timeout,
//!   which is what lets a hang actually hang. Set it to bound a sweep.
//! - `NETTEST_NEW_CLIENT=1` — build a fresh `Client` per sweep row, defeating
//!   connection reuse. Default reuses one `Client` across the sweep, matching
//!   nca (and matching the archive doc's keep-alive observation).
//! - `NETTEST_HTTP1=1` — force HTTP/1.1 (`http1_only`). Splits "HTTP/2 over
//!   TLS" off as its own axis, the way the sibling curl probe's
//!   `easy11`/`easy2` modes do.
//! - `NETTEST_ALL_CHUNKS=1`, `NETTEST_GAP_MS=<n>` — chunk logging, same
//!   meaning as in `nettest-std`.
//!
//! # Build
//!
//! `../build-musl.sh`. Output lands in `bootstrap/bin/`, so
//! `scripts/populate_disk.sh` ships it to `/bin`.

use std::env;
use std::time::{Duration, Instant};

// ============================================================================
// Shared line vocabulary — MUST stay identical to ../stdlib/src/main.rs
// ============================================================================

#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage {
    Connect,
    Send,
    FirstByte,
    Body,
}

impl Stage {
    fn as_str(self) -> &'static str {
        match self {
            Stage::Connect => "connect",
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
    /// reqwest folds DNS, connect, TLS and request-write into one `send()`
    /// future, so a pre-headers failure cannot be attributed more precisely
    /// than "before the response". `classify` reads reqwest's own predicates to
    /// recover as much of the split as reqwest exposes.
    fn from_reqwest(after: Duration, e: &reqwest::Error) -> Self {
        let stage = if e.is_connect() {
            Stage::Connect
        } else if e.is_body() || e.is_decode() {
            Stage::Body
        } else if e.is_request() || e.is_timeout() {
            Stage::FirstByte
        } else {
            Stage::Send
        };
        let mut kind = Vec::new();
        if e.is_timeout() {
            kind.push("timeout");
        }
        if e.is_connect() {
            kind.push("connect");
        }
        if e.is_request() {
            kind.push("request");
        }
        if e.is_body() {
            kind.push("body");
        }
        if e.is_decode() {
            kind.push("decode");
        }
        if kind.is_empty() {
            kind.push("other");
        }
        // The io::ErrorKind underneath is the interesting half — ETIMEDOUT from
        // the kernel's hidden 30 s blocking-recv cap looks entirely different
        // from reqwest's own client timeout, and only the source chain says
        // which one fired.
        let mut msg = e.to_string();
        let mut src: Option<&(dyn std::error::Error + 'static)> = std::error::Error::source(e);
        while let Some(s) = src {
            msg.push_str(" <- ");
            msg.push_str(&s.to_string());
            src = std::error::Error::source(s);
        }
        ProbeError {
            stage,
            after,
            kind: kind.join("+"),
            msg,
        }
    }
}

struct Timeline {
    t0: Instant,
    first_byte: Option<Duration>,
    last_byte: Option<Duration>,
    status: u16,
    version: String,
    body_bytes: usize,
    chunks: usize,
}

impl Timeline {
    fn new() -> Self {
        Timeline {
            t0: Instant::now(),
            first_byte: None,
            last_byte: None,
            status: 0,
            version: "-".into(),
            body_bytes: 0,
            chunks: 0,
        }
    }
    fn now(&self) -> Duration {
        self.t0.elapsed()
    }
    fn mark(&mut self, name: &str) {
        let d = self.now();
        let slot = match name {
            "first_byte" => &mut self.first_byte,
            "last_byte" => &mut self.last_byte,
            _ => unreachable!("unknown mark {name}"),
        };
        if slot.is_none() {
            *slot = Some(d);
            println!("[probe] mark {}={}ms", name, ms(d));
        }
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
        "[probe] RESULT ok status={} http={} body={} chunks={} first_byte_ms={} total_ms={}",
        tl.status,
        tl.version,
        tl.body_bytes,
        tl.chunks,
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
// Chunk accounting — identical policy to nettest-std so the two logs line up
// ============================================================================

struct ChunkLog {
    all: bool,
    gap_ms: u128,
    last_at: Option<Duration>,
    held: Option<(usize, Duration, usize, Duration)>,
}

impl ChunkLog {
    fn new(force_all: bool) -> Self {
        ChunkLog {
            all: force_all || env::var("NETTEST_ALL_CHUNKS").is_ok(),
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
// Client construction — nca's shape
// ============================================================================

fn build_client() -> reqwest::Client {
    let mut b = reqwest::Client::builder()
        .user_agent("nettest-reqwest/0.1 (akuma probe)");
    if let Some(secs) = env::var("NETTEST_TIMEOUT").ok().and_then(|v| v.parse().ok()) {
        b = b.timeout(Duration::from_secs(secs));
    }
    if env::var("NETTEST_HTTP1").is_ok() {
        b = b.http1_only();
    }
    if env::var("NETTEST_NEW_CLIENT").is_ok() {
        // Pool size 0 makes every request dial fresh even within one Client,
        // so "does the hang need a reused connection?" is a one-env-var test.
        b = b.pool_max_idle_per_host(0);
    }
    if env::var("NETTEST_INSECURE_TLS").is_ok() {
        // For `scripts/net_delay_server.py --tls`'s throwaway self-signed
        // cert ONLY. This probe exists to catch kernel/stack bugs in the
        // shutdown path, not to validate certs, and a real endpoint is never
        // pointed at with this set.
        b = b.danger_accept_invalid_certs(true);
    }
    b.build().expect("reqwest Client::build")
}

async fn run_request(
    req: reqwest::RequestBuilder,
    all_chunks: bool,
) -> Result<Timeline, ProbeError> {
    let mut tl = Timeline::new();

    // `send()` resolves when the response HEAD has arrived, so this await IS
    // the first-byte wait — the exact window the archive doc says never ends.
    let mut resp = match req.send().await {
        Ok(r) => r,
        Err(e) => return Err(ProbeError::from_reqwest(tl.now(), &e)),
    };
    tl.mark("first_byte");
    tl.status = resp.status().as_u16();
    tl.version = format!("{:?}", resp.version());
    println!("[probe] head status={} http={}", tl.status, tl.version);

    let mut log = ChunkLog::new(all_chunks);
    loop {
        match resp.chunk().await {
            Ok(Some(c)) => {
                let at = tl.now();
                tl.chunks += 1;
                tl.body_bytes += c.len();
                log.record(tl.chunks, at, c.len());
            }
            Ok(None) => break,
            Err(e) => {
                log.finish();
                return Err(ProbeError::from_reqwest(tl.now(), &e));
            }
        }
    }
    log.finish();
    tl.mark("last_byte");
    Ok(tl)
}

// ============================================================================
// Modes
// ============================================================================

async fn mode_get(
    client: &reqwest::Client,
    url: &str,
    all_chunks: bool,
) -> Result<Timeline, ProbeError> {
    run_request(client.get(url), all_chunks).await
}

async fn mode_post(client: &reqwest::Client, url: &str, kib: usize) -> Result<Timeline, ProbeError> {
    // A body big enough to exceed the kernel's 16 KB TCP TX buffer
    // (`TCP_TX_BUFFER_SIZE`, crates/akuma-net/src/smoltcp_net.rs) when asked
    // for more than 16 KiB — which is the regime where `socket_send`'s 5 s
    // blocking-write cap can fire mid-request. nca's failing case was a
    // ~2900-token system prompt, i.e. a request body in exactly this range.
    let filler = "x".repeat(kib * 1024);
    let body = format!("{{\"probe\":\"nettest-reqwest\",\"filler\":\"{filler}\"}}");
    println!("[probe] post body={} bytes", body.len());
    run_request(
        client
            .post(url)
            .header("content-type", "application/json")
            .body(body),
        false,
    )
    .await
}

fn default_delays() -> Vec<u64> {
    // Same ladder as nettest-std: under the "always works" band, across the
    // "always hangs" band, past the kernel's 5 s blocking-send cap and past its
    // 30 s blocking-recv cap.
    vec![0, 1, 3, 5, 8, 12, 20, 35]
}

async fn mode_sweep(base: &str, delays: &[u64]) -> i32 {
    let base = base.trim_end_matches('/');
    let fresh_client = env::var("NETTEST_NEW_CLIENT").is_ok();
    let shared = build_client();
    let mut worst_ok = 0u64;
    let mut first_fail: Option<u64> = None;
    println!("[probe] sweep base={base} delays={delays:?} fresh_client={fresh_client}");
    for &d in delays {
        let url = format!("{base}/delay/{d}");
        println!("[probe] --- delay={d}s url={url}");
        let client = if fresh_client { build_client() } else { shared.clone() };
        match mode_get(&client, &url, false).await {
            Ok(tl) => {
                let fb = tl.first_byte.map_or(0, ms);
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
            println!(
                "[probe] SWEEP SUMMARY all {} delays passed (max {worst_ok}s)",
                delays.len()
            );
            0
        }
        Some(d) => {
            println!("[probe] SWEEP SUMMARY threshold: last OK={worst_ok}s, first FAIL={d}s");
            1
        }
    }
}

async fn mode_gap(base: &str, pre: u64, gap: u64) -> i32 {
    let base = base.trim_end_matches('/');
    let url = format!("{base}/gap/{pre}/{gap}");
    println!("[probe] gap pre={pre}s gap={gap}s url={url}");
    let client = build_client();
    // Answers the question the archive doc left open: delayed FIRST byte, or
    // ANY long idle window on an established connection?
    match mode_get(&client, &url, false).await {
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
                if e.stage == Stage::FirstByte || e.stage == Stage::Connect {
                    "died before the FIRST byte: delayed-first-byte class"
                } else {
                    "died MID-STREAM: any-long-idle class, not delayed-first-byte"
                }
            );
            1
        }
    }
}

/// Two requests on one shared `Client`, with a real wall-clock gap between
/// them long enough for the SERVER to force-close the idle pooled connection
/// in between. Every other mode in this probe hits routes that send
/// `Connection: close`, so reqwest never pools them — this is the one mode
/// that can catch "reused a connection the peer already hung up on", which
/// is nca's actual keep-alive shape and was never covered before 2026-08-22
/// (`docs/runbooks/diagnose-hung-userspace-process.md`).
async fn mode_reuse(base: &str, idle: u64) -> i32 {
    let base = base.trim_end_matches('/');
    let url = format!("{base}/keepalive/{idle}");
    let client = build_client();
    println!("[probe] reuse idle={idle}s url={url}");
    println!("[probe] reuse: first request (populates the pool)");
    match mode_get(&client, &url, false).await {
        Ok(tl) => println!("[probe] reuse: first OK body={} chunks={}", tl.body_bytes, tl.chunks),
        Err(e) => {
            print_fail(&e);
            println!("[probe] REUSE FAIL: the FIRST request itself failed, not the reuse");
            return 1;
        }
    }
    println!("[probe] reuse: sleeping past the server's {idle}s idle-close so the pool holds a dead connection");
    tokio::time::sleep(std::time::Duration::from_secs(idle + 1)).await;
    println!("[probe] reuse: second request (reuses the pooled connection, if reqwest thinks it's still good)");
    match mode_get(&client, &url, false).await {
        Ok(tl) => {
            println!("[probe] REUSE OK second request succeeded body={} chunks={}", tl.body_bytes, tl.chunks);
            0
        }
        Err(e) => {
            print_fail(&e);
            println!(
                "[probe] REUSE FAIL stage={} after_ms={} kind={} — reused a connection the peer already closed",
                e.stage.as_str(), ms(e.after), e.kind
            );
            1
        }
    }
}

// ============================================================================
// CLI
// ============================================================================

fn usage() -> ! {
    eprintln!("nettest-reqwest <mode> [args]");
    eprintln!();
    eprintln!("  get    <url>                  one GET, full timeline");
    eprintln!("  stream <url>                  GET, one line per body chunk (implies NETTEST_ALL_CHUNKS)");
    eprintln!("  post   <url> <kb>             POST <kb> KiB of JSON-ish body");
    eprintln!("  sweep  <base> [secs,secs,…]   GET <base>/delay/<n> per n");
    eprintln!("  gap    <base> <pre> <gap>     GET <base>/gap/<pre>/<gap>");
    eprintln!("  reuse  <base> [idle]          GET <base>/keepalive/<idle> twice, reusing the");
    eprintln!("                                pool across the server's idle-close (default idle=2)");
    eprintln!();
    eprintln!("<base> is scripts/net_delay_server.py, e.g. http://10.0.2.2:18080");
    eprintln!("env: NETTEST_RT=current|multi NETTEST_TIMEOUT=<s> NETTEST_NEW_CLIENT=1");
    eprintln!("     NETTEST_HTTP1=1 NETTEST_ALL_CHUNKS=1 NETTEST_GAP_MS=<n>");
    std::process::exit(2);
}

async fn dispatch(args: Vec<String>) -> i32 {
    let mode = args[1].as_str();
    match mode {
        "get" | "stream" => {
            println!("[probe] impl=reqwest mode={mode} url={}", args[2]);
            let client = build_client();
            // `stream` is `get` with per-chunk logging forced on: the shape of
            // an SSE body over time is the measurement, not the byte count.
            match mode_get(&client, &args[2], mode == "stream").await {
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
        "post" => {
            let kib = args.get(3).and_then(|v| v.parse().ok()).unwrap_or(16);
            println!("[probe] impl=reqwest mode=post url={} kb={kib}", args[2]);
            let client = build_client();
            match mode_post(&client, &args[2], kib).await {
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
        "sweep" => {
            let delays: Vec<u64> = match args.get(3) {
                Some(s) => {
                    let parsed: Vec<u64> =
                        s.split(',').filter_map(|v| v.trim().parse().ok()).collect();
                    if parsed.is_empty() {
                        default_delays()
                    } else {
                        parsed
                    }
                }
                None => default_delays(),
            };
            println!("[probe] impl=reqwest mode=sweep");
            mode_sweep(&args[2], &delays).await
        }
        "gap" => {
            let pre = args.get(3).and_then(|v| v.parse().ok()).unwrap_or(0);
            let gap = args.get(4).and_then(|v| v.parse().ok()).unwrap_or(10);
            println!("[probe] impl=reqwest mode=gap");
            mode_gap(&args[2], pre, gap).await
        }
        "reuse" => {
            let idle = args.get(3).and_then(|v| v.parse().ok()).unwrap_or(2);
            println!("[probe] impl=reqwest mode=reuse");
            mode_reuse(&args[2], idle).await
        }
        _ => usage(),
    }
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 3 {
        usage();
    }

    // nca is `#[tokio::main]`, i.e. the multi-thread runtime. `current` is
    // offered because it is the configuration where the reactor and the
    // application share a thread — if a kernel syscall blocks longer than the
    // caller expects, `multi` hides it behind a worker and `current` does not.
    let flavour = env::var("NETTEST_RT").unwrap_or_else(|_| "multi".into());
    println!("[probe] tokio runtime={flavour}");
    let rt = match flavour.as_str() {
        "current" => tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build(),
        _ => tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build(),
    }
    .expect("tokio runtime");

    let code = rt.block_on(dispatch(args));
    std::process::exit(code);
}
