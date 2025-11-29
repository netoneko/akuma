// Network stack using smoltcp WITHOUT alloc feature (static buffers only)
use smoltcp::iface::{Config, Interface};
use virtio_drivers::transport::Transport;
use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::socket::{tcp, udp};
use smoltcp::time::Instant;
use smoltcp::wire::{EthernetAddress, IpCidr, IpAddress, HardwareAddress};
use spinning_top::Spinlock;
use virtio_drivers::device::net::VirtIONetRaw;
use virtio_drivers::transport::mmio::{MmioTransport, VirtIOHeader};

use crate::virtio_hal::VirtioHal;

// Static buffers for smoltcp
static mut RX_BUFFER: [u8; 4096] = [0; 4096];
static mut TX_BUFFER: [u8; 4096] = [0; 4096];

// Virtio-net device wrapper
pub struct VirtioNetDevice {
    inner: VirtIONetRaw<VirtioHal, MmioTransport, 16>,
}

pub struct VirtioRxToken {
    len: usize,
}

pub struct VirtioTxToken<'a> {
    device: &'a mut VirtioNetDevice,
}

impl RxToken for VirtioRxToken {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        unsafe { f(&mut RX_BUFFER[..self.len]) }
    }
}

impl<'a> TxToken for VirtioTxToken<'a> {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        unsafe {
            let result = f(&mut TX_BUFFER[..len]);
            // Actually send
            if let Err(_) = self.device.inner.send(&TX_BUFFER[..len]) {
                // TX error, ignore
            }
            result
        }
    }
}

impl Device for VirtioNetDevice {
    type RxToken<'a> = VirtioRxToken;
    type TxToken<'a> = VirtioTxToken<'a>;

    fn receive(&mut self, _timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        // Polling mode - always return None (no packet waiting)
        None
    }

    fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
        // Return None to prevent transmit during Interface::new()
        None
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.max_transmission_unit = 1500;
        caps.medium = Medium::Ethernet;
        caps
    }
}

fn find_virtio_mmio_devices(_dtb_ptr: usize) -> Result<[usize; 8], &'static str> {
    // QEMU virt machine has 8 virtio-mmio slots at these addresses
    Ok([
        0x0a000000, 0x0a000200, 0x0a000400, 0x0a000600,
        0x0a000800, 0x0a000a00, 0x0a000c00, 0x0a000e00,
    ])
}

fn virtio_irq_handler(_irq: u32) {
    // Handle virtio interrupt (placeholder)
}

pub fn init(_dtb_ptr: usize) -> Result<(), &'static str> {
    unsafe {
        const UART: *mut u8 = 0x0900_0000 as *mut u8;
        for c in b"[Net] init...\n" { UART.write_volatile(*c); }
    }
    
    // First test: smoltcp with dummy device (no virtio)
    unsafe {
        const UART: *mut u8 = 0x0900_0000 as *mut u8;
        for c in b"[Net] Testing dummy dev...\n" { UART.write_volatile(*c); }
    }
    
    {
        // Create minimal dummy device
        struct DummyDevice;
        struct DummyRx;
        struct DummyTx;
        
        impl RxToken for DummyRx {
            fn consume<R, F>(self, f: F) -> R where F: FnOnce(&mut [u8]) -> R {
                static mut BUF: [u8; 64] = [0; 64];
                unsafe { f(core::ptr::addr_of_mut!(BUF).as_mut().unwrap()) }
            }
        }
        
        impl TxToken for DummyTx {
            fn consume<R, F>(self, _len: usize, f: F) -> R where F: FnOnce(&mut [u8]) -> R {
                static mut BUF: [u8; 1500] = [0; 1500];
                unsafe { f(core::ptr::addr_of_mut!(BUF).as_mut().unwrap()) }
            }
        }
        
        impl Device for DummyDevice {
            type RxToken<'a> = DummyRx;
            type TxToken<'a> = DummyTx;
            
            fn receive(&mut self, _ts: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
                None
            }
            
            fn transmit(&mut self, _ts: Instant) -> Option<Self::TxToken<'_>> {
                None
            }
            
            fn capabilities(&self) -> DeviceCapabilities {
                let mut caps = DeviceCapabilities::default();
                caps.max_transmission_unit = 1500;
                caps.medium = Medium::Ethernet;
                caps
            }
        }
        
        let mut dummy = DummyDevice;
        
        unsafe {
            const UART: *mut u8 = 0x0900_0000 as *mut u8;
            for c in b"[Net] Dummy created\n" { UART.write_volatile(*c); }
        }
        
        let hw_addr = EthernetAddress::from_bytes(&[0x02, 0x00, 0x00, 0x00, 0x00, 0x01]);
        let config = Config::new(HardwareAddress::Ethernet(hw_addr));
        
        unsafe {
            const UART: *mut u8 = 0x0900_0000 as *mut u8;
            for c in b"[Net] Interface::new...\n" { UART.write_volatile(*c); }
        }
        
        let _iface = Interface::new(config, &mut dummy, Instant::ZERO);
        
        unsafe {
            const UART: *mut u8 = 0x0900_0000 as *mut u8;
            for c in b"[Net] Dummy OK!\n" { UART.write_volatile(*c); }
        }
    }
    
    // Dummy device works! smoltcp no-alloc is functional
    // Skip virtio for now - VirtIONetRaw::new() hangs
    unsafe {
        const UART: *mut u8 = 0x0900_0000 as *mut u8;
        for c in b"[Net] smoltcp OK!\n" { UART.write_volatile(*c); }
        for c in b"[Net] (virtio disabled)\n" { UART.write_volatile(*c); }
    }
    return Err("smoltcp OK, virtio disabled");
    
    #[allow(unreachable_code)]
    let virtio_addrs = find_virtio_mmio_devices(0)?;
    
    for (idx, addr) in virtio_addrs.iter().enumerate() {
        let addr = *addr;
        unsafe {
            let device_id = core::ptr::read_volatile((addr + 0x008) as *const u32);
            if device_id == 0 {
                continue;
            }
            
            // Log device found
            {
                const UART: *mut u8 = 0x0900_0000 as *mut u8;
                for c in b"[Net] dev " { UART.write_volatile(*c); }
                UART.write_volatile(b'0' + (device_id as u8));
                UART.write_volatile(b'\n');
            }
            
            let header_ptr = core::ptr::NonNull::new_unchecked(addr as *mut VirtIOHeader);
            
            match MmioTransport::new(header_ptr) {
                Ok(transport) => {
                    use virtio_drivers::transport::DeviceType;
                    if matches!(transport.device_type(), DeviceType::Network) {
                        {
                            const UART: *mut u8 = 0x0900_0000 as *mut u8;
                            for c in b"[Net] found!\n" { UART.write_volatile(*c); }
                        }
                        
                        // Wait for device to settle (nop-based, no timer)
                        {
                            const UART: *mut u8 = 0x0900_0000 as *mut u8;
                            for c in b"[Net] wait...\n" { UART.write_volatile(*c); }
                        }
                        for _ in 0..50_000_000 {
                            core::arch::asm!("nop");
                        }
                        
                        unsafe {
                            const UART: *mut u8 = 0x0900_0000 as *mut u8;
                            for c in b"\n[Net] VirtIO::new\n" { UART.write_volatile(*c); }
                        }
                        
                        let net_device = VirtIONetRaw::<VirtioHal, MmioTransport, 16>::new(transport)
                            .map_err(|_| "VirtIO init failed")?;
                        
                        {
                            const UART: *mut u8 = 0x0900_0000 as *mut u8;
                            for c in b"[Net] VirtIO OK\n" { UART.write_volatile(*c); }
                        }
                        
                        let mac = net_device.mac_address();
                        let mut device = VirtioNetDevice { inner: net_device };
                        
                        // Create smoltcp interface with static config
                        {
                            const UART: *mut u8 = 0x0900_0000 as *mut u8;
                            for c in b"[Net] smoltcp\n" { UART.write_volatile(*c); }
                        }
                        
                        let hw_addr = EthernetAddress::from_bytes(&mac);
                        let config = Config::new(HardwareAddress::Ethernet(hw_addr));
                        
                        {
                            const UART: *mut u8 = 0x0900_0000 as *mut u8;
                            for c in b"[Net] Iface::new\n" { UART.write_volatile(*c); }
                        }
                        
                        let _iface = Interface::new(config, &mut device, Instant::ZERO);
                        
                        {
                            const UART: *mut u8 = 0x0900_0000 as *mut u8;
                            for c in b"[Net] SUCCESS!\n" { UART.write_volatile(*c); }
                        }
                        
                        return Ok(());
                    }
                }
                Err(_) => continue,
            }
        }
    }
    
    Err("No virtio-net found")
}

// Stub for polling (no-op for now)
pub fn poll() {
    // No-op
}
