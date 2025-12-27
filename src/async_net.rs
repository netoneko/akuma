//! Async Network Stack using Embassy-Net
//!
//! Provides async TCP networking with:
//! - Network stack initialization with virtio driver
//! - Async TCP listener for accepting connections
//! - Async TCP stream for reading/writing

use alloc::boxed::Box;
use alloc::vec;
use embassy_net::tcp::TcpSocket;
use embassy_net::{Config, Ipv4Address, Ipv4Cidr, Runner, Stack, StackResources, StaticConfigV4};
use embassy_time::Duration;
use virtio_drivers::device::net::VirtIONetRaw;
use virtio_drivers::transport::mmio::{MmioTransport, VirtIOHeader};

use crate::console;
use crate::embassy_virtio_driver::EmbassyVirtioDriver;
use crate::virtio_hal::VirtioHal;

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

// ============================================================================
// Network Stack
// ============================================================================

/// Network initialization result containing stack and runner
pub struct NetworkInit {
    pub stack: Stack<'static>,
    pub runner: Runner<'static, EmbassyVirtioDriver>,
}

/// Initialize the async network stack
/// Returns the stack and runner on success
pub fn init() -> Result<NetworkInit, &'static str> {
    log("[AsyncNet] Initializing async network stack...\n");

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

    // Static IP configuration for QEMU user-mode networking
    let config = Config::ipv4_static(StaticConfigV4 {
        address: Ipv4Cidr::new(Ipv4Address::new(10, 0, 2, 15), 24),
        gateway: Some(Ipv4Address::new(10, 0, 2, 2)),
        dns_servers: Default::default(),
    });

    // Create the stack - returns (Stack, Runner)
    let (stack, runner) = embassy_net::new(device, config, resources_ref, seed);

    log("[AsyncNet] IP: 10.0.2.15/24, Gateway: 10.0.2.2\n");
    log("[AsyncNet] Async network stack ready\n");

    Ok(NetworkInit { stack, runner })
}

// ============================================================================
// Async TCP Listener
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
    /// Returns a TcpStream for the accepted connection
    pub async fn accept(&self) -> Result<TcpStream, TcpError> {
        // Allocate buffers for this connection
        let rx_buffer = vec![0u8; TCP_RX_BUFFER_SIZE].into_boxed_slice();
        let tx_buffer = vec![0u8; TCP_TX_BUFFER_SIZE].into_boxed_slice();

        // Leak the buffers for 'static lifetime
        let rx_ref: &'static mut [u8] = Box::leak(rx_buffer);
        let tx_ref: &'static mut [u8] = Box::leak(tx_buffer);

        // Create socket - Stack is Copy so we can clone it
        let mut socket = TcpSocket::new(self.stack, rx_ref, tx_ref);
        socket.set_timeout(Some(Duration::from_secs(60)));

        // Accept connection
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
/// Uses 'static lifetime since we leak socket buffers
pub struct TcpStream {
    socket: TcpSocket<'static>,
}

impl TcpStream {
    /// Read data from the stream
    /// Returns the number of bytes read, or 0 if connection closed
    pub async fn read(&mut self, buf: &mut [u8]) -> Result<usize, TcpError> {
        self.socket
            .read(buf)
            .await
            .map_err(|_| TcpError::ReadFailed)
    }

    /// Write data to the stream
    /// Returns the number of bytes written
    pub async fn write(&mut self, data: &[u8]) -> Result<usize, TcpError> {
        self.socket
            .write(data)
            .await
            .map_err(|_| TcpError::WriteFailed)
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
