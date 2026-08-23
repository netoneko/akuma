#![cfg_attr(not(test), no_std)]
#![allow(clippy::future_not_send)]

extern crate alloc;

pub mod runtime;
// The native smoltcp stack + the smoltcp-coupled protocol modules. Optional so a
// rump-only build (devbox) compiles them out; the rump path below is smoltcp-free.
#[cfg(feature = "smoltcp")]
/// Re-exported so `crate::safe_print!(…)` resolves here as it does in
/// `akuma-virtio`. This crate prints with `safe_print!` rather than `log::`:
/// the `log` dependency exists for **smoltcp**, and it is deliberately built
/// with `max_level_off` so smoltcp's per-packet tracing compiles out entirely.
/// Routing our own messages through the same facade would either resurrect that
/// tracing or make our messages disappear with it.
pub use akuma_primitives::safe_print;

pub mod smoltcp_net;
// Static RX/TX frame rings backing the async transmit path. Only meaningful
// with the smoltcp device, and only compiled when `net-noalloc` selects it.
#[cfg(all(feature = "smoltcp", feature = "net-noalloc"))]
pub mod virtio_rings;
// Raw L2 packet path (second NIC → /dev/net/tap0) for the kernel `rump` feature.
#[cfg(feature = "rump")]
pub mod rump_tap;
// `socket` stays compiled (its address/errno/stat types are used pervasively by
// non-network code); the smoltcp socket-table internals inside it are gated on
// `smoltcp` (see socket.rs).
pub mod socket;
#[cfg(feature = "smoltcp")]
pub mod dns;

// The AF_UNIX socket state machine. Unconditional, like `socket`'s address
// types: AF_UNIX must exist on the rump-only devbox build, where box 0's
// `rump_server` answers every proxied syscall over a `UnixSocket` at fd 3.
// Contains no smoltcp references. See docs/archive/UNIX_SOCKET_IMPROVEMENTS.md.
pub mod unix;
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
#[cfg(test)]
mod unix_tests;

pub use runtime::NetRuntime;

/// Initialize the full networking stack (registers the runtime callbacks, then
/// brings up the smoltcp device + interface).
///
/// # Arguments
/// * `rt` — Kernel runtime callbacks (timer, yield, RNG, etc.)
/// * `enable_dhcp` — Whether to enable DHCP (vs static IP fallback)
///
/// The MMIO slots to probe are no longer a parameter: they are
/// `akuma_virtio::slot_addr`, which every caller derived the same way anyway.
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
