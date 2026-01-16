//! SSH Server - Thread-per-Session Architecture
//!
//! Implements preemptive multitasking for SSH sessions:
//! - Thread 0: Accept loop and network runner
//! - Threads 1-7: SSH session threads (one per active session)
//! - Threads 8+: User process threads
//!
//! Each SSH session runs on its own kernel thread, allowing true concurrent
//! execution and preemption via the timer interrupt.

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::future::Future;
use core::pin::Pin;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

use embassy_net::Stack;
use embassy_net::tcp::TcpSocket;
use embassy_time::Duration;

use super::protocol;
use crate::async_net::TcpStream;
use crate::console;

// ============================================================================
// Constants
// ============================================================================

const SSH_PORT: u16 = 22;
const MAX_CONNECTIONS: usize = 4;
const TCP_RX_BUFFER_SIZE: usize = 4096;
const TCP_TX_BUFFER_SIZE: usize = 4096;

// ============================================================================
// Static Buffer Pool - Thread-safe using atomics
// ============================================================================

/// Buffer pool for TCP connections
/// Uses atomics for allocation tracking and UnsafeCell for buffer storage
struct BufferPool {
    rx_buffers: [UnsafeCell<[u8; TCP_RX_BUFFER_SIZE]>; MAX_CONNECTIONS + 1],
    tx_buffers: [UnsafeCell<[u8; TCP_TX_BUFFER_SIZE]>; MAX_CONNECTIONS + 1],
    in_use: [AtomicBool; MAX_CONNECTIONS + 1],
}

// SAFETY: Access to individual buffer slots is serialized via the AtomicBool flags.
// Each slot can only be used by one caller at a time (ensured by alloc/free protocol).
unsafe impl Sync for BufferPool {}

impl BufferPool {
    const fn new() -> Self {
        // Helper to create arrays with const initialization
        const RX_INIT: UnsafeCell<[u8; TCP_RX_BUFFER_SIZE]> =
            UnsafeCell::new([0u8; TCP_RX_BUFFER_SIZE]);
        const TX_INIT: UnsafeCell<[u8; TCP_TX_BUFFER_SIZE]> =
            UnsafeCell::new([0u8; TCP_TX_BUFFER_SIZE]);
        const IN_USE_INIT: AtomicBool = AtomicBool::new(false);

        Self {
            rx_buffers: [RX_INIT; MAX_CONNECTIONS + 1],
            tx_buffers: [TX_INIT; MAX_CONNECTIONS + 1],
            in_use: [IN_USE_INIT; MAX_CONNECTIONS + 1],
        }
    }

    /// Try to allocate a buffer slot
    fn alloc(&self) -> Option<usize> {
        for i in 0..=MAX_CONNECTIONS {
            // Try to atomically claim this slot
            if self.in_use[i]
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                return Some(i);
            }
        }
        None
    }

    /// Free a buffer slot
    fn free(&self, slot: usize) {
        if slot <= MAX_CONNECTIONS {
            self.in_use[slot].store(false, Ordering::Release);
        }
    }

    /// Get buffer references for an allocated slot
    /// SAFETY: Caller must have successfully allocated this slot via `alloc()`
    /// and not yet freed it. The slot must not be accessed concurrently.
    unsafe fn get_buffers(&self, slot: usize) -> (&'static mut [u8], &'static mut [u8]) {
        debug_assert!(slot <= MAX_CONNECTIONS);
        debug_assert!(self.in_use[slot].load(Ordering::Acquire));

        // SAFETY: We have exclusive access to this slot (enforced by atomic in_use flag)
        unsafe {
            let rx = &mut *self.rx_buffers[slot].get();
            let tx = &mut *self.tx_buffers[slot].get();
            (rx, tx)
        }
    }
}

static BUFFER_POOL: BufferPool = BufferPool::new();

/// Count of active SSH sessions
static ACTIVE_SESSIONS: AtomicUsize = AtomicUsize::new(0);

/// Allocate a buffer slot
fn alloc_buffer_slot() -> Option<usize> {
    BUFFER_POOL.alloc()
}

/// Free a buffer slot
fn free_buffer_slot(slot: usize) {
    BUFFER_POOL.free(slot);
}

/// Get buffer references for a slot
/// SAFETY: Caller must ensure slot is allocated and not concurrently accessed
unsafe fn get_buffer_refs(slot: usize) -> (&'static mut [u8], &'static mut [u8]) {
    // SAFETY: Caller guarantees slot is allocated via alloc_buffer_slot()
    unsafe { BUFFER_POOL.get_buffers(slot) }
}

// ============================================================================
// SendableTcpStream - Wrapper for cross-thread TcpStream transfer
// ============================================================================

/// Wrapper that allows TcpStream to be sent to another thread.
///
/// SAFETY: This assumes the underlying embassy-net stack is thread-safe
/// when the runner is being polled on thread 0. The TcpSocket operations
/// go through the Stack's internal critical sections.
struct SendableTcpStream(TcpStream);

// SAFETY: We ensure that:
// 1. The network runner is continuously polled on thread 0
// 2. Socket operations use internal synchronization
// 3. Each socket is only accessed from one thread at a time
unsafe impl Send for SendableTcpStream {}

// ============================================================================
// Blocking Executor for Session Threads
// ============================================================================

/// Run an async future to completion using a blocking executor.
///
/// This is used by session threads to run async operations without
/// needing embassy's executor. It polls the future in a loop, yielding
/// to the kernel scheduler between polls.
fn block_on<F: Future>(mut future: F) -> F::Output {
    // Pin the future on the stack
    let mut future = unsafe { Pin::new_unchecked(&mut future) };

    // Create a no-op waker
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

        // CRITICAL: Disable preemption during poll to prevent RefCell conflicts
        // Embassy-net uses RefCell internally which is not thread-safe.
        // If we get preempted while borrowing the RefCell, another thread
        // might try to borrow it too → panic.
        crate::threading::disable_preemption();
        let poll_result = future.as_mut().poll(&mut cx);
        crate::threading::enable_preemption();

        match poll_result {
            Poll::Ready(output) => return output,
            Poll::Pending => {
                // Yield to the scheduler to allow other threads to run
                // and to let thread 0 poll the network runner
                crate::threading::yield_now();

                // Small spin delay to avoid hammering the scheduler
                for _ in 0..100 {
                    core::hint::spin_loop();
                }
            }
        }
    }
}

/// Run an SSH session on a dedicated thread.
///
/// This function is called by session threads to handle a single SSH connection.
/// It runs a blocking executor that polls the async connection handler.
fn run_session_on_thread(stream: SendableTcpStream, session_id: usize, buffer_slot: usize) -> ! {
    log(&alloc::format!(
        "[SSH Session {}] Starting on thread {}\n",
        session_id,
        crate::threading::current_thread_id()
    ));

    ACTIVE_SESSIONS.fetch_add(1, Ordering::Relaxed);

    // Run the async connection handler using our blocking executor
    let stream = stream.0;
    block_on(async {
        protocol::handle_connection(stream).await;
    });

    ACTIVE_SESSIONS.fetch_sub(1, Ordering::Relaxed);

    log(&alloc::format!(
        "[SSH Session {}] Ended (active: {})\n",
        session_id,
        ACTIVE_SESSIONS.load(Ordering::Relaxed)
    ));

    // Free the buffer slot
    free_buffer_slot(buffer_slot);

    // Mark this thread as terminated and yield
    crate::threading::mark_current_terminated();
    loop {
        crate::threading::yield_now();
    }
}

// ============================================================================
// Connection State (for fallback async handling)
// ============================================================================

/// Active SSH connection being handled (used when thread spawning fails)
struct ActiveConnection {
    future: Pin<Box<dyn Future<Output = ()>>>,
    id: usize,
    buffer_slot: usize,
}

// ============================================================================
// SSH Server with Concurrent Connections
// ============================================================================

/// Run the SSH server with thread-per-session architecture
///
/// This is the main accept loop that runs on thread 0. When a connection
/// is accepted, it spawns a system thread (1-7) to handle the session.
/// This allows true preemptive multitasking between SSH sessions.
pub async fn run(stack: Stack<'static>) {
    log("[SSH Server] Starting SSH server on port 22 (thread-per-session mode)...\n");
    log(&alloc::format!(
        "[SSH Server] Max concurrent sessions: {} (system threads 1-7)\n",
        MAX_CONNECTIONS
    ));
    log("[SSH Server] Connect with: ssh -o StrictHostKeyChecking=no user@localhost -p 2222\n");

    // Enable async process execution now that SSH server is running
    crate::shell::enable_async_exec();

    // Initialize shared host key from filesystem
    protocol::init_host_key_async().await;

    // Ensure default config exists, then load and cache it
    super::config::ensure_default_config().await;
    super::config::load_config().await;

    // Fallback connections (used when thread spawning fails)
    let mut fallback_connections: Vec<ActiveConnection> = Vec::new();
    let mut next_id: usize = 0;

    // Listening socket state
    let mut listen_socket: Option<TcpSocket<'static>> = None;
    let mut listen_buffer_slot: Option<usize> = None;

    // Create waker for manual polling
    static VTABLE_POLL: RawWakerVTable = RawWakerVTable::new(
        |_| RawWaker::new(core::ptr::null(), &VTABLE_POLL),
        |_| {},
        |_| {},
        |_| {},
    );

    loop {
        let raw_waker = RawWaker::new(core::ptr::null(), &VTABLE_POLL);
        let waker = unsafe { Waker::from_raw(raw_waker) };
        let mut cx = Context::from_waker(&waker);

        // =====================================================================
        // Poll fallback connections (sessions that couldn't get a thread)
        // =====================================================================
        let mut i = 0;
        while i < fallback_connections.len() {
            match fallback_connections[i].future.as_mut().poll(&mut cx) {
                Poll::Ready(()) => {
                    let conn = fallback_connections.swap_remove(i);
                    log(&alloc::format!(
                        "[SSH Server] Fallback connection {} ended\n",
                        conn.id
                    ));
                    free_buffer_slot(conn.buffer_slot);
                }
                Poll::Pending => {
                    i += 1;
                }
            }
        }

        // =====================================================================
        // Accept new connections
        // =====================================================================
        let active_sessions = ACTIVE_SESSIONS.load(Ordering::Relaxed);
        let total_active = active_sessions + fallback_connections.len();

        if total_active < MAX_CONNECTIONS {
            // Ensure we have a listening socket
            if listen_socket.is_none() {
                let slot = listen_buffer_slot
                    .unwrap_or_else(|| alloc_buffer_slot().expect("No buffer slots available"));
                listen_buffer_slot = Some(slot);

                let (rx, tx) = unsafe { get_buffer_refs(slot) };
                let mut socket = TcpSocket::new(stack, rx, tx);
                socket.set_timeout(Some(Duration::from_secs(60)));
                listen_socket = Some(socket);
            }

            if let Some(ref mut socket) = listen_socket {
                // Use short timeout to keep the loop responsive
                let accept_timeout = if total_active == 0 {
                    Duration::from_millis(100)
                } else {
                    Duration::from_millis(10)
                };

                match embassy_time::with_timeout(accept_timeout, socket.accept(SSH_PORT)).await {
                    Ok(Ok(())) => {
                        let id = next_id;
                        next_id = next_id.wrapping_add(1);
                        let buffer_slot = listen_buffer_slot.take().unwrap();

                        crate::network::increment_connections();

                        let connected_socket = listen_socket.take().unwrap();
                        let stream = TcpStream::from_socket(connected_socket);

                        // Try to spawn a system thread for this session
                        let available = crate::threading::system_threads_available();
                        if available > 0 {
                            let sendable = SendableTcpStream(stream);
                            match crate::threading::spawn_system_thread_fn(move || {
                                run_session_on_thread(sendable, id, buffer_slot)
                            }) {
                                Ok(thread_id) => {
                                    log(&alloc::format!(
                                        "[SSH Server] Connection {} spawned on thread {} (active: {})\n",
                                        id,
                                        thread_id,
                                        ACTIVE_SESSIONS.load(Ordering::Relaxed) + 1
                                    ));
                                }
                                Err(e) => {
                                    log(&alloc::format!(
                                        "[SSH Server] Thread spawn failed: {}, using fallback\n",
                                        e
                                    ));
                                    // Reconstruct stream (can't unwrap sendable) - use fallback
                                    // Note: The sendable was moved into the closure, so we need
                                    // to handle this case differently. For now, just allocate new buffers.
                                    let slot2 = alloc_buffer_slot();
                                    if let Some(new_slot) = slot2 {
                                        let (rx2, tx2) = unsafe { get_buffer_refs(new_slot) };
                                        let mut sock2 = TcpSocket::new(stack, rx2, tx2);
                                        sock2.set_timeout(Some(Duration::from_secs(60)));
                                        // Can't reuse the stream, it was consumed
                                        free_buffer_slot(new_slot);
                                    }
                                    free_buffer_slot(buffer_slot);
                                }
                            }
                        } else {
                            // No system threads available - use fallback async mode
                            log(&alloc::format!(
                                "[SSH Server] No system threads, connection {} using fallback async\n",
                                id
                            ));
                            let future = Box::pin(handle_connection_wrapper(stream, id));
                            fallback_connections.push(ActiveConnection {
                                future,
                                id,
                                buffer_slot,
                            });
                        }
                    }
                    Ok(Err(e)) => {
                        log(&alloc::format!("[SSH Server] Accept error: {:?}\n", e));
                        listen_socket = None;
                    }
                    Err(_) => {
                        // Timeout - abort and recreate socket
                        socket.abort();
                        listen_socket = None;
                    }
                }
            }
        } else {
            // At capacity - wait briefly
            embassy_time::Timer::after(Duration::from_millis(10)).await;
        }

        // Clean up terminated threads periodically (not every loop iteration)
        // This reduces POOL lock contention
        static mut CLEANUP_COUNTER: u32 = 0;
        unsafe {
            CLEANUP_COUNTER = CLEANUP_COUNTER.wrapping_add(1);
            if CLEANUP_COUNTER % 100 == 0 {
                crate::threading::cleanup_terminated();
            }
        }
        
        // Note: on_timer_interrupt() is already called by the timer interrupt handler
        // so we don't need to call it here. Calling it redundantly could cause races
        // with the critical section in the time driver.
    }
}

/// Wrapper for handle_connection that logs start/end (used for fallback async mode)
async fn handle_connection_wrapper(stream: TcpStream, id: usize) {
    log(&alloc::format!(
        "[SSH {}] Starting fallback async session on thread {}\n",
        id,
        crate::threading::current_thread_id()
    ));
    protocol::handle_connection(stream).await;
    log(&alloc::format!("[SSH {}] Fallback session ended\n", id));
}

fn log(msg: &str) {
    console::print(msg);
}
