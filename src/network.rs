use alloc::vec::Vec;
use smoltcp::iface::{Config, Interface, SocketSet};
use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::socket::{tcp, udp};
use smoltcp::time::Instant;
use smoltcp::wire::{EthernetAddress, IpAddress, IpCidr};
use spinning_top::Spinlock;

// Dummy network device for now
pub struct DummyDevice {
    rx_buffer: Vec<u8>,
    tx_buffer: Vec<u8>,
}

impl DummyDevice {
    pub fn new() -> Self {
        Self {
            rx_buffer: Vec::new(),
            tx_buffer: Vec::new(),
        }
    }
}

pub struct DummyRxToken {
    buffer: Vec<u8>,
}

pub struct DummyTxToken;

impl RxToken for DummyRxToken {
    fn consume<R, F>(mut self, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        f(&mut self.buffer)
    }
}

impl TxToken for DummyTxToken {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut buffer = alloc::vec![0u8; len];
        f(&mut buffer)
    }
}

impl Device for DummyDevice {
    type RxToken<'a> = DummyRxToken;
    type TxToken<'a> = DummyTxToken;

    fn receive(&mut self, _timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        if !self.rx_buffer.is_empty() {
            let buffer = core::mem::take(&mut self.rx_buffer);
            Some((DummyRxToken { buffer }, DummyTxToken))
        } else {
            None
        }
    }

    fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
        Some(DummyTxToken)
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.max_transmission_unit = 1500;
        caps.medium = Medium::Ethernet;
        caps
    }
}

pub struct NetworkStack {
    device: DummyDevice,
    interface: Interface,
    sockets: SocketSet<'static>,
}

static NETWORK: Spinlock<Option<NetworkStack>> = Spinlock::new(None);

impl NetworkStack {
    pub fn new() -> Self {
        let device = DummyDevice::new();

        let config = Config::new(EthernetAddress([0x02, 0x00, 0x00, 0x00, 0x00, 0x01]).into());
        let mut interface = Interface::new(config, &mut DummyDevice::new(), Instant::ZERO);

        // Configure IP address
        interface.update_ip_addrs(|ip_addrs| {
            ip_addrs
                .push(IpCidr::new(IpAddress::v4(192, 168, 1, 100), 24))
                .unwrap();
        });

        let sockets = SocketSet::new(Vec::new());

        Self {
            device,
            interface,
            sockets,
        }
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

pub fn init() {
    let mut network = NETWORK.lock();
    *network = Some(NetworkStack::new());
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
