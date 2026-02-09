//! Smoltcp Network Stack (Thread-Safe)
//!
//! Replaces embassy-net with a direct smoltcp integration protected by a kernel Spinlock.
//! This allows any thread (kernel or userspace via syscall) to drive the network stack,
//! eliminating the need for a dedicated network thread and complex preemption management.

use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use spinning_top::Spinlock;

use smoltcp::iface::{Config, Interface, SocketSet, SocketStorage, PollResult};
pub use smoltcp::iface::SocketHandle;
use smoltcp::phy::{Device, Medium, Loopback};
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
    pub loopback_iface: Interface,
    pub sockets: SocketSet<'static>,
    pub device: VirtioSmoltcpDevice,
    pub loopback_device: Loopback,
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
                        Ok((_hdr_len, pkt_len)) => {
                            let rx = VirtioRxToken {
                                buffer: unsafe { core::slice::from_raw_parts_mut(self.rx_buffer.as_mut_ptr(), pkt_len) },
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
        caps.max_transmission_unit = 1500;
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

    let mut device = VirtioSmoltcpDevice::new(found_device.ok_or("No virtio-net device found")?);
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
    
    // Set static IP fallback (standard QEMU user networking)
    iface.update_ip_addrs(|ip_addrs| {
        ip_addrs.push(IpCidr::new(IpAddress::v4(10, 0, 2, 15), 24)).unwrap();
    });
    iface.routes_mut().add_default_ipv4_route(smoltcp::wire::Ipv4Address::new(10, 0, 2, 2)).unwrap();

    // Initialize Loopback Interface
    let mut loopback_device = Loopback::new(Medium::Ethernet);
    let mut loopback_config = Config::new(HardwareAddress::Ethernet(EthernetAddress([0; 6])));
    loopback_config.random_seed = crate::timer::uptime_us() ^ 0xCAFEBABE;
    
    let mut loopback_iface = Interface::new(loopback_config, &mut loopback_device, timestamp);
    loopback_iface.update_ip_addrs(|ip_addrs| {
        ip_addrs.push(IpCidr::new(IpAddress::v4(127, 0, 0, 1), 8)).unwrap();
    });

    let mut sockets = unsafe { SocketSet::new(&mut SOCKET_STORAGE[..]) };

    // Initialize DHCP
    let dhcp_socket = dhcpv4::Socket::new();
    let dhcp_handle = sockets.add(dhcp_socket);

    *NETWORK.lock() = Some(NetworkState {
        iface,
        loopback_iface,
        sockets,
        device,
        loopback_device,
        dhcp_handle: Some(dhcp_handle),
        pending_removal: Vec::new(),
    });

    NETWORK_READY.store(true, Ordering::Release);
    log("[SmolNet] Initialized successfully (Static IP 10.0.2.15 + DHCP started)\n");
    Ok(())
}

// ============================================================================
// Public API
// ============================================================================

pub fn poll() -> bool {
    if let Some(net) = NETWORK.lock().as_mut() {
        let timestamp = Instant::from_micros(crate::timer::uptime_us() as i64);
        
        let p1 = net.iface.poll(timestamp, &mut net.device, &mut net.sockets);
        let p2 = net.loopback_iface.poll(timestamp, &mut net.loopback_device, &mut net.sockets);
        
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
                        });
                        if let Some(router) = config.router {
                            net.iface.routes_mut().add_default_ipv4_route(router).unwrap();
                        }
                        
                        log(&alloc::format!("[SmolNet] IP: {}\n", config.address));
                    }
                    dhcpv4::Event::Deconfigured => {
                        log("[SmolNet] DHCP deconfigured - reverting to static fallback\n");
                        net.iface.update_ip_addrs(|addrs| {
                            addrs.clear();
                            addrs.push(IpCidr::new(IpAddress::v4(10, 0, 2, 15), 24)).unwrap();
                        });
                        net.iface.routes_mut().add_default_ipv4_route(smoltcp::wire::Ipv4Address::new(10, 0, 2, 2)).unwrap();
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
                // Don't increment i
            } else {
                i += 1;
            }
        }

        if matches!(p1, PollResult::SocketStateChanged) || matches!(p2, PollResult::SocketStateChanged) {
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
        // Set Nagle's off for better interactive performance
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
