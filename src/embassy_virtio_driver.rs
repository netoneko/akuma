//! Embassy Network Driver for Virtio-Net
//!
//! Wraps the VirtioNetDevice to implement embassy_net_driver::Driver trait,
//! enabling async networking with embassy-net.

use alloc::boxed::Box;
use core::cell::RefCell;
use core::task::Waker;

use critical_section::Mutex;
use embassy_net_driver::{Capabilities, Driver, HardwareAddress, LinkState, RxToken, TxToken};
use virtio_drivers::device::net::VirtIONetRaw;
use virtio_drivers::transport::mmio::MmioTransport;

use crate::virtio_hal::VirtioHal;

// ============================================================================
// Constants
// ============================================================================

const VIRTIO_BUFFER_SIZE: usize = 2048;

// ============================================================================
// RX Data Buffer
// ============================================================================

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

// ============================================================================
// Embassy Virtio Driver
// ============================================================================

/// Embassy-compatible wrapper for virtio-net device
pub struct EmbassyVirtioDriver {
    inner: VirtIONetRaw<VirtioHal, MmioTransport, 16>,
    tx_buffer: Box<[u8; VIRTIO_BUFFER_SIZE]>,
    rx_pending_token: Option<u16>,
    rx_data: RefCell<RxData>,
    mac_addr: [u8; 6],
    /// Waker to notify when RX data is available
    rx_waker: Mutex<RefCell<Option<Waker>>>,
    /// Waker to notify when TX is ready
    tx_waker: Mutex<RefCell<Option<Waker>>>,
}

impl EmbassyVirtioDriver {
    /// Create a new Embassy virtio driver from a raw virtio-net device
    pub fn new(inner: VirtIONetRaw<VirtioHal, MmioTransport, 16>) -> Self {
        let mac = inner.mac_address();
        Self {
            inner,
            tx_buffer: Box::new([0u8; VIRTIO_BUFFER_SIZE]),
            rx_pending_token: None,
            rx_data: RefCell::new(RxData::new()),
            mac_addr: mac,
            rx_waker: Mutex::new(RefCell::new(None)),
            tx_waker: Mutex::new(RefCell::new(None)),
        }
    }

    /// Get the MAC address
    pub fn mac_address(&self) -> [u8; 6] {
        self.mac_addr
    }

    /// Try to receive a packet, returning true if one is available
    fn try_receive(&mut self) -> bool {
        let mut rx = self.rx_data.borrow_mut();

        // Check if we already have valid data
        if rx.valid {
            return true;
        }

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

    /// Wake any pending RX waker
    pub fn wake_rx(&self) {
        critical_section::with(|cs| {
            if let Some(waker) = self.rx_waker.borrow(cs).borrow_mut().take() {
                waker.wake();
            }
        });
    }

    /// Wake any pending TX waker
    pub fn wake_tx(&self) {
        critical_section::with(|cs| {
            if let Some(waker) = self.tx_waker.borrow(cs).borrow_mut().take() {
                waker.wake();
            }
        });
    }
}

// ============================================================================
// Embassy Driver Implementation
// ============================================================================

impl Driver for EmbassyVirtioDriver {
    type RxToken<'a>
        = VirtioRxToken<'a>
    where
        Self: 'a;
    type TxToken<'a>
        = VirtioTxToken<'a>
    where
        Self: 'a;

    fn receive(
        &mut self,
        cx: &mut core::task::Context,
    ) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        if self.try_receive() {
            // Use raw pointers to split borrow (same pattern as smoltcp impl)
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
            // Store waker for later notification
            critical_section::with(|cs| {
                self.rx_waker
                    .borrow(cs)
                    .borrow_mut()
                    .replace(cx.waker().clone());
            });
            None
        }
    }

    fn transmit(&mut self, cx: &mut core::task::Context) -> Option<Self::TxToken<'_>> {
        // Virtio-net can always transmit (we don't track queue fullness for simplicity)
        // In a real implementation, we'd check if the TX queue is full
        let _ = cx; // Store waker if needed
        Some(VirtioTxToken { device: self })
    }

    fn link_state(&mut self, _cx: &mut core::task::Context) -> LinkState {
        // Virtio-net in QEMU is always up
        LinkState::Up
    }

    fn capabilities(&self) -> Capabilities {
        let mut caps = Capabilities::default();
        caps.max_transmission_unit = 1514; // Ethernet frame size
        caps.max_burst_size = Some(1);
        caps
    }

    fn hardware_address(&self) -> HardwareAddress {
        HardwareAddress::Ethernet(self.mac_addr)
    }
}

// ============================================================================
// RX Token
// ============================================================================

pub struct VirtioRxToken<'a> {
    device: &'a EmbassyVirtioDriver,
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

// ============================================================================
// TX Token
// ============================================================================

pub struct VirtioTxToken<'a> {
    device: &'a mut EmbassyVirtioDriver,
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
