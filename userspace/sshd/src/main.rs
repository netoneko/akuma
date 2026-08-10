#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use core::future::poll_fn;
use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
use core::pin::Pin;

// Only the cooperative `serve` keeps a heap-allocated list of in-flight session
// futures; the forking one hands each session to a process and keeps nothing.
#[cfg(not(feature = "fork-sessions"))]
use alloc::boxed::Box;
#[cfg(not(feature = "fork-sessions"))]
use alloc::vec::Vec;
#[cfg(not(feature = "fork-sessions"))]
use core::future::Future;

use libakuma::*;
use libakuma::net::{TcpListener, TcpStream, Error as NetError, ErrorKind as NetErrorKind};
use embedded_io_async::{Read, Write, ErrorType};

mod crypto;
mod auth;
mod keys;
mod config;
mod protocol;

// ============================================================================
// TcpStream Wrapper for embedded-io-async
// ============================================================================

pub struct SshStream {
    inner: TcpStream,
}

impl SshStream {
    pub fn new(inner: TcpStream) -> Self {
        Self { inner }
    }

    /// Raw fd of the underlying socket — used by the interactive bridge to set
    /// the connection non-blocking so it can poll the SSH channel and the
    /// shell's stdout in the same loop.
    pub fn as_raw_fd(&self) -> i32 {
        self.inner.as_raw_fd()
    }

    /// One-shot, non-suspending read: returns immediately (`Err(WouldBlock)`
    /// if nothing is available) instead of yielding to the executor. Used by
    /// `bridge_process`'s own manual multiplexer, which must check the SSH
    /// socket and the child's stdout fd within the *same* poll tick — if this
    /// suspended on `WouldBlock` like `Read::read` below, the bridge would
    /// stop draining the child's stdout the moment the client had nothing to
    /// send, freezing the session's output.
    pub fn try_read(&mut self, buf: &mut [u8]) -> Result<usize, NetError> {
        self.inner.read(buf)
    }

    fn poll_read(&mut self, cx: &mut Context<'_>, buf: &mut [u8]) -> Poll<Result<usize, NetError>> {
        match self.inner.read(buf) {
            Ok(n) => Poll::Ready(Ok(n)),
            Err(e) if e.kind() == NetErrorKind::WouldBlock => {
                cx.waker().wake_by_ref();
                Poll::Pending
            }
            Err(e) => Poll::Ready(Err(e)),
        }
    }

    fn poll_write(&mut self, cx: &mut Context<'_>, buf: &[u8]) -> Poll<Result<usize, NetError>> {
        match self.inner.write(buf) {
            Ok(n) => Poll::Ready(Ok(n)),
            Err(e) if e.kind() == NetErrorKind::WouldBlock => {
                cx.waker().wake_by_ref();
                Poll::Pending
            }
            Err(e) => Poll::Ready(Err(e)),
        }
    }
}

impl ErrorType for SshStream {
    type Error = NetError;
}

// These suspend (`Poll::Pending`) rather than error out on `WouldBlock`, so a
// socket with no data/space right now yields back to `block_on` instead of
// tearing the session down. Since the process-per-session split (see `main`)
// there is exactly one session per process, so "yielding" means sleeping 1ms in
// `block_on` — but the `Poll::Pending` contract is still what keeps a
// `WouldBlock` from being mistaken for an error, and `bridge_process`'s manual
// multiplexer still depends on it.
impl Read for SshStream {
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
        poll_fn(|cx| self.poll_read(cx, buf)).await
    }
}

impl Write for SshStream {
    async fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
        poll_fn(|cx| self.poll_write(cx, buf)).await
    }

    async fn flush(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}

// ============================================================================
// Entry Point
// ============================================================================

#[unsafe(no_mangle)]
pub extern "C" fn main() {
    println("[SSHD] Starting userspace SSH server...");

    // 1. Load config from file first
    block_on(config::load_config());
    block_on(keys::load_or_generate_host_key());
    
    let mut ssh_config = config::get_config();
    let mut cli_port: Option<u16> = None;

    // 2. Parse CLI arguments (overrides config file)
    let mut args = args();
    args.next(); // Skip program name
    
    while let Some(arg) = args.next() {
        match arg {
            "--shell" => {
                if let Some(shell_path) = args.next() {
                    ssh_config.shell = alloc::string::String::from(shell_path);
                    println(&format!("[SSHD] Shell override from CLI: {}", shell_path));
                }
            }
            "--shell-arg" => {
                // Extra argv for the spawned shell (e.g. the applet name for a
                // multicall binary: `--shell /bin/toybox --shell-arg sh`). May be
                // repeated; each occurrence appends one argument.
                if let Some(shell_arg) = args.next() {
                    ssh_config.shell_args.push(alloc::string::String::from(shell_arg));
                    println(&format!("[SSHD] Shell arg from CLI: {}", shell_arg));
                }
            }
            "--port" => {
                if let Some(port_str) = args.next()
                    && let Ok(p) = port_str.parse::<u16>()
                {
                    cli_port = Some(p);
                    println(&format!("[SSHD] Port override from CLI: {}", p));
                }
            }
            "--no-banner" => {
                ssh_config.banner = false;
                println("[SSHD] Banner disabled from CLI");
            }
            "--max-sessions" => {
                // Same 0-is-rejected rule as the config file parser: the cap
                // exists to protect the global process table, so a bad value
                // must not turn it off.
                if let Some(n_str) = args.next()
                    && let Ok(n) = n_str.parse::<usize>()
                    && n > 0
                {
                    ssh_config.max_sessions = n;
                    println(&format!("[SSHD] Max sessions override from CLI: {}", n));
                }
            }
            _ => {
                println(&format!("[SSHD] Unknown argument: {}", arg));
            }
        }
    }

    // Determine final port: CLI > Config > Default(2222)
    let final_port = cli_port.or(ssh_config.port).unwrap_or(2222);

    let addr = format!("0.0.0.0:{}", final_port);
    let listener = match TcpListener::bind(&addr) {
        Ok(l) => l,
        Err(e) => {
            eprintln(&format!("[SSHD] Failed to bind to {}: {:?}", addr, e));
            exit(1);
        }
    };

    println(&format!("[SSHD] Listening on {}...", addr));
    println(&format!("[SSHD] Shell: {}", ssh_config.shell));

    if let Err(e) = listener.set_nonblocking(true) {
        eprintln(&format!("[SSHD] Failed to set listener non-blocking: {:?}", e));
        exit(1);
    }

    serve(listener, ssh_config);
}

/// Serve connections from a single process, cooperatively.
///
/// Every live connection is one `handle_connection` future in `sessions`,
/// polled round-robin each tick. The listener and every accepted socket are
/// non-blocking, and `SshStream`'s `Read`/`Write` yield `Poll::Pending` on
/// `WouldBlock` rather than erroring, so a session idling on its socket
/// suspends and lets its peers make progress.
///
/// This is the default because it is the path with service history, not because
/// it is the better design — it has two limits nothing inside it can fix: a
/// panic is `panic = "abort"` and so takes down every session at once, and it is
/// one OS thread, so sessions never use a second core and any genuinely blocking
/// syscall stalls every peer for its duration (hence the standing rule that
/// nothing reachable from a session may call `sleep_ms`; see [`yield_now`]).
/// The `fork-sessions` build removes both.
#[cfg(not(feature = "fork-sessions"))]
fn serve(listener: TcpListener, ssh_config: config::SshdConfig) -> ! {
    let mut sessions: Vec<Pin<Box<dyn Future<Output = ()>>>> = Vec::new();
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);

    loop {
        let mut did_work = false;

        match listener.try_accept() {
            Ok((stream, _addr)) => {
                did_work = true;
                if sessions.len() >= ssh_config.max_sessions {
                    // Refuse rather than queue: an accepted-but-unpolled
                    // connection looks alive to the client and hangs until it
                    // times out, and holding the fd pins one of the stack's
                    // global socket slots meanwhile.
                    eprintln(&format!(
                        "[SSHD] Refusing connection: {} sessions already live (max_sessions={})",
                        sessions.len(),
                        ssh_config.max_sessions
                    ));
                    drop(stream);
                } else {
                    println("[SSHD] Accepted connection");
                    set_nonblocking(stream.as_raw_fd(), true);
                    let ssh_stream = SshStream::new(stream);
                    let config = ssh_config.clone();
                    sessions.push(Box::pin(protocol::handle_connection(ssh_stream, config)));
                }
            }
            Err(e) if e.kind() == NetErrorKind::WouldBlock => {}
            Err(e) => {
                eprintln(&format!("[SSHD] Accept error: {:?}", e));
            }
        }

        let mut i = 0;
        while i < sessions.len() {
            match sessions[i].as_mut().poll(&mut cx) {
                Poll::Ready(()) => {
                    drop(sessions.swap_remove(i));
                    did_work = true;
                }
                Poll::Pending => i += 1,
            }
        }

        if !did_work {
            sleep_ms(1);
        }
    }
}

/// Serve connections from one forked process each (the `fork-sessions` build).
#[cfg(feature = "fork-sessions")]
fn serve(listener: TcpListener, ssh_config: config::SshdConfig) -> ! {
    println(&format!("[SSHD] Max concurrent sessions: {}", ssh_config.max_sessions));

    // Process per session: `accept()`, then `fork()`, and let the child own the
    // connection for its entire life.
    //
    // This replaced a single-process cooperative multiplexer (every connection a
    // future in a `Vec`, polled round-robin) that had two structural problems no
    // amount of care inside it could fix. First, a panic anywhere — one
    // malformed packet on one connection — is `panic = "abort"`, which is
    // process-wide, so it killed *every* live session. Second, it was one OS
    // thread: sessions could not use more than one core, and any genuinely
    // blocking syscall in one session stalled all the others (which is why every
    // loop inside a session had to remember to use `yield_now()` and never
    // `sleep_ms` — a rule enforced only by comments).
    //
    // Forking fixes both at once, and it needs no new kernel machinery.
    // `fork_process` deep-copies the fd table and refcounts each socket
    // (`FdTable::clone_deep_for_fork` → `socket_clone_ref`), so the accepted
    // connection is simply *inherited* — no fd has to be passed anywhere, which
    // is what `docs/MISSING_SOCKET_MACHINERY.md` concluded was impossible after
    // surveying `sys_spawn`, `SCM_RIGHTS` and procfs, but never `fork()`.
    // `userspace/forkprobe` is the standing proof that all of this holds for a
    // `no_std` libakuma binary specifically.
    let mut live_sessions: usize = 0;

    loop {
        let mut did_work = false;

        // Reap first, so the slot a just-finished session freed is available to
        // the connection arriving on this same tick rather than the next one.
        while let Some(status) = wait_any() {
            live_sessions = live_sessions.saturating_sub(1);
            if status.signaled() {
                // The whole point of the fork: this killed one session, not the
                // server. Worth logging loudly — it means a real bug in the
                // protocol code that would previously have taken everything down.
                eprintln(&format!(
                    "[SSHD] Session pid {} died from signal {:?} ({} live)",
                    status.pid,
                    status.term_signal(),
                    live_sessions
                ));
            }
            did_work = true;
        }

        match listener.try_accept() {
            Ok((stream, _addr)) => {
                did_work = true;
                let conn_fd = stream.as_raw_fd();

                if live_sessions >= ssh_config.max_sessions {
                    // Refuse rather than queue. A connection accepted but not
                    // served looks alive to the client and hangs until it times
                    // out; closing immediately gives it an honest error, and
                    // holding the fd would also leak one of the 128 global
                    // socket slots for as long as the flood lasts.
                    eprintln(&format!(
                        "[SSHD] Refusing connection: {} sessions already live (max_sessions={})",
                        live_sessions, ssh_config.max_sessions
                    ));
                    drop(stream);
                } else {
                    match fork() {
                        Ok(ForkResult::Child) => {
                            // Drop the listener: a child that keeps it alive
                            // would both hold the port open past the parent's
                            // death and be able to steal connections from it.
                            // `TcpListener`'s fd is closed by the kernel on our
                            // `exit()` anyway, but only after this session ends
                            // — which can be hours.
                            close(listener.as_raw_fd());

                            // The connection stays *non-blocking* even though
                            // this process now has only one session to serve.
                            // Two things still require it: `SshStream`'s
                            // `Read`/`Write` turn `WouldBlock` into
                            // `Poll::Pending` (a blocking fd would never
                            // produce it, so `block_on` would park inside the
                            // syscall instead of at its own sleep), and
                            // `bridge_process` polls the socket and the shell's
                            // stdout in one loop via `try_read`, which is only
                            // correct on a non-blocking fd.
                            set_nonblocking(conn_fd, true);

                            let ssh_stream = SshStream::new(stream);
                            block_on(protocol::handle_connection(ssh_stream, ssh_config.clone()));
                            exit(0);
                        }
                        Ok(ForkResult::Parent(pid)) => {
                            live_sessions += 1;
                            println(&format!(
                                "[SSHD] Accepted connection -> session pid {} ({}/{} live)",
                                pid, live_sessions, ssh_config.max_sessions
                            ));
                            // Critical: drop the parent's copy of the accepted
                            // fd. It is refcounted, so this does not disturb the
                            // child — but skipping it leaks a socket slot per
                            // connection and, worse, leaves the parent holding a
                            // half of a connection it will never read, so the
                            // peer never sees a clean close.
                            drop(stream);
                        }
                        Err(e) => {
                            // ENOMEM here means the global process table or
                            // kernel memory is exhausted — nothing sshd can do
                            // but decline this one and keep serving the rest.
                            eprintln(&format!(
                                "[SSHD] fork failed ({}), dropping connection",
                                e
                            ));
                            drop(stream);
                        }
                    }
                }
            }
            Err(e) if e.kind() == NetErrorKind::WouldBlock => {}
            Err(e) => {
                eprintln(&format!("[SSHD] Accept error: {:?}", e));
            }
        }

        if !did_work {
            sleep_ms(1);
        }
    }}

/// Yield exactly one poll cycle back to this session's `block_on`, then resume
/// on the next poll.
///
/// Since the process-per-session split (see `main`) this is a busy-poll: each
/// session owns its process, so yielding no longer hands time to a *peer
/// session*, only back to the local `block_on` loop, which sleeps 1ms whenever
/// a poll comes back `Pending`. It is still the right thing to call inside a
/// session's nested loops — `sleep_ms` there would park this session's process
/// for the full duration without giving `block_on` a chance to notice the
/// future is ready — but starving a sibling connection is no longer among the
/// consequences of getting it wrong.
///
/// Kept as its own function rather than inlined because the *previous*
/// architecture made this load-bearing in a way worth remembering: under the
/// single-process cooperative executor, one session calling `sleep_ms` in a
/// loop (rather than `.await`ing) never returned `Poll::Pending`, so the
/// executor's `poll()` on that session never returned, and the first session to
/// reach `bridge_process`'s idle branch monopolized the whole server until its
/// shell exited. Forking removed the blast radius, not the reason to yield.
pub async fn yield_now() {
    let mut yielded = false;
    poll_fn(|cx| {
        if yielded {
            Poll::Ready(())
        } else {
            yielded = true;
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    })
    .await
}

fn noop_waker() -> Waker {
    static VTABLE: RawWakerVTable = RawWakerVTable::new(
        |_| RawWaker::new(core::ptr::null(), &VTABLE),
        |_| {},
        |_| {},
        |_| {},
    );
    unsafe { Waker::from_raw(RawWaker::new(core::ptr::null(), &VTABLE)) }
}

fn block_on<F: core::future::Future>(mut future: F) -> F::Output {
    let mut future = unsafe { Pin::new_unchecked(&mut future) };
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);

    loop {
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(val) => return val,
            Poll::Pending => {
                sleep_ms(1);
            }
        }
    }
}
