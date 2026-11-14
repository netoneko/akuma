//! tokioprobe - the smallest program that proves tokio works on a kernel.
//!
//! On a multi-thread tokio runtime (what goose and nca use) it does two things:
//!
//! 1. runs `uname -a` through `tokio::process` (pipe2, clone/execve, wait4, and
//!    SIGCHLD through tokio's signal driver);
//! 2. fetches a page over HTTPS with reqwest + rustls (DNS on the blocking pool,
//!    non-blocking connect, epoll + the eventfd waker, TLS, timers).
//!
//! Each step prints one `PASS`/`FAIL` line and is bounded by a timeout, so a hang
//! shows up as a `FAIL ... timed out` line instead of a stuck probe. The last
//! line is `RESULT PASS` or `RESULT FAIL`, and the exit status matches (0 / 1).
//!
//! Usage: `tokioprobe [URL] [--body]`
//!   URL       defaults to https://akuma.sh/
//!   --body    print the whole response body (default: the page's <title>)
//!   TOKIOPROBE_TIMEOUT=<secs>   per-step timeout, default 30
//!   TOKIOPROBE_WORKERS=<n>      worker threads, default tokio's (the CPU count)
//!
//! `tokioprobe --serve [addr]` (2026-11-11, for the headless/Taoxichuan RAM-floor matrix): a
//! long-lived TCP echo server instead of the one-shot probe above, so a sustained concurrent
//! tokio workload can be driven at a given SMP count and RAM size, rather than a few-hundred-ms
//! probe that is over before the scheduler and the net stack are under any real load. One task per
//! connection (`tokio::spawn`, so this exercises the same worker-thread scheduling a real server
//! would), reads-and-echoes until EOF, and prints one `[tokioprobe-serve]` status line every
//! `TOKIOPROBE_SERVE_STATS_SECS` (default 5) with live connections, total accepted, bytes
//! echoed and `rt.metrics()` worker/task counts — the numbers the matrix run reads off the
//! console. `addr` defaults to `0.0.0.0:7979`. Runs until killed; never exits on its own.

use std::error::Error;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const DEFAULT_URL: &str = "https://akuma.sh/";
const DEFAULT_SERVE_ADDR: &str = "0.0.0.0:7979";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--serve") {
        let addr = args.get(1).cloned().unwrap_or_else(|| DEFAULT_SERVE_ADDR.to_string());
        run_server(addr);
        return;
    }

    let mut url = DEFAULT_URL.to_string();
    let mut print_body = false;
    for arg in args {
        match arg.as_str() {
            "--body" => print_body = true,
            "-h" | "--help" => {
                println!("usage: tokioprobe [URL] [--body]   (default URL {DEFAULT_URL})");
                println!("       tokioprobe --serve [addr]   (default addr {DEFAULT_SERVE_ADDR})");
                return;
            }
            _ => url = arg,
        }
    }
    let timeout = Duration::from_secs(env_u64("TOKIOPROBE_TIMEOUT").unwrap_or(30));

    let rt = match build_runtime() {
        Ok(rt) => rt,
        Err(e) => {
            // The first wall a kernel without epoll hits: mio's Poll::new.
            println!("RUNTIME FAIL {}", chain(&e));
            println!("RESULT FAIL");
            std::process::exit(1);
        }
    };
    println!("RUNTIME PASS multi-thread workers={}", rt.metrics().num_workers());

    let ok = rt.block_on(async {
        let uname_ok = step("UNAME", timeout, uname()).await.is_some();
        let fetched = step("HTTPS", timeout, fetch(&url)).await;
        if let Some(body) = &fetched {
            if print_body {
                println!("--- body ---");
                println!("{}", String::from_utf8_lossy(body));
                println!("--- end ---");
            } else if let Some(title) = title(body) {
                println!("  title: {title}");
            }
        }
        uname_ok && fetched.is_some()
    });

    println!("RESULT {}", if ok { "PASS" } else { "FAIL" });
    std::process::exit(if ok { 0 } else { 1 });
}

fn build_runtime() -> std::io::Result<tokio::runtime::Runtime> {
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.enable_all();
    if let Some(n) = env_u64("TOKIOPROBE_WORKERS") {
        builder.worker_threads(n.max(1) as usize);
    }
    builder.build()
}

/// `--serve`: a long-lived echo server. See the module doc for why — the matrix run's workload,
/// not a correctness probe.
fn run_server(addr: String) {
    let rt = match build_runtime() {
        Ok(rt) => rt,
        Err(e) => {
            println!("RUNTIME FAIL {}", chain(&e));
            println!("SERVE FAIL");
            std::process::exit(1);
        }
    };
    println!("RUNTIME PASS multi-thread workers={}", rt.metrics().num_workers());

    let stats_every = Duration::from_secs(env_u64("TOKIOPROBE_SERVE_STATS_SECS").unwrap_or(5));
    rt.block_on(async move {
        let sock_addr: SocketAddr = match addr.parse() {
            Ok(a) => a,
            Err(e) => {
                println!("SERVE FAIL bad addr {addr}: {e}");
                std::process::exit(1);
            }
        };
        let listener = match TcpListener::bind(sock_addr).await {
            Ok(l) => l,
            Err(e) => {
                println!("SERVE FAIL bind {addr}: {}", chain(&e));
                std::process::exit(1);
            }
        };
        println!("SERVER LISTENING {addr}");

        let live = Arc::new(AtomicU64::new(0));
        let accepted = Arc::new(AtomicU64::new(0));
        let bytes_echoed = Arc::new(AtomicU64::new(0));
        let failed = Arc::new(AtomicU64::new(0));

        let (rt_handle, stats_live, stats_accepted, stats_bytes, stats_failed) =
            (tokio::runtime::Handle::current(), live.clone(), accepted.clone(), bytes_echoed.clone(), failed.clone());
        tokio::spawn(async move {
            let start = Instant::now();
            loop {
                tokio::time::sleep(stats_every).await;
                let m = rt_handle.metrics();
                println!(
                    "[tokioprobe-serve] t={:.0}s live={} accepted={} bytes_echoed={} failed={} workers={} alive_tasks={}",
                    start.elapsed().as_secs_f64(),
                    stats_live.load(Ordering::Relaxed),
                    stats_accepted.load(Ordering::Relaxed),
                    stats_bytes.load(Ordering::Relaxed),
                    stats_failed.load(Ordering::Relaxed),
                    m.num_workers(),
                    m.num_alive_tasks(),
                );
            }
        });

        loop {
            let (mut sock, peer) = match listener.accept().await {
                Ok(x) => x,
                Err(e) => {
                    failed.fetch_add(1, Ordering::Relaxed);
                    println!("ACCEPT FAIL {}", chain(&e));
                    continue;
                }
            };
            accepted.fetch_add(1, Ordering::Relaxed);
            live.fetch_add(1, Ordering::Relaxed);
            let (live, bytes_echoed, failed) = (live.clone(), bytes_echoed.clone(), failed.clone());
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                loop {
                    let n = match sock.read(&mut buf).await {
                        Ok(0) => break,
                        Ok(n) => n,
                        Err(e) => {
                            failed.fetch_add(1, Ordering::Relaxed);
                            println!("READ FAIL {peer}: {}", chain(&e));
                            break;
                        }
                    };
                    if let Err(e) = sock.write_all(&buf[..n]).await {
                        failed.fetch_add(1, Ordering::Relaxed);
                        println!("WRITE FAIL {peer}: {}", chain(&e));
                        break;
                    }
                    bytes_echoed.fetch_add(n as u64, Ordering::Relaxed);
                }
                live.fetch_sub(1, Ordering::Relaxed);
            });
        }
    });
}

/// Run one step under the timeout and print its verdict line. The step's own
/// detail goes on the PASS line; the error chain goes on the FAIL line.
async fn step<T, F>(name: &str, timeout: Duration, fut: F) -> Option<T>
where
    F: std::future::Future<Output = Result<(T, String), String>>,
{
    let start = Instant::now();
    let outcome = match tokio::time::timeout(timeout, fut).await {
        Ok(r) => r,
        Err(_) => Err(format!("timed out after {}s", timeout.as_secs())),
    };
    let took = start.elapsed().as_secs_f64();
    match outcome {
        Ok((value, detail)) => {
            println!("{name} PASS {detail} ({took:.2}s)");
            Some(value)
        }
        Err(e) => {
            println!("{name} FAIL {e} ({took:.2}s)");
            None
        }
    }
}

async fn uname() -> Result<((), String), String> {
    let out = tokio::process::Command::new("uname")
        .arg("-a")
        .output()
        .await
        .map_err(|e| format!("spawn: {}", chain(&e)))?;
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(format!("{}: {}", out.status, stderr.trim()));
    }
    if stdout.is_empty() {
        return Err("exit 0 but no output".into());
    }
    Ok(((), stdout))
}

async fn fetch(url: &str) -> Result<(Vec<u8>, String), String> {
    let client = reqwest::Client::builder()
        .build()
        .map_err(|e| format!("client: {}", chain(&e)))?;
    let resp = client.get(url).send().await.map_err(|e| format!("send: {}", chain(&e)))?;
    let status = resp.status();
    let version = resp.version();
    let body = resp.bytes().await.map_err(|e| format!("body: {}", chain(&e)))?;
    if !status.is_success() {
        return Err(format!("{url} status={} bytes={}", status.as_u16(), body.len()));
    }
    let detail = format!("{url} status={} {version:?} bytes={}", status.as_u16(), body.len());
    Ok((body.to_vec(), detail))
}

/// The page's `<title>`, enough to show the body is the real page.
fn title(body: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(body);
    let lower = text.to_ascii_lowercase();
    let start = lower.find("<title>")? + "<title>".len();
    let end = start + lower[start..].find("</title>")?;
    Some(text[start..end].trim().to_string())
}

/// An error and its sources on one line - reqwest's top-level message alone
/// ("error sending request") never says which layer failed.
fn chain(e: &dyn Error) -> String {
    let mut s = e.to_string();
    let mut cur = e.source();
    while let Some(src) = cur {
        s.push_str(": ");
        s.push_str(&src.to_string());
        cur = src.source();
    }
    s
}

fn env_u64(name: &str) -> Option<u64> {
    std::env::var(name).ok()?.trim().parse().ok()
}
