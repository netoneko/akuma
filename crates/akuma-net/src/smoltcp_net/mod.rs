//! Smoltcp Network Stack (Thread-Safe)
//!
//! Provides the core networking stack using smoltcp, protected by a Spinlock.
//! This allows any thread (kernel or userspace via syscall) to drive the network stack.
//!
//! # Layout
//!
//! Split out of a single 2,134-line `smoltcp_net.rs` on 2026-08-30, ahead of
//! lifting the whole thing into `akuma-net-smoltcp`. The module names are the
//! seam that move will follow, so they are drawn where the *dependencies* are,
//! not where the file happened to be longest:
//!
//! | module | what |
//! |---|---|
//! | [`consts`] | capacities, buffer sizes, timeouts |
//! | [`state`] | the `NETWORK` lock, `NetworkState`, holder instrumentation |
//! | [`irq`] | NIC MMIO — interrupt ack and the TX doorbell. Runs from IRQ context |
//! | [`device`] | `VirtioSmoltcpDevice`, smoltcp's `Device` over virtio-net |
//! | [`loopback`] | `LoopbackAwareDevice` and its ring |
//! | [`init`] | interface bring-up |
//! | [`poll`] | `poll()` and the `NETWORK` critical section |
//! | [`resolve`] | `dns_query` |
//! | [`connect`] | async `tcp_connect` |
//! | [`sockets`] | socket-set slots, soft cap, reclaim |
//! | [`udp_api`] | the UDP API — named `udp_api`, not `udp`, so it does not
//!   shadow smoltcp's own `udp` socket module in path position |
//! | [`iface`] | local IP / `ifconfig` snapshot |
//! | [`lifecycle`] | socket teardown, connecting-handle bookkeeping |
//! | [`stream`] | `TcpStream` and `SocketHandle` indexing |
//!
//! Every item is re-exported flat below, so `akuma_net::smoltcp_net::X` resolves
//! exactly as it did when this was one file — the 36 kernel call sites did not
//! change.

use alloc::vec;
use alloc::vec::Vec;
use core::task::Poll;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU16, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use spinning_top::Spinlock;

use smoltcp::iface::{Config, Interface, SocketSet, SocketStorage, PollResult};
pub use smoltcp::iface::SocketHandle;
use smoltcp::phy::Device;
use smoltcp::socket::{tcp, udp, dhcpv4, dns};
use smoltcp::time::Instant;
use smoltcp::wire::{EthernetAddress, HardwareAddress, IpAddress, IpCidr};

use virtio_drivers::device::net::VirtIONetRaw;
use akuma_virtio::VirtioTransport;
use akuma_virtio::VirtioHal;
use crate::runtime::runtime;
use crate::runtime::PreemptGuard;
use akuma_net_nic::nicstat;
use akuma_net_nic::nic::Nic;
pub use akuma_net_nic::{VirtioSmoltcpDevice, LoopbackAwareDevice, ExternalDevice,
    LoopbackAwareRxToken, LoopbackAwareTxToken, VirtioRxToken, VirtioTxToken,
    RX_BUFFER_LEN, loopback_drop_count, nic_irq_ack, nic_irq_count, nic_slot,
    NIC_SLOT_NONE, link_state, rx_counters, tx_drop_count, tx_frames_sent};
use akuma_primitives::TakeOnce;

pub mod consts;
pub mod state;
pub mod init;
pub mod poll;
pub mod resolve;
pub mod connect;
pub mod sockets;
pub mod udp_api;
pub mod iface;
pub mod lifecycle;
pub mod stream;

// `pub(crate)`, not `pub`: since the device counters moved to `akuma-net-nic`,
// nothing in `consts` is part of this module's public surface — it is all
// tunables the sibling modules read. A `pub use` of it re-exports nothing and
// rustc rejects the glob outright.
pub(crate) use consts::*;
pub use state::*;
pub use init::*;
pub use poll::*;
pub use resolve::*;
pub use connect::*;
pub use sockets::*;
pub use udp_api::*;
pub use iface::*;
pub use lifecycle::*;
pub use stream::*;
