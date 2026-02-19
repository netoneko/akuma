//! Kernel Socket Management
//!
//! Provides socket abstractions for userspace programs via syscalls.
//! Wraps smoltcp sockets via the thread-safe smoltcp_net module.

use alloc::vec::Vec;
use alloc::collections::VecDeque;
use core::sync::atomic::{AtomicU16, Ordering};
use spinning_top::Spinlock;

use crate::smoltcp_net::{self, SocketHandle, with_network};
use smoltcp::socket::tcp;

// ============================================================================
// Constants
// ============================================================================

/// Maximum number of concurrent sockets (FDs)
pub const MAX_SOCKETS: usize = 128;

/// Maximum number of sockets to pre-allocate for a listener's backlog
const MAX_BACKLOG: usize = 8;

/// Ephemeral port range start
const EPHEMERAL_PORT_START: u16 = 49152;
/// Ephemeral port range end
const EPHEMERAL_PORT_END: u16 = 65535;

/// Global atomic for ephemeral port allocation
static NEXT_EPHEMERAL_PORT: AtomicU16 = AtomicU16::new(EPHEMERAL_PORT_START);

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
// Socket Constants
// ============================================================================

pub mod socket_const {
    pub const AF_INET: i32 = 2;
    pub const SOCK_STREAM: i32 = 1;
}

// ============================================================================
// Socket Type
// ============================================================================

pub enum SocketType {
    /// A connected or connecting socket (one smoltcp handle)
    Stream(SocketHandle),
    /// A listening socket (manages a pool of smoltcp handles)
    Listener {
        local_port: u16,
        handles: VecDeque<SocketHandle>,
    },
}

// ============================================================================
// Kernel Socket
// ============================================================================

pub struct KernelSocket {
    pub inner: SocketType,
    pub bind_port: Option<u16>,
    pub box_id: u64,
}

impl KernelSocket {
    pub fn new_stream() -> Option<Self> {
        let handle = smoltcp_net::socket_create()?;
        let box_id = crate::process::current_process().map(|p| p.box_id).unwrap_or(0);
        Some(Self {
            inner: SocketType::Stream(handle),
            bind_port: None,
            box_id,
        })
    }

    pub fn new_listener(port: u16, backlog: usize) -> Option<Self> {
        let actual_backlog = backlog.min(MAX_BACKLOG);
        let mut handles = VecDeque::new();
        
        for _ in 0..actual_backlog {
            if let Some(handle) = smoltcp_net::socket_create() {
                with_network(|net| {
                    let socket = net.sockets.get_mut::<tcp::Socket>(handle);
                    let _ = socket.listen(port);
                });
                handles.push_back(handle);
            }
        }
        
        if handles.is_empty() {
            return None;
        }
        
        let box_id = crate::process::current_process().map(|p| p.box_id).unwrap_or(0);
        
        Some(Self {
            inner: SocketType::Listener { local_port: port, handles },
            bind_port: Some(port),
            box_id,
        })
    }
}

// ============================================================================
// Helpers
// ============================================================================

/// Allocate an ephemeral port
fn alloc_ephemeral_port() -> u16 {
    let port = NEXT_EPHEMERAL_PORT.fetch_add(1, Ordering::Relaxed);
    if port >= EPHEMERAL_PORT_END {
        NEXT_EPHEMERAL_PORT.store(EPHEMERAL_PORT_START, Ordering::Relaxed);
        EPHEMERAL_PORT_START
    } else {
        port
    }
}

// ============================================================================
// Global Socket Table
// ============================================================================

/// Global table of sockets (indexed by integer "socket descriptor")
static SOCKET_TABLE: Spinlock<Option<Vec<Option<KernelSocket>>>> = Spinlock::new(None);

fn with_table<F, R>(f: F) -> R 
where F: FnOnce(&mut Vec<Option<KernelSocket>>) -> R 
{
    let mut guard = SOCKET_TABLE.lock();
    if guard.is_none() {
        *guard = Some(Vec::new());
    }
    f(guard.as_mut().unwrap())
}

/// Allocate a socket index
pub fn alloc_socket(socket_type: i32) -> Option<usize> {
    if socket_type != socket_const::SOCK_STREAM {
        return None; // Only TCP supported
    }

    let socket = KernelSocket::new_stream()?;

    with_table(|table| {
        for (i, slot) in table.iter_mut().enumerate() {
            if slot.is_none() {
                *slot = Some(socket);
                return Some(i);
            }
        }
        if table.len() < MAX_SOCKETS {
            table.push(Some(socket));
            Some(table.len() - 1)
        } else {
            None
        }
    })
}

pub fn remove_socket(idx: usize) {
    with_table(|table| {
        if idx < table.len() {
            if let Some(sock) = table[idx].take() {
                match sock.inner {
                    SocketType::Stream(h) => smoltcp_net::socket_close(h),
                    SocketType::Listener { handles, .. } => {
                        for h in handles {
                            smoltcp_net::socket_close(h);
                        }
                    }
                }
            }
        }
    });
}

// ============================================================================
// Socket Operations (Blocking with Yield)
// ============================================================================

/// Helper to poll and yield until a condition is met or timeout.
///
/// Drains all pending network work before checking the condition, since the
/// calling thread is about to block anyway. This ensures TCP ACKs, window
/// updates, and retransmissions are processed promptly.
fn wait_until<F>(mut condition: F, timeout_us: Option<u64>) -> Result<(), i32>
where F: FnMut() -> bool
{
    let start = crate::timer::uptime_us();
    
    loop {
        // Drain all pending network work (not just one poll)
        let mut any_progress = false;
        for _ in 0..64 {
            if !smoltcp_net::poll() {
                break;
            }
            any_progress = true;
        }

        if condition() {
            return Ok(());
        }

        if crate::process::is_current_interrupted() {
            return Err(libc_errno::EINTR);
        }

        if let Some(timeout) = timeout_us {
            if crate::timer::uptime_us() - start > timeout {
                return Err(libc_errno::ETIMEDOUT);
            }
        }

        if !any_progress {
            crate::threading::yield_now();
        }
    }
}

pub fn socket_bind(idx: usize, addr: SocketAddrV4) -> Result<(), i32> {
    with_table(|table| {
        if let Some(Some(sock)) = table.get_mut(idx) {
            sock.bind_port = Some(addr.port);
            Ok(())
        } else {
            Err(libc_errno::EBADF)
        }
    })
}

pub fn socket_listen(idx: usize, backlog: usize) -> Result<(), i32> {
    with_table(|table| {
        if idx >= table.len() || table[idx].is_none() {
            return Err(libc_errno::EBADF);
        }
        
        let port = table[idx].as_ref().unwrap().bind_port.ok_or(libc_errno::EINVAL)?;
        
        if let Some(sock) = table[idx].take() {
            if let SocketType::Stream(h) = sock.inner {
                smoltcp_net::socket_close(h);
            }
            
            if let Some(new_sock) = KernelSocket::new_listener(port, backlog) {
                table[idx] = Some(new_sock);
                Ok(())
            } else {
                Err(libc_errno::ENOMEM)
            }
        } else {
            Err(libc_errno::EBADF)
        }
    })
}

pub fn socket_accept(idx: usize) -> Result<(usize, SocketAddrV4), i32> {
    wait_until(|| {
        let mut result = false;
        with_table(|table| {
            if let Some(Some(KernelSocket { inner: SocketType::Listener { handles, .. }, .. })) = table.get(idx) {
                for &handle in handles {
                    let state = with_network(|net| net.sockets.get::<tcp::Socket>(handle).state());
                    if state == Some(tcp::State::Established) {
                        result = true;
                        break;
                    }
                }
            }
        });
        result
    }, None)?;

    let (handle, addr) = with_table(|table| {
        if let Some(Some(KernelSocket { inner: SocketType::Listener { handles, local_port }, .. })) = table.get_mut(idx) {
             let port = *local_port;
             for (i, &handle) in handles.iter().enumerate() {
                let state = with_network(|net| net.sockets.get::<tcp::Socket>(handle).state());
                if state == Some(tcp::State::Established) {
                    let h = handles.remove(i).unwrap();
                    if let Some(new_h) = smoltcp_net::socket_create() {
                        with_network(|net| { let _ = net.sockets.get_mut::<tcp::Socket>(new_h).listen(port); });
                        handles.push_back(new_h);
                    }
                    let remote = with_network(|net| {
                        let socket = net.sockets.get::<tcp::Socket>(h);
                        socket.remote_endpoint().map(|ep| SocketAddrV4 { 
                            ip: if let smoltcp::wire::IpAddress::Ipv4(addr) = ep.addr { addr.octets() } else { [0;4] },
                            port: ep.port 
                        })
                    }).flatten().unwrap_or(SocketAddrV4::new([0;4], 0));
                    return Some((h, remote));
                }
             }
        }
        None
    }).ok_or(libc_errno::ECONNABORTED)?;

    let current_box_id = crate::process::current_process().map(|p| p.box_id).unwrap_or(0);
    let new_sock = KernelSocket { 
        inner: SocketType::Stream(handle), 
        bind_port: None,
        box_id: current_box_id,
    };
    let new_idx = with_table(|table| {
        for (i, slot) in table.iter_mut().enumerate() {
            if slot.is_none() { *slot = Some(new_sock); return Some(i); }
        }
        if table.len() < MAX_SOCKETS { table.push(Some(new_sock)); Some(table.len() - 1) } else { None }
    }).ok_or(libc_errno::ENOMEM)?;

    Ok((new_idx, addr))
}

pub fn socket_connect(idx: usize, addr: SocketAddrV4) -> Result<(), i32> {
    let (h, bound_port): (SocketHandle, Option<u16>) = with_table(|table| {
        if let Some(Some(sock)) = table.get(idx) {
            if let SocketType::Stream(handle) = sock.inner {
                return Some((handle, sock.bind_port));
            }
        }
        None
    }).ok_or(libc_errno::EBADF)?;

    let local_port = bound_port.unwrap_or_else(|| {
        let p = alloc_ephemeral_port();
        with_table(|table| {
            if let Some(Some(sock)) = table.get_mut(idx) {
                sock.bind_port = Some(p);
            }
        });
        p
    });

    let res = with_network(|net| {
        let socket = net.sockets.get_mut::<tcp::Socket>(h);
        
        // Single-interface model: loopback is handled by the main interface
        let cx = net.iface.context();

        socket.connect(cx, 
            (smoltcp::wire::IpAddress::Ipv4(smoltcp::wire::Ipv4Address::from(addr.ip)), addr.port),
            local_port
        ).map_err(|_| libc_errno::ECONNREFUSED)
    });
    
    match res {
        Some(Ok(())) => {},
        Some(Err(_)) => return Err(libc_errno::ECONNREFUSED),
        None => return Err(libc_errno::ENETDOWN),
    }

    wait_until(|| {
        with_network(|net| {
            let socket = net.sockets.get::<tcp::Socket>(h);
            match socket.state() {
                tcp::State::Established => true,
                tcp::State::Closed | tcp::State::Closing | tcp::State::TimeWait => true,
                _ => false
            }
        }).unwrap_or(true)
    }, Some(10_000_000))?;

    let connected = with_network(|net| net.sockets.get::<tcp::Socket>(h).state() == tcp::State::Established).unwrap_or(false);
    if connected { Ok(()) } else { Err(libc_errno::ECONNREFUSED) }
}

pub fn socket_send(idx: usize, buf: &[u8]) -> Result<usize, i32> {
    let handle = with_table(|table| {
        if let Some(Some(KernelSocket { inner: SocketType::Stream(h), .. })) = table.get(idx) { Some(*h) } else { None }
    }).ok_or(libc_errno::EBADF)?;

    wait_until(|| with_network(|net| net.sockets.get::<tcp::Socket>(handle).can_send()).unwrap_or(true), Some(5_000_000))?;

    let res = with_network(|net| {
        let socket = net.sockets.get_mut::<tcp::Socket>(handle);
        if !socket.can_send() { return Err(libc_errno::EPIPE); }
        socket.send_slice(buf).map_err(|_| libc_errno::EIO)
    });
    
    // Poll immediately to transmit the queued data without waiting for thread 0
    smoltcp_net::poll();
    
    match res {
        Some(r) => r,
        None => Err(libc_errno::ENETDOWN),
    }
}

pub fn socket_recv(idx: usize, buf: &mut [u8]) -> Result<usize, i32> {
    let handle = with_table(|table| {
        if let Some(Some(KernelSocket { inner: SocketType::Stream(h), .. })) = table.get(idx) { Some(*h) } else { None }
    }).ok_or(libc_errno::EBADF)?;

    wait_until(|| with_network(|net| {
        let socket = net.sockets.get::<tcp::Socket>(handle);
        socket.can_recv() || !socket.is_active()
    }).unwrap_or(true), Some(30_000_000))?;

    let res = with_network(|net| {
        let socket = net.sockets.get_mut::<tcp::Socket>(handle);
        if socket.can_recv() {
            socket.recv(|data| {
                let len = data.len().min(buf.len());
                buf[..len].copy_from_slice(&data[..len]);
                (len, len)
            }).map_err(|_| libc_errno::EIO)
        } else if !socket.is_active() { Ok(0) } else { Err(libc_errno::EAGAIN) }
    });
    
    // Poll immediately after recv to send TCP window update ACK.
    // Reading data frees RX buffer space, but the updated window is only
    // advertised to the remote when the next ACK goes out. Without this,
    // the window update waits until thread 0 polls, causing the remote
    // sender to stall with a shrunken/zero window.
    smoltcp_net::poll();
    
    match res {
        Some(r) => r,
        None => Err(libc_errno::ENETDOWN),
    }
}

pub struct SocketStat {
    pub local_port: u16,
    pub remote_ip: [u8; 4],
    pub remote_port: u16,
    pub state: &'static str,
    pub box_id: u64,
}

pub fn list_sockets() -> Vec<SocketStat> {
    let mut stats = Vec::new();
    let current_box_id = crate::process::current_process().map(|p| p.box_id).unwrap_or(0);

    with_table(|table| {
        for slot in table.iter().flatten() {
            // Isolation: only show sockets from current box (unless Box 0)
            if current_box_id != 0 && slot.box_id != current_box_id {
                continue;
            }

            match slot.inner {
                SocketType::Stream(h) => {
                    with_network(|net| {
                        let socket = net.sockets.get::<tcp::Socket>(h);
                        let remote = socket.remote_endpoint().map(|ep| {
                            (if let smoltcp::wire::IpAddress::Ipv4(addr) = ep.addr { addr.octets() } else { [0;4] }, ep.port)
                        }).unwrap_or(([0;4], 0));
                        
                        let state = match socket.state() {
                            tcp::State::Closed => "CLOSED",
                            tcp::State::Listen => "LISTEN",
                            tcp::State::SynSent => "SYN_SENT",
                            tcp::State::SynReceived => "SYN_RECV",
                            tcp::State::Established => "ESTABLISHED",
                            tcp::State::FinWait1 => "FIN_WAIT1",
                            tcp::State::FinWait2 => "FIN_WAIT2",
                            tcp::State::CloseWait => "CLOSE_WAIT",
                            tcp::State::Closing => "CLOSING",
                            tcp::State::LastAck => "LAST_ACK",
                            tcp::State::TimeWait => "TIME_WAIT",
                        };

                        stats.push(SocketStat {
                            local_port: slot.bind_port.unwrap_or(0),
                            remote_ip: remote.0,
                            remote_port: remote.1,
                            state,
                            box_id: slot.box_id,
                        });
                    });
                }
                SocketType::Listener { local_port, .. } => {
                    stats.push(SocketStat {
                        local_port,
                        remote_ip: [0;4],
                        remote_port: 0,
                        state: "LISTEN",
                        box_id: slot.box_id,
                    });
                }
            }
        }
    });
    stats
}

// ============================================================================
// Error Numbers
// ============================================================================

pub mod libc_errno {
    pub const EINTR: i32 = 4;
    pub const EIO: i32 = 5;
    pub const EBADF: i32 = 9;
    pub const EAGAIN: i32 = 11;
    pub const ENOMEM: i32 = 12;
    pub const EINVAL: i32 = 22;
    pub const ENETDOWN: i32 = 100;
    pub const ETIMEDOUT: i32 = 110;
    pub const ECONNREFUSED: i32 = 111;
    pub const ECONNABORTED: i32 = 103;
    pub const EPIPE: i32 = 32;
}
