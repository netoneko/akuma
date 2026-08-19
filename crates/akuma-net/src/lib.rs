#![cfg_attr(not(test), no_std)]
#![allow(clippy::future_not_send)]

extern crate alloc;

pub mod runtime;
// The native smoltcp stack + the smoltcp-coupled protocol modules. Optional so a
// rump-only build (devbox) compiles them out; the rump path below is smoltcp-free.
#[cfg(feature = "smoltcp")]
pub mod smoltcp_net;
// Raw L2 packet path (second NIC → /dev/net/tap0) for the kernel `rump` feature.
#[cfg(feature = "rump")]
pub mod rump_tap;
// `socket` stays compiled (its address/errno/stat types are used pervasively by
// non-network code); the smoltcp socket-table internals inside it are gated on
// `smoltcp` (see socket.rs).
pub mod socket;
#[cfg(feature = "smoltcp")]
pub mod dns;

// Lock infrastructure for fine-grained locking (Phase 1 of BKL removal)
pub mod locks;
// Device-level traffic/latency counters. Always compiled (the module's public
// API is the same either way); the counters themselves only exist under the
// `net-profile` feature.
pub mod nicstat;


#[cfg(test)]
mod tests;
#[cfg(test)]
mod lock_tests;

pub use runtime::NetRuntime;

/// Initialize the full networking stack (registers the runtime callbacks, then
/// brings up the smoltcp device + interface).
///
/// # Arguments
/// * `rt` — Kernel runtime callbacks (timer, yield, RNG, etc.)
/// * `enable_dhcp` — Whether to enable DHCP (vs static IP fallback)
///
/// The MMIO slots to probe are no longer a parameter: they are
/// `akuma_virtio::VIRTIO_MMIO_ADDRS`, which every caller passed anyway.
#[cfg(feature = "smoltcp")]
pub fn init(rt: NetRuntime, enable_dhcp: bool) -> Result<(), &'static str> {
    runtime::register(rt);
    smoltcp_net::init(enable_dhcp)
}

/// Smoltcp-free variant of [`init`].
///
/// With the native stack compiled out (devbox / rump-only) there is no NIC0
/// interface to bring up — just register the runtime callbacks so the rump path
/// and timers work. NIC1/`/dev/net/tap0` is bound separately by the kernel's
/// `rump` feature.
#[cfg(not(feature = "smoltcp"))]
pub fn init(rt: NetRuntime, _enable_dhcp: bool) -> Result<(), &'static str> {
    runtime::register(rt);
    Ok(())
}
