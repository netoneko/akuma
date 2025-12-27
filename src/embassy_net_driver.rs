//! Embassy network driver implementations
//!
//! Provides:
//! - LoopbackDevice: A loopback network device for testing async networking
//!   without real hardware. Packets sent are immediately available to receive.

use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::cell::RefCell;

use critical_section::Mutex;
use embassy_net_driver::{Capabilities, Driver, HardwareAddress, LinkState, RxToken, TxToken};

// ============================================================================
// Loopback Device
// ============================================================================

/// Maximum number of packets that can be queued in the loopback device
const LOOPBACK_QUEUE_SIZE: usize = 16;
/// Maximum packet size
const MAX_PACKET_SIZE: usize = 1514;

/// A packet in the loopback queue
struct LoopbackPacket {
    data: Vec<u8>,
}

/// Internal state for the loopback device
struct LoopbackState {
    /// Queue of packets waiting to be received
    rx_queue: VecDeque<LoopbackPacket>,
    /// Link is always up for loopback
    link_up: bool,
}

impl LoopbackState {
    const fn new() -> Self {
        Self {
            rx_queue: VecDeque::new(),
            link_up: true,
        }
    }
}

/// A loopback network device for testing
///
/// Packets transmitted are immediately available for reception.
/// This allows testing async TCP client-server code without real hardware.
pub struct LoopbackDevice {
    state: Mutex<RefCell<LoopbackState>>,
    mac_addr: [u8; 6],
}

impl LoopbackDevice {
    /// Create a new loopback device with a generated MAC address
    pub fn new() -> Self {
        Self {
            state: Mutex::new(RefCell::new(LoopbackState::new())),
            // Use a locally administered MAC address
            mac_addr: [0x02, 0x00, 0x00, 0x00, 0x00, 0x01],
        }
    }

    /// Create a new loopback device with a specific MAC address
    pub fn with_mac(mac: [u8; 6]) -> Self {
        Self {
            state: Mutex::new(RefCell::new(LoopbackState::new())),
            mac_addr: mac,
        }
    }
}

impl Default for LoopbackDevice {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// Embassy Driver Implementation for Loopback
// ============================================================================

impl Driver for LoopbackDevice {
    type RxToken<'a>
        = LoopbackRxToken<'a>
    where
        Self: 'a;
    type TxToken<'a>
        = LoopbackTxToken<'a>
    where
        Self: 'a;

    fn receive(
        &mut self,
        cx: &mut core::task::Context,
    ) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        critical_section::with(|cs| {
            let state = self.state.borrow(cs);
            let state_ref = state.borrow();

            if state_ref.rx_queue.is_empty() {
                // No packets available - register waker for later
                // Note: In a real implementation, we'd store the waker
                // For loopback, packets are available immediately after tx
                drop(state_ref);
                let _ = cx.waker(); // Acknowledge the waker
                None
            } else {
                drop(state_ref);
                Some((
                    LoopbackRxToken {
                        device: &self.state,
                    },
                    LoopbackTxToken {
                        device: &self.state,
                    },
                ))
            }
        })
    }

    fn transmit(&mut self, _cx: &mut core::task::Context) -> Option<Self::TxToken<'_>> {
        // Loopback can always transmit (up to queue limit)
        critical_section::with(|cs| {
            let state = self.state.borrow(cs);
            let state_ref = state.borrow();

            if state_ref.rx_queue.len() < LOOPBACK_QUEUE_SIZE {
                drop(state_ref);
                Some(LoopbackTxToken {
                    device: &self.state,
                })
            } else {
                None
            }
        })
    }

    fn link_state(&mut self, _cx: &mut core::task::Context) -> LinkState {
        // Loopback is always up
        LinkState::Up
    }

    fn capabilities(&self) -> Capabilities {
        let mut caps = Capabilities::default();
        caps.max_transmission_unit = MAX_PACKET_SIZE;
        caps.max_burst_size = Some(LOOPBACK_QUEUE_SIZE);
        caps
    }

    fn hardware_address(&self) -> HardwareAddress {
        HardwareAddress::Ethernet(self.mac_addr)
    }
}

// ============================================================================
// RX Token for Loopback
// ============================================================================

pub struct LoopbackRxToken<'a> {
    device: &'a Mutex<RefCell<LoopbackState>>,
}

impl<'a> RxToken for LoopbackRxToken<'a> {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        critical_section::with(|cs| {
            let state = self.device.borrow(cs);
            let mut state_ref = state.borrow_mut();

            if let Some(mut packet) = state_ref.rx_queue.pop_front() {
                f(&mut packet.data)
            } else {
                // This shouldn't happen if receive() returned Some
                panic!("LoopbackRxToken::consume called with empty queue");
            }
        })
    }
}

// ============================================================================
// TX Token for Loopback
// ============================================================================

pub struct LoopbackTxToken<'a> {
    device: &'a Mutex<RefCell<LoopbackState>>,
}

impl<'a> TxToken for LoopbackTxToken<'a> {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        critical_section::with(|cs| {
            let state = self.device.borrow(cs);
            let mut state_ref = state.borrow_mut();

            // Allocate buffer for the packet
            let mut data = alloc::vec![0u8; len];
            let result = f(&mut data);

            // Queue the packet for reception (loopback behavior)
            if state_ref.rx_queue.len() < LOOPBACK_QUEUE_SIZE {
                state_ref.rx_queue.push_back(LoopbackPacket { data });
            }
            // If queue is full, packet is dropped (like a real network)

            result
        })
    }
}

// ============================================================================
// Helper Functions
// ============================================================================

/// Create a new loopback device for testing
pub fn create_loopback() -> LoopbackDevice {
    LoopbackDevice::new()
}
