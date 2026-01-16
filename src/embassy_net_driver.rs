//! Embassy network driver implementations
//!
//! Provides:
//! - LoopbackDevice: A loopback network device for async networking
//!   Packets sent are immediately available to receive.

use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::task::Waker;

use embassy_net_driver::{Capabilities, Driver, HardwareAddress, LinkState, RxToken, TxToken};
use spinning_top::Spinlock;

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
    /// Waker to notify when packets are available
    rx_waker: Option<Waker>,
}

impl LoopbackState {
    const fn new() -> Self {
        Self {
            rx_queue: VecDeque::new(),
            rx_waker: None,
        }
    }
}

/// A loopback network device
///
/// Packets transmitted are immediately available for reception.
/// This allows TCP client-server communication on localhost.
pub struct LoopbackDevice {
    state: Spinlock<LoopbackState>,
    mac_addr: [u8; 6],
}

impl LoopbackDevice {
    /// Create a new loopback device with a generated MAC address
    pub fn new() -> Self {
        Self {
            state: Spinlock::new(LoopbackState::new()),
            // Use a locally administered MAC address for loopback
            mac_addr: [0x02, 0x00, 0x00, 0x00, 0x00, 0x01],
        }
    }

    /// Create a new loopback device with a specific MAC address
    pub fn with_mac(mac: [u8; 6]) -> Self {
        Self {
            state: Spinlock::new(LoopbackState::new()),
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
        let mut state = self.state.lock();

        if !state.rx_queue.is_empty() {
            // Clear waker since we're returning a packet
            state.rx_waker = None;
            drop(state);
            Some((
                LoopbackRxToken {
                    device: &self.state,
                },
                LoopbackTxToken {
                    device: &self.state,
                },
            ))
        } else {
            // Store waker to be notified when packets arrive
            state.rx_waker = Some(cx.waker().clone());
            None
        }
    }

    fn transmit(&mut self, _cx: &mut core::task::Context) -> Option<Self::TxToken<'_>> {
        let state = self.state.lock();
        let can_transmit = state.rx_queue.len() < LOOPBACK_QUEUE_SIZE;
        drop(state);

        if can_transmit {
            Some(LoopbackTxToken {
                device: &self.state,
            })
        } else {
            None
        }
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
    device: &'a Spinlock<LoopbackState>,
}

impl<'a> RxToken for LoopbackRxToken<'a> {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut state = self.device.lock();
        if let Some(mut packet) = state.rx_queue.pop_front() {
            drop(state); // Release lock before calling f
            f(&mut packet.data)
        } else {
            panic!("LoopbackRxToken::consume called with empty queue");
        }
    }
}

// ============================================================================
// TX Token for Loopback
// ============================================================================

pub struct LoopbackTxToken<'a> {
    device: &'a Spinlock<LoopbackState>,
}

impl<'a> TxToken for LoopbackTxToken<'a> {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        // Allocate buffer for the packet
        let mut data = alloc::vec![0u8; len];

        // Call f to fill the buffer (no lock held)
        let result = f(&mut data);

        // Now lock and queue the packet
        let mut state = self.device.lock();
        if state.rx_queue.len() < LOOPBACK_QUEUE_SIZE {
            state.rx_queue.push_back(LoopbackPacket { data });

            // Wake the receiver if it's waiting
            if let Some(waker) = state.rx_waker.take() {
                drop(state); // Release lock before waking
                waker.wake();
                // Signal executor to wake from WFE
                crate::executor::signal_wake();
            }
        }
        // If queue is full, packet is dropped (like a real network)

        result
    }
}

// ============================================================================
// Helper Functions
// ============================================================================

/// Create a new loopback device
pub fn create_loopback() -> LoopbackDevice {
    LoopbackDevice::new()
}
