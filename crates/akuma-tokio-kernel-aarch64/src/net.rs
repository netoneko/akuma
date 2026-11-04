//! The socket layer on this machine: `akuma_tokio_kernel_core::net`'s table,
//! pump and DHCP wait over the CNTV clock. The NIC's interrupt is not taken
//! here either: the tick (`trap::irq`) and the idle thread pump it.

use akuma_tokio_kernel_core::net::{self, NetMachine};

/// The AArch64 machine as `akuma_tokio_kernel_core::net` sees it.
pub struct Aarch64Net;

impl NetMachine for Aarch64Net {
    fn uptime_us() -> u64 {
        crate::fs::uptime_us()
    }
    /// Wait for the next interrupt with the kernel lock released, then take
    /// it. `wfi` wakes on a pending interrupt even while masked; the window
    /// after it runs the handler, so the lap is bounded by one tick.
    fn wait_lap() {
        crate::smp::park_halt();
        akuma_cpu::park::irq_window();
    }
    fn rng_fill(buf: &mut [u8]) {
        crate::fs::rng_fill(buf);
    }
}

/// One lap of the stack (a no-op until the stack is up).
pub fn netpoll() {
    net::netpoll::<Aarch64Net>();
}

/// Bring the socket layer up: on the first virtio-net device when `nic`,
/// loopback only otherwise or when there is no device.
pub fn init(nic: bool) {
    if nic && net::net_init::<Aarch64Net>() {
        return;
    }
    if !net::is_up() {
        net::loopback_init::<Aarch64Net>();
    }
}
