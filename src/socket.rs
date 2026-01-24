//! Kernel Socket Management
//!
//! Provides socket abstractions for userspace programs via syscalls.
//! Wraps embassy-net TcpSocket with state management and buffer pooling.
//!
//! ## Concurrency Safety
//!
//! - SOCKET_TABLE lock ordering: acquire after FD_TABLE, before BLOCK_DEVICE
//! - All embassy-net operations must be wrapped in preemption-disabled sections
//! - Buffer cleanup is deferred to network runner thread (thread 0)
//! - Reference counting prevents close-during-use races

use alloc::collections::BTreeMap;
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

use embassy_net::tcp::TcpSocket;
use spinning_top::Spinlock;

// ============================================================================
// Constants
// ============================================================================

/// Maximum number of concurrent sockets
pub const MAX_SOCKETS: usize = 32;

/// Size of TCP receive buffer per socket
pub const TCP_RX_BUFFER_SIZE: usize = 4096;

/// Size of TCP transmit buffer per socket
pub const TCP_TX_BUFFER_SIZE: usize = 4096;

// ============================================================================
// Socket Address Types
// ============================================================================

/// IPv4 socket address
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SocketAddrV4 {
    pub ip: [u8; 4],
    pub port: u16,
}

impl SocketAddrV4 {
    pub const fn new(ip: [u8; 4], port: u16) -> Self {
        Self { ip, port }
    }

    /// Convert to embassy-net IpEndpoint
    pub fn to_endpoint(&self) -> embassy_net::IpEndpoint {
        embassy_net::IpEndpoint::new(
            embassy_net::IpAddress::Ipv4(embassy_net::Ipv4Address::new(
                self.ip[0], self.ip[1], self.ip[2], self.ip[3],
            )),
            self.port,
        )
    }

    /// Create from embassy-net IpEndpoint
    pub fn from_endpoint(ep: embassy_net::IpEndpoint) -> Option<Self> {
        match ep.addr {
            embassy_net::IpAddress::Ipv4(v4) => Some(Self {
                ip: v4.octets(),
                port: ep.port,
            }),
            _ => None,
        }
    }
}

/// Linux sockaddr_in structure (for syscall interface)
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct SockAddrIn {
    pub sin_family: u16,    // AF_INET = 2
    pub sin_port: u16,      // Network byte order (big-endian)
    pub sin_addr: u32,      // Network byte order (big-endian)
    pub sin_zero: [u8; 8],  // Padding
}

impl SockAddrIn {
    /// Size of the structure
    pub const SIZE: usize = 16;

    /// Convert to SocketAddrV4 (handles byte order conversion)
    pub fn to_addr(&self) -> SocketAddrV4 {
        let ip_bytes = self.sin_addr.to_be_bytes();
        let port = u16::from_be(self.sin_port);
        SocketAddrV4::new(ip_bytes, port)
    }

    /// Create from SocketAddrV4 (handles byte order conversion)
    pub fn from_addr(addr: &SocketAddrV4) -> Self {
        Self {
            sin_family: 2, // AF_INET
            sin_port: addr.port.to_be(),
            sin_addr: u32::from_be_bytes(addr.ip),
            sin_zero: [0u8; 8],
        }
    }
}

// ============================================================================
// Socket Constants (Linux compatible)
// ============================================================================

pub mod socket_const {
    // Address families
    pub const AF_INET: i32 = 2;

    // Socket types
    pub const SOCK_STREAM: i32 = 1;
    pub const SOCK_DGRAM: i32 = 2;

    // Protocols
    pub const IPPROTO_TCP: i32 = 6;
    pub const IPPROTO_UDP: i32 = 17;

    // Shutdown modes
    pub const SHUT_RD: i32 = 0;
    pub const SHUT_WR: i32 = 1;
    pub const SHUT_RDWR: i32 = 2;
}

// ============================================================================
// Socket State Machine
// ============================================================================

/// Socket state
#[derive(Debug, Clone)]
pub enum SocketState {
    /// Socket created but not bound
    Unbound,
    /// Socket bound to local address
    Bound { local_addr: SocketAddrV4 },
    /// Socket listening for connections
    Listening { local_addr: SocketAddrV4, backlog: usize },
    /// Socket connected to remote address
    Connected {
        local_addr: SocketAddrV4,
        remote_addr: SocketAddrV4,
    },
    /// Socket is being closed
    Closing,
    /// Socket is closed
    Closed,
}

// ============================================================================
// Buffer Pool
// ============================================================================

/// Static buffer pool for socket I/O
/// Uses atomics for allocation tracking (thread-safe, lock-free)
struct SocketBufferPool {
    rx_buffers: [UnsafeCell<[u8; TCP_RX_BUFFER_SIZE]>; MAX_SOCKETS],
    tx_buffers: [UnsafeCell<[u8; TCP_TX_BUFFER_SIZE]>; MAX_SOCKETS],
    in_use: [AtomicBool; MAX_SOCKETS],
}

// SAFETY: Access to individual buffer slots is serialized via AtomicBool flags.
// Each slot can only be used by one caller at a time.
unsafe impl Sync for SocketBufferPool {}

impl SocketBufferPool {
    const fn new() -> Self {
        const RX_INIT: UnsafeCell<[u8; TCP_RX_BUFFER_SIZE]> =
            UnsafeCell::new([0u8; TCP_RX_BUFFER_SIZE]);
        const TX_INIT: UnsafeCell<[u8; TCP_TX_BUFFER_SIZE]> =
            UnsafeCell::new([0u8; TCP_TX_BUFFER_SIZE]);
        const IN_USE_INIT: AtomicBool = AtomicBool::new(false);

        Self {
            rx_buffers: [RX_INIT; MAX_SOCKETS],
            tx_buffers: [TX_INIT; MAX_SOCKETS],
            in_use: [IN_USE_INIT; MAX_SOCKETS],
        }
    }

    /// Try to allocate a buffer slot
    fn alloc(&self) -> Option<usize> {
        for i in 0..MAX_SOCKETS {
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
        if slot < MAX_SOCKETS {
            self.in_use[slot].store(false, Ordering::Release);
        }
    }

    /// Get buffer references for an allocated slot
    /// SAFETY: Caller must have successfully allocated this slot
    unsafe fn get_buffers(&self, slot: usize) -> (&'static mut [u8], &'static mut [u8]) {
        debug_assert!(slot < MAX_SOCKETS);
        debug_assert!(self.in_use[slot].load(Ordering::Acquire));

        unsafe {
            let rx = &mut *self.rx_buffers[slot].get();
            let tx = &mut *self.tx_buffers[slot].get();
            (rx, tx)
        }
    }
}

static BUFFER_POOL: SocketBufferPool = SocketBufferPool::new();

// ============================================================================
// Deferred Cleanup Queue
// ============================================================================

/// Queue for deferred buffer cleanup
/// Buffers are queued here and freed by network runner after polling embassy-net
const CLEANUP_QUEUE_SIZE: usize = 64;
static CLEANUP_QUEUE: [AtomicUsize; CLEANUP_QUEUE_SIZE] = {
    const INIT: AtomicUsize = AtomicUsize::new(usize::MAX);
    [INIT; CLEANUP_QUEUE_SIZE]
};
static CLEANUP_HEAD: AtomicUsize = AtomicUsize::new(0);
static CLEANUP_TAIL: AtomicUsize = AtomicUsize::new(0);

/// Queue a buffer slot for deferred cleanup
pub fn queue_buffer_cleanup(slot: usize) {
    let tail = CLEANUP_TAIL.load(Ordering::Relaxed);
    let next_tail = (tail + 1) % CLEANUP_QUEUE_SIZE;
    let head = CLEANUP_HEAD.load(Ordering::Relaxed);

    if next_tail == head {
        // Queue full - free immediately (less safe but avoids leak)
        BUFFER_POOL.free(slot);
        return;
    }

    CLEANUP_QUEUE[tail].store(slot, Ordering::Release);
    CLEANUP_TAIL.store(next_tail, Ordering::Release);
}

/// Process deferred buffer cleanup (call from network runner after polling)
pub fn process_buffer_cleanup() -> usize {
    let mut count = 0;
    loop {
        let head = CLEANUP_HEAD.load(Ordering::Acquire);
        let tail = CLEANUP_TAIL.load(Ordering::Acquire);

        if head == tail {
            break; // Queue empty
        }

        let slot = CLEANUP_QUEUE[head].load(Ordering::Acquire);
        if slot != usize::MAX {
            BUFFER_POOL.free(slot);
            CLEANUP_QUEUE[head].store(usize::MAX, Ordering::Release);
            count += 1;
        }

        CLEANUP_HEAD.store((head + 1) % CLEANUP_QUEUE_SIZE, Ordering::Release);
    }
    count
}

// ============================================================================
// Socket Handle (embassy-net TcpSocket wrapper)
// ============================================================================

/// Wrapper for TcpSocket that allows cross-thread access
///
/// SAFETY: Access is serialized via:
/// 1. Preemption control (disable_preemption/enable_preemption)
/// 2. Socket table lock during state transitions
///
/// Embassy-net's TcpSocket is not Send, but we can safely share it because:
/// - All access happens with preemption disabled (no concurrent access)
/// - Thread 0 polls the network runner, other threads poll individual sockets
pub struct SocketHandle {
    socket: UnsafeCell<Option<TcpSocket<'static>>>,
}

// SAFETY: Access is serialized via preemption control and socket table lock
unsafe impl Send for SocketHandle {}
unsafe impl Sync for SocketHandle {}

impl SocketHandle {
    /// Create a new empty socket handle
    pub fn new() -> Self {
        Self {
            socket: UnsafeCell::new(None),
        }
    }

    /// Create a socket handle with an existing TcpSocket
    pub fn with_socket(socket: TcpSocket<'static>) -> Self {
        Self {
            socket: UnsafeCell::new(Some(socket)),
        }
    }

    /// Get mutable socket reference
    ///
    /// SAFETY: Must be called with preemption disabled
    pub unsafe fn get(&self) -> &mut Option<TcpSocket<'static>> {
        &mut *self.socket.get()
    }

    /// Take the socket out of the handle
    ///
    /// SAFETY: Must be called with preemption disabled
    pub unsafe fn take(&self) -> Option<TcpSocket<'static>> {
        (*self.socket.get()).take()
    }

    /// Store a socket in the handle
    ///
    /// SAFETY: Must be called with preemption disabled
    pub unsafe fn store(&self, socket: TcpSocket<'static>) {
        *self.socket.get() = Some(socket);
    }

    /// Check if handle contains a socket
    ///
    /// SAFETY: Must be called with preemption disabled
    pub unsafe fn is_some(&self) -> bool {
        (*self.socket.get()).is_some()
    }
}

impl Default for SocketHandle {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for SocketHandle {
    fn drop(&mut self) {
        // Abort the socket if it exists to release embassy-net resources
        // SAFETY: Called during socket cleanup, preemption should be disabled
        // or we're the only reference
        if let Some(mut socket) = unsafe { self.take() } {
            socket.abort();
        }
    }
}

// ============================================================================
// Kernel Socket
// ============================================================================

/// A kernel socket that wraps embassy-net TcpSocket
pub struct KernelSocket {
    /// Current socket state
    pub state: SocketState,
    /// Buffer pool slot index
    pub buffer_slot: usize,
    /// Reference count for close-during-use protection
    pub ref_count: AtomicU32,
    /// Socket type (SOCK_STREAM or SOCK_DGRAM)
    pub socket_type: i32,
    /// Whether this socket is a listener (accept returns new sockets)
    pub is_listener: bool,
    /// Embassy-net TcpSocket handle (for connected sockets)
    pub handle: SocketHandle,
}

impl KernelSocket {
    /// Create a new unbound socket
    pub fn new(socket_type: i32) -> Option<Self> {
        let buffer_slot = BUFFER_POOL.alloc()?;
        Some(Self {
            state: SocketState::Unbound,
            buffer_slot,
            ref_count: AtomicU32::new(1),
            socket_type,
            is_listener: false,
            handle: SocketHandle::new(),
        })
    }

    /// Create a connected socket (for accept) with a TcpSocket handle
    pub fn new_connected(
        buffer_slot: usize,
        local_addr: SocketAddrV4,
        remote_addr: SocketAddrV4,
    ) -> Self {
        Self {
            state: SocketState::Connected { local_addr, remote_addr },
            buffer_slot,
            ref_count: AtomicU32::new(1),
            socket_type: socket_const::SOCK_STREAM,
            is_listener: false,
            handle: SocketHandle::new(),
        }
    }

    /// Create a connected socket with an existing TcpSocket handle
    pub fn new_connected_with_handle(
        buffer_slot: usize,
        local_addr: SocketAddrV4,
        remote_addr: SocketAddrV4,
        socket: TcpSocket<'static>,
    ) -> Self {
        Self {
            state: SocketState::Connected { local_addr, remote_addr },
            buffer_slot,
            ref_count: AtomicU32::new(1),
            socket_type: socket_const::SOCK_STREAM,
            is_listener: false,
            handle: SocketHandle::with_socket(socket),
        }
    }

    /// Increment reference count
    pub fn inc_ref(&self) -> u32 {
        self.ref_count.fetch_add(1, Ordering::SeqCst)
    }

    /// Decrement reference count, returns true if this was the last reference
    pub fn dec_ref(&self) -> bool {
        self.ref_count.fetch_sub(1, Ordering::SeqCst) == 1
    }

    /// Get current reference count
    pub fn ref_count(&self) -> u32 {
        self.ref_count.load(Ordering::SeqCst)
    }

    /// Get buffer references
    /// SAFETY: Must have exclusive access to the socket
    pub unsafe fn get_buffers(&self) -> (&'static mut [u8], &'static mut [u8]) {
        // SAFETY: Caller ensures exclusive access
        unsafe { BUFFER_POOL.get_buffers(self.buffer_slot) }
    }
}

impl Drop for KernelSocket {
    fn drop(&mut self) {
        // Queue buffer for deferred cleanup
        queue_buffer_cleanup(self.buffer_slot);
    }
}

// ============================================================================
// Global Socket Table
// ============================================================================

/// Next socket index
static NEXT_SOCKET_IDX: AtomicUsize = AtomicUsize::new(0);

/// Global socket table
static SOCKET_TABLE: Spinlock<BTreeMap<usize, KernelSocket>> = Spinlock::new(BTreeMap::new());

/// Allocate a new socket and add it to the table
pub fn alloc_socket(socket_type: i32) -> Option<usize> {
    let socket = KernelSocket::new(socket_type)?;
    let idx = NEXT_SOCKET_IDX.fetch_add(1, Ordering::SeqCst);

    crate::irq::with_irqs_disabled(|| {
        SOCKET_TABLE.lock().insert(idx, socket);
    });

    Some(idx)
}

/// Get socket state (cloned) - does not hold lock
pub fn get_socket_state(idx: usize) -> Option<SocketState> {
    crate::irq::with_irqs_disabled(|| {
        SOCKET_TABLE.lock().get(&idx).map(|s| s.state.clone())
    })
}

/// Increment socket reference count
pub fn socket_inc_ref(idx: usize) -> Option<u32> {
    crate::irq::with_irqs_disabled(|| {
        SOCKET_TABLE.lock().get(&idx).map(|s| s.inc_ref())
    })
}

/// Decrement socket reference count
pub fn socket_dec_ref(idx: usize) -> Option<bool> {
    crate::irq::with_irqs_disabled(|| {
        SOCKET_TABLE.lock().get(&idx).map(|s| s.dec_ref())
    })
}

/// Update socket with a function
pub fn with_socket<F, R>(idx: usize, f: F) -> Option<R>
where
    F: FnOnce(&mut KernelSocket) -> R,
{
    crate::irq::with_irqs_disabled(|| {
        let mut table = SOCKET_TABLE.lock();
        table.get_mut(&idx).map(f)
    })
}

/// Remove a socket from the table
pub fn remove_socket(idx: usize) -> Option<KernelSocket> {
    crate::irq::with_irqs_disabled(|| {
        SOCKET_TABLE.lock().remove(&idx)
    })
}

/// Get socket buffer slot
pub fn get_socket_buffer_slot(idx: usize) -> Option<usize> {
    crate::irq::with_irqs_disabled(|| {
        SOCKET_TABLE.lock().get(&idx).map(|s| s.buffer_slot)
    })
}

// ============================================================================
// Socket Operations (blocking syscall implementations)
// ============================================================================

/// Bind a socket to a local address
pub fn socket_bind(idx: usize, addr: SocketAddrV4) -> Result<(), i32> {
    with_socket(idx, |socket| {
        match &socket.state {
            SocketState::Unbound => {
                socket.state = SocketState::Bound { local_addr: addr };
                Ok(())
            }
            _ => Err(-libc_errno::EINVAL),
        }
    }).unwrap_or(Err(-libc_errno::EBADF))
}

/// Set socket to listening state
pub fn socket_listen(idx: usize, backlog: usize) -> Result<(), i32> {
    with_socket(idx, |socket| {
        match &socket.state {
            SocketState::Bound { local_addr } => {
                let addr = *local_addr;
                socket.state = SocketState::Listening { local_addr: addr, backlog };
                socket.is_listener = true;
                Ok(())
            }
            _ => Err(-libc_errno::EINVAL),
        }
    }).unwrap_or(Err(-libc_errno::EBADF))
}

/// Mark socket as connected
pub fn socket_set_connected(idx: usize, local_addr: SocketAddrV4, remote_addr: SocketAddrV4) -> Result<(), i32> {
    with_socket(idx, |socket| {
        socket.state = SocketState::Connected { local_addr, remote_addr };
        Ok(())
    }).unwrap_or(Err(-libc_errno::EBADF))
}

/// Close a socket
pub fn socket_close(idx: usize) -> Result<(), i32> {
    // First mark as closing
    with_socket(idx, |socket| {
        socket.state = SocketState::Closing;
    });

    // Then remove from table (this will trigger Drop which queues buffer cleanup)
    // The SocketHandle's Drop impl will call abort() on the TcpSocket
    remove_socket(idx);
    Ok(())
}

// ============================================================================
// Buffer Pool Public Interface
// ============================================================================

/// Allocate a buffer slot from the pool
pub fn alloc_buffer_slot() -> Option<usize> {
    BUFFER_POOL.alloc()
}

/// Free a buffer slot back to the pool
pub fn free_buffer_slot(slot: usize) {
    BUFFER_POOL.free(slot);
}

/// Get buffer references for an allocated slot
///
/// SAFETY: Caller must have successfully allocated this slot and not freed it.
/// The slot must not be accessed concurrently.
pub unsafe fn get_buffers(slot: usize) -> (&'static mut [u8], &'static mut [u8]) {
    BUFFER_POOL.get_buffers(slot)
}

// ============================================================================
// Socket Handle Operations
// ============================================================================

/// Store a TcpSocket handle in an existing socket entry
///
/// SAFETY: Must be called with preemption disabled
pub fn store_socket_handle(idx: usize, socket: TcpSocket<'static>) {
    crate::irq::with_irqs_disabled(|| {
        let mut table = SOCKET_TABLE.lock();
        if let Some(ks) = table.get_mut(&idx) {
            // SAFETY: We have exclusive access via table lock and IRQs disabled
            unsafe { ks.handle.store(socket); }
        }
    });
}

/// Allocate a new socket with an existing TcpSocket handle
pub fn alloc_socket_with_handle(
    socket_type: i32,
    buffer_slot: usize,
    tcp_socket: TcpSocket<'static>,
    state: SocketState,
) -> usize {
    let socket = KernelSocket {
        state,
        buffer_slot,
        ref_count: AtomicU32::new(1),
        socket_type,
        is_listener: false,
        handle: SocketHandle::with_socket(tcp_socket),
    };
    
    let idx = NEXT_SOCKET_IDX.fetch_add(1, Ordering::SeqCst);
    
    crate::irq::with_irqs_disabled(|| {
        SOCKET_TABLE.lock().insert(idx, socket);
    });
    
    idx
}

/// Check if a socket has a TcpSocket handle
pub fn socket_has_handle(idx: usize) -> bool {
    crate::irq::with_irqs_disabled(|| {
        crate::threading::disable_preemption();
        let result = SOCKET_TABLE.lock()
            .get(&idx)
            .map(|s| unsafe { s.handle.is_some() })
            .unwrap_or(false);
        crate::threading::enable_preemption();
        result
    })
}

/// Execute a function with the socket's TcpSocket handle
///
/// This is the safe way to access a socket's TcpSocket for I/O operations.
/// The closure is called with preemption disabled to protect embassy-net's RefCell.
///
/// Returns Err(EBADF) if socket doesn't exist or has no handle.
/// Returns Err(ENOTCONN) if socket is not connected.
pub fn with_socket_handle<F, R>(idx: usize, f: F) -> Result<R, i32>
where
    F: FnOnce(&mut TcpSocket<'static>) -> R,
{
    // First verify socket exists and is connected
    let state = get_socket_state(idx).ok_or(libc_errno::EBADF)?;
    match state {
        SocketState::Connected { .. } => {}
        SocketState::Closing | SocketState::Closed => return Err(libc_errno::EBADF),
        _ => return Err(libc_errno::ENOTCONN),
    }

    // Access the socket handle with preemption disabled
    crate::threading::disable_preemption();
    let result = crate::irq::with_irqs_disabled(|| {
        let mut table = SOCKET_TABLE.lock();
        if let Some(ks) = table.get_mut(&idx) {
            // SAFETY: Preemption is disabled, we have exclusive access
            let socket_opt = unsafe { ks.handle.get() };
            if let Some(socket) = socket_opt.as_mut() {
                Some(f(socket))
            } else {
                None
            }
        } else {
            None
        }
    });
    crate::threading::enable_preemption();

    result.ok_or(libc_errno::EBADF)
}

// ============================================================================
// Error Numbers (Linux compatible)
// ============================================================================

pub mod libc_errno {
    pub const ENOENT: i32 = 2;       // No such file or directory
    pub const EINTR: i32 = 4;        // Interrupted system call
    pub const EIO: i32 = 5;          // I/O error
    pub const EBADF: i32 = 9;        // Bad file descriptor
    pub const EAGAIN: i32 = 11;      // Try again
    pub const EWOULDBLOCK: i32 = 11; // Same as EAGAIN
    pub const ENOMEM: i32 = 12;      // Out of memory
    pub const EFAULT: i32 = 14;      // Bad address
    pub const EINVAL: i32 = 22;      // Invalid argument
    pub const ENOTSOCK: i32 = 88;    // Socket operation on non-socket
    pub const EADDRINUSE: i32 = 98;  // Address already in use
    pub const ENETDOWN: i32 = 100;   // Network is down
    pub const EISCONN: i32 = 106;    // Transport endpoint is already connected
    pub const ENOTCONN: i32 = 107;   // Transport endpoint not connected
    pub const ETIMEDOUT: i32 = 110;  // Connection timed out
    pub const ECONNREFUSED: i32 = 111; // Connection refused
    pub const EHOSTUNREACH: i32 = 113; // No route to host
    pub const EINPROGRESS: i32 = 115;  // Operation now in progress
}

// ============================================================================
// Debug/Statistics
// ============================================================================

/// Get number of active sockets
pub fn active_socket_count() -> usize {
    crate::irq::with_irqs_disabled(|| {
        SOCKET_TABLE.lock().len()
    })
}

/// Get number of allocated buffer slots
pub fn allocated_buffer_count() -> usize {
    let mut count = 0;
    for i in 0..MAX_SOCKETS {
        if BUFFER_POOL.in_use[i].load(Ordering::Relaxed) {
            count += 1;
        }
    }
    count
}
