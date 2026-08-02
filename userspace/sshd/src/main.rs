#![no_std]
#![no_main]

extern crate alloc;

use alloc::boxed::Box;
use alloc::format;
use alloc::vec::Vec;
use core::future::{poll_fn, Future};
use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
use core::pin::Pin;

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
// connection whose fd has no data/space right now yields control back to the
// multi-session executor in `main()` instead of tearing down the session —
// that's what lets a second SSH connection make progress while the first is
// idle waiting on its socket.
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
                if let Some(port_str) = args.next() {
                    if let Ok(p) = port_str.parse::<u16>() {
                        cli_port = Some(p);
                        println(&format!("[SSHD] Port override from CLI: {}", p));
                    }
                }
            }
            "--no-banner" => {
                ssh_config.banner = false;
                println("[SSHD] Banner disabled from CLI");
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

    // A single-threaded cooperative multiplexer: every live connection is one
    // `handle_connection` future here, polled to completion in turn each
    // tick. Previously `accept()` blocked and each connection ran via
    // `block_on(...)` to completion before the next `accept()`, so a second
    // simultaneous connection just waited for the first to finish. Now the
    // listener and every accepted socket are non-blocking, and `SshStream`'s
    // `Read`/`Write` impls yield `Poll::Pending` (instead of erroring out) on
    // `WouldBlock`, so a session that's idle waiting on its socket suspends
    // and lets the others make progress.
    let mut sessions: Vec<Pin<Box<dyn Future<Output = ()>>>> = Vec::new();
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);

    loop {
        let mut did_work = false;

        match listener.try_accept() {
            Ok((stream, _addr)) => {
                println("[SSHD] Accepted connection");
                set_nonblocking(stream.as_raw_fd(), true);
                let ssh_stream = SshStream::new(stream);
                let config = ssh_config.clone();
                sessions.push(Box::pin(protocol::handle_connection(ssh_stream, config)));
                did_work = true;
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

/// Yield exactly one poll cycle back to the multi-session executor in
/// `main()`, then resume on the next poll.
///
/// `sleep_ms` is a blocking syscall (raw `NANOSLEEP`) — it parks the *entire*
/// OS thread `sshd` runs on, not just the calling future. Rust only suspends
/// an `async fn` at an explicit `.await` point; a loop that calls `sleep_ms`
/// directly (no `.await` on it) never actually returns `Poll::Pending` to
/// its caller, so the executor's `poll()` call on that session never
/// returns either. In practice that meant the *first* session to reach
/// `bridge_process`'s idle "nothing to do this tick" branch monopolized the
/// executor for its entire lifetime — every other connection's
/// `try_accept`/poll starved until that session's shell exited. Use this in
/// place of `sleep_ms` in any loop nested inside a session's future.
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
