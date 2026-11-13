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

use std::error::Error;
use std::time::{Duration, Instant};

const DEFAULT_URL: &str = "https://akuma.sh/";

fn main() {
    let mut url = DEFAULT_URL.to_string();
    let mut print_body = false;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--body" => print_body = true,
            "-h" | "--help" => {
                println!("usage: tokioprobe [URL] [--body]   (default URL {DEFAULT_URL})");
                return;
            }
            _ => url = arg,
        }
    }
    let timeout = Duration::from_secs(env_u64("TOKIOPROBE_TIMEOUT").unwrap_or(30));

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.enable_all();
    if let Some(n) = env_u64("TOKIOPROBE_WORKERS") {
        builder.worker_threads(n.max(1) as usize);
    }
    let rt = match builder.build() {
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
