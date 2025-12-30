//! Async Network Stack using Embassy-Net
//!
//! Provides async TCP networking with:
//! - Network stack initialization with virtio driver
//! - Loopback stack for localhost/127.0.0.1 connections
//! - DHCP support with fallback to static config
//! - Async TCP listener for accepting connections
//! - Async TCP stream for reading/writing

use alloc::boxed::Box;
use alloc::vec;
use core::cell::UnsafeCell;
use embassy_net::tcp::TcpSocket;
use embassy_net::{
    Config, ConfigV4, DhcpConfig, Ipv4Address, Ipv4Cidr, Runner, Stack, StackResources,
    StaticConfigV4,
};
use embassy_time::Duration;
use virtio_drivers::device::net::VirtIONetRaw;
use virtio_drivers::transport::mmio::{MmioTransport, VirtIOHeader};

use crate::console;
use crate::embassy_net_driver::LoopbackDevice;
use crate::embassy_virtio_driver::EmbassyVirtioDriver;
use crate::virtio_hal::VirtioHal;

// ============================================================================
// Global Stack References
// ============================================================================

struct StackHolder(UnsafeCell<Option<Stack<'static>>>);
unsafe impl Sync for StackHolder {}

/// Main network stack (virtio - external network)
static GLOBAL_STACK: StackHolder = StackHolder(UnsafeCell::new(None));

/// Loopback network stack (127.0.0.1)
static LOOPBACK_STACK: StackHolder = StackHolder(UnsafeCell::new(None));

/// Store the main stack reference for global access
pub fn set_global_stack(stack: Stack<'static>) {
    unsafe {
        *GLOBAL_STACK.0.get() = Some(stack);
    }
}

/// Get a copy of the main stack reference
pub fn get_global_stack() -> Option<Stack<'static>> {
    unsafe { (*GLOBAL_STACK.0.get()).clone() }
}

/// Store the loopback stack reference for global access
pub fn set_loopback_stack(stack: Stack<'static>) {
    unsafe {
        *LOOPBACK_STACK.0.get() = Some(stack);
    }
}

/// Get a copy of the loopback stack reference
pub fn get_loopback_stack() -> Option<Stack<'static>> {
    unsafe { (*LOOPBACK_STACK.0.get()).clone() }
}

/// Check if an address is a loopback address
pub fn is_loopback_address(host: &str) -> bool {
    host == "localhost" || host == "127.0.0.1"
}

/// Get the appropriate stack for a given host
/// Returns loopback stack for localhost/127.0.0.1, main stack otherwise
pub fn get_stack_for_host(host: &str) -> Option<Stack<'static>> {
    if is_loopback_address(host) {
        get_loopback_stack()
    } else {
        get_global_stack()
    }
}

// ============================================================================
// Constants
// ============================================================================

/// Maximum concurrent connections (sockets in pool)
const MAX_SOCKETS: usize = 16;

/// TCP buffer sizes
const TCP_RX_BUFFER_SIZE: usize = 4096;
const TCP_TX_BUFFER_SIZE: usize = 4096;

/// QEMU virt machine virtio MMIO addresses
const VIRTIO_MMIO_ADDRS: [usize; 8] = [
    0x0a000000, 0x0a000200, 0x0a000400, 0x0a000600, 0x0a000800, 0x0a000a00, 0x0a000c00, 0x0a000e00,
];

/// DHCP timeout before falling back to static config (seconds)
const DHCP_TIMEOUT_SECS: u64 = 5;

/// Default QEMU user-mode networking addresses (fallback)
const DEFAULT_IP: Ipv4Address = Ipv4Address::new(10, 0, 2, 15);
const DEFAULT_GATEWAY: Ipv4Address = Ipv4Address::new(10, 0, 2, 2);
const DEFAULT_PREFIX_LEN: u8 = 24;

// ============================================================================
// Network Stack
// ============================================================================

/// Loopback network initialization result
pub struct LoopbackInit {
    pub stack: Stack<'static>,
    pub runner: Runner<'static, LoopbackDevice>,
}

/// Network initialization result containing stack and runner
pub struct NetworkInit {
    pub stack: Stack<'static>,
    pub runner: Runner<'static, EmbassyVirtioDriver>,
    pub loopback: LoopbackInit,
}

/// Initialize the loopback network stack with 127.0.0.1
fn init_loopback() -> LoopbackInit {
    log("[AsyncNet] Initializing loopback interface (127.0.0.1)...\n");

    // Create loopback device
    let device = LoopbackDevice::new();

    // Create static storage for the loopback resources
    let resources_box = Box::new(StackResources::<8>::new());
    let resources_ref: &'static mut StackResources<8> = Box::leak(resources_box);

    // Configure with 127.0.0.1
    let static_config = StaticConfigV4 {
        address: Ipv4Cidr::new(Ipv4Address::new(127, 0, 0, 1), 8),
        gateway: None,
        dns_servers: Default::default(),
    };
    let config = Config::ipv4_static(static_config);

    // Random seed
    let seed = crate::timer::uptime_us() + 1;

    // Create the loopback stack
    let (stack, runner) = embassy_net::new(device, config, resources_ref, seed);

    log("[AsyncNet] Loopback interface ready: 127.0.0.1/8\n");

    LoopbackInit { stack, runner }
}

/// Initialize the async network stack
/// Returns the stack and runner on success
pub fn init() -> Result<NetworkInit, &'static str> {
    log("[AsyncNet] Initializing async network stack...\n");

    // Initialize loopback first
    let loopback = init_loopback();

    // Find virtio-net device
    let mut found_device: Option<EmbassyVirtioDriver> = None;

    for (i, &addr) in VIRTIO_MMIO_ADDRS.iter().enumerate() {
        // SAFETY: Reading from MMIO registers at known QEMU virt machine addresses
        let device_id = unsafe { core::ptr::read_volatile((addr + 0x008) as *const u32) };
        if device_id != 1 {
            continue;
        }

        log("[AsyncNet] Found virtio-net at slot ");
        console::print(&alloc::format!("{}\n", i));

        let header_ptr = match core::ptr::NonNull::new(addr as *mut VirtIOHeader) {
            Some(p) => p,
            None => continue,
        };

        // SAFETY: Creating MmioTransport for verified virtio device
        let transport = match unsafe { MmioTransport::new(header_ptr) } {
            Ok(t) => t,
            Err(_) => {
                log("[AsyncNet] Failed to create transport\n");
                continue;
            }
        };

        let net = match VirtIONetRaw::<VirtioHal, MmioTransport, 16>::new(transport) {
            Ok(n) => n,
            Err(_) => {
                log("[AsyncNet] Failed to init virtio device\n");
                continue;
            }
        };

        found_device = Some(EmbassyVirtioDriver::new(net));
        break;
    }

    let device = found_device.ok_or("No virtio-net device found")?;

    // Log MAC address
    let mac = device.mac_address();
    log("[AsyncNet] MAC: ");
    for (i, b) in mac.iter().enumerate() {
        if i > 0 {
            console::print(":");
        }
        console::print(&alloc::format!("{:02x}", b));
    }
    log("\n");

    // Create static storage for the network resources
    // These are leaked to get 'static lifetimes
    let resources_box = Box::new(StackResources::<MAX_SOCKETS>::new());
    let resources_ref: &'static mut StackResources<MAX_SOCKETS> = Box::leak(resources_box);

    // Random seed from timer
    let seed = crate::timer::uptime_us();

    // Use DHCP to get network configuration
    log("[AsyncNet] Using DHCP for network configuration...\n");
    let config = Config::dhcpv4(DhcpConfig::default());

    // Create the stack - returns (Stack, Runner)
    let (stack, runner) = embassy_net::new(device, config, resources_ref, seed);

    log("[AsyncNet] Network stack created, waiting for IP...\n");

    Ok(NetworkInit {
        stack,
        runner,
        loopback,
    })
}

/// Wait for network to get an IP address via DHCP
/// Falls back to static QEMU defaults if DHCP times out
pub async fn wait_for_ip(stack: &Stack<'static>) {
    log("[AsyncNet] Waiting for DHCP (");
    console::print(&alloc::format!("{}s timeout)...\n", DHCP_TIMEOUT_SECS));

    let deadline = embassy_time::Instant::now() + Duration::from_secs(DHCP_TIMEOUT_SECS);

    loop {
        // Check if we got an IP via DHCP
        if let Some(config) = stack.config_v4() {
            let addr = config.address;
            log("[AsyncNet] Got IP via DHCP: ");
            console::print(&alloc::format!(
                "{}.{}.{}.{}/{}\n",
                addr.address().octets()[0],
                addr.address().octets()[1],
                addr.address().octets()[2],
                addr.address().octets()[3],
                addr.prefix_len()
            ));
            if let Some(gw) = config.gateway {
                log("[AsyncNet] Gateway: ");
                console::print(&alloc::format!(
                    "{}.{}.{}.{}\n",
                    gw.octets()[0],
                    gw.octets()[1],
                    gw.octets()[2],
                    gw.octets()[3]
                ));
            }
            // Log DNS servers
            if config.dns_servers.is_empty() {
                log("[AsyncNet] WARNING: No DNS servers provided by DHCP\n");
            } else {
                log("[AsyncNet] DNS Servers: ");
                for (i, dns) in config.dns_servers.iter().enumerate() {
                    if i > 0 {
                        console::print(", ");
                    }
                    console::print(&alloc::format!(
                        "{}.{}.{}.{}",
                        dns.octets()[0],
                        dns.octets()[1],
                        dns.octets()[2],
                        dns.octets()[3]
                    ));
                }
                log("\n");
            }
            return;
        }

        // Check for timeout
        if embassy_time::Instant::now() >= deadline {
            log("[AsyncNet] DHCP timeout, using static fallback...\n");

            // Apply static configuration
            let static_config = StaticConfigV4 {
                address: Ipv4Cidr::new(DEFAULT_IP, DEFAULT_PREFIX_LEN),
                gateway: Some(DEFAULT_GATEWAY),
                dns_servers: Default::default(),
            };
            stack.set_config_v4(ConfigV4::Static(static_config));

            log("[AsyncNet] Static IP: ");
            console::print(&alloc::format!(
                "{}.{}.{}.{}/{}, Gateway: {}.{}.{}.{}\n",
                DEFAULT_IP.octets()[0],
                DEFAULT_IP.octets()[1],
                DEFAULT_IP.octets()[2],
                DEFAULT_IP.octets()[3],
                DEFAULT_PREFIX_LEN,
                DEFAULT_GATEWAY.octets()[0],
                DEFAULT_GATEWAY.octets()[1],
                DEFAULT_GATEWAY.octets()[2],
                DEFAULT_GATEWAY.octets()[3]
            ));
            log("[AsyncNet] WARNING: No DNS servers configured (static fallback)\n");
            return;
        }

        // Wait a bit before checking again
        embassy_time::Timer::after(Duration::from_millis(100)).await;
    }
}

// ============================================================================
// Async TCP Listener (uses Box::leak - for non-high-frequency accept)
// ============================================================================

/// Async TCP listener for accepting connections
pub struct TcpListener {
    port: u16,
    stack: Stack<'static>,
}

impl TcpListener {
    /// Create a new TCP listener on the given port
    pub fn new(stack: Stack<'static>, port: u16) -> Self {
        Self { port, stack }
    }

    /// Accept a new connection
    /// Note: This leaks buffers - use sparingly
    pub async fn accept(&self) -> Result<TcpStream, TcpError> {
        let rx_buffer = vec![0u8; TCP_RX_BUFFER_SIZE].into_boxed_slice();
        let tx_buffer = vec![0u8; TCP_TX_BUFFER_SIZE].into_boxed_slice();
        let rx_ref: &'static mut [u8] = Box::leak(rx_buffer);
        let tx_ref: &'static mut [u8] = Box::leak(tx_buffer);

        let mut socket = TcpSocket::new(self.stack, rx_ref, tx_ref);
        socket.set_timeout(Some(Duration::from_secs(60)));

        socket
            .accept(self.port)
            .await
            .map_err(|_| TcpError::AcceptFailed)?;

        Ok(TcpStream { socket })
    }
}

// ============================================================================
// Async TCP Stream
// ============================================================================

/// Async TCP stream for reading and writing
pub struct TcpStream {
    socket: TcpSocket<'static>,
}

impl TcpStream {
    /// Create a TcpStream from an already-connected socket
    pub fn from_socket(socket: TcpSocket<'static>) -> Self {
        Self { socket }
    }

    /// Read data from the stream
    pub async fn read(&mut self, buf: &mut [u8]) -> Result<usize, TcpError> {
        let result = self
            .socket
            .read(buf)
            .await
            .map_err(|_| TcpError::ReadFailed);

        if let Ok(n) = &result {
            crate::network::add_bytes_rx(*n as u64);
        }

        result
    }

    /// Write data to the stream
    pub async fn write(&mut self, data: &[u8]) -> Result<usize, TcpError> {
        let result = self
            .socket
            .write(data)
            .await
            .map_err(|_| TcpError::WriteFailed);

        if let Ok(n) = &result {
            crate::network::add_bytes_tx(*n as u64);
        }

        result
    }

    /// Write all data to the stream
    pub async fn write_all(&mut self, data: &[u8]) -> Result<(), TcpError> {
        let mut offset = 0;
        while offset < data.len() {
            let n = self.write(&data[offset..]).await?;
            if n == 0 {
                return Err(TcpError::WriteFailed);
            }
            offset += n;
        }
        Ok(())
    }

    /// Flush the stream
    pub async fn flush(&mut self) -> Result<(), TcpError> {
        self.socket.flush().await.map_err(|_| TcpError::FlushFailed)
    }

    /// Close the connection
    pub fn close(&mut self) {
        self.socket.close();
    }

    /// Check if the socket can receive data
    pub fn may_recv(&self) -> bool {
        self.socket.may_recv()
    }

    /// Check if the socket can send data
    pub fn may_send(&self) -> bool {
        self.socket.may_send()
    }

    /// Get the local endpoint
    pub fn local_endpoint(&self) -> Option<embassy_net::IpEndpoint> {
        self.socket.local_endpoint()
    }

    /// Get the remote endpoint
    pub fn remote_endpoint(&self) -> Option<embassy_net::IpEndpoint> {
        self.socket.remote_endpoint()
    }
}

// ============================================================================
// Error Types
// ============================================================================

/// TCP error type
#[derive(Debug, Clone, Copy)]
pub enum TcpError {
    AcceptFailed,
    ReadFailed,
    WriteFailed,
    FlushFailed,
    ConnectionClosed,
}

// ============================================================================
// Logging
// ============================================================================

fn log(msg: &str) {
    console::print(msg);
}
