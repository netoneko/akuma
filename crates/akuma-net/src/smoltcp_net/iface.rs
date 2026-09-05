//! Interface introspection — local IP and the `ifconfig` snapshot.

use super::*;

/// Static IPv4 address/prefix `init()` (above) always configures this
/// interface with — the fallback when [`with_network`] can't take the lock,
/// not a placeholder guess.
const DEFAULT_IP: [u8; 4] = [10, 0, 2, 15];
const DEFAULT_PREFIX_LEN: u8 = 24;
pub(crate) const LOOPBACK_IP: [u8; 4] = [127, 0, 0, 1];

#[must_use]
pub fn get_local_ip() -> [u8; 4] {
    interface_snapshot().ip
}

/// The non-loopback interface's address/prefix, MAC, and MTU.
///
/// Everything a read-only `ifconfig`/`SIOCGIF*` needs
/// (`docs/reference/subsystems/networking.md`). The loopback address itself is
/// not reported here: it is a second address on this same interface
/// (`LoopbackAwareDevice`), not a distinct device, so the `ioctl` layer
/// synthesizes its own fixed `lo` entry rather than deriving one.
#[derive(Debug, Clone, Copy)]
pub struct IfaceInfo {
    pub ip: [u8; 4],
    pub prefix_len: u8,
    pub mac: [u8; 6],
    pub mtu: u16,
}

#[must_use]
pub fn interface_snapshot() -> IfaceInfo {
    with_network(|net| {
        // The first non-loopback IPv4 address, if any. `(0.0.0.0, 0)` when the
        // interface is up but unaddressed — a loopback-only kernel, or before
        // DHCP on a kernel that configures no static fallback. `DEFAULT_IP` is
        // *not* used here: it is the "couldn't read the stack at all" answer
        // (the outer `unwrap_or`), not "no address configured".
        let (ip, prefix_len) = net.iface.ip_addrs().iter()
            .find_map(|cidr| {
                let IpCidr::Ipv4(v4) = cidr;
                let octets = v4.address().octets();
                (octets != LOOPBACK_IP).then_some((octets, v4.prefix_len()))
            })
            .unwrap_or(([0, 0, 0, 0], 0));
        IfaceInfo {
            ip,
            prefix_len,
            mac: net.device.mac_address(),
            mtu: net.device.capabilities().max_transmission_unit as u16,
        }
    }).unwrap_or(IfaceInfo { ip: DEFAULT_IP, prefix_len: DEFAULT_PREFIX_LEN, mac: [0; 6], mtu: 1500 })
}
