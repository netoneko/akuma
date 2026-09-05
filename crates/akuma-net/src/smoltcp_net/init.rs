//! Bringing the interface up: probe, configure, seed the socket set.

use super::*;
use akuma_net_nic::ExternalDevice;

// Initialization
// ============================================================================

/// Probe for virtio-net and bring the stack up on it (plus loopback).
///
/// The VMM path: every VMM target announces a virtio-net transport, and a
/// kernel that runs under one expects a NIC — so a missing device is an error
/// here, not a silent downgrade. The bare-metal amd64 target, which has no
/// virtio-net, uses [`init_loopback_only`] / [`init_with_external`] instead.
#[allow(clippy::cast_possible_wrap)]
pub fn init(enable_dhcp: bool) -> Result<(), &'static str> {
    crate::safe_print!(64, "[SmolNet] Initializing network stack...\n");

    let mut found_device: Option<VirtIONetRaw<VirtioHal, VirtioTransport, 16>> = None;
    if let Some((i, transport)) = akuma_virtio::probe::probe(akuma_virtio::device_id::NET) {
        crate::safe_print!(64, "[SmolNet] Found virtio-net at slot {i}\n");
        if let Ok(dev) = VirtIONetRaw::new(transport) {
            // Record the slot and its MMIO base for the IRQ handler before the
            // device is moved into `NETWORK` — afterwards it is only reachable
            // under the lock, which IRQ context must not take.
            akuma_net_nic::irq::bind(i as u32, akuma_virtio::slot_addr(i));
            found_device = Some(dev);
        }
    }

    let device = ExternalDevice::Virtio(VirtioSmoltcpDevice::new(Nic::new(
        found_device.ok_or("No virtio-net device found")?,
    )));
    build(enable_dhcp, device, true)
}

/// Bring the stack up with **no wire** — loopback only.
///
/// For a kernel with no NIC: the amd64 bare-metal target before its Realtek
/// driver is wired. `socket(AF_INET)` works, `127.0.0.1` is reachable, and
/// `ifconfig` sees `lo` plus an unaddressed `eth0`. DHCP is off (nothing to
/// run it on) and no static IPv4 address is configured, so
/// `interface_snapshot()` reports `0.0.0.0` for `eth0` rather than the
/// fallback address a real interface would carry.
pub fn init_loopback_only() -> Result<(), &'static str> {
    crate::safe_print!(48, "[SmolNet] Initializing (loopback only, no NIC)\n");
    build(false, ExternalDevice::Absent, false)
}

/// Bring the stack up on a caller-supplied external device (plus loopback).
///
/// The seam for a NIC this crate cannot probe for itself — the amd64 Realtek,
/// built by `akuma-net-nic`'s `rtl8169` module from a mapped BAR.
#[allow(clippy::cast_possible_wrap)]
pub fn init_with_external(enable_dhcp: bool, device: ExternalDevice) -> Result<(), &'static str> {
    build(enable_dhcp, device, true)
}

/// The shared part: seed the interface, addresses, routes and socket set from
/// an already-chosen device, and install it as `NETWORK`.
#[allow(clippy::cast_possible_wrap)]
fn build(
    enable_dhcp: bool,
    external: ExternalDevice,
    add_static_v4: bool,
) -> Result<(), &'static str> {
    DHCP_ENABLED.store(enable_dhcp, Ordering::Relaxed);

    let mut device = LoopbackAwareDevice::new(external);
    let mac = device.mac_address();
    crate::safe_print!(
        96,
        "[SmolNet] MAC: {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
    );

    let timestamp = Instant::from_micros((runtime().uptime_us)() as i64);

    let mut config = Config::new(HardwareAddress::Ethernet(EthernetAddress(mac)));
    config.random_seed = (runtime().uptime_us)();

    let mut iface = Interface::new(config, &mut device, timestamp);

    // `push` fails only if smoltcp's fixed address list is full, and
    // `add_default_ipv4_route` only if its route table is. Neither can happen on
    // a freshly built interface — but `unwrap()` here would take the whole kernel
    // down for a misconfiguration, on a path that has a perfectly good degraded
    // mode (no static fallback address; DHCP still runs). Log and continue.
    iface.update_ip_addrs(|ip_addrs| {
        if add_static_v4
            && ip_addrs.push(IpCidr::new(IpAddress::v4(10, 0, 2, 15), 24)).is_err()
        {
            crate::safe_print!(80, "[SmolNet] could not add static IPv4 address: list full\n");
        }
        if ip_addrs.push(IpCidr::new(IpAddress::v4(127, 0, 0, 1), 8)).is_err() {
            crate::safe_print!(80, "[SmolNet] could not add loopback address: list full\n");
        }
    });
    if add_static_v4
        && iface
            .routes_mut()
            .add_default_ipv4_route(smoltcp::wire::Ipv4Address::new(10, 0, 2, 2))
            .is_err()
    {
        crate::safe_print!(80, "[SmolNet] could not add default IPv4 route: table full\n");
    }

    // `None` means `init` ran twice, which the kernel's boot path does not do.
    // Refusing beats handing out a second `&mut` to the same table.
    let storage = SOCKET_STORAGE
        .take()
        .ok_or("smoltcp_net::init called twice — socket storage already claimed")?;
    let mut sockets = SocketSet::new(&mut storage[..]);

    let dhcp_handle = if enable_dhcp {
        crate::safe_print!(32, "[SmolNet] DHCP enabled\n");
        let dhcp_socket = dhcpv4::Socket::new();
        SOCKETS_LIVE.fetch_add(1, Ordering::Relaxed);
        Some(sockets.add(dhcp_socket))
    } else {
        None
    };

    let dns_servers = &[QEMU_DNS_SERVER];
    let dns_socket = dns::Socket::new(dns_servers, vec![]);
    SOCKETS_LIVE.fetch_add(1, Ordering::Relaxed);
    let dns_handle = sockets.add(dns_socket);
    crate::safe_print!(64, "[SmolNet] DNS socket initialized (server: 10.0.2.3)\n");

    *NETWORK.lock() = Some(NetworkState {
        iface,
        sockets,
        device,
        dhcp_handle,
        dns_handle,
        pending_removal: Vec::new(),
        connecting: Vec::new(),
    });

    NETWORK_READY.store(true, Ordering::Release);
    crate::safe_print!(64, "[SmolNet] Initialized successfully\n");
    Ok(())
}
