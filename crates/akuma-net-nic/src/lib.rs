//! The virtio-net device: DMA buffers, the raw NIC wrapper, and smoltcp's
//! `Device` impls.
//!
//! # What this crate is for
//!
//! It is where **all** of networking's `unsafe` lives. Split out of `akuma-net`
//! on 2026-08-30 so its three siblings — `akuma-net-sockets`,
//! `akuma-net-smoltcp` and `akuma-net-unix` — can each carry
//! `#![forbid(unsafe_code)]`. Making the question "is Akuma's networking sound?"
//! a ~1,750-line question instead of a 6,400-line one is the entire point; see
//! `docs/archive/AKUMA_NET_SPLIT.md` §5.1c.
//!
//! # The one obligation
//!
//! Everything unsafe here is the same contract, stated once in [`nic`]:
//!
//! > A buffer handed to `receive_begin`/`transmit_begin` is owned by the device
//! > — written by DMA, or read by it — until the matching completion for that
//! > token. It must stay allocated, at a fixed address, and untouched by the
//! > driver for that whole window.
//!
//! [`frames`] discharges it by owning the storage ([`frames::FrameArena`], in
//! BSS) and handing out [`frames::FrameLease`] guards, so the safe entry points
//! in [`nic`] take an arena slot rather than a caller's buffer.
//!
//! # Why smoltcp is here
//!
//! Because the `Device` impls are, and they are here because otherwise their
//! five `RxToken`-construction `unsafe` sites would be stranded in
//! `akuma-net-smoltcp` and that crate could not be safe.
//!
//! The first draft kept this crate smoltcp-free so a future rump backend could
//! share it. That was protecting a consumer that does not exist: the device
//! cluster has exactly one user, and `rump_tap` reaches the hardware through
//! `akuma_rump::RawNic` on a completely separate path. If a second backend ever
//! wants these buffers, split [`frames`] out then — it has no smoltcp in it.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

/// Re-exported so `crate::safe_print!(…)` resolves here as it does in the
/// sibling crates.
pub use akuma_primitives::safe_print;

pub mod counters;
pub mod frames;
pub mod nic;
pub mod nicstat;
pub mod irq;
pub mod device;
pub mod loopback;
#[cfg(feature = "net-noalloc")]
pub mod virtio_rings;
#[cfg(feature = "rump")]
pub mod rump_tap;

/// The Realtek RTL8169/8168 glue — `unsafe` MMIO + DMA behind the `rtl8169`
/// feature. Off for every target but amd64 bare metal.
#[cfg(feature = "rtl8169")]
pub mod rtl8169;

pub use device::{VirtioSmoltcpDevice, VirtioRxToken, VirtioTxToken, RX_BUFFER_LEN};
pub use loopback::{
    ExternalDevice, LoopbackAwareDevice, LoopbackAwareRxToken, LoopbackAwareTxToken,
    loopback_drop_count,
};
#[cfg(feature = "rtl8169")]
pub use rtl8169::Rtl8169Device;
pub use irq::{bind as nic_bind, nic_irq_ack, nic_irq_count, nic_slot, NIC_SLOT_NONE};
pub use nic::{Nic, NetDev};
pub use counters::{isr_history, link_state, rx_counters, tx_drop_count, tx_frames_sent};
