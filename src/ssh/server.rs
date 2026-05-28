//! SSH Server
//!
//! Implements the SSH server loop using smoltcp sockets.
//! Runs on a dedicated system thread.

use alloc::format;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};

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

/// Last step the accept loop reached, for the supervisor's stall report.
/// 0=idle, 1=tick, 2=pre-with_network, 3=post-with_network, 4=spawn,
/// 5=create_listener, 6=poll, 7=yield. If the supervisor sees STALLED with
/// step stuck at 2, candidate (a) (NETWORK contention) is the cause; at 6,
/// candidate (b) (poll() stuck); at 5, candidate (c) (listener handle bad).
pub(crate) static SERVER_STEP: AtomicU8 = AtomicU8::new(0);

/// True if the current `listen_handle` in the accept loop is still a valid
/// smoltcp socket. Cleared if the GC ever frees it out from under us.
static LISTENER_HANDLE_VALID: AtomicBool = AtomicBool::new(false);

pub mod step {
    #[allow(dead_code)]
    pub const IDLE: u8 = 0;
    pub const TICK: u8 = 1;
    pub const PRE_WITH_NETWORK: u8 = 2;
    pub const POST_WITH_NETWORK: u8 = 3;
    pub const SPAWN: u8 = 4;
    pub const CREATE_LISTENER: u8 = 5;
    pub const POLL: u8 = 6;
    pub const YIELD: u8 = 7;

    #[must_use]
    pub fn name(v: u8) -> &'static str {
        match v {
            TICK => "tick",
            PRE_WITH_NETWORK => "pre_with_network",
            POST_WITH_NETWORK => "post_with_network",
            SPAWN => "spawn",
            CREATE_LISTENER => "create_listener",
            POLL => "poll",
            YIELD => "yield",
            _ => "idle",
        }
    }
}

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
    /// Last accept-loop step the server reached. See `server::step::*`.
    pub last_step: u8,
    /// Whether the current `listen_handle` is still a valid smoltcp socket.
    pub listener_valid: bool,
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
        last_step: SERVER_STEP.load(Ordering::Relaxed),
        listener_valid: LISTENER_HANDLE_VALID.load(Ordering::Relaxed),
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
    LISTENER_HANDLE_VALID.store(true, Ordering::Relaxed);

    log("[SSH Server] Listening...\n");
    SERVER_ALIVE.store(true, Ordering::Release);

    loop {
        SERVER_STEP.store(step::TICK, Ordering::Relaxed);
        SERVER_TICK_US.store((akuma_exec::runtime::runtime().uptime_us)(), Ordering::Relaxed);

        // Poll for new connection
        SERVER_STEP.store(step::PRE_WITH_NETWORK, Ordering::Relaxed);
        let mut established = false;
        let lookup = with_network(|net| {
            let socket = net.sockets.get_mut::<tcp::Socket>(listen_handle);
            if socket.state() == tcp::State::Established {
                established = true;
            }
        });
        // `with_network` returns `None` only if NETWORK isn't initialized,
        // which is unreachable here — but if smoltcp ever frees our handle
        // out from under us (candidate (c) in STABILITY_URGENT_ISSUES.md),
        // `sockets.get_mut` would have panicked above. Stamp the flag on
        // success.
        LISTENER_HANDLE_VALID.store(lookup.is_some(), Ordering::Relaxed);
        SERVER_STEP.store(step::POST_WITH_NETWORK, Ordering::Relaxed);

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
                SERVER_STEP.store(step::SPAWN, Ordering::Relaxed);
                if let Err(_e) = akuma_exec::threading::spawn_system_thread_fn(move || {
                    run_session(session_handle);
                }) {
                    log("[SSH Server] WARNING: spawn_system_thread_fn failed; rolling back counters\n");
                    smoltcp_net::socket_close(session_handle);
                    ACTIVE_SESSIONS.fetch_sub(1, Ordering::AcqRel);
                    SESSIONS_CLOSED.fetch_add(1, Ordering::Relaxed);
                }

                // Create a NEW listening socket for the server loop. If the
                // socket pool is exhausted (e.g. pending_removal hasn't
                // drained yet under connect-storm), retry with backoff
                // rather than breaking out of the accept loop — pending_removal
                // is GC'd inside poll() and clears within SOCKET_GC_TIMEOUT_US
                // (30s) of the last close. Phase-2 fix for the stall observed
                // in logs/stall-20260529-021802.log: previously `None => break`
                // would terminate the accept loop permanently with
                // SERVER_ALIVE still set. See docs/STABILITY_URGENT_ISSUES.md.
                SERVER_STEP.store(step::CREATE_LISTENER, Ordering::Relaxed);
                LISTENER_HANDLE_VALID.store(false, Ordering::Relaxed);
                listen_handle = recreate_listener_with_retry();
                LISTENER_HANDLE_VALID.store(true, Ordering::Relaxed);
            } else {
                log("[SSH Server] Too many connections, rejecting\n");
                SERVER_STEP.store(step::CREATE_LISTENER, Ordering::Relaxed);
                LISTENER_HANDLE_VALID.store(false, Ordering::Relaxed);
                smoltcp_net::socket_close(listen_handle);
                listen_handle = recreate_listener_with_retry();
                LISTENER_HANDLE_VALID.store(true, Ordering::Relaxed);
            }
        }

        SERVER_STEP.store(step::POLL, Ordering::Relaxed);
        smoltcp_net::poll();
        SERVER_STEP.store(step::YIELD, Ordering::Relaxed);
        akuma_exec::threading::yield_now();
    }
}

/// Recreate the listener, retrying forever if the socket pool is exhausted.
/// Each attempt drives one `poll()` (so the GC sweep can drain
/// `pending_removal`) and yields. Logs a warning every 100 attempts so a
/// pathological case is still visible in the heartbeat.
fn recreate_listener_with_retry() -> SocketHandle {
    let mut attempts: u32 = 0;
    loop {
        if let Some(h) = create_listener() {
            if attempts > 0 {
                log(&format!(
                    "[SSH Server] Recovered listener after {} attempts\n",
                    attempts
                ));
            }
            return h;
        }
        attempts = attempts.saturating_add(1);
        if attempts % 100 == 0 {
            log(&format!(
                "[SSH Server] Listener creation still failing after {} attempts (socket pool exhausted; waiting for pending_removal GC)\n",
                attempts
            ));
        }
        // Drive poll() to advance the GC sweep, then yield so other threads
        // (including session threads closing their sockets) can run.
        smoltcp_net::poll();
        akuma_exec::threading::yield_now();
    }
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
