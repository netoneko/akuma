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

// Fixed-size socket table - no BTreeMap needed
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

use embassy_net::tcp::TcpSocket;
// Spinlock no longer needed - using fixed array with atomic flags

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
///
/// NOTE: We Box the TcpSocket to keep it at a fixed heap location.
/// Moving TcpSocket after accept() causes corruption (likely due to internal state).
pub struct SocketHandle {
    socket: UnsafeCell<Option<alloc::boxed::Box<TcpSocket<'static>>>>,
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

    /// Create a socket handle with a pre-boxed TcpSocket
    /// IMPORTANT: Caller must Box the socket BEFORE calling this to avoid moves
    pub fn with_socket_boxed(socket: alloc::boxed::Box<TcpSocket<'static>>) -> Self {
        Self {
            socket: UnsafeCell::new(Some(socket)),
        }
    }

    /// Create a socket handle with an existing TcpSocket
    /// The socket is boxed to keep it at a fixed heap location
    /// WARNING: This moves the socket, which may corrupt it after accept()
    #[allow(dead_code)]
    pub fn with_socket(socket: TcpSocket<'static>) -> Self {
        Self {
            socket: UnsafeCell::new(Some(alloc::boxed::Box::new(socket))),
        }
    }

    /// Get mutable socket reference
    ///
    /// SAFETY: Must be called with preemption disabled
    pub unsafe fn get(&self) -> Option<&mut TcpSocket<'static>> {
        (*self.socket.get()).as_mut().map(|b| b.as_mut())
    }

    /// Take the socket out of the handle (unboxes it)
    ///
    /// SAFETY: Must be called with preemption disabled
    pub unsafe fn take(&self) -> Option<TcpSocket<'static>> {
        (*self.socket.get()).take().map(|b| *b)
    }

    /// Store a socket in the handle (boxes it)
    ///
    /// SAFETY: Must be called with preemption disabled
    pub unsafe fn store(&self, socket: TcpSocket<'static>) {
        *self.socket.get() = Some(alloc::boxed::Box::new(socket));
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
        // Release embassy-net resources
        // SAFETY: We're in drop, have exclusive access
        // Note: socket.close() should have been called already for graceful shutdown
        // If not, we abort as a fallback to release resources
        if let Some(mut boxed) = unsafe { (*self.socket.get()).take() } {
            // Check if socket is still open - if so, abort it
            // (This is the fallback case - normally socket_close() already called close())
            if boxed.may_send() || boxed.may_recv() {
                boxed.abort();
            }
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
// Global Socket Table (Fixed Array - avoids heap allocation and moving sockets)
// ============================================================================

/// Next socket index (wraps around, we check slot availability)
static NEXT_SOCKET_IDX: AtomicUsize = AtomicUsize::new(0);

/// Socket slot - stores KernelSocket in a fixed location
struct SocketSlot {
    socket: UnsafeCell<Option<KernelSocket>>,
    in_use: AtomicBool,
}

// SAFETY: Access is serialized via in_use flag and IRQ disable
unsafe impl Sync for SocketSlot {}

impl SocketSlot {
    const fn new() -> Self {
        Self {
            socket: UnsafeCell::new(None),
            in_use: AtomicBool::new(false),
        }
    }
}

/// Global socket table - fixed array to avoid BTreeMap heap allocations
/// and to keep sockets at fixed memory locations (important for TcpSocket)
static SOCKET_TABLE: [SocketSlot; MAX_SOCKETS] = {
    const SLOT_INIT: SocketSlot = SocketSlot::new();
    [SLOT_INIT; MAX_SOCKETS]
};

/// Find a free socket slot
fn find_free_slot() -> Option<usize> {
    for i in 0..MAX_SOCKETS {
        if SOCKET_TABLE[i].in_use
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
        {
            return Some(i);
        }
    }
    None
}

/// Allocate a new socket and add it to the table
pub fn alloc_socket(socket_type: i32) -> Option<usize> {
    let socket = KernelSocket::new(socket_type)?;
    let idx = find_free_slot()?;

    crate::irq::with_irqs_disabled(|| {
        // SAFETY: We have exclusive access via in_use flag
        unsafe {
            *SOCKET_TABLE[idx].socket.get() = Some(socket);
        }
    });

    Some(idx)
}

/// Get socket state (cloned) - does not hold lock
pub fn get_socket_state(idx: usize) -> Option<SocketState> {
    if idx >= MAX_SOCKETS || !SOCKET_TABLE[idx].in_use.load(Ordering::Acquire) {
        return None;
    }
    crate::irq::with_irqs_disabled(|| {
        // SAFETY: Slot is in use, we have IRQs disabled
        unsafe {
            (*SOCKET_TABLE[idx].socket.get()).as_ref().map(|s| s.state.clone())
        }
    })
}

/// Increment socket reference count
pub fn socket_inc_ref(idx: usize) -> Option<u32> {
    if idx >= MAX_SOCKETS || !SOCKET_TABLE[idx].in_use.load(Ordering::Acquire) {
        return None;
    }
    crate::irq::with_irqs_disabled(|| {
        unsafe {
            (*SOCKET_TABLE[idx].socket.get()).as_ref().map(|s| s.inc_ref())
        }
    })
}

/// Decrement socket reference count
pub fn socket_dec_ref(idx: usize) -> Option<bool> {
    if idx >= MAX_SOCKETS || !SOCKET_TABLE[idx].in_use.load(Ordering::Acquire) {
        return None;
    }
    crate::irq::with_irqs_disabled(|| {
        unsafe {
            (*SOCKET_TABLE[idx].socket.get()).as_ref().map(|s| s.dec_ref())
        }
    })
}

/// Update socket with a function
pub fn with_socket<F, R>(idx: usize, f: F) -> Option<R>
where
    F: FnOnce(&mut KernelSocket) -> R,
{
    if idx >= MAX_SOCKETS || !SOCKET_TABLE[idx].in_use.load(Ordering::Acquire) {
        return None;
    }
    crate::irq::with_irqs_disabled(|| {
        unsafe {
            (*SOCKET_TABLE[idx].socket.get()).as_mut().map(f)
        }
    })
}

/// Remove a socket from the table
pub fn remove_socket(idx: usize) -> Option<KernelSocket> {
    if idx >= MAX_SOCKETS || !SOCKET_TABLE[idx].in_use.load(Ordering::Acquire) {
        return None;
    }
    crate::irq::with_irqs_disabled(|| {
        let socket = unsafe { (*SOCKET_TABLE[idx].socket.get()).take() };
        SOCKET_TABLE[idx].in_use.store(false, Ordering::Release);
        socket
    })
}

/// Get socket buffer slot
pub fn get_socket_buffer_slot(idx: usize) -> Option<usize> {
    if idx >= MAX_SOCKETS || !SOCKET_TABLE[idx].in_use.load(Ordering::Acquire) {
        return None;
    }
    crate::irq::with_irqs_disabled(|| {
        unsafe {
            (*SOCKET_TABLE[idx].socket.get()).as_ref().map(|s| s.buffer_slot)
        }
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

    // Gracefully close the socket - flush and close instead of abort
    // This ensures buffered data is transmitted before the connection is terminated
    crate::threading::disable_preemption();
    let close_result = crate::irq::with_irqs_disabled(|| {
        if idx >= MAX_SOCKETS || !SOCKET_TABLE[idx].in_use.load(Ordering::Acquire) {
            return;
        }
        unsafe {
            if let Some(ks) = (*SOCKET_TABLE[idx].socket.get()).as_mut() {
                if let Some(socket) = ks.handle.get() {
                    // Graceful close - sends FIN and allows buffered data to be transmitted
                    socket.close();
                }
            }
        }
    });
    crate::threading::enable_preemption();
    
    // Yield a few times to give the network stack time to transmit data
    for _ in 0..10 {
        crate::threading::yield_now();
    }

    // Then remove from table (this will trigger Drop which queues buffer cleanup)
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
    if idx >= MAX_SOCKETS || !SOCKET_TABLE[idx].in_use.load(Ordering::Acquire) {
        return;
    }
    crate::irq::with_irqs_disabled(|| {
        unsafe {
            if let Some(ks) = (*SOCKET_TABLE[idx].socket.get()).as_mut() {
                ks.handle.store(socket);
            }
        }
    });
}

/// Allocate a new socket with an existing TcpSocket handle (pre-boxed)
///
/// IMPORTANT: The TcpSocket must be pre-boxed by the caller to avoid
/// any moves after accept() completes. Moving TcpSocket corrupts internal state.
pub fn alloc_socket_with_handle_boxed(
    socket_type: i32,
    buffer_slot: usize,
    tcp_socket_boxed: alloc::boxed::Box<TcpSocket<'static>>,
    state: SocketState,
) -> usize {
    // Find a free slot first
    let idx = match find_free_slot() {
        Some(i) => i,
        None => {
            // Return a sentinel value - caller should check
            return MAX_SOCKETS;
        }
    };
    
    // Store the socket directly in the slot
    crate::irq::with_irqs_disabled(|| {
        unsafe {
            let slot = &SOCKET_TABLE[idx];
            let socket_ptr = slot.socket.get();
            
            // Initialize KernelSocket in place with the pre-boxed socket
            (*socket_ptr) = Some(KernelSocket {
                state,
                buffer_slot,
                ref_count: AtomicU32::new(1),
                socket_type,
                is_listener: false,
                handle: SocketHandle::with_socket_boxed(tcp_socket_boxed),
            });
        }
    });
    
    idx
}

/// Check if a socket has a TcpSocket handle
pub fn socket_has_handle(idx: usize) -> bool {
    if idx >= MAX_SOCKETS || !SOCKET_TABLE[idx].in_use.load(Ordering::Acquire) {
        return false;
    }
    crate::irq::with_irqs_disabled(|| {
        crate::threading::disable_preemption();
        let result = unsafe {
            (*SOCKET_TABLE[idx].socket.get())
                .as_ref()
                .map(|s| s.handle.is_some())
                .unwrap_or(false)
        };
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
    if idx >= MAX_SOCKETS || !SOCKET_TABLE[idx].in_use.load(Ordering::Acquire) {
        return Err(libc_errno::EBADF);
    }
    
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
        unsafe {
            if let Some(ks) = (*SOCKET_TABLE[idx].socket.get()).as_mut() {
                // SAFETY: Preemption is disabled, we have exclusive access
                // get() now returns Option<&mut TcpSocket>
                if let Some(socket) = ks.handle.get() {
                    Some(f(socket))
                } else {
                    None
                }
            } else {
                None
            }
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
    pub const ESRCH: i32 = 3;        // No such process
    pub const EINTR: i32 = 4;        // Interrupted system call
    pub const EIO: i32 = 5;          // I/O error
    pub const EBADF: i32 = 9;        // Bad file descriptor
    pub const ECHILD: i32 = 10;      // No child processes
    pub const EAGAIN: i32 = 11;      // Try again
    pub const EWOULDBLOCK: i32 = 11; // Same as EAGAIN
    pub const ENOSYS: i32 = 38;      // Function not implemented (invalid syscall number)
    pub const ENOMEM: i32 = 12;      // Out of memory
    pub const EFAULT: i32 = 14;      // Bad address
    pub const ENOTDIR: i32 = 20;     // Not a directory
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
    let mut count = 0;
    for i in 0..MAX_SOCKETS {
        if SOCKET_TABLE[i].in_use.load(Ordering::Relaxed) {
            count += 1;
        }
    }
    count
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
