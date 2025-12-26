// Network stack with TCP netcat-like server
// Uses smoltcp for TCP/IP and virtio-net for the network device
//
// This version uses safe Rust with Box allocations instead of static mut

use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;
use smoltcp::iface::{Config, Interface, SocketSet, SocketStorage};
use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::socket::tcp::{
    Socket as TcpSocket, SocketBuffer as TcpSocketBuffer, State as TcpState,
};
use smoltcp::time::Instant;
use smoltcp::wire::{EthernetAddress, HardwareAddress, IpAddress, IpCidr, Ipv4Address};
use spinning_top::Spinlock;
use virtio_drivers::device::net::VirtIONetRaw;
use virtio_drivers::transport::mmio::{MmioTransport, VirtIOHeader};

use crate::akuma::AKUMA_79;
use crate::console;
use crate::timer;
use crate::virtio_hal::VirtioHal;

// ============================================================================
// Constants
// ============================================================================

const LISTEN_PORT: u16 = 23; // Telnet port, forwarded from host 2323
const SSH_PORT: u16 = 22; // SSH port, forwarded from host 2222
const VIRTIO_BUFFER_SIZE: usize = 2048;
const TCP_BUFFER_SIZE: usize = 4096;

// ============================================================================
// Statistics (protected by spinlock)
// ============================================================================

struct NetStats {
    bytes_rx: u64,
    bytes_tx: u64,
    connections: u64,
}

impl NetStats {
    const fn new() -> Self {
        Self {
            bytes_rx: 0,
            bytes_tx: 0,
            connections: 0,
        }
    }
}

static NET_STATS: Spinlock<NetStats> = Spinlock::new(NetStats::new());

// ============================================================================
// Virtio Network Device
// ============================================================================

/// Holds RX data after receiving, separate from the device to allow split borrows
struct RxData {
    buffer: Box<[u8; VIRTIO_BUFFER_SIZE]>,
    offset: usize,
    len: usize,
    valid: bool,
}

impl RxData {
    fn new() -> Self {
        Self {
            buffer: Box::new([0u8; VIRTIO_BUFFER_SIZE]),
            offset: 0,
            len: 0,
            valid: false,
        }
    }
}

pub struct VirtioNetDevice {
    inner: VirtIONetRaw<VirtioHal, MmioTransport, 16>,
    tx_buffer: Box<[u8; VIRTIO_BUFFER_SIZE]>,
    rx_pending_token: Option<u16>,
    // RX data is in a RefCell to allow interior mutability for split borrows
    rx_data: RefCell<RxData>,
}

impl VirtioNetDevice {
    fn new(inner: VirtIONetRaw<VirtioHal, MmioTransport, 16>) -> Self {
        Self {
            inner,
            tx_buffer: Box::new([0u8; VIRTIO_BUFFER_SIZE]),
            rx_pending_token: None,
            rx_data: RefCell::new(RxData::new()),
        }
    }

    /// Try to receive a packet, returning true if one is available
    fn try_receive(&mut self) -> bool {
        let mut rx = self.rx_data.borrow_mut();

        // Check if we have a pending receive
        if let Some(token) = self.rx_pending_token {
            if self.inner.poll_receive().is_some() {
                self.rx_pending_token = None;
                // SAFETY: receive_complete requires the buffer passed to receive_begin
                match unsafe { self.inner.receive_complete(token, &mut rx.buffer[..]) } {
                    Ok((hdr_len, data_len)) => {
                        rx.offset = hdr_len;
                        rx.len = data_len;
                        rx.valid = true;
                        return true;
                    }
                    Err(_) => {}
                }
            }
        } else {
            // Start a new receive
            // SAFETY: receive_begin requires a buffer to store incoming data
            match unsafe { self.inner.receive_begin(&mut rx.buffer[..]) } {
                Ok(token) => {
                    self.rx_pending_token = Some(token);
                    if self.inner.poll_receive().is_some() {
                        self.rx_pending_token = None;
                        match unsafe { self.inner.receive_complete(token, &mut rx.buffer[..]) } {
                            Ok((hdr_len, data_len)) => {
                                rx.offset = hdr_len;
                                rx.len = data_len;
                                rx.valid = true;
                                return true;
                            }
                            Err(_) => {}
                        }
                    }
                }
                Err(_) => {}
            }
        }
        false
    }
}

// ============================================================================
// smoltcp Device Implementation
// ============================================================================

/// RxToken borrows the device's rx_data via RefCell
pub struct VirtioRxToken<'a> {
    device: &'a VirtioNetDevice,
}

/// TxToken holds a mutable reference to the device for sending
pub struct VirtioTxToken<'a> {
    device: &'a mut VirtioNetDevice,
}

impl<'a> RxToken for VirtioRxToken<'a> {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut rx = self.device.rx_data.borrow_mut();
        let offset = rx.offset;
        let len = rx.len;
        let data = &mut rx.buffer[offset..offset + len];
        let result = f(data);
        rx.valid = false;
        result
    }
}

impl<'a> TxToken for VirtioTxToken<'a> {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let result = f(&mut self.device.tx_buffer[..len]);
        let _ = self.device.inner.send(&self.device.tx_buffer[..len]);
        result
    }
}

impl Device for VirtioNetDevice {
    type RxToken<'a>
        = VirtioRxToken<'a>
    where
        Self: 'a;
    type TxToken<'a>
        = VirtioTxToken<'a>
    where
        Self: 'a;

    fn receive(&mut self, _timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        if self.try_receive() {
            // We need to split self into rx and tx parts
            // RxToken uses &self (immutable, accesses rx_data via RefCell)
            // TxToken uses &mut self (for tx_buffer and inner.send)
            // This is sound because RxToken only accesses rx_data through RefCell
            let self_ptr = self as *mut Self;
            Some((
                VirtioRxToken {
                    device: unsafe { &*self_ptr },
                },
                VirtioTxToken {
                    device: unsafe { &mut *self_ptr },
                },
            ))
        } else {
            None
        }
    }

    fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
        Some(VirtioTxToken { device: self })
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.max_transmission_unit = 1500;
        caps.medium = Medium::Ethernet;
        caps
    }
}

// ============================================================================
// Network Stack State
// ============================================================================

struct NetStack {
    device: VirtioNetDevice,
    iface: Interface,
    sockets: SocketSet<'static>,
    tcp_handle: smoltcp::iface::SocketHandle,
    ssh_handle: smoltcp::iface::SocketHandle,
    was_connected: bool,
    ssh_was_connected: bool,
    // Note: TCP buffers and socket storage are leaked via Box::leak() for 'static lifetime
}

static NET_STACK: Spinlock<Option<NetStack>> = Spinlock::new(None);
static NET_INITIALIZED: Spinlock<bool> = Spinlock::new(false);

// ============================================================================
// Logging Helper
// ============================================================================

fn log(msg: &str) {
    console::print(msg);
}

// ============================================================================
// Initialization
// ============================================================================

/// QEMU virt machine virtio MMIO addresses
const VIRTIO_MMIO_ADDRS: [usize; 8] = [
    0x0a000000, 0x0a000200, 0x0a000400, 0x0a000600, 0x0a000800, 0x0a000a00, 0x0a000c00, 0x0a000e00,
];

pub fn init(_dtb_ptr: usize) -> Result<(), &'static str> {
    log("[Net] Initializing network stack...\n");

    // Find virtio-net device
    let mut found_device: Option<VirtioNetDevice> = None;
    let mut mac = [0u8; 6];

    for (i, &addr) in VIRTIO_MMIO_ADDRS.iter().enumerate() {
        // SAFETY: Reading from MMIO registers at known QEMU virt machine addresses
        let device_id = unsafe { core::ptr::read_volatile((addr + 0x008) as *const u32) };
        if device_id != 1 {
            continue;
        }

        log("[Net] Found virtio-net at slot ");
        console::print(&alloc::format!("{}\n", i));

        let header_ptr = match core::ptr::NonNull::new(addr as *mut VirtIOHeader) {
            Some(p) => p,
            None => continue,
        };

        // SAFETY: Creating MmioTransport for verified virtio device
        let transport = match unsafe { MmioTransport::new(header_ptr) } {
            Ok(t) => t,
            Err(_) => {
                log("[Net] Failed to create transport\n");
                continue;
            }
        };

        let net = match VirtIONetRaw::<VirtioHal, MmioTransport, 16>::new(transport) {
            Ok(n) => n,
            Err(_) => {
                log("[Net] Failed to init virtio device\n");
                continue;
            }
        };

        mac = net.mac_address();
        found_device = Some(VirtioNetDevice::new(net));
        break;
    }

    let mut device = found_device.ok_or("No virtio-net device found")?;

    // Log MAC address
    log("[Net] MAC: ");
    for (i, b) in mac.iter().enumerate() {
        if i > 0 {
            console::print(":");
        }
        console::print(&alloc::format!("{:02x}", b));
    }
    log("\n");

    // Create smoltcp interface
    let hw_addr = EthernetAddress::from_bytes(&mac);
    let config = Config::new(HardwareAddress::Ethernet(hw_addr));

    let mut iface = Interface::new(config, &mut device, get_time());

    // Configure IP address (10.0.2.15 is the default for QEMU user-mode networking)
    iface.update_ip_addrs(|addrs| {
        addrs
            .push(IpCidr::new(IpAddress::v4(10, 0, 2, 15), 24))
            .ok();
    });

    // Set default gateway
    iface
        .routes_mut()
        .add_default_ipv4_route(Ipv4Address::new(10, 0, 2, 2))
        .ok();

    log("[Net] IP: 10.0.2.15/24, Gateway: 10.0.2.2\n");

    // Allocate TCP socket buffers on the heap and leak them for 'static lifetime
    // These live for the lifetime of the kernel - no deallocation needed
    let tcp_rx_buf: Box<[u8]> = vec![0u8; TCP_BUFFER_SIZE].into_boxed_slice();
    let tcp_tx_buf: Box<[u8]> = vec![0u8; TCP_BUFFER_SIZE].into_boxed_slice();

    // Box::leak gives us 'static references - safe because these are never deallocated
    let tcp_rx_ref: &'static mut [u8] = Box::leak(tcp_rx_buf);
    let tcp_tx_ref: &'static mut [u8] = Box::leak(tcp_tx_buf);

    let tcp_rx_buffer = TcpSocketBuffer::new(tcp_rx_ref);
    let tcp_tx_buffer = TcpSocketBuffer::new(tcp_tx_ref);
    let tcp_socket = TcpSocket::new(tcp_rx_buffer, tcp_tx_buffer);

    // Allocate SSH socket buffers
    let ssh_rx_buf: Box<[u8]> = vec![0u8; TCP_BUFFER_SIZE].into_boxed_slice();
    let ssh_tx_buf: Box<[u8]> = vec![0u8; TCP_BUFFER_SIZE].into_boxed_slice();
    let ssh_rx_ref: &'static mut [u8] = Box::leak(ssh_rx_buf);
    let ssh_tx_ref: &'static mut [u8] = Box::leak(ssh_tx_buf);

    let ssh_rx_buffer = TcpSocketBuffer::new(ssh_rx_ref);
    let ssh_tx_buffer = TcpSocketBuffer::new(ssh_tx_ref);
    let ssh_socket = TcpSocket::new(ssh_rx_buffer, ssh_tx_buffer);

    // Allocate socket storage on heap and leak for 'static lifetime
    // 4 slots: telnet socket + SSH socket (each needs 1 slot)
    let mut storage_vec: Vec<SocketStorage<'static>> = Vec::with_capacity(4);
    storage_vec.push(SocketStorage::EMPTY);
    storage_vec.push(SocketStorage::EMPTY);
    storage_vec.push(SocketStorage::EMPTY);
    storage_vec.push(SocketStorage::EMPTY);
    let socket_storage: Box<[SocketStorage<'static>]> = storage_vec.into_boxed_slice();

    // Box::leak gives us 'static reference - safe because this is never deallocated
    let storage_ref: &'static mut [SocketStorage<'static>] = Box::leak(socket_storage);

    let mut sockets = SocketSet::new(storage_ref);
    let tcp_handle = sockets.add(tcp_socket);
    let ssh_handle = sockets.add(ssh_socket);

    // Start listening on telnet port
    {
        let socket = sockets.get_mut::<TcpSocket>(tcp_handle);
        socket
            .listen(LISTEN_PORT)
            .map_err(|_| "Failed to listen on telnet port")?;
    }

    // Start listening on SSH port
    {
        let socket = sockets.get_mut::<TcpSocket>(ssh_handle);
        socket
            .listen(SSH_PORT)
            .map_err(|_| "Failed to listen on SSH port")?;
    }

    log(&alloc::format!(
        "[Net] Listening on port {} (telnet)\n",
        LISTEN_PORT
    ));
    log(&alloc::format!(
        "[Net] Listening on port {} (SSH)\n",
        SSH_PORT
    ));
    log("[Net] Connect from host: nc localhost 2323 (telnet)\n");
    log("[Net] Connect from host: nc localhost 2222 (SSH)\n");

    // Store in global state
    {
        let mut stack = NET_STACK.lock();
        *stack = Some(NetStack {
            device,
            iface,
            sockets,
            tcp_handle,
            ssh_handle,
            was_connected: false,
            ssh_was_connected: false,
        });
    }

    *NET_INITIALIZED.lock() = true;

    log("[Net] Network stack ready\n");
    Ok(())
}

// ============================================================================
// Time Helper
// ============================================================================

fn get_time() -> Instant {
    let us = timer::uptime_us();
    Instant::from_micros(us as i64)
}

// ============================================================================
// Network Polling & TCP Handler
// ============================================================================

/// Poll the network stack - call this regularly from a thread
/// Returns true if there was activity
pub fn poll() -> bool {
    let mut stack_guard = NET_STACK.lock();
    let stack = match stack_guard.as_mut() {
        Some(s) => s,
        _ => return false,
    };

    let timestamp = get_time();

    // Process network interface
    let activity = stack
        .iface
        .poll(timestamp, &mut stack.device, &mut stack.sockets);

    // Handle TCP socket
    let socket = stack.sockets.get_mut::<TcpSocket>(stack.tcp_handle);

    // Check for new connection
    if socket.state() == TcpState::Established && !stack.was_connected {
        stack.was_connected = true;
        NET_STATS.lock().connections += 1;
        drop(stack_guard);
        log("\n[Net] *** Client connected! ***\n");
        log("[Net] Type something and press Enter (echo server)\n");
        log("[Net] Type 'quit' to disconnect\n\n");
        return true;
    } else if socket.state() == TcpState::Listen || socket.state() == TcpState::Closed {
        stack.was_connected = false;
    }

    // Echo any received data back
    if socket.can_recv() {
        let mut buf = [0u8; 512];
        match socket.recv_slice(&mut buf) {
            Ok(len) if len > 0 => {
                NET_STATS.lock().bytes_rx += len as u64;

                // Check for 'quit' command
                let data = &buf[..len];
                if len >= 4 && (data.starts_with(b"quit") || data.starts_with(b"exit")) {
                    let _ = socket.send_slice(b"Goodbye!\r\n");
                    socket.close();
                    drop(stack_guard);
                    log("[Net] Client disconnected (quit)\n");
                    return true;
                }

                if len >= 3 && data.starts_with(b"cat") {
                    let _ = socket.send_slice(AKUMA_79);
                    NET_STATS.lock().bytes_tx += AKUMA_79.len() as u64;
                    return true;
                }

                // Echo back with prefix
                if socket.can_send() {
                    let _ = socket.send_slice(b"echo: ");
                    let _ = socket.send_slice(&buf[..len]);
                    NET_STATS.lock().bytes_tx += 6 + len as u64;
                }
                return true;
            }
            _ => {}
        }
    }

    // Re-listen if socket closed
    if socket.state() == TcpState::Closed {
        let _ = socket.listen(LISTEN_PORT);
    }

    activity
}

// ============================================================================
// SSH Socket Polling
// ============================================================================

/// SSH connection event returned by poll_ssh
pub enum SshEvent {
    /// No event
    None,
    /// New connection established
    Connected,
    /// Data received (contains the data)
    Data(Vec<u8>),
    /// Connection closed
    Disconnected,
}

/// Poll the SSH socket and return any events
/// This is called by the SSH module to handle the transport layer
pub fn poll_ssh() -> SshEvent {
    let mut stack_guard = NET_STACK.lock();
    let stack = match stack_guard.as_mut() {
        Some(s) => s,
        _ => return SshEvent::None,
    };

    let timestamp = get_time();

    // Process network interface
    stack
        .iface
        .poll(timestamp, &mut stack.device, &mut stack.sockets);

    // Handle SSH socket
    let socket = stack.sockets.get_mut::<TcpSocket>(stack.ssh_handle);

    // Check for new connection
    if socket.state() == TcpState::Established && !stack.ssh_was_connected {
        stack.ssh_was_connected = true;
        NET_STATS.lock().connections += 1;
        return SshEvent::Connected;
    }

    // Check for disconnect
    if (socket.state() == TcpState::Listen || socket.state() == TcpState::Closed)
        && stack.ssh_was_connected
    {
        stack.ssh_was_connected = false;
        // Re-listen if socket closed
        if socket.state() == TcpState::Closed {
            let _ = socket.listen(SSH_PORT);
        }
        return SshEvent::Disconnected;
    }

    // Check for received data
    if socket.can_recv() {
        let mut buf = [0u8; 512];
        match socket.recv_slice(&mut buf) {
            Ok(len) if len > 0 => {
                NET_STATS.lock().bytes_rx += len as u64;
                return SshEvent::Data(buf[..len].to_vec());
            }
            _ => {}
        }
    }

    // Re-listen if socket closed
    if socket.state() == TcpState::Closed {
        let _ = socket.listen(SSH_PORT);
    }

    SshEvent::None
}

/// Send data on the SSH socket
pub fn ssh_send(data: &[u8]) -> bool {
    let mut stack_guard = NET_STACK.lock();
    let stack = match stack_guard.as_mut() {
        Some(s) => s,
        _ => return false,
    };

    let socket = stack.sockets.get_mut::<TcpSocket>(stack.ssh_handle);
    if socket.can_send() {
        match socket.send_slice(data) {
            Ok(len) => {
                NET_STATS.lock().bytes_tx += len as u64;
                return true;
            }
            Err(_) => return false,
        }
    }
    false
}

/// Close the SSH connection
pub fn ssh_close() {
    let mut stack_guard = NET_STACK.lock();
    let stack = match stack_guard.as_mut() {
        Some(s) => s,
        _ => return,
    };

    let socket = stack.sockets.get_mut::<TcpSocket>(stack.ssh_handle);
    socket.close();
}

/// Get network statistics: (connections, bytes_rx, bytes_tx)
pub fn get_stats() -> (u64, u64, u64) {
    let s = NET_STATS.lock();
    (s.connections, s.bytes_rx, s.bytes_tx)
}

// ============================================================================
// Public API
// ============================================================================

/// Thread entry point for network server
pub fn netcat_server_entry() -> ! {
    log("[Net] Netcat server thread started\n");

    loop {
        poll();
        crate::threading::yield_now();
    }
}

/// Print network statistics
pub fn stats() {
    let s = NET_STATS.lock();
    log(&alloc::format!(
        "[Net] Stats: {} connections, {} bytes RX, {} bytes TX\n",
        s.connections,
        s.bytes_rx,
        s.bytes_tx
    ));
}

/// Check if network is initialized
pub fn is_ready() -> bool {
    *NET_INITIALIZED.lock()
}
