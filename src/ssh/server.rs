//! SSH Server
//!
//! Implements the SSH server loop using smoltcp sockets.
//! Runs on a dedicated system thread.

use alloc::format;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use smoltcp::socket::tcp;
use akuma_net::smoltcp_net::{self, SocketHandle, with_network};
use super::protocol;

// ============================================================================
// Constants
// ============================================================================

const MAX_CONNECTIONS: usize = 4;

/// Count of active SSH sessions (live gauge)
static ACTIVE_SESSIONS: AtomicUsize = AtomicUsize::new(0);

/// Cumulative count of sessions ever accepted
static SESSIONS_OPENED: AtomicU64 = AtomicU64::new(0);
/// Cumulative count of sessions that ran to completion (success or normal close)
static SESSIONS_CLOSED: AtomicU64 = AtomicU64::new(0);
/// Sessions that failed before SSH handshake completed
static HANDSHAKE_FAIL: AtomicU64 = AtomicU64::new(0);
/// Sessions that failed pubkey/password auth
static AUTH_FAIL: AtomicU64 = AtomicU64::new(0);
/// Sessions whose handler thread panicked (incremented from SessionGuard::drop on unwind)
static PANICKED: AtomicU64 = AtomicU64::new(0);
/// True while the accept loop is alive. Last-tick uptime is in SERVER_TICK_US.
static SERVER_ALIVE: AtomicBool = AtomicBool::new(false);
/// Uptime (us) at the last accept-loop iteration; used by the supervisor in main.rs.
static SERVER_TICK_US: AtomicU64 = AtomicU64::new(0);

/// Snapshot of SSH server counters for the heartbeat in src/main.rs.
#[derive(Copy, Clone)]
pub struct SshStats {
    pub alive: bool,
    pub active: usize,
    pub opened: u64,
    pub closed: u64,
    pub handshake_fail: u64,
    pub auth_fail: u64,
    pub panicked: u64,
    pub last_tick_us: u64,
}

pub fn stats() -> SshStats {
    SshStats {
        alive: SERVER_ALIVE.load(Ordering::Acquire),
        active: ACTIVE_SESSIONS.load(Ordering::Acquire),
        opened: SESSIONS_OPENED.load(Ordering::Relaxed),
        closed: SESSIONS_CLOSED.load(Ordering::Relaxed),
        handshake_fail: HANDSHAKE_FAIL.load(Ordering::Relaxed),
        auth_fail: AUTH_FAIL.load(Ordering::Relaxed),
        panicked: PANICKED.load(Ordering::Relaxed),
        last_tick_us: SERVER_TICK_US.load(Ordering::Relaxed),
    }
}

pub(crate) fn note_handshake_fail() {
    HANDSHAKE_FAIL.fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn note_auth_fail() {
    AUTH_FAIL.fetch_add(1, Ordering::Relaxed);
}

/// Test entry point: simulate the counter bookkeeping that
/// `SessionGuard::drop` does, without touching a real socket. Called from
/// `ssh_tests.rs`. Kept `pub(crate)` and unconditional because the kernel
/// has no `cfg(test)` story — its self-tests are regular functions.
pub(crate) fn test_note_session_open() {
    ACTIVE_SESSIONS.fetch_add(1, Ordering::AcqRel);
    SESSIONS_OPENED.fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn test_note_session_close(panicked: bool) {
    ACTIVE_SESSIONS.fetch_sub(1, Ordering::AcqRel);
    SESSIONS_CLOSED.fetch_add(1, Ordering::Relaxed);
    if panicked {
        PANICKED.fetch_add(1, Ordering::Relaxed);
    }
}

// ============================================================================
// SSH Server Loop
// ============================================================================

/// Run the SSH server (Blocking - should be spawned on a thread)
pub fn run() -> ! {
    log(&format!("[SSH Server] Starting SSH server on port {}...\n", crate::config::SSH_PORT));

    // Enable async process execution for pipeline/buffered commands
    crate::shell::enable_async_exec();

    // Initialize host keys
    super::init_host_key();

    // Create initial listening socket
    let mut listen_handle = match create_listener() {
        Some(h) => h,
        None => {
            log(&format!("[SSH Server] FATAL: Failed to create listener on port {}\n", crate::config::SSH_PORT));
            loop { akuma_exec::threading::yield_now(); }
        }
    };

    log("[SSH Server] Listening...\n");
    SERVER_ALIVE.store(true, Ordering::Release);

    loop {
        SERVER_TICK_US.store((akuma_exec::runtime::runtime().uptime_us)(), Ordering::Relaxed);

        // Poll for new connection
        let mut established = false;
        with_network(|net| {
            let socket = net.sockets.get_mut::<tcp::Socket>(listen_handle);
            if socket.state() == tcp::State::Established {
                established = true;
            }
        });

        if established {
            let active = ACTIVE_SESSIONS.load(Ordering::Acquire);
            if active < MAX_CONNECTIONS {
                ACTIVE_SESSIONS.fetch_add(1, Ordering::AcqRel);
                SESSIONS_OPENED.fetch_add(1, Ordering::Relaxed);
                log(&format!("[SSH Accept] new session (active: {})\n", active + 1));

                // Hand off the connected socket to a session thread
                let session_handle = listen_handle;

                // CRITICAL: if spawn fails the SessionGuard never runs, so
                // we must roll the counters back ourselves AND close the
                // socket. Without this rollback, every failed spawn leaks
                // a slot in ACTIVE_SESSIONS forever. Discovered via harness
                // `parallel` run, 2026-05-29.
                if let Err(_e) = akuma_exec::threading::spawn_system_thread_fn(move || {
                    run_session(session_handle);
                }) {
                    log("[SSH Server] WARNING: spawn_system_thread_fn failed; rolling back counters\n");
                    smoltcp_net::socket_close(session_handle);
                    ACTIVE_SESSIONS.fetch_sub(1, Ordering::AcqRel);
                    SESSIONS_CLOSED.fetch_add(1, Ordering::Relaxed);
                }

                // Create a NEW listening socket for the server loop
                match create_listener() {
                    Some(h) => listen_handle = h,
                    None => {
                        log("[SSH Server] Failed to recreate listener\n");
                        break;
                    }
                }
            } else {
                log("[SSH Server] Too many connections, rejecting\n");
                smoltcp_net::socket_close(listen_handle);
                
                // Recreate listener
                match create_listener() {
                    Some(h) => listen_handle = h,
                    None => break,
                }
            }
        }

        smoltcp_net::poll();
        akuma_exec::threading::yield_now();
    }
    
    log("[SSH Server] Server loop exited abnormally\n");
    loop { akuma_exec::threading::yield_now(); }
}

fn create_listener() -> Option<SocketHandle> {
    let handle = smoltcp_net::socket_create()?;
    let res = with_network(|net| {
        let socket = net.sockets.get_mut::<tcp::Socket>(handle);
        socket.listen(crate::config::SSH_PORT)
    });
    
    match res {
        Some(Ok(())) => Some(handle),
        _ => {
            smoltcp_net::socket_close(handle);
            None
        }
    }
}

use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};

fn block_on<F: Future>(mut future: F) -> F::Output {
    let mut future = unsafe { Pin::new_unchecked(&mut future) };

    // Real waker so smoltcp properly tracks this thread. We use yield_now()
    // instead of schedule_blocking() because schedule_blocking sets the thread
    // WAITING, which causes ThreadWaker::wake() to trigger SGI for an immediate
    // context switch. If that SGI fires while the network thread holds the
    // NETWORK spinlock (inside iface.poll()), we context-switch here and
    // deadlock trying to acquire NETWORK in the next future.poll().
    let waker = akuma_exec::threading::current_thread_waker();
    let mut cx = Context::from_waker(&waker);

    loop {
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(val) => return val,
            Poll::Pending => {
                if smoltcp_net::poll() {
                    continue;
                }
                akuma_exec::threading::yield_now();
            }
        }
    }
}

/// RAII guard that releases all per-session resources on either normal
/// return OR panic-unwind from `block_on`.
///
/// Without this, a panic inside `handle_connection` skipped `socket_close`
/// and left the smoltcp handle stuck in `pending_removal` for the 30s
/// `SOCKET_GC_TIMEOUT_US` window — four panics within 30s wedged SSH
/// because `ACTIVE_SESSIONS` also stayed inflated.
struct SessionGuard {
    handle: SocketHandle,
    panicked: bool,
}

impl SessionGuard {
    fn new(handle: SocketHandle) -> Self {
        Self { handle, panicked: true }
    }
    fn finish(mut self) {
        self.panicked = false;
    }
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        smoltcp_net::socket_close(self.handle);
        ACTIVE_SESSIONS.fetch_sub(1, Ordering::AcqRel);
        SESSIONS_CLOSED.fetch_add(1, Ordering::Relaxed);
        if self.panicked {
            PANICKED.fetch_add(1, Ordering::Relaxed);
            log("[SSH Session] WARNING: handler panicked — resources reclaimed via SessionGuard\n");
        }
    }
}

fn run_session(handle: SocketHandle) -> ! {
    // CRITICAL: keep SessionGuard inside an inner block so its Drop runs
    // BEFORE we enter the terminal `loop { yield_now() }`. The function
    // never returns (-> !), so a guard at the function scope would
    // never be dropped — socket_close + ACTIVE_SESSIONS.fetch_sub +
    // SESSIONS_CLOSED.fetch_add would never run, and `open - close`
    // would diverge from the real live-session count under sustained
    // load. (Discovered via harness `parallel` run on 2026-05-29:
    // ACTIVE_SESSIONS stuck at 4 while the actual thread count was 3
    // and the accept loop was rejecting "Too many connections".)
    {
        let stream = smoltcp_net::TcpStream::new(handle);
        let guard = SessionGuard::new(handle);

        block_on(async {
            protocol::handle_connection(stream).await;
        });

        guard.finish();
        // `guard` drops here at end of block: socket_close,
        // ACTIVE_SESSIONS.fetch_sub, SESSIONS_CLOSED.fetch_add.
    }

    akuma_exec::threading::mark_current_terminated();
    loop { akuma_exec::threading::yield_now(); }
}

fn log(msg: &str) {
    safe_print!(256, "{}", msg);
}
