//! SSH Server - Concurrent Multi-Session Accept Loop
//!
//! Manages the SSH server accept loop that handles multiple
//! concurrent SSH sessions. Each connection runs in parallel.

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

use embassy_net::Stack;
use embassy_net::tcp::TcpSocket;
use embassy_time::Duration;

use crate::async_net::TcpStream;
use crate::console;
use crate::ssh;

// ============================================================================
// Constants
// ============================================================================

const SSH_PORT: u16 = 22;
const MAX_CONNECTIONS: usize = 4;
const TCP_RX_BUFFER_SIZE: usize = 4096;
const TCP_TX_BUFFER_SIZE: usize = 4096;

// ============================================================================
// Static Buffers - Simple approach without complex pool
// ============================================================================

/// Static buffers for connections - avoids dynamic allocation
/// We use a simple static array approach
static mut RX_BUFFERS: [[u8; TCP_RX_BUFFER_SIZE]; MAX_CONNECTIONS + 1] =
    [[0u8; TCP_RX_BUFFER_SIZE]; MAX_CONNECTIONS + 1];
static mut TX_BUFFERS: [[u8; TCP_TX_BUFFER_SIZE]; MAX_CONNECTIONS + 1] =
    [[0u8; TCP_TX_BUFFER_SIZE]; MAX_CONNECTIONS + 1];
static mut BUFFER_IN_USE: [bool; MAX_CONNECTIONS + 1] = [false; MAX_CONNECTIONS + 1];

/// Allocate a buffer slot
fn alloc_buffer_slot() -> Option<usize> {
    unsafe {
        for i in 0..=MAX_CONNECTIONS {
            if !BUFFER_IN_USE[i] {
                BUFFER_IN_USE[i] = true;
                return Some(i);
            }
        }
        None
    }
}

/// Free a buffer slot
fn free_buffer_slot(slot: usize) {
    unsafe {
        if slot <= MAX_CONNECTIONS {
            BUFFER_IN_USE[slot] = false;
        }
    }
}

/// Get buffer references for a slot
/// SAFETY: Caller must ensure single-threaded access and slot is allocated
unsafe fn get_buffer_refs(slot: usize) -> (&'static mut [u8], &'static mut [u8]) {
    unsafe { (&mut RX_BUFFERS[slot][..], &mut TX_BUFFERS[slot][..]) }
}

// ============================================================================
// Connection State
// ============================================================================

/// Active SSH connection being handled
struct ActiveConnection {
    future: Pin<Box<dyn Future<Output = ()>>>,
    id: usize,
    buffer_slot: usize,
}

// ============================================================================
// SSH Server with Concurrent Connections
// ============================================================================

/// Run the SSH server with support for multiple concurrent connections
pub async fn run(stack: Stack<'static>) {
    log("[SSH Server] Starting SSH server on port 22...\n");
    log(&alloc::format!(
        "[SSH Server] Max concurrent connections: {}\n",
        MAX_CONNECTIONS
    ));
    log("[SSH Server] Connect with: ssh -o StrictHostKeyChecking=no user@localhost -p 2222\n");

    // Initialize shared host key
    ssh::init_host_key();

    // Active connections
    let mut connections: Vec<ActiveConnection> = Vec::new();
    let mut next_id: usize = 0;

    // Listening socket state
    let mut listen_socket: Option<TcpSocket<'static>> = None;
    let mut listen_buffer_slot: Option<usize> = None;

    // Create waker for manual polling
    static VTABLE: RawWakerVTable = RawWakerVTable::new(
        |_| RawWaker::new(core::ptr::null(), &VTABLE),
        |_| {},
        |_| {},
        |_| {},
    );

    loop {
        let raw_waker = RawWaker::new(core::ptr::null(), &VTABLE);
        let waker = unsafe { Waker::from_raw(raw_waker) };
        let mut cx = Context::from_waker(&waker);

        // =====================================================================
        // Poll all active connections first
        // =====================================================================
        let mut i = 0;
        while i < connections.len() {
            match connections[i].future.as_mut().poll(&mut cx) {
                Poll::Ready(()) => {
                    let conn = connections.swap_remove(i);
                    log(&alloc::format!(
                        "[SSH Server] Connection {} ended (active: {})\n",
                        conn.id,
                        connections.len()
                    ));
                    // Return the buffer slot
                    free_buffer_slot(conn.buffer_slot);
                }
                Poll::Pending => {
                    i += 1;
                }
            }
        }

        // =====================================================================
        // Accept new connection if we have capacity
        // =====================================================================
        if connections.len() < MAX_CONNECTIONS {
            // Ensure we have a listening socket
            if listen_socket.is_none() {
                // Get or allocate buffer slot
                let slot = listen_buffer_slot
                    .unwrap_or_else(|| alloc_buffer_slot().expect("No buffer slots available"));
                listen_buffer_slot = Some(slot);

                // Create socket with static buffers
                let (rx, tx) = unsafe { get_buffer_refs(slot) };
                let mut socket = TcpSocket::new(stack, rx, tx);
                socket.set_timeout(Some(Duration::from_secs(60)));
                listen_socket = Some(socket);
            }

            if let Some(ref mut socket) = listen_socket {
                // If no active connections, we can block on accept
                if connections.is_empty() {
                    match socket.accept(SSH_PORT).await {
                        Ok(()) => {
                            let id = next_id;
                            next_id = next_id.wrapping_add(1);
                            let buffer_slot = listen_buffer_slot.take().unwrap();

                            log(&alloc::format!(
                                "[SSH Server] Accepted connection {} (active: 1)\n",
                                id
                            ));

                            crate::network::increment_connections();

                            let connected_socket = listen_socket.take().unwrap();
                            let stream = TcpStream::from_socket(connected_socket);
                            let future = Box::pin(handle_connection_wrapper(stream, id));
                            connections.push(ActiveConnection {
                                future,
                                id,
                                buffer_slot,
                            });
                        }
                        Err(e) => {
                            log(&alloc::format!("[SSH Server] Accept error: {:?}\n", e));
                            // Keep buffer slot, just recreate socket
                            listen_socket = None;
                        }
                    }
                } else {
                    // Have active connections - use timeout to avoid blocking
                    match embassy_time::with_timeout(
                        Duration::from_millis(10),
                        socket.accept(SSH_PORT),
                    )
                    .await
                    {
                        Ok(Ok(())) => {
                            let id = next_id;
                            next_id = next_id.wrapping_add(1);
                            let buffer_slot = listen_buffer_slot.take().unwrap();

                            log(&alloc::format!(
                                "[SSH Server] Accepted connection {} (active: {})\n",
                                id,
                                connections.len() + 1
                            ));

                            crate::network::increment_connections();

                            let connected_socket = listen_socket.take().unwrap();
                            let stream = TcpStream::from_socket(connected_socket);
                            let future = Box::pin(handle_connection_wrapper(stream, id));
                            connections.push(ActiveConnection {
                                future,
                                id,
                                buffer_slot,
                            });
                        }
                        Ok(Err(e)) => {
                            log(&alloc::format!("[SSH Server] Accept error: {:?}\n", e));
                            listen_socket = None;
                        }
                        Err(_) => {
                            // Timeout - abort socket but keep buffer slot
                            socket.abort();
                            listen_socket = None;
                        }
                    }
                }
            }
        } else {
            embassy_time::Timer::after(Duration::from_millis(1)).await;
        }

        // Check embassy time alarms
        crate::embassy_time_driver::on_timer_interrupt();
    }
}

/// Wrapper for handle_connection that logs start/end
async fn handle_connection_wrapper(stream: TcpStream, id: usize) {
    log(&alloc::format!("[SSH {}] Starting session\n", id));
    ssh::handle_connection(stream).await;
    log(&alloc::format!("[SSH {}] Session ended\n", id));
}

fn log(msg: &str) {
    console::print(msg);
}
