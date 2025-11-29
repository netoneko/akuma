// Network stack with device registry (no spinlocks in init)
use smoltcp::iface::{Config, Interface};
use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::time::Instant;
use smoltcp::wire::{EthernetAddress, HardwareAddress};
use virtio_drivers::device::net::VirtIONetRaw;
use virtio_drivers::transport::Transport;
use virtio_drivers::transport::mmio::{MmioTransport, VirtIOHeader};

use crate::virtio_hal::VirtioHal;

// Maximum interfaces
const MAX_INTERFACES: usize = 4;

// Static buffers
static mut VIRTIO_RX_BUFFER: [u8; 4096] = [0; 4096];
static mut VIRTIO_TX_BUFFER: [u8; 4096] = [0; 4096];

// Interface registry (simple static array, no locks during init)
static mut INTERFACE_COUNT: usize = 0;
static mut INTERFACE_NAMES: [[u8; 8]; MAX_INTERFACES] = [[0; 8]; MAX_INTERFACES];
static mut INTERFACE_TYPES: [u8; MAX_INTERFACES] = [0; MAX_INTERFACES]; // 0=none, 1=loopback, 2=ethernet
static mut INTERFACE_MACS: [[u8; 6]; MAX_INTERFACES] = [[0; 6]; MAX_INTERFACES];

// ============================================================================
// Virtio-net Device
// ============================================================================

pub struct VirtioNetDevice {
    inner: VirtIONetRaw<VirtioHal, MmioTransport, 16>,
}

pub struct VirtioRxToken { len: usize }
pub struct VirtioTxToken<'a> { device: &'a mut VirtioNetDevice }

impl RxToken for VirtioRxToken {
    fn consume<R, F>(self, f: F) -> R where F: FnOnce(&mut [u8]) -> R {
        unsafe { f(&mut VIRTIO_RX_BUFFER[..self.len]) }
    }
}

impl<'a> TxToken for VirtioTxToken<'a> {
    fn consume<R, F>(self, len: usize, f: F) -> R where F: FnOnce(&mut [u8]) -> R {
        unsafe {
            let result = f(&mut VIRTIO_TX_BUFFER[..len]);
            let _ = self.device.inner.send(&VIRTIO_TX_BUFFER[..len]);
            result
        }
    }
}

impl Device for VirtioNetDevice {
    type RxToken<'a> = VirtioRxToken;
    type TxToken<'a> = VirtioTxToken<'a>;

    fn receive(&mut self, _ts: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        None
    }

    fn transmit(&mut self, _ts: Instant) -> Option<Self::TxToken<'_>> {
        None // Return None during init
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.max_transmission_unit = 1500;
        caps.medium = Medium::Ethernet;
        caps
    }
}

// ============================================================================
// Helpers
// ============================================================================

fn log(msg: &[u8]) {
    unsafe {
        const UART: *mut u8 = 0x0900_0000 as *mut u8;
        for c in msg { UART.write_volatile(*c); }
    }
}

fn log_hex_byte(b: u8) {
    let hex = |n: u8| if n < 10 { b'0' + n } else { b'a' + n - 10 };
    log(&[hex((b >> 4) & 0xF), hex(b & 0xF)]);
}

fn register_interface(name: &[u8], iface_type: u8, mac: [u8; 6]) {
    unsafe {
        if INTERFACE_COUNT >= MAX_INTERFACES { return; }
        
        let idx = INTERFACE_COUNT;
        INTERFACE_COUNT += 1;
        
        for (i, &c) in name.iter().enumerate() {
            if i < 8 { INTERFACE_NAMES[idx][i] = c; }
        }
        INTERFACE_TYPES[idx] = iface_type;
        INTERFACE_MACS[idx] = mac;
    }
}

fn find_virtio_mmio_devices() -> [usize; 8] {
    [
        0x0a000000, 0x0a000200, 0x0a000400, 0x0a000600,
        0x0a000800, 0x0a000a00, 0x0a000c00, 0x0a000e00,
    ]
}

// ============================================================================
// Initialization
// ============================================================================

pub fn init(_dtb_ptr: usize) -> Result<(), &'static str> {
    log(b"[Net] init\n");
    
    // Register lo0 (loopback)
    log(b"[Net] lo0: loopback\n");
    register_interface(b"lo0", 1, [0; 6]);
    log(b"[Net] lo0 registered\n");
    
    // TODO: ethernet scan disabled - causes hang with preemptive threading
    // The virtio-drivers crate has blocking behavior that conflicts with our scheduler
    log(b"[Net] eth0: disabled\n");
    
    log(b"[Net] Ready\n");
    Ok(())
}

/// List interfaces
pub fn list_interfaces() {
    unsafe {
        log(b"\nInterfaces:\n");
        for i in 0..INTERFACE_COUNT {
            // Name
            for &c in INTERFACE_NAMES[i].iter() {
                if c != 0 { log(&[c]); }
            }
            log(b": ");
            
            match INTERFACE_TYPES[i] {
                1 => log(b"loopback"),
                2 => {
                    log(b"ethernet ");
                    for (j, &b) in INTERFACE_MACS[i].iter().enumerate() {
                        if j > 0 { log(b":"); }
                        log_hex_byte(b);
                    }
                }
                _ => log(b"unknown"),
            }
            log(b" UP\n");
        }
        log(b"\n");
    }
}

pub fn poll() {}
