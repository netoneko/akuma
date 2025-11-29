use alloc::vec::Vec;
use smoltcp::iface::{Config, Interface, SocketSet};
use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::socket::{tcp, udp};
use smoltcp::time::Instant;
use smoltcp::wire::EthernetAddress;
use spinning_top::Spinlock;
use virtio_drivers::device::net::VirtIONetRaw;
use virtio_drivers::transport::Transport;
use virtio_drivers::transport::mmio::{MmioTransport, VirtIOHeader};

use crate::virtio_hal::VirtioHal;

// Virtio-net device wrapper - using raw driver for polling mode
pub struct VirtioNetDevice {
    inner: VirtIONetRaw<VirtioHal, MmioTransport, 16>,
    rx_buffer: Vec<u8>,
}

pub struct VirtioRxToken {
    buffer: Vec<u8>,
}

pub struct VirtioTxToken<'a> {
    device: &'a mut VirtioNetDevice,
}

impl RxToken for VirtioRxToken {
    fn consume<R, F>(mut self, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        f(&mut self.buffer)
    }
}

impl<'a> TxToken for VirtioTxToken<'a> {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut buffer = alloc::vec![0u8; len];
        let result = f(&mut buffer);

        // Send using VirtIONetRaw API
        if let Err(_e) = self.device.inner.send(&buffer) {
            crate::console::print("[Net] TX error\n");
        }

        result
    }
}

impl Device for VirtioNetDevice {
    type RxToken<'a> = VirtioRxToken;
    type TxToken<'a> = VirtioTxToken<'a>;

    fn receive(&mut self, _timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        // VirtIONetRaw doesn't have high-level receive() method
        // For now, return None (no packets available)
        // TODO: Implement proper polling with receive_begin/receive_complete
        None
    }

    fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
        // Check if we can transmit
        if self.inner.can_send() {
            Some(VirtioTxToken { device: self })
        } else {
            None
        }
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.max_transmission_unit = 1500;
        caps.medium = Medium::Ethernet;
        caps
    }
}

pub struct NetworkStack {
    device: VirtioNetDevice,
    interface: Interface,
    sockets: SocketSet<'static>,
}

static NETWORK: Spinlock<Option<NetworkStack>> = Spinlock::new(None);

impl NetworkStack {
    pub fn new(dtb_ptr: usize) -> Result<Self, &'static str> {
        // Find virtio-mmio devices from device tree
        let virtio_addrs = find_virtio_mmio_devices(dtb_ptr)?;

        // Try each virtio device we found
        for (idx, addr) in virtio_addrs.iter().enumerate() {
            let addr = *addr;
            unsafe {
                // Read version and device_id
                let device_id = core::ptr::read_volatile((addr + 0x008) as *const u32);

                // Skip empty slots
                if device_id == 0 {
                    continue;
                }

                // Create VirtIOHeader pointer
                let header_ptr = core::ptr::NonNull::new_unchecked(addr as *mut VirtIOHeader);

                // Try to create MMIO transport
                match MmioTransport::new(header_ptr) {
                    Ok(transport) => {
                        let device_type = transport.device_type();

                        // Check if it's virtio-net using the enum
                        use virtio_drivers::transport::DeviceType;
                        if matches!(device_type, DeviceType::Network) {
                            // Register IRQ handler for this virtio device
                            // QEMU virt machine maps virtio-mmio devices to IRQ 16+ (0x10+)
                            let irq = 16 + idx as u32;
                            crate::irq::register_handler(irq, virtio_irq_handler);

                            // Use VirtIONetRaw which doesn't pre-allocate RX buffers
                            let net_device =
                                VirtIONetRaw::<VirtioHal, MmioTransport, 16>::new(transport)
                                    .map_err(|_| "Failed to initialize virtio-net")?;

                            let mac = net_device.mac_address();

                            // Try different allocation strategies
                            crate::console::print(
                                "Allocating RX buffer with Vec::new + resize...\n",
                            );
                            let mut rx_buffer = Vec::new();
                            rx_buffer.resize(4096, 0u8);
                            crate::console::print("RX buffer allocated\n");

                            let _device = VirtioNetDevice {
                                inner: net_device,
                                rx_buffer,
                            };

                            // NOTE: Cannot use smoltcp - Interface::new() hangs trying to send packets
                            // which triggers add_notify_wait_pop() -> spin_loop() in virtio-drivers
                            //
                            // To enable networking, need either:
                            // 1. Preemptive threading (so spin_loop yields CPU to QEMU)
                            // 2. Custom async virtio driver (use .await instead of spin_loop)
                            // 3. Manual packet handling without smoltcp

                            return Err(
                                "VirtIO-net device detected but TCP/IP stack requires preemptive threading",
                            );
                        }
                    }
                    Err(_) => {
                        // Not a valid virtio device at this address, continue
                    }
                }
            }
        }

        Err("No virtio-net device found")
    }

    pub fn poll(&mut self) {
        let timestamp = Instant::from_millis(crate::timer::get_time_us() as i64 / 1000);
        self.interface
            .poll(timestamp, &mut self.device, &mut self.sockets);
    }

    pub fn add_tcp_socket(&mut self) -> tcp::Socket<'static> {
        let rx_buffer = tcp::SocketBuffer::new(alloc::vec![0; 4096]);
        let tx_buffer = tcp::SocketBuffer::new(alloc::vec![0; 4096]);
        tcp::Socket::new(rx_buffer, tx_buffer)
    }

    pub fn add_udp_socket(&mut self) -> udp::Socket<'static> {
        let rx_buffer = udp::PacketBuffer::new(
            alloc::vec![udp::PacketMetadata::EMPTY; 4],
            alloc::vec![0; 4096],
        );
        let tx_buffer = udp::PacketBuffer::new(
            alloc::vec![udp::PacketMetadata::EMPTY; 4],
            alloc::vec![0; 4096],
        );
        udp::Socket::new(rx_buffer, tx_buffer)
    }
}

pub async fn init(dtb_ptr: usize) -> Result<(), &'static str> {
    let mut network = NETWORK.lock();
    match NetworkStack::new(dtb_ptr) {
        Ok(stack) => {
            *network = Some(stack);
            Ok(())
        }
        Err(e) => Err(e),
    }
}

// Find virtio-mmio devices - use hardcoded addresses for QEMU virt machine
fn find_virtio_mmio_devices(_dtb_addr: usize) -> Result<Vec<usize>, &'static str> {
    // These are the standard virtio-mmio addresses for QEMU ARM64 virt machine
    let mut addresses = Vec::new();
    addresses.push(0xa000000usize);
    addresses.push(0xa000200);
    addresses.push(0xa000400);
    addresses.push(0xa000600);
    addresses.push(0xa000800);
    addresses.push(0xa000a00);
    addresses.push(0xa000c00);
    addresses.push(0xa000e00);

    Ok(addresses)
}

pub fn poll() {
    let mut network = NETWORK.lock();
    if let Some(stack) = network.as_mut() {
        stack.poll();
    }
}

pub fn with_network<F, R>(f: F) -> Option<R>
where
    F: FnOnce(&mut NetworkStack) -> R,
{
    let mut network = NETWORK.lock();
    network.as_mut().map(f)
}

// Virtio IRQ handler
fn virtio_irq_handler(_irq: u32) {
    // The virtio-drivers crate handles the actual interrupt processing
    // when we call methods like recv() or send()
    // For now, just acknowledge the interrupt (already done by IRQ infrastructure)
}
