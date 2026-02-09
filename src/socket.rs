//! Kernel Socket Management
//!
//! Provides socket abstractions for userspace programs via syscalls.
//! Wraps smoltcp sockets via the thread-safe smoltcp_net module.

use alloc::vec::Vec;
use alloc::collections::VecDeque;
use core::sync::atomic::{AtomicUsize, Ordering};
use spinning_top::Spinlock;

use crate::smoltcp_net::{self, SocketHandle, with_network};
use smoltcp::socket::tcp;

// ============================================================================
// Constants
// ============================================================================

/// Maximum number of concurrent sockets (FDs)
pub const MAX_SOCKETS: usize = 64;

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
// Socket Constants
// ============================================================================

pub mod socket_const {
    pub const AF_INET: i32 = 2;
    pub const SOCK_STREAM: i32 = 1;
    pub const SOCK_DGRAM: i32 = 2;
    pub const IPPROTO_TCP: i32 = 6;
    pub const IPPROTO_UDP: i32 = 17;
    pub const SHUT_RD: i32 = 0;
    pub const SHUT_WR: i32 = 1;
    pub const SHUT_RDWR: i32 = 2;
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
        backlog: usize,
    },
}

// ============================================================================
// Kernel Socket
// ============================================================================

pub struct KernelSocket {
    pub inner: SocketType,
    pub bind_port: Option<u16>,
}

impl KernelSocket {
    pub fn new_stream() -> Option<Self> {
        let handle = smoltcp_net::socket_create()?;
        Some(Self {
            inner: SocketType::Stream(handle),
            bind_port: None,
        })
    }

    pub fn new_listener(port: u16, backlog: usize) -> Option<Self> {
        let mut handles = VecDeque::new();
        for _ in 0..backlog {
            if let Some(handle) = smoltcp_net::socket_create() {
                with_network(|net| {
                    let socket = net.sockets.get_mut::<tcp::Socket>(handle);
                    let _ = socket.listen(port);
                });
                handles.push_back(handle);
            }
        }
        
        Some(Self {
            inner: SocketType::Listener { local_port: port, handles, backlog },
            bind_port: Some(port),
        })
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
            if let Some(mut sock) = table[idx].take() {
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

/// Helper to poll and yield until a condition is met or timeout
fn wait_until<F>(mut condition: F, timeout_us: Option<u64>) -> Result<(), i32>
where F: FnMut() -> bool
{
    let start = crate::timer::uptime_us();
    let mut last_poll = smoltcp_net::poll_count();
    
    loop {
        smoltcp_net::poll();

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

        let current_poll = smoltcp_net::poll_count();
        if current_poll == last_poll {
            crate::threading::yield_now();
        }
        last_poll = current_poll;
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
        
        if let Some(mut sock) = table[idx].take() {
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
        if let Some(Some(KernelSocket { inner: SocketType::Listener { handles, .. }, .. })) = table.get_mut(idx) {
             for (i, &handle) in handles.iter().enumerate() {
                let state = with_network(|net| net.sockets.get::<tcp::Socket>(handle).state());
                if state == Some(tcp::State::Established) {
                    let h = handles.remove(i).unwrap();
                    let local_port = with_network(|net| net.sockets.get::<tcp::Socket>(h).local_endpoint().map(|ep| ep.port).unwrap_or(0));
                    if let Some(new_h) = smoltcp_net::socket_create() {
                        with_network(|net| { let _ = net.sockets.get_mut::<tcp::Socket>(new_h).listen(local_port.unwrap_or(0)); });
                        handles.push_back(new_h);
                    }
                    let remote = with_network(|net| {
                        let socket = net.sockets.get::<tcp::Socket>(h);
                        socket.remote_endpoint().map(|ep| SocketAddrV4 { 
                            ip: if let smoltcp::wire::IpAddress::Ipv4(addr) = ep.addr { addr.0 } else { [0;4] },
                            port: ep.port 
                        })
                    }).unwrap_or(None).unwrap_or(SocketAddrV4::new([0;4], 0));
                    return Some((h, remote));
                }
             }
        }
        None
    }).ok_or(libc_errno::ECONNABORTED)?;

    let new_sock = KernelSocket { inner: SocketType::Stream(handle), bind_port: None };
    let new_idx = with_table(|table| {
        for (i, slot) in table.iter_mut().enumerate() {
            if slot.is_none() { *slot = Some(new_sock); return Some(i); }
        }
        if table.len() < MAX_SOCKETS { table.push(Some(new_sock)); Some(table.len() - 1) } else { None }
    }).ok_or(libc_errno::ENOMEM)?;

    Ok((new_idx, addr))
}

pub fn socket_connect(idx: usize, addr: SocketAddrV4) -> Result<(), i32> {
    let handle = with_table(|table| {
        if let Some(Some(KernelSocket { inner: SocketType::Stream(h), .. })) = table.get(idx) { Some(*h) } else { None }
    }).ok_or(libc_errno::EBADF)?;

    let res = with_network(|net| {
        let socket = net.sockets.get_mut::<tcp::Socket>(handle);
        let cx = net.iface.context();
        socket.connect(cx, 
            (smoltcp::wire::IpAddress::Ipv4(smoltcp::wire::Ipv4Address(addr.ip)), addr.port),
            (smoltcp::wire::IpAddress::Ipv4(smoltcp::wire::Ipv4Address([0;4])), 0)
        ).map_err(|_| libc_errno::ECONNREFUSED)
    });
    
    match res {
        Some(Ok(())) => {},
        Some(Err(e)) => return Err(e),
        None => return Err(libc_errno::ENETDOWN),
    }

    wait_until(|| {
        with_network(|net| {
            let socket = net.sockets.get::<tcp::Socket>(handle);
            match socket.state() {
                tcp::State::Established => true,
                tcp::State::Closed | tcp::State::Closing | tcp::State::TimeWait => true,
                _ => false
            }
        }).unwrap_or(true)
    }, Some(10_000_000))?;

    let connected = with_network(|net| net.sockets.get::<tcp::Socket>(handle).state() == tcp::State::Established).unwrap_or(false);
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
    
    match res {
        Some(r) => r,
        None => Err(libc_errno::ENETDOWN),
    }
}

// ============================================================================
// Error Numbers
// ============================================================================

pub mod libc_errno {
    pub const ENOENT: i32 = 2;
    pub const EINTR: i32 = 4;
    pub const EIO: i32 = 5;
    pub const EBADF: i32 = 9;
    pub const EAGAIN: i32 = 11;
    pub const EWOULDBLOCK: i32 = 11;
    pub const ENOMEM: i32 = 12;
    pub const EFAULT: i32 = 14;
    pub const EINVAL: i32 = 22;
    pub const ENOTSOCK: i32 = 88;
    pub const ENETDOWN: i32 = 100;
    pub const EISCONN: i32 = 106;
    pub const ENOTCONN: i32 = 107;
    pub const ETIMEDOUT: i32 = 110;
    pub const ECONNREFUSED: i32 = 111;
    pub const EHOSTUNREACH: i32 = 113;
    pub const EOPNOTSUPP: i32 = 95;
    pub const ECHILD: i32 = 10;
    pub const ESRCH: i32 = 3;
    pub const ECONNABORTED: i32 = 103;
    pub const EPIPE: i32 = 32;
    pub const ENOTDIR: i32 = 20;
}