#![cfg_attr(not(test), no_std)]
#![allow(clippy::future_not_send)]

extern crate alloc;

pub mod runtime;
pub mod hal;
pub mod smoltcp_net;
pub mod socket;
pub mod dns;
pub mod stats;
// In-kernel TLS client (outbound HTTPS for the shell `curl` command). Gated so
// the extreme profile builds without embedded-tls / the cert verifier. SSH is
// independent (akuma-ssh-crypto) and is never gated by this.
#[cfg(feature = "kernel-tls")]
pub mod tls;
#[cfg(feature = "kernel-tls")]
pub mod tls_rng;
#[cfg(feature = "kernel-tls")]
pub mod tls_verifier;
pub mod http;


#[cfg(test)]
mod tests;

pub use runtime::NetRuntime;

/// Initialize the full networking stack.
///
/// # Arguments
/// * `rt` — Kernel runtime callbacks (timer, yield, RNG, address translation, etc.)
/// * `mmio_addrs` — `VirtIO` MMIO addresses to probe for a net device
/// * `enable_dhcp` — Whether to enable DHCP (vs static IP fallback)
pub fn init(
    rt: NetRuntime,
    mmio_addrs: &[usize],
    enable_dhcp: bool,
) -> Result<(), &'static str> {
    runtime::register(rt);
    smoltcp_net::init(mmio_addrs, enable_dhcp)
}
