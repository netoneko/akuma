//! nettest — cargo-network-pattern divergence probe.
//!
//! Distills cargo's HTTPS client (`rust-lang/cargo` master,
//! `src/cargo/sources/registry/http_remote.rs` for the sparse-index path that
//! hits `index.crates.io:443`, and `src/cargo/util/network/http_async.rs` /
//! `src/cargo/util/network/http.rs` for the libcurl driver) into a 4-mode
//! binary so we can bisect which cargo-side knob makes the Akuma smoltcp
//! kernel return `[7] Could not connect to server (Failed to connect to
//! index.crates.io:443 after ~300 ms)`.
//!
//! # Modes
//!
//! | mode     | Multi | multiplex | worker thread | what it mirrors |
//! |----------|-------|-----------|---------------|-----------------|
//! | easy11   | no    | no        | no            | `curl https://...` CLI baseline (works in VM) |
//! | easy2    | no    | yes       | no            | one cargo easy handle perform()'d inline (also works in VM) |
//! | multi11  | yes   | no        | yes           | cargo Multi but forced HTTP/1.1 (no ALPN h2) |
//! | multi2   | yes   | yes       | yes           | cargo's exact pattern: Multi + multiplex + worker |
//!
//! `multi2` is the only mode that should reproduce the cargo failure — it is
//! the only one that exercises (a) HTTP/2 ALPN over TLS, (b) curl's
//! multi-perform/wait loop driven from a spawned pthread, and (c) the
//! `CURLOPT_PIPEWAIT` "wait for an existing connection instead of opening a
//! new one" code path.
//!
//! # Usage
//!
//!     nettest <mode> [url]
//!
//! Default URL is `https://index.crates.io/config.json` (the exact request
//! that fails inside the VM). Pass any URL to test a different target — e.g.
//! the user's 431 MB flac that `curl` downloads fine:
//!
//!     nettest multi2 https://example.com/tokyo_rider_omegashima.flac
//!
//! # Build
//!
//! See `build.sh` in this directory. Host build verifies the curl crate API;
//! the binary that goes into the VM is cross-built inside an Alpine arm64
//! docker container so it links against apk's libcurl.so + libssl.so — the
//! exact same dynamic libraries apk-installed cargo links against.

use std::env;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use curl::easy::{Easy2, Handler, HttpVersion, ReadError, WriteError};
use curl::multi::{Easy2Handle, Multi};

// ============================================================================
// Collector — cargo's http_async.rs:419-488 (write/header/read callbacks)
// ============================================================================

struct Collector {
    body: Vec<u8>,
}

impl Collector {
    fn new() -> Self {
        Collector { body: Vec::new() }
    }
}

impl Handler for Collector {
    fn write(&mut self, data: &[u8]) -> Result<usize, WriteError> {
        self.body.extend_from_slice(data);
        Ok(data.len())
    }
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, ReadError> {
        // Cargo drains a Cursor; we never upload, so always EOF.
        let _ = buf;
        Ok(0)
    }
}

// ============================================================================
// Handle configuration — cargo's http.rs:203-233 (configure2) +
// util/network/mod.rs:73-82 (try_old_curl_http2_pipewait macro)
// ============================================================================

fn configure_cargo_handle(handle: &mut Easy2<Collector>, multiplex: bool) {
    // http.rs:216
    handle.useragent("nettest/0.1 (akuma probe)").expect("useragent");
    // http.rs:218 — empty string expands to whatever libcurl was built with
    handle.accept_encoding("").expect("accept_encoding");
    // http_remote.rs:133
    handle.follow_location(true).expect("follow_location");
    // http_remote.rs:134
    handle.progress(true).expect("progress");

    // mod.rs:73-82 — the macro: HTTP/2 when multiplexing, HTTP/1.1 otherwise,
    // plus pipewait in both branches.
    if multiplex {
        // Errors are swallowed by `try_old_curl!` on macOS only; on Linux
        // cargo treats HTTP/2 set failure as fatal. We swallow and report
        // through verbose — the build-time curl either has h2 or it doesn't.
        let _ = handle.http_version(HttpVersion::V2);
    } else {
        let _ = handle.http_version(HttpVersion::V11);
    }
    let _ = handle.pipewait(true);

    // Helpful diagnostic: dump libcurl's plan to stderr so we can see the
    // ALPN offer, the connect timeline, etc. Mirrors cargo's
    // CARGO_HTTP_DEBUG=true / `http.verbose = true`.
    handle.verbose(true).expect("verbose");
}

// ============================================================================
// Modes 1+2: single Easy2, perform() in caller thread — no Multi, no worker
// ============================================================================

fn run_easy(url: &str, multiplex: bool) -> Result<(u16, usize, Duration), String> {
    let mut handle = Easy2::new(Collector::new());
    handle.url(url).map_err(|e| format!("url: {e}"))?;
    handle.get(true).map_err(|e| format!("get: {e}"))?;
    configure_cargo_handle(&mut handle, multiplex);

    let t0 = Instant::now();
    if let Err(e) = handle.perform() {
        return Err(format!("perform failed after {:?}: {e}", t0.elapsed()));
    }
    let elapsed = t0.elapsed();
    let code = handle.response_code().map_err(|e| format!("response_code: {e}"))?;
    let body_len = handle.get_ref().body.len();
    Ok((code as u16, body_len, elapsed))
}

// ============================================================================
// Modes 3+4: cargo's exact worker-thread pattern (http_async.rs:71-416)
// ============================================================================

struct Message {
    easy: Easy2<Collector>,
    sender: Sender<Result<(u16, usize, Duration), String>>,
    started: Instant,
}

fn run_multi(url: &str, multiplex: bool) -> Result<(u16, usize, Duration), String> {
    let (work_tx, work_rx) = mpsc::channel::<Message>();

    // http_async.rs:82-96 — Client::new spawns the worker thread that owns
    // the curl Multi for the lifetime of the client.
    let worker = thread::Builder::new()
        .name("nettest-curl-worker".into())
        .spawn(move || worker_loop(work_rx, multiplex))
        .map_err(|e| format!("spawn worker: {e}"))?;

    // http_async.rs:110-119 — Client::request builds an Easy2, ships it over
    // the channel, and awaits a oneshot reply.
    let mut handle = Easy2::new(Collector::new());
    handle.url(url).map_err(|e| format!("url: {e}"))?;
    handle.get(true).map_err(|e| format!("get: {e}"))?;
    configure_cargo_handle(&mut handle, multiplex);

    let (reply_tx, reply_rx) = mpsc::channel::<Result<(u16, usize, Duration), String>>();
    let started = Instant::now();
    work_tx
        .send(Message { easy: handle, sender: reply_tx, started })
        .map_err(|e| format!("send to worker: {e}"))?;
    drop(work_tx); // signal worker it can exit once this last request drains

    let result = reply_rx
        .recv()
        .map_err(|e| format!("recv from worker: {e}"))??;
    let _ = worker.join();
    Ok(result)
}

/// http_async.rs:223-401 — WorkerServer::run + worker_loop, distilled.
fn worker_loop(incoming: Receiver<Message>, multiplex: bool) {
    let mut multi = Multi::new();
    // http_async.rs:232 — "let's not flood the server with connections"
    let _ = multi.set_max_host_connections(2);
    // http_async.rs:235 — pipelining(false, multiplex). HTTP/1 pipelining off,
    // HTTP/2 multiplexing on (or off, in mode multi11).
    if let Err(e) = multi.pipelining(false, multiplex) {
        eprintln!("[worker] multi.pipelining(false, {multiplex}) failed: {e}");
    }

    // http_async.rs:204-210 — handles table. We store the token here because
    // Easy2Handle does not expose a getter; the Message callback hands us the
    // token, we match it against this Vec.
    type Slot = (
        usize, // token
        Easy2Handle<Collector>,
        Sender<Result<(u16, usize, Duration), String>>,
        Instant,
    );
    let mut handles: Vec<Slot> = Vec::new();
    let mut token_next: usize = 0;

    // http_async.rs:309-401
    let initial_delay = Duration::from_millis(1);
    let mut wait_backoff = initial_delay;
    let enqueue = |multi: &Multi, handles: &mut Vec<Slot>, token: &mut usize, msg: Message| {
        *token = token.wrapping_add(1);
        match multi.add2(msg.easy) {
            Ok(mut h) => {
                let _ = h.set_token(*token);
                handles.push((*token, h, msg.sender, msg.started));
            }
            Err(e) => {
                let _ = msg.sender.send(Err(format!("multi.add2: {e}")));
            }
        }
    };

    loop {
        // 314-317 — drain the channel
        let mut new_work = false;
        while let Ok(msg) = incoming.try_recv() {
            enqueue(&multi, &mut handles, &mut token_next, msg);
            new_work = true;
        }
        if new_work {
            wait_backoff = initial_delay;
        }

        // 319 — multi.perform()
        match multi.perform() {
            Err(e) if e.is_call_perform() => { /* retry, per http_async.rs:320-322 */ }
            Err(e) => {
                for (_, _, sender, _) in &handles {
                    let _ = sender.send(Err(format!("multi.perform: {e}")));
                }
                return;
            }
            Ok(running) => {
                // 327-338 — drain completion messages. We collect finished
                // tokens first then act outside the messages() callback so we
                // never alias &multi (used by messages) with &mut handles.
                let mut finished: Vec<(usize, Option<Result<(), curl::Error>>)> = Vec::new();
                multi.messages(|msg| {
                    let token = match msg.token() {
                        Ok(t) => t,
                        Err(_) => return,
                    };
                    let pos = handles.iter().position(|(t, _, _, _)| *t == token);
                    let Some(idx) = pos else { return };
                    let result = msg.result_for2(&handles[idx].1);
                    finished.push((token, result));
                });
                for (token, result) in finished {
                    let Some(idx) = handles.iter().position(|(t, _, _, _)| *t == token) else {
                        continue;
                    };
                    let (_, h, sender, started) = handles.remove(idx);
                    let easy = match multi.remove2(h) {
                        Ok(e) => e,
                        Err(e) => {
                            let _ = sender.send(Err(format!("multi.remove2: {e}")));
                            continue;
                        }
                    };
                    let code = easy.response_code().unwrap_or(0);
                    let body_len = easy.get_ref().body.len();
                    let elapsed = started.elapsed();
                    match result {
                        Some(Ok(())) => {
                            let _ = sender.send(Ok((code as u16, body_len, elapsed)));
                        }
                        Some(Err(e)) => {
                            let _ = sender.send(Err(format!(
                                "curl result after {:?}: {e}",
                                elapsed
                            )));
                        }
                        None => {
                            let _ = sender.send(Err(format!(
                                "curl result: no message body for token {token}"
                            )));
                        }
                    }
                }

                if running > 0 {
                    // 340-381 — wait for activity, exponential-backoff like cargo
                    let max_timeout = Duration::from_millis(1000);
                    let mut timeout = multi
                        .get_timeout()
                        .ok()
                        .flatten()
                        .unwrap_or(max_timeout)
                        .min(max_timeout);
                    if timeout.is_zero() {
                        continue;
                    }
                    if wait_backoff < timeout {
                        wait_backoff *= 2;
                        timeout = wait_backoff;
                    }
                    if let Err(e) = multi.wait(&mut [], timeout) {
                        eprintln!("[worker] multi.wait: {e}");
                    }
                } else {
                    // 382-396 — all idle. Block on the channel for new work
                    // (cargo: self.incoming_work.recv()). If the channel is
                    // closed, we are done.
                    match incoming.recv() {
                        Ok(msg) => {
                            enqueue(&multi, &mut handles, &mut token_next, msg);
                            wait_backoff = initial_delay;
                        }
                        Err(_) => break,
                    }
                }
            }
        }
    }
}

// ============================================================================
// parkprobe — the SMP=1 idle_halt scheduler-freeze acceptance probe
// ============================================================================
//
// Reproduces, in one process tree with zero external binaries, the exact
// scheduling configuration that froze single-core kernels before the
// `idle_halt` preemption fix (docs/archive/SECOND_LISTENER_SMP1_FREEZE.md):
//
//   child A   binds a loopback listener and parks in blocking `accept(2)` —
//             the `wait_until` → `blocking_relax_net` → `idle_halt` path —
//             on a quiet network (nothing will ever connect first);
//   child B   forked next and `_exit()`s immediately — the "READY, never yet
//             run" thread (the second server's exec in the original repro);
//   parent    parks in blocking `wait4(B)` — a waker-only park whose wake is
//             B's exit (the session shell in the original repro).
//
// Healthy kernel: B runs within a tick, the parent reaps it, then the parent
// CONNECTS to A's listener — exercising the wake path end to end (loopback
// frame → netpoll doorbell → stack poll → accept returns) — echoes one byte,
// SIGKILLs A while it is parked in the *next* accept, and reaps it.
//
// Frozen kernel: the probe never prints its PASS line — the frozen
// preempt-disabled halt parks the probe's own parent, so nothing userland can
// self-timeout. **The harness's timeout is the failure signal.**
//
// Usage: nettest parkprobe

fn run_parkprobe() -> Result<Duration, String> {
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};

    const REQ: u8 = b'Q';
    const RSP: u8 = b'R';

    let listener = TcpListener::bind("127.0.0.1:0")
        .map_err(|e| format!("bind loopback listener: {e}"))?;
    let port = listener
        .local_addr()
        .map_err(|e| format!("local_addr: {e}"))?
        .port();

    // Readiness pipe: A announces itself just before entering accept. Raw
    // libc pipe, because this must survive fork() (an mpsc channel would be
    // duplicated, not shared).
    let mut rdy_pipe = [0i32; 2];
    if unsafe { libc::pipe(rdy_pipe.as_mut_ptr()) } != 0 {
        return Err(format!("pipe: {}", std::io::Error::last_os_error()));
    }

    let t0 = Instant::now();
    let pid_a = unsafe { libc::fork() };
    if pid_a < 0 {
        return Err(format!("fork A: {}", std::io::Error::last_os_error()));
    }
    if pid_a == 0 {
        // Child A: the parked socket waiter. Announce, then serve one echo
        // connection and park in the next accept — where the parent kills us.
        unsafe {
            libc::close(rdy_pipe[0]);
            let byte = [1u8; 1];
            let _ = libc::write(rdy_pipe[1], byte.as_ptr().cast(), 1);
            libc::close(rdy_pipe[1]);
        }
        eprintln!("[parkprobe] A: parked in accept on 127.0.0.1:{port}");
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            let mut b = [0u8; 1];
            let _ = s.read(&mut b);
            let _ = s.write_all(&[RSP]);
        }
        std::process::exit(0); // unreachable: killed while parked above
    }

    // Parent: wait for A's announcement (a blocking pipe read — A is running
    // now, this returns within a scheduling slice on any healthy kernel).
    unsafe {
        libc::close(rdy_pipe[1]);
        let mut b = [0u8; 1];
        let n = libc::read(rdy_pipe[0], b.as_mut_ptr().cast(), 1);
        libc::close(rdy_pipe[0]);
        if n != 1 {
            return Err("A never announced readiness".into());
        }
    }

    // The trigger: B is forked while A holds the accept park. On the frozen
    // kernel B never gets scheduled and the wait4 below never returns.
    let pid_b = unsafe { libc::fork() };
    if pid_b < 0 {
        return Err(format!("fork B: {}", std::io::Error::last_os_error()));
    }
    if pid_b == 0 {
        // Child B: exit before ever being scheduled, if the scheduler is
        // healthy. Code 7 so the parent can verify it reaped the RIGHT child.
        unsafe { libc::_exit(7) };
    }
    eprintln!("[parkprobe] parent: B forked (pid {pid_b}), parking in wait4");

    let mut status: i32 = 0;
    if unsafe { libc::waitpid(pid_b, &mut status, 0) } != pid_b {
        return Err(format!("wait4(B): {}", std::io::Error::last_os_error()));
    }
    if !libc::WIFEXITED(status) || libc::WEXITSTATUS(status) != 7 {
        return Err(format!("B exited abnormally: status={status:#x}"));
    }
    let reaped_at = t0.elapsed();
    eprintln!("[parkprobe] parent: B reaped after {reaped_at:?} — scheduler escaped the park");

    // Wake-path check: connect to A's listener. This is the part the freeze
    // doc's original repro never reached — a surviving machine whose accept
    // park forgot how to wake would hang here instead.
    let mut conn = TcpStream::connect(("127.0.0.1", port))
        .map_err(|e| format!("connect to A's listener: {e}"))?;
    conn.set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| format!("set_read_timeout: {e}"))?;
    conn.write_all(&[REQ])
        .map_err(|e| format!("write to A: {e}"))?;
    let mut b = [0u8; 1];
    conn.read_exact(&mut b)
        .map_err(|e| format!("read echo from A: {e}"))?;
    if b[0] != RSP {
        return Err(format!("echo mismatch: sent {REQ:?}, got {:?}", b[0]));
    }
    drop(conn);
    let echoed_at = t0.elapsed();

    // Cleanup under fire: SIGKILL A while it sits in the NEXT accept park —
    // signal delivery to a preempt-disabled (old kernel: frozen; new kernel:
    // merely halted) socket waiter.
    unsafe {
        libc::kill(pid_a, libc::SIGKILL);
        let mut st: i32 = 0;
        libc::waitpid(pid_a, &mut st, 0);
    }

    Ok(t0.elapsed())
}

// ============================================================================
// CLI
// ============================================================================

fn usage() -> ! {
    eprintln!("nettest <mode> [url]");
    eprintln!();
    eprintln!("modes (cargo pattern, see src/main.rs header for the source-mapping table):");
    eprintln!("  easy11    Easy2 + HTTP/1.1, no Multi, no worker thread  (curl CLI equivalent)");
    eprintln!("  easy2     Easy2 + HTTP/2 + pipewait, no Multi, no worker thread");
    eprintln!("  multi11   Multi + worker thread, multiplexing OFF");
    eprintln!("  multi2    Multi + worker thread + multiplexing  (cargo's exact pattern)");
    eprintln!();
    eprintln!("scheduler probes (no url):");
    eprintln!("  parkprobe SMP=1 idle_halt freeze probe — parked accept + fork-exit + wait4;");
    eprintln!("            PASS = reap+echo+kill complete; a hang needs the harness timeout");
    eprintln!("            (docs/archive/SECOND_LISTENER_SMP1_FREEZE.md)");
    eprintln!();
    eprintln!("default url: https://index.crates.io/config.json  (the cargo failure)");
    std::process::exit(2);
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        usage();
    }
    let mode = args[1].as_str();

    // parkprobe takes no url and must not touch curl at all.
    if mode == "parkprobe" {
        eprintln!("[nettest] mode=parkprobe (scheduler freeze probe)");
        match run_parkprobe() {
            Ok(total) => {
                println!("[nettest] mode=parkprobe OK total={total:?}");
                std::process::exit(0);
            }
            Err(e) => {
                eprintln!("[nettest] mode=parkprobe FAIL: {e}");
                std::process::exit(1);
            }
        }
    }

    let url = args
        .get(2)
        .cloned()
        .unwrap_or_else(|| "https://index.crates.io/config.json".to_string());

    eprintln!("[nettest] mode={mode} url={url}");
    let t0 = Instant::now();
    let result = match mode {
        "easy11" => run_easy(&url, /*multiplex=*/false),
        "easy2" => run_easy(&url, /*multiplex=*/true),
        "multi11" => run_multi(&url, /*multiplex=*/false),
        "multi2" => run_multi(&url, /*multiplex=*/true),
        _ => usage(),
    };
    let total = t0.elapsed();

    match result {
        Ok((code, body_len, perform_elapsed)) => {
            println!(
                "[nettest] mode={mode} OK status={code} body={body_len}B perform={perform_elapsed:?} total={total:?}"
            );
            std::process::exit(0);
        }
        Err(e) => {
            eprintln!("[nettest] mode={mode} FAIL after {total:?}: {e}");
            std::process::exit(1);
        }
    }
}
