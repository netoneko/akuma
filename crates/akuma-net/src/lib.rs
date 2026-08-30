#![cfg_attr(not(test), no_std)]
// Unsafe-free by design, and `forbid` so no module can opt back in with a local
// `allow`. Same reasoning as `akuma-net-yarn` and `akuma-syscalls-sync`.
//
// This crate held every `unsafe` in networking until 2026-08-30. Two moves
// emptied it: the device layer left for `akuma-net-nic` (DMA buffers, the NIC
// wrapper, smoltcp's `Device` impls, the MMIO doorbell, the rump tap), and the
// last two — `transmute`ing smoltcp's private `SocketHandle` to an index —
// were deleted rather than moved, because asking the socket set whether it
// still holds a handle needs no transmute and catches strictly more (see
// `smoltcp_net::stream::is_valid_handle`).
#![forbid(unsafe_code)]
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

// The device layer moved OUT to `akuma-net-nic` on 2026-08-30: DMA buffers, the
// NIC wrapper, smoltcp's `Device` impls, the MMIO doorbell and the per-packet
// counters. It holds every `unsafe` in networking, which is the point — see
// that crate's docs and docs/archive/AKUMA_NET_SPLIT.md §5.1c.
//
// Re-exported at the old paths so the kernel's call sites did not move.
pub use akuma_net_nic::{frames, nic, nicstat};
#[cfg(feature = "net-noalloc")]
pub use akuma_net_nic::virtio_rings;

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
// The rump tap NIC moved to `akuma-net-nic` with the other drivers.
#[cfg(feature = "rump")]
pub use akuma_net_nic::rump_tap;
// `socket` stays compiled (its address/errno/stat types are used pervasively by
// non-network code); the smoltcp socket-table internals inside it are gated on
// `smoltcp` (see socket.rs).
pub mod socket;
#[cfg(feature = "smoltcp")]
pub mod dns;

// The AF_UNIX state machine moved OUT to `akuma-net-unix` on 2026-08-30. It is
// IPC over pipes, not networking: no NIC, no IP, no port, no smoltcp — and
// keeping it here forced the rump-only devbox to pull this whole crate to reach
// it. Not re-exported: reaching AF_UNIX through the TCP/IP crate is the
// coupling the move removes. See docs/archive/AKUMA_NET_SPLIT.md §5.1.


#[cfg(test)]
mod tests;

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
