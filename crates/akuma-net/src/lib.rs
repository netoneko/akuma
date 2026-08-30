#![cfg_attr(not(test), no_std)]
#![allow(clippy::future_not_send)]

extern crate alloc;

pub mod runtime;
/// Re-exported so `crate::safe_print!(…)` resolves here as it does in
/// `akuma-virtio`. This crate prints with `safe_print!` rather than `log::`:
/// the `log` dependency exists for **smoltcp**, and it is deliberately built
/// with `max_level_off` so smoltcp's per-packet tracing compiles out entirely.
/// Routing our own messages through the same facade would either resurrect that
/// tracing or make our messages disappear with it.
///
/// Unconditional. It used to carry `#[cfg(feature = "smoltcp")]`, which is what
/// separated that attribute from the module below — see there.
pub use akuma_primitives::safe_print;

// BSS frame storage with bounds- and borrow-checked slot access. Unconditional:
// both the smoltcp device and the rump tap path stage frames in it. See
// `frames.rs` for what it replaced and why the buffers are not struct fields.
pub mod frames;
// The virtio-net device wrapper. THE one place virtio-drivers' unsafe
// begin/complete API is called — see the module header before adding another.
#[cfg(feature = "smoltcp")]
pub mod nic;

// The native smoltcp stack + the smoltcp-coupled protocol modules. Optional so a
// rump-only build (devbox) compiles them out; the rump path below is smoltcp-free.
//
// **This gate was lost and the rump-only build could not compile.** The
// `#[cfg(feature = "smoltcp")]` that belongs here had drifted upwards: a doc
// comment and the `pub use` above were inserted between the attribute and this
// `mod`, so the attribute silently started gating the re-export instead, and
// `smoltcp_net.rs` — which is nothing but smoltcp types — was compiled
// unconditionally. `scripts/build_devbox.sh` then failed with 40+
// "unresolved module or unlinked crate `smoltcp`" errors.
//
// The class of mistake is worth naming: an attribute attaches to the next item,
// and a doc comment IS an item's attribute, so anything inserted between a
// `#[cfg]` and its target moves the gate rather than breaking the build at the
// point of the edit. Keep the attribute adjacent to `pub mod`.
#[cfg(feature = "smoltcp")]
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
