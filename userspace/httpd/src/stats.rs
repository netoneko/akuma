//! Per-request phase timing for `httpd` (`HTTPD_STATS=1`).
//!
//! Exists to answer one question the kernel's `[NICSTAT]` counters cannot:
//! **of the milliseconds a client waits for an HTTP response, how many are the
//! server's own work?**
//!
//! Measured 2026-08-19 on `devbox-smoltcp`, an HTTP/1.0 GET of a 23-byte file
//! took ~7.4 ms end to end while the NIC moved ~1.5 KB. Nothing about serving
//! 23 bytes costs 7.4 ms, so the time is a *wait* — but the kernel counters
//! cannot say whether it is the accept path, the socket read, ext2, or the
//! console. This splits it:
//!
//! | phase | what it covers |
//! |---|---|
//! | `accept` | blocked in `TcpListener::accept` — the gap between requests. On Akuma this is a kernel `wait_until` that parks in `blocking_relax` (WFI), woken only by the 3 ms timer tick, because no virtio-net IRQ is registered |
//! | `read`   | `TcpStream::read` of the request line |
//! | `file`   | `open`/`fstat`/`read` of the served file — ext2 and the block cache |
//! | `write`  | `TcpStream::write` of headers + body |
//! | `log`    | the two `print!`s per request. **Not free**: console output is a per-byte MMIO store, so a 70-character log line is 70 vmexits |
//! | `other`  | request parsing, path building, `format!` — the actual compute |
//!
//! `other` is the honest "how much computing costs" number. If it is a rounding
//! error next to `accept`, the server is not the problem and no amount of
//! optimising it will help.
//!
//! # Cost, and why it is opt-in
//!
//! Each phase boundary is a `libakuma::uptime()` syscall (~1-2 us). Six phases
//! is a dozen syscalls per request, which is real next to a fast request — so
//! the whole thing is behind `HTTPD_STATS=1` and every timing call short-
//! circuits on a single bool load when it is off. `HTTPD_QUIET=1` separately
//! suppresses the per-request log lines, which is how you confirm what the
//! `log` phase costs: run once with and once without.

use alloc::format;
use libakuma::{print, uptime};

/// The phases a request is split into. Ordering matches the report line.
#[derive(Copy, Clone)]
pub enum Phase {
    Accept = 0,
    Read = 1,
    File = 2,
    Write = 3,
    Log = 4,
    Other = 5,
}

const PHASES: usize = 6;
const NAMES: [&str; PHASES] = ["accept", "read", "file", "write", "log", "other"];

/// Accumulated timings. A plain struct rather than atomics: `httpd` serves
/// connections one at a time on a single thread (see `main`'s accept loop), so
/// there is no concurrency to guard against, and a `static mut` behind
/// `&'static mut` access from that one thread is the whole synchronisation
/// story.
pub struct Stats {
    enabled: bool,
    quiet: bool,
    every: u32,
    requests: u32,
    /// Cumulative microseconds per phase since the last report.
    us: [u64; PHASES],
    /// Worst single request per phase since the last report.
    max_us: [u64; PHASES],
    /// Wall clock at the start of the current report window.
    window_start_us: u64,
    /// Timestamp of the last `mark`, i.e. the open end of the current phase.
    last_us: u64,
    /// Total bytes written to clients in this window.
    tx_bytes: u64,
}

impl Stats {
    #[must_use]
    pub fn new() -> Self {
        let enabled = env_flag("HTTPD_STATS");
        let every = libakuma::env("HTTPD_STATS_EVERY")
            .and_then(|s| s.trim().parse::<u32>().ok())
            .filter(|n| *n > 0)
            .unwrap_or(50);
        let now = if enabled { uptime() } else { 0 };
        if enabled {
            print(&format!(
                "httpd: stats on, reporting every {every} requests\n"
            ));
        }
        Self {
            enabled,
            quiet: env_flag("HTTPD_QUIET"),
            every,
            requests: 0,
            us: [0; PHASES],
            max_us: [0; PHASES],
            window_start_us: now,
            last_us: now,
            tx_bytes: 0,
        }
    }

    /// Whether the per-request `print!` log lines should be emitted. Off with
    /// `HTTPD_QUIET=1`; A/B that against the default to price the console.
    #[must_use]
    pub const fn verbose(&self) -> bool {
        !self.quiet
    }

    /// Start the clock. Call once immediately before blocking in `accept`, so
    /// the first `mark(Phase::Accept)` measures the wait and nothing else.
    pub fn begin(&mut self) {
        if self.enabled {
            self.last_us = uptime();
        }
    }

    /// Close the phase that has been running since the previous `begin`/`mark`
    /// and attribute its elapsed time to `p`.
    pub fn mark(&mut self, p: Phase) {
        if !self.enabled {
            return;
        }
        let now = uptime();
        let d = now.saturating_sub(self.last_us);
        self.last_us = now;
        let i = p as usize;
        self.us[i] += d;
        if d > self.max_us[i] {
            self.max_us[i] = d;
        }
    }

    /// Count response bytes, so the report can show bytes/request and make it
    /// obvious when a "slow" request moved almost nothing.
    pub fn add_tx(&mut self, n: usize) {
        if self.enabled {
            self.tx_bytes += n as u64;
        }
    }

    /// End of one request. Reports and resets every `HTTPD_STATS_EVERY`.
    pub fn finish_request(&mut self) {
        if !self.enabled {
            return;
        }
        self.requests += 1;
        if self.requests >= self.every {
            self.report();
        }
    }

    fn report(&mut self) {
        let n = u64::from(self.requests.max(1));
        let wall = uptime().saturating_sub(self.window_start_us);
        let total: u64 = self.us.iter().sum();

        // One line for the averages, one for the maxima. Two short lines rather
        // than one long one because this goes to a serial console that other
        // threads interleave into.
        let mut avg = format!("httpd: [{} req, {} us/req wall]", self.requests, wall / n);
        for (i, name) in NAMES.iter().enumerate() {
            let pct = if total == 0 { 0 } else { self.us[i] * 100 / total };
            avg.push_str(&format!(" {}={}us({}%)", name, self.us[i] / n, pct));
        }
        avg.push_str(&format!(" tx={}B/req\n", self.tx_bytes / n));
        print(&avg);

        let mut mx = format!("httpd: [{} req maxima]", self.requests);
        for (i, name) in NAMES.iter().enumerate() {
            mx.push_str(&format!(" {}={}us", name, self.max_us[i]));
        }
        mx.push('\n');
        print(&mx);

        self.requests = 0;
        self.us = [0; PHASES];
        self.max_us = [0; PHASES];
        self.tx_bytes = 0;
        self.window_start_us = uptime();
    }
}

impl Default for Stats {
    fn default() -> Self {
        Self::new()
    }
}

/// True when the env var is set to anything other than `0`, `""` or `false`.
/// Deliberately permissive: `HTTPD_STATS=1`, `=y`, `=on` all work, and only an
/// explicit falsey value turns it back off.
fn env_flag(name: &str) -> bool {
    match libakuma::env(name) {
        None => false,
        Some(v) => {
            let v = v.trim();
            !(v.is_empty() || v == "0" || v.eq_ignore_ascii_case("false"))
        }
    }
}
