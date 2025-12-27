//! SSH Server - Concurrent Multi-Session Accept Loop
//!
//! Manages the SSH server accept loop that handles multiple
//! concurrent SSH sessions. Each connection runs in parallel.
//!
//! Uses a static buffer pool to avoid memory leaks from Box::leak().

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

use embassy_net::tcp::TcpSocket;
use embassy_net::Stack;
use embassy_time::Duration;

use crate::async_net::TcpStream;
use crate::console;
use crate::ssh;

// ============================================================================
// Constants
// ============================================================================

const SSH_PORT: u16 = 22;
const MAX_CONNECTIONS: usize = 8;
const TCP_RX_BUFFER_SIZE: usize = 4096;
const TCP_TX_BUFFER_SIZE: usize = 4096;

// ============================================================================
// Static Buffer Pool
// ============================================================================

/// Total buffer slots: MAX_CONNECTIONS for active connections + 1 for listening
const BUFFER_POOL_SIZE: usize = MAX_CONNECTIONS + 1;

/// Static buffer storage for TCP sockets
/// Each slot has rx and tx buffers
struct BufferPool {
    /// RX buffers for each slot
    rx_buffers: [[u8; TCP_RX_BUFFER_SIZE]; BUFFER_POOL_SIZE],
    /// TX buffers for each slot  
    tx_buffers: [[u8; TCP_TX_BUFFER_SIZE]; BUFFER_POOL_SIZE],
    /// Bitmap of which slots are in use (true = in use)
    in_use: [bool; BUFFER_POOL_SIZE],
}

impl BufferPool {
    const fn new() -> Self {
        Self {
            rx_buffers: [[0u8; TCP_RX_BUFFER_SIZE]; BUFFER_POOL_SIZE],
            tx_buffers: [[0u8; TCP_TX_BUFFER_SIZE]; BUFFER_POOL_SIZE],
            in_use: [false; BUFFER_POOL_SIZE],
        }
    }

    /// Allocate a buffer slot, returns the slot index or None if pool exhausted
    fn alloc(&mut self) -> Option<usize> {
        for i in 0..BUFFER_POOL_SIZE {
            if !self.in_use[i] {
                self.in_use[i] = true;
                return Some(i);
            }
        }
        None
    }

    /// Free a buffer slot
    fn free(&mut self, slot: usize) {
        if slot < BUFFER_POOL_SIZE {
            self.in_use[slot] = false;
            // Zero the buffers for security
            self.rx_buffers[slot].fill(0);
            self.tx_buffers[slot].fill(0);
        }
    }

    /// Get mutable references to the rx and tx buffers for a slot
    /// SAFETY: Caller must ensure slot is allocated and not used elsewhere
    unsafe fn get_buffers(&mut self, slot: usize) -> (&'static mut [u8], &'static mut [u8]) {
        let rx = &mut self.rx_buffers[slot] as *mut [u8; TCP_RX_BUFFER_SIZE];
        let tx = &mut self.tx_buffers[slot] as *mut [u8; TCP_TX_BUFFER_SIZE];
        unsafe {
            (
                core::slice::from_raw_parts_mut(rx as *mut u8, TCP_RX_BUFFER_SIZE),
                core::slice::from_raw_parts_mut(tx as *mut u8, TCP_TX_BUFFER_SIZE),
            )
        }
    }
}

/// Global buffer pool protected by spinlock
static BUFFER_POOL: spinning_top::Spinlock<BufferPool> = spinning_top::Spinlock::new(BufferPool::new());

// ============================================================================
// Connection State
// ============================================================================

/// Active SSH connection being handled
struct ActiveConnection {
    future: Pin<Box<dyn Future<Output = ()>>>,
    id: usize,
    /// Buffer slot index used by this connection (for returning to pool)
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
    log(&alloc::format!(
        "[SSH Server] Buffer pool size: {} slots ({} KB)\n",
        BUFFER_POOL_SIZE,
        BUFFER_POOL_SIZE * (TCP_RX_BUFFER_SIZE + TCP_TX_BUFFER_SIZE) / 1024
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
                    // Return the buffer slot to the pool
                    BUFFER_POOL.lock().free(conn.buffer_slot);
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
                match create_listen_socket(stack) {
                    Some((socket, slot)) => {
                        listen_socket = Some(socket);
                        listen_buffer_slot = Some(slot);
                    }
                    None => {
                        // No buffers available, wait and try again
                        embassy_time::Timer::after(Duration::from_millis(10)).await;
                        continue;
                    }
                }
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

                            // Track connection in stats
                            crate::network::increment_connections();

                            // Take the socket for this connection
                            let connected_socket = listen_socket.take().unwrap();
                            let stream = TcpStream::from_socket(connected_socket);
                            let future = Box::pin(handle_connection_wrapper(stream, id));
                            connections.push(ActiveConnection { future, id, buffer_slot });
                        }
                        Err(e) => {
                            log(&alloc::format!("[SSH Server] Accept error: {:?}\n", e));
                            // Return buffer to pool and reset socket
                            if let Some(slot) = listen_buffer_slot.take() {
                                BUFFER_POOL.lock().free(slot);
                            }
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

                            // Track connection in stats
                            crate::network::increment_connections();

                            // Take the socket for this connection
                            let connected_socket = listen_socket.take().unwrap();
                            let stream = TcpStream::from_socket(connected_socket);
                            let future = Box::pin(handle_connection_wrapper(stream, id));
                            connections.push(ActiveConnection { future, id, buffer_slot });
                        }
                        Ok(Err(e)) => {
                            log(&alloc::format!("[SSH Server] Accept error: {:?}\n", e));
                            // Return buffer to pool and reset socket
                            if let Some(slot) = listen_buffer_slot.take() {
                                BUFFER_POOL.lock().free(slot);
                            }
                            listen_socket = None;
                        }
                        Err(_) => {
                            // Timeout - no new connection, that's okay
                            // Abort the socket but DON'T destroy it - reuse it!
                            // This avoids leaking buffers on every timeout
                            socket.abort();
                            // Socket stays in listen_socket, buffer_slot stays allocated
                            // Next iteration will call accept() again on the same socket
                        }
                    }
                }
            }
        } else {
            // At max capacity, just yield briefly
            embassy_time::Timer::after(Duration::from_millis(1)).await;
        }

        // Check embassy time alarms
        crate::embassy_time_driver::on_timer_interrupt();
    }
}

/// Create a new socket for listening using the buffer pool
/// Returns (socket, buffer_slot) or None if no buffers available
fn create_listen_socket(stack: Stack<'static>) -> Option<(TcpSocket<'static>, usize)> {
    // Lock once and do both alloc and get_buffers
    let (slot, rx_ref, tx_ref) = {
        let mut pool = BUFFER_POOL.lock();
        let slot = pool.alloc()?;
        // SAFETY: We just allocated this slot, so we have exclusive access
        let (rx, tx) = unsafe { pool.get_buffers(slot) };
        (slot, rx, tx)
    };

    let mut socket = TcpSocket::new(stack, rx_ref, tx_ref);
    socket.set_timeout(Some(Duration::from_secs(60)));
    Some((socket, slot))
}

/// Wrapper for handle_connection that logs start/end
async fn handle_connection_wrapper(stream: TcpStream, id: usize) {
    log(&alloc::format!("[SSH {}] Starting session\n", id));
    ssh::handle_connection(stream).await;
    log(&alloc::format!("[SSH {}] Session ended\n", id));
}

// ============================================================================
// Logging
// ============================================================================

fn log(msg: &str) {
    console::print(msg);
}
