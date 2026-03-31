//! SSH Server
//!
//! Implements the SSH server loop using smoltcp sockets.
//! Runs on a dedicated system thread.

use alloc::format;
use core::sync::atomic::{AtomicUsize, Ordering};

use smoltcp::socket::tcp;
use akuma_net::smoltcp_net::{self, SocketHandle, with_network};
use super::protocol;

// ============================================================================
// Constants
// ============================================================================

const MAX_CONNECTIONS: usize = 4;

/// Count of active SSH sessions
static ACTIVE_SESSIONS: AtomicUsize = AtomicUsize::new(0);

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

    loop {
        // Poll for new connection
        let mut established = false;
        with_network(|net| {
            let socket = net.sockets.get_mut::<tcp::Socket>(listen_handle);
            if socket.state() == tcp::State::Established {
                established = true;
            }
        });

        if established {
            let active = ACTIVE_SESSIONS.load(Ordering::Relaxed);
            if active < MAX_CONNECTIONS {
                ACTIVE_SESSIONS.fetch_add(1, Ordering::Relaxed);
                log(&format!("[SSH Server] Accepted connection (active: {})\n", active + 1));

                // Hand off the connected socket to a session thread
                let session_handle = listen_handle;
                
                let _ = akuma_exec::threading::spawn_system_thread_fn(move || {
                    run_session(session_handle);
                });

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

fn run_session(handle: SocketHandle) -> ! {
    let stream = smoltcp_net::TcpStream::new(handle);
    
    block_on(async {
        protocol::handle_connection(stream).await;
    });

    smoltcp_net::socket_close(handle);
    ACTIVE_SESSIONS.fetch_sub(1, Ordering::Relaxed);
    
    akuma_exec::threading::mark_current_terminated();
    loop { akuma_exec::threading::yield_now(); }
}

fn log(msg: &str) {
    safe_print!(256, "{}", msg);
}
