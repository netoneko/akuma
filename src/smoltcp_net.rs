//! Smoltcp Network Stack (Thread-Safe)
//!
//! Replaces embassy-net with a direct smoltcp integration protected by a kernel Spinlock.
//! This allows any thread (kernel or userspace via syscall) to drive the network stack,
//! eliminating the need for a dedicated network thread and complex preemption management.

use alloc::collections::VecDeque;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use spinning_top::Spinlock;

use smoltcp::iface::{Config, Interface, SocketSet, SocketStorage, PollResult};
pub use smoltcp::iface::SocketHandle;
use smoltcp::phy::Device;
use smoltcp::socket::{tcp, dhcpv4};
use smoltcp::time::Instant;
use smoltcp::wire::{EthernetAddress, HardwareAddress, IpAddress, IpCidr};

use virtio_drivers::device::net::VirtIONetRaw;
use virtio_drivers::transport::mmio::{MmioTransport, VirtIOHeader};

use crate::console;
use crate::virtio_hal::VirtioHal;

// ============================================================================
// Constants
// ============================================================================

const MAX_SOCKETS: usize = 128;
const TCP_RX_BUFFER_SIZE: usize = 65535;
const TCP_TX_BUFFER_SIZE: usize = 65535;

/// QEMU virt machine virtio MMIO addresses
const VIRTIO_MMIO_ADDRS: [usize; 8] = [
    0x0a000000, 0x0a000200, 0x0a000400, 0x0a000600, 0x0a000800, 0x0a000a00, 0x0a000c00, 0x0a000e00,
];

// ============================================================================
// Global Network State
// ============================================================================

/// Atomic flag indicating the network stack is initialized and ready
static NETWORK_READY: AtomicBool = AtomicBool::new(false);

/// Atomic counter incremented when progress is made (e.g. packets processed)
static POLL_COUNT: AtomicUsize = AtomicUsize::new(0);

pub fn is_ready() -> bool {
    NETWORK_READY.load(Ordering::Acquire)
}

pub fn poll_count() -> usize {
    POLL_COUNT.load(Ordering::Acquire)
}

pub struct NetworkState {
    pub iface: Interface,
    pub sockets: SocketSet<'static>,
    pub device: LoopbackAwareDevice,
    pub dhcp_handle: Option<SocketHandle>,
    /// Sockets that have been closed by the user but are waiting for the stack to finish
    pub pending_removal: Vec<SocketHandle>,
}

/// Global network stack protected by a Spinlock.
static NETWORK: Spinlock<Option<NetworkState>> = Spinlock::new(None);

/// Static storage for sockets (required by smoltcp)
static mut SOCKET_STORAGE: [SocketStorage<'static>; MAX_SOCKETS] = [SocketStorage::EMPTY; MAX_SOCKETS];

// ============================================================================
// VirtIO Smoltcp Device Wrapper
// ============================================================================

pub struct VirtioSmoltcpDevice {
    inner: VirtIONetRaw<VirtioHal, MmioTransport, 16>,
    rx_buffer: [u8; 2048],
    tx_buffer: [u8; 2048],
}

impl VirtioSmoltcpDevice {
    pub fn new(inner: VirtIONetRaw<VirtioHal, MmioTransport, 16>) -> Self {
        Self {
            inner,
            rx_buffer: [0u8; 2048],
            tx_buffer: [0u8; 2048],
        }
    }

    pub fn mac_address(&self) -> [u8; 6] {
        self.inner.mac_address()
    }
}

impl Device for VirtioSmoltcpDevice {
    type RxToken<'a> = VirtioRxToken<'a>;
    type TxToken<'a> = VirtioTxToken<'a>;

    fn receive(&mut self, _timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        if self.inner.poll_receive().is_some() {
            match unsafe { self.inner.receive_begin(&mut self.rx_buffer) } {
                Ok(token) => {
                    match unsafe { self.inner.receive_complete(token, &mut self.rx_buffer) } {
                        Ok((hdr_len, pkt_len)) => {
                            let rx = VirtioRxToken {
                                buffer: unsafe { core::slice::from_raw_parts_mut(self.rx_buffer.as_mut_ptr().add(hdr_len), pkt_len) },
                            };
                            let tx = VirtioTxToken {
                                inner: unsafe { &mut *(&mut self.inner as *mut _) },
                                buffer: unsafe { core::slice::from_raw_parts_mut(self.tx_buffer.as_mut_ptr(), 2048) },
                            };
                            return Some((rx, tx));
                        }
                        Err(_) => return None,
                    }
                }
                Err(_) => return None,
            }
        }
        None
    }

    fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
        Some(VirtioTxToken {
            inner: unsafe { &mut *(&mut self.inner as *mut _) },
            buffer: unsafe { core::slice::from_raw_parts_mut(self.tx_buffer.as_mut_ptr(), 2048) },
        })
    }

    fn capabilities(&self) -> smoltcp::phy::DeviceCapabilities {
        let mut caps = smoltcp::phy::DeviceCapabilities::default();
        caps.max_transmission_unit = 1514;
        caps
    }
}

pub struct VirtioRxToken<'a> {
    buffer: &'a mut [u8],
}

impl<'a> smoltcp::phy::RxToken for VirtioRxToken<'a> {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        f(self.buffer)
    }
}

pub struct VirtioTxToken<'a> {
    inner: &'a mut VirtIONetRaw<VirtioHal, MmioTransport, 16>,
    buffer: &'a mut [u8],
}

impl<'a> smoltcp::phy::TxToken for VirtioTxToken<'a> {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let res = f(&mut self.buffer[..len]);
        let _ = self.inner.send(&self.buffer[..len]);
        res
    }
}

// ============================================================================
// Loopback-Aware Device Wrapper
// ============================================================================

/// Check if an Ethernet frame is destined for loopback (127.x.x.x).
///
/// Inspects the EtherType and the relevant IP address field:
/// - ARP (0x0806): target protocol address at bytes [38:42]
/// - IPv4 (0x0800): destination IP at bytes [30:34]
fn is_loopback_frame(frame: &[u8]) -> bool {
    if frame.len() < 14 {
        return false;
    }
    let ethertype = u16::from_be_bytes([frame[12], frame[13]]);
    match ethertype {
        // ARP: match if either sender (bytes 28) or target (bytes 38) IP is 127.x.x.x
        0x0806 => frame.len() >= 42 && (frame[28] == 127 || frame[38] == 127),
        // IPv4: match if either source (byte 26) or dest (byte 30) IP is 127.x.x.x
        0x0800 => frame.len() >= 34 && (frame[26] == 127 || frame[30] == 127),
        _ => false,
    }
}

/// A composite device that wraps VirtIO for external traffic and an internal
/// queue for loopback (127.x.x.x) traffic. Outgoing frames destined for
/// loopback addresses are intercepted in `TxToken::consume()` and queued
/// internally rather than being sent through VirtIO. `receive()` checks
/// the loopback queue first, then falls back to VirtIO.
pub struct LoopbackAwareDevice {
    virtio: VirtioSmoltcpDevice,
    pub loopback_queue: VecDeque<Vec<u8>>,
}

impl LoopbackAwareDevice {
    pub fn new(virtio: VirtioSmoltcpDevice) -> Self {
        Self {
            virtio,
            loopback_queue: VecDeque::new(),
        }
    }

    pub fn mac_address(&self) -> [u8; 6] {
        self.virtio.mac_address()
    }
}

impl Device for LoopbackAwareDevice {
    type RxToken<'a> = LoopbackAwareRxToken<'a>;
    type TxToken<'a> = LoopbackAwareTxToken<'a>;

    fn receive(&mut self, _timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        // Priority: loopback queue first
        if let Some(frame) = self.loopback_queue.pop_front() {
            let rx = LoopbackAwareRxToken::Loopback(frame);
            let tx = LoopbackAwareTxToken {
                virtio_inner: unsafe { &mut *(&mut self.virtio.inner as *mut _) },
                virtio_buffer: unsafe {
                    core::slice::from_raw_parts_mut(self.virtio.tx_buffer.as_mut_ptr(), 2048)
                },
                loopback_queue: unsafe { &mut *(&mut self.loopback_queue as *mut _) },
            };
            return Some((rx, tx));
        }

        // Fall back to VirtIO
        if self.virtio.inner.poll_receive().is_some() {
            match unsafe { self.virtio.inner.receive_begin(&mut self.virtio.rx_buffer) } {
                Ok(token) => {
                    match unsafe {
                        self.virtio
                            .inner
                            .receive_complete(token, &mut self.virtio.rx_buffer)
                    } {
                        Ok((hdr_len, pkt_len)) => {
                            let rx = LoopbackAwareRxToken::Virtio(unsafe {
                                core::slice::from_raw_parts_mut(
                                    self.virtio.rx_buffer.as_mut_ptr().add(hdr_len),
                                    pkt_len,
                                )
                            });
                            let tx = LoopbackAwareTxToken {
                                virtio_inner: unsafe { &mut *(&mut self.virtio.inner as *mut _) },
                                virtio_buffer: unsafe {
                                    core::slice::from_raw_parts_mut(
                                        self.virtio.tx_buffer.as_mut_ptr(),
                                        2048,
                                    )
                                },
                                loopback_queue: unsafe {
                                    &mut *(&mut self.loopback_queue as *mut _)
                                },
                            };
                            return Some((rx, tx));
                        }
                        Err(_) => return None,
                    }
                }
                Err(_) => return None,
            }
        }
        None
    }

    fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
        Some(LoopbackAwareTxToken {
            virtio_inner: unsafe { &mut *(&mut self.virtio.inner as *mut _) },
            virtio_buffer: unsafe {
                core::slice::from_raw_parts_mut(self.virtio.tx_buffer.as_mut_ptr(), 2048)
            },
            loopback_queue: unsafe { &mut *(&mut self.loopback_queue as *mut _) },
        })
    }

    fn capabilities(&self) -> smoltcp::phy::DeviceCapabilities {
        self.virtio.capabilities()
    }
}

pub enum LoopbackAwareRxToken<'a> {
    /// An owned frame that was looped back internally.
    Loopback(Vec<u8>),
    /// A borrowed frame received from VirtIO.
    Virtio(&'a mut [u8]),
}

impl<'a> smoltcp::phy::RxToken for LoopbackAwareRxToken<'a> {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        match self {
            Self::Loopback(buf) => f(&buf),
            Self::Virtio(buf) => f(buf),
        }
    }
}

pub struct LoopbackAwareTxToken<'a> {
    virtio_inner: &'a mut VirtIONetRaw<VirtioHal, MmioTransport, 16>,
    virtio_buffer: &'a mut [u8],
    loopback_queue: &'a mut VecDeque<Vec<u8>>,
}

impl<'a> smoltcp::phy::TxToken for LoopbackAwareTxToken<'a> {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        // Write the frame into the VirtIO tx buffer (avoids allocation for external traffic)
        let res = f(&mut self.virtio_buffer[..len]);

        if is_loopback_frame(&self.virtio_buffer[..len]) {
            // Loopback: copy into an owned Vec and queue for the next receive()
            let mut frame = vec![0u8; len];
            frame.copy_from_slice(&self.virtio_buffer[..len]);
            self.loopback_queue.push_back(frame);
        } else {
            // External: send through VirtIO
            let _ = self.virtio_inner.send(&self.virtio_buffer[..len]);
        }

        res
    }
}

// ============================================================================
// Initialization
// ============================================================================

pub fn init() -> Result<(), &'static str> {
    log("[SmolNet] Initializing network stack...\n");

    let mut found_device: Option<VirtIONetRaw<VirtioHal, MmioTransport, 16>> = None;

    for (i, &addr) in VIRTIO_MMIO_ADDRS.iter().enumerate() {
        let device_id = unsafe { core::ptr::read_volatile((addr + 0x008) as *const u32) };
        if device_id != 1 { continue; }

        log("[SmolNet] Found virtio-net at slot ");
        crate::safe_print!(32, "{}\n", i);

        let header_ptr = match core::ptr::NonNull::new(addr as *mut VirtIOHeader) {
            Some(p) => p,
            None => continue,
        };

        let transport = unsafe { MmioTransport::new(header_ptr) }.map_err(|_| "Transport init failed")?;
        
        match VirtIONetRaw::new(transport) {
            Ok(dev) => {
                found_device = Some(dev);
                break;
            }
            Err(_) => continue,
        }
    }

    let mut device = LoopbackAwareDevice::new(
        VirtioSmoltcpDevice::new(found_device.ok_or("No virtio-net device found")?)
    );
    let mac = device.mac_address();
    log("[SmolNet] MAC: ");
    for (i, b) in mac.iter().enumerate() {
        if i > 0 { console::print(":"); }
        crate::safe_print!(32, "{:02x}", b);
    }
    log("\n");

    let timestamp = Instant::from_micros(crate::timer::uptime_us() as i64);

    let mut config = Config::new(HardwareAddress::Ethernet(EthernetAddress(mac)));
    config.random_seed = crate::timer::uptime_us();
    
    let mut iface = Interface::new(config, &mut device, timestamp);
    
    // Set static IP fallback (standard QEMU user networking) + loopback.
    // Loopback works via LoopbackAwareDevice which intercepts 127.x.x.x frames
    // at the device layer and queues them internally instead of sending via VirtIO.
    iface.update_ip_addrs(|ip_addrs| {
        ip_addrs.push(IpCidr::new(IpAddress::v4(10, 0, 2, 15), 24)).unwrap();
        ip_addrs.push(IpCidr::new(IpAddress::v4(127, 0, 0, 1), 8)).unwrap();
    });
    iface.routes_mut().add_default_ipv4_route(smoltcp::wire::Ipv4Address::new(10, 0, 2, 2)).unwrap();

    let mut sockets = unsafe { SocketSet::new(&mut SOCKET_STORAGE[..]) };

    // Initialize DHCP if enabled
    let dhcp_handle = if crate::config::ENABLE_DHCP {
        log("[SmolNet] DHCP enabled\n");
        let dhcp_socket = dhcpv4::Socket::new();
        Some(sockets.add(dhcp_socket))
    } else {
        None
    };

    *NETWORK.lock() = Some(NetworkState {
        iface,
        sockets,
        device,
        dhcp_handle,
        pending_removal: Vec::new(),
    });

    NETWORK_READY.store(true, Ordering::Release);
    log("[SmolNet] Initialized successfully (VirtIO + Loopback)\n");
    Ok(())
}

// ============================================================================
// Public API
// ============================================================================

pub fn poll() -> bool {
    if let Some(net) = NETWORK.lock().as_mut() {
        let timestamp = Instant::from_micros(crate::timer::uptime_us() as i64);
        
        let p1 = net.iface.poll(timestamp, &mut net.device, &mut net.sockets);
        
        // Handle DHCP
        if let Some(handle) = net.dhcp_handle {
            let event = net.sockets.get_mut::<dhcpv4::Socket>(handle).poll();
            if let Some(event) = event {
                match event {
                    dhcpv4::Event::Configured(config) => {
                        log("[SmolNet] DHCP configured\n");
                        net.iface.update_ip_addrs(|addrs| {
                            addrs.clear();
                            addrs.push(IpCidr::Ipv4(config.address)).unwrap();
                            addrs.push(IpCidr::new(IpAddress::v4(127, 0, 0, 1), 8)).unwrap();
                        });
                        if let Some(router) = config.router {
                            let _ = net.iface.routes_mut().add_default_ipv4_route(router);
                        }
                        
                        log(&alloc::format!("[SmolNet] IP: {}\n", config.address));
                    }
                    dhcpv4::Event::Deconfigured => {
                        log("[SmolNet] DHCP deconfigured - reverting to static fallback\n");
                        net.iface.update_ip_addrs(|addrs| {
                            addrs.clear();
                            addrs.push(IpCidr::new(IpAddress::v4(10, 0, 2, 15), 24)).unwrap();
                            addrs.push(IpCidr::new(IpAddress::v4(127, 0, 0, 1), 8)).unwrap();
                        });
                        let _ = net.iface.routes_mut().add_default_ipv4_route(smoltcp::wire::Ipv4Address::new(10, 0, 2, 2));
                    }
                }
            }
        }

        // Garbage collect pending removals
        let mut i = 0;
        while i < net.pending_removal.len() {
            let handle = net.pending_removal[i];
            let should_remove = match net.sockets.get::<tcp::Socket>(handle).state() {
                tcp::State::Closed => true,
                _ => false,
            };
            
            if should_remove {
                net.sockets.remove(handle);
                net.pending_removal.swap_remove(i);
            } else {
                i += 1;
            }
        }

        if matches!(p1, PollResult::SocketStateChanged) {
            POLL_COUNT.fetch_add(1, Ordering::Release);
            return true;
        }
        false
    } else {
        false
    }
}

pub fn with_network<F, R>(f: F) -> Option<R>
where
    F: FnOnce(&mut NetworkState) -> R,
{
    let mut guard = NETWORK.lock();
    guard.as_mut().map(f)
}

// ============================================================================
// Socket API (Wrappers)
// ============================================================================

pub fn socket_create() -> Option<SocketHandle> {
    with_network(|net| {
        let rx_buffer = tcp::SocketBuffer::new(vec![0; TCP_RX_BUFFER_SIZE]);
        let tx_buffer = tcp::SocketBuffer::new(vec![0; TCP_TX_BUFFER_SIZE]);
        let mut socket = tcp::Socket::new(rx_buffer, tx_buffer);
        socket.set_nagle_enabled(false);
        net.sockets.add(socket)
    })
}

pub fn socket_close(handle: SocketHandle) {
    with_network(|net| {
        let socket = net.sockets.get_mut::<tcp::Socket>(handle);
        socket.close();
        net.pending_removal.push(handle);
    });
}

fn log(msg: &str) {
    console::print(msg);
}

// ============================================================================
// Async TCP Stream (embedded-io-async)
// ============================================================================

use core::task::Poll;

#[derive(Debug, Clone, Copy)]
pub enum TcpError {
    ReadError,
    WriteError,
}

impl embedded_io_async::Error for TcpError {
    fn kind(&self) -> embedded_io_async::ErrorKind {
        embedded_io_async::ErrorKind::Other
    }
}

pub struct TcpStream {
    handle: SocketHandle,
}

impl TcpStream {
    pub fn new(handle: SocketHandle) -> Self {
        Self { handle }
    }
}

impl embedded_io_async::ErrorType for TcpStream {
    type Error = TcpError;
}

impl embedded_io_async::Read for TcpStream {
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
        core::future::poll_fn(|cx| {
            with_network(|net| {
                let socket = net.sockets.get_mut::<tcp::Socket>(self.handle);
                if socket.can_recv() {
                    match socket.recv(|data| {
                        let len = data.len().min(buf.len());
                        buf[..len].copy_from_slice(&data[..len]);
                        (len, len)
                    }) {
                        Ok(n) => Poll::Ready(Ok(n)),
                        Err(_) => Poll::Ready(Err(TcpError::ReadError)),
                    }
                } else if socket.state() == tcp::State::Closed || socket.state() == tcp::State::CloseWait {
                    Poll::Ready(Ok(0)) // EOF
                } else {
                    socket.register_recv_waker(cx.waker());
                    Poll::Pending
                }
            }).unwrap_or(Poll::Ready(Err(TcpError::ReadError)))
        }).await
    }
}

impl embedded_io_async::Write for TcpStream {
    async fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
        core::future::poll_fn(|cx| {
            with_network(|net| {
                let socket = net.sockets.get_mut::<tcp::Socket>(self.handle);
                if socket.can_send() {
                    match socket.send_slice(buf) {
                        Ok(n) => Poll::Ready(Ok(n)),
                        Err(_) => Poll::Ready(Err(TcpError::WriteError)),
                    }
                } else if socket.state() == tcp::State::Closed || socket.state() == tcp::State::CloseWait {
                    Poll::Ready(Err(TcpError::WriteError)) // Broken pipe
                } else {
                    socket.register_send_waker(cx.waker());
                    Poll::Pending
                }
            }).unwrap_or(Poll::Ready(Err(TcpError::WriteError)))
        }).await
    }
    
    async fn flush(&mut self) -> Result<(), Self::Error> {
        core::future::poll_fn(|cx| {
            with_network(|net| {
                let socket = net.sockets.get_mut::<tcp::Socket>(self.handle);
                if socket.send_queue() == 0 {
                    Poll::Ready(Ok(()))
                } else if socket.state() == tcp::State::Closed || socket.state() == tcp::State::CloseWait {
                    Poll::Ready(Err(TcpError::WriteError))
                } else {
                    socket.register_send_waker(cx.waker());
                    Poll::Pending
                }
            }).unwrap_or(Poll::Ready(Err(TcpError::WriteError)))
        }).await
    }
}
