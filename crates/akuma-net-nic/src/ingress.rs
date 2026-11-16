//! A default-deny filter on frames arriving from the wire.
//!
//! Off unless a kernel calls [`configure`] (the `net.allow=` boot parameter).
//! With it on, the machine accepts from outside only what the list names:
//!
//! * **TCP:** a connection request (`SYN` without `ACK`) is dropped unless its
//!   destination port is listed. Every other segment passes, so connections
//!   this machine opened, and listed ones, work unchanged.
//! * **UDP:** a datagram to a port below 49152 is dropped unless the port is
//!   listed. 49152 and up is where this stack allocates the source ports of its
//!   own queries (`akuma-net`'s `alloc_ephemeral_port`), so replies pass. The
//!   DHCP client port (68) is always allowed: without a lease there is no
//!   network to filter.
//! * **ICMP, ICMPv6, ARP:** pass (echo, errors, neighbour discovery).
//! * Anything else is dropped: other IP protocols, IPv4 fragments other than
//!   the first (their ports cannot be read), IPv6 with extension headers, and
//!   anything too short to parse.
//!
//! It is stateless on purpose: no table to fill, nothing to allocate, and the
//! verdict is a pure function of the frame ([`verdict`], host-tested). What it
//! does not do is filter outbound traffic or frames on the loopback ring.
//!
//! Without it, any process that binds a port is reachable from the network,
//! and the only control is whatever sits in front of the machine
//! (`docs/archive/SECURITY_AUDIT_2026_11_16.md` H5).

use core::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering};

/// Ports per protocol the list can hold. A server image names two or three.
pub const MAX_PORTS: usize = 16;

/// First port of the range this stack's own sockets are given.
const EPHEMERAL_START: u16 = 49152;
const DHCP_CLIENT: u16 = 68;

/// What may come in. `0` in a slot means "unused" (port 0 is not a real port).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Policy {
    pub tcp: [u16; MAX_PORTS],
    pub udp: [u16; MAX_PORTS],
}

/// Why [`Policy::parse`] refused a list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParseError {
    /// A word that is not `PORT`, `tcp:PORT` or `udp:PORT` with 1..=65535.
    BadEntry,
    /// More than [`MAX_PORTS`] ports for one protocol.
    TooMany,
}

impl Policy {
    /// A policy that admits nothing from outside (replies and DHCP aside).
    #[must_use]
    pub const fn deny_all() -> Self {
        Self { tcp: [0; MAX_PORTS], udp: [0; MAX_PORTS] }
    }

    /// Parse `22,tcp:443,udp:53`: comma-separated ports, TCP unless prefixed.
    /// `none` (or an empty list) is [`Self::deny_all`].
    ///
    /// # Errors
    /// On any entry it cannot read. A typo must not become "allow".
    pub fn parse(spec: &str) -> Result<Self, ParseError> {
        let mut policy = Self::deny_all();
        if spec.is_empty() || spec == "none" {
            return Ok(policy);
        }
        for word in spec.split(',') {
            let (list, port) = match word.strip_prefix("udp:") {
                Some(p) => (&mut policy.udp, p),
                None => (&mut policy.tcp, word.strip_prefix("tcp:").unwrap_or(word)),
            };
            let port: u16 = port.parse().map_err(|_| ParseError::BadEntry)?;
            if port == 0 {
                return Err(ParseError::BadEntry);
            }
            if list.contains(&port) {
                continue;
            }
            let slot = list.iter_mut().find(|p| **p == 0).ok_or(ParseError::TooMany)?;
            *slot = port;
        }
        Ok(policy)
    }

    fn tcp_allowed(&self, port: u16) -> bool {
        port != 0 && self.tcp.contains(&port)
    }

    fn udp_allowed(&self, port: u16) -> bool {
        port == DHCP_CLIENT || port >= EPHEMERAL_START || (port != 0 && self.udp.contains(&port))
    }
}

/// What to do with a frame from the wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Pass,
    Drop,
}

const ETH_HDR: usize = 14;
const PROTO_ICMP: u8 = 1;
const PROTO_TCP: u8 = 6;
const PROTO_UDP: u8 = 17;
const PROTO_ICMPV6: u8 = 58;

/// The verdict for one Ethernet frame under `policy`. Pure.
#[must_use]
pub fn verdict(policy: &Policy, frame: &[u8]) -> Verdict {
    let Some(ethertype) = frame.get(12..14) else { return Verdict::Drop };
    let Some(ip) = frame.get(ETH_HDR..) else { return Verdict::Drop };
    match u16::from_be_bytes([ethertype[0], ethertype[1]]) {
        0x0806 => Verdict::Pass,
        0x0800 => {
            let Some(&vihl) = ip.first() else { return Verdict::Drop };
            let header_len = usize::from(vihl & 0x0F) * 4;
            if vihl >> 4 != 4 || header_len < 20 || ip.len() < header_len {
                return Verdict::Drop;
            }
            // Fragment offset, low 13 bits of bytes 6..8, in 8-byte units.
            let later_fragment = u16::from_be_bytes([ip[6], ip[7]]) & 0x1FFF != 0;
            let proto = ip[9];
            if proto == PROTO_ICMP {
                return Verdict::Pass;
            }
            if later_fragment {
                return Verdict::Drop;
            }
            transport(policy, proto, &ip[header_len..])
        }
        0x86DD => {
            if ip.len() < 40 || ip[0] >> 4 != 6 {
                return Verdict::Drop;
            }
            let next = ip[6];
            if next == PROTO_ICMPV6 {
                return Verdict::Pass;
            }
            // An extension header is neither TCP nor UDP here, so it drops.
            transport(policy, next, &ip[40..])
        }
        _ => Verdict::Drop,
    }
}

fn transport(policy: &Policy, proto: u8, l4: &[u8]) -> Verdict {
    let Some(ports) = l4.get(..4) else { return Verdict::Drop };
    let dst = u16::from_be_bytes([ports[2], ports[3]]);
    let pass = match proto {
        PROTO_TCP => {
            let Some(&flags) = l4.get(13) else { return Verdict::Drop };
            let syn_only = flags & 0x02 != 0 && flags & 0x10 == 0;
            !syn_only || policy.tcp_allowed(dst)
        }
        PROTO_UDP => policy.udp_allowed(dst),
        _ => false,
    };
    if pass { Verdict::Pass } else { Verdict::Drop }
}

// ── the kernel's one policy ─────────────────────────────────────────────────

static ENABLED: AtomicBool = AtomicBool::new(false);
static TCP: [AtomicU16; MAX_PORTS] = [const { AtomicU16::new(0) }; MAX_PORTS];
static UDP: [AtomicU16; MAX_PORTS] = [const { AtomicU16::new(0) }; MAX_PORTS];
static DROPPED: AtomicU64 = AtomicU64::new(0);

/// Install `policy` and turn the filter on. Boot-time, before traffic matters.
pub fn configure(policy: &Policy) {
    for (slot, port) in TCP.iter().zip(policy.tcp) {
        slot.store(port, Ordering::Relaxed);
    }
    for (slot, port) in UDP.iter().zip(policy.udp) {
        slot.store(port, Ordering::Relaxed);
    }
    ENABLED.store(true, Ordering::Release);
}

/// What [`configure_from_boot_line`] did, for the kernel to print.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Configured {
    /// No `net.allow=`: the filter stays off and everything is admitted.
    Off,
    /// The list was installed.
    On,
    /// The list could not be read, so **nothing** is admitted: a typo in a
    /// firewall rule must not become "no firewall".
    DenyAllOnError(ParseError),
}

/// Install the policy `net.allow=<spec>` names (`None`: the word is absent).
pub fn configure_from_boot_line(spec: Option<&str>) -> Configured {
    match spec.map(Policy::parse) {
        None => Configured::Off,
        Some(Ok(policy)) => {
            configure(&policy);
            Configured::On
        }
        Some(Err(e)) => {
            configure(&Policy::deny_all());
            Configured::DenyAllOnError(e)
        }
    }
}

/// Whether a policy is installed.
#[must_use]
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Acquire)
}

/// Frames dropped by the filter since boot.
#[must_use]
pub fn dropped() -> u64 {
    DROPPED.load(Ordering::Relaxed)
}

/// Whether the installed policy drops `frame`. `false` when no policy is
/// installed, which is one relaxed load on the receive path.
#[must_use]
pub fn drops(frame: &[u8]) -> bool {
    if !enabled() {
        return false;
    }
    let mut policy = Policy::deny_all();
    for (port, slot) in policy.tcp.iter_mut().zip(&TCP) {
        *port = slot.load(Ordering::Relaxed);
    }
    for (port, slot) in policy.udp.iter_mut().zip(&UDP) {
        *port = slot.load(Ordering::Relaxed);
    }
    let drop = verdict(&policy, frame) == Verdict::Drop;
    if drop {
        DROPPED.fetch_add(1, Ordering::Relaxed);
    }
    drop
}

#[cfg(test)]
mod tests {
    use super::{ParseError, Policy, Verdict, verdict};

    /// An Ethernet + IPv4 frame carrying `l4` as protocol `proto`.
    fn v4(proto: u8, frag_off: u16, l4: &[u8]) -> Vec<u8> {
        let mut f = vec![0u8; 14];
        f[12] = 0x08;
        let mut ip = [0u8; 20];
        ip[0] = 0x45;
        ip[6..8].copy_from_slice(&frag_off.to_be_bytes());
        ip[9] = proto;
        f.extend_from_slice(&ip);
        f.extend_from_slice(l4);
        f
    }

    fn tcp(dst: u16, flags: u8) -> Vec<u8> {
        let mut t = [0u8; 20];
        t[0..2].copy_from_slice(&40000u16.to_be_bytes());
        t[2..4].copy_from_slice(&dst.to_be_bytes());
        t[12] = 0x50;
        t[13] = flags;
        t.to_vec()
    }

    fn udp(dst: u16) -> Vec<u8> {
        let mut u = [0u8; 8];
        u[2..4].copy_from_slice(&dst.to_be_bytes());
        u.to_vec()
    }

    const SYN: u8 = 0x02;
    const ACK: u8 = 0x10;

    fn ssh_only() -> Policy {
        Policy::parse("22").unwrap()
    }

    #[test]
    fn a_new_connection_passes_only_to_a_listed_port() {
        let p = ssh_only();
        assert_eq!(verdict(&p, &v4(6, 0, &tcp(22, SYN))), Verdict::Pass);
        assert_eq!(verdict(&p, &v4(6, 0, &tcp(8080, SYN))), Verdict::Drop);
        assert_eq!(verdict(&p, &v4(6, 0, &tcp(6379, SYN))), Verdict::Drop);
    }

    /// Replies to connections this machine opened carry `ACK` and must pass
    /// whatever the port, or outbound TCP stops working.
    #[test]
    fn segments_of_established_connections_pass() {
        let p = Policy::deny_all();
        assert_eq!(verdict(&p, &v4(6, 0, &tcp(51000, SYN | ACK))), Verdict::Pass);
        assert_eq!(verdict(&p, &v4(6, 0, &tcp(51000, ACK))), Verdict::Pass);
        assert_eq!(verdict(&p, &v4(6, 0, &tcp(8080, 0x04))), Verdict::Pass, "RST");
    }

    #[test]
    fn udp_passes_to_ephemeral_ports_dhcp_and_listed_ports_only() {
        let p = Policy::parse("udp:53").unwrap();
        assert_eq!(verdict(&p, &v4(17, 0, &udp(53))), Verdict::Pass);
        assert_eq!(verdict(&p, &v4(17, 0, &udp(68))), Verdict::Pass, "DHCP client");
        assert_eq!(verdict(&p, &v4(17, 0, &udp(49152))), Verdict::Pass, "a reply");
        assert_eq!(verdict(&p, &v4(17, 0, &udp(49151))), Verdict::Drop);
        assert_eq!(verdict(&p, &v4(17, 0, &udp(123))), Verdict::Drop);
        // A TCP entry does not open the UDP port of the same number.
        assert_eq!(verdict(&ssh_only(), &v4(17, 0, &udp(22))), Verdict::Drop);
    }

    #[test]
    fn icmp_and_arp_pass_and_unknown_protocols_drop() {
        let p = Policy::deny_all();
        assert_eq!(verdict(&p, &v4(1, 0, &[8, 0, 0, 0])), Verdict::Pass);
        let mut arp = vec![0u8; 42];
        arp[12] = 0x08;
        arp[13] = 0x06;
        assert_eq!(verdict(&p, &arp), Verdict::Pass);
        assert_eq!(verdict(&p, &v4(47, 0, &[0; 8])), Verdict::Drop, "GRE");
        let mut other = vec![0u8; 60];
        other[12] = 0x88;
        assert_eq!(verdict(&p, &other), Verdict::Drop);
    }

    /// A later fragment has no ports to read, and a short frame has nothing to
    /// trust: both drop rather than pass unexamined.
    #[test]
    fn fragments_and_truncated_frames_drop() {
        let p = ssh_only();
        assert_eq!(verdict(&p, &v4(6, 185, &tcp(22, SYN))), Verdict::Drop);
        assert_eq!(verdict(&p, &v4(6, 0, &tcp(22, SYN)[..10])), Verdict::Drop);
        assert_eq!(verdict(&p, &v4(17, 0, &[0, 1])), Verdict::Drop);
        assert_eq!(verdict(&p, &[0u8; 13]), Verdict::Drop);
        assert_eq!(verdict(&p, &[]), Verdict::Drop);
        let mut bad_ihl = v4(6, 0, &tcp(22, SYN));
        bad_ihl[14] = 0x4F;
        assert_eq!(verdict(&p, &bad_ihl), Verdict::Drop);
    }

    #[test]
    fn ipv6_follows_the_same_port_rules() {
        let p = ssh_only();
        let v6 = |next: u8, l4: &[u8]| {
            let mut f = vec![0u8; 14];
            f[12] = 0x86;
            f[13] = 0xDD;
            let mut ip = [0u8; 40];
            ip[0] = 0x60;
            ip[6] = next;
            f.extend_from_slice(&ip);
            f.extend_from_slice(l4);
            f
        };
        assert_eq!(verdict(&p, &v6(6, &tcp(22, SYN))), Verdict::Pass);
        assert_eq!(verdict(&p, &v6(6, &tcp(80, SYN))), Verdict::Drop);
        assert_eq!(verdict(&p, &v6(58, &[135, 0, 0, 0])), Verdict::Pass, "neighbour discovery");
        assert_eq!(verdict(&p, &v6(0, &tcp(22, SYN))), Verdict::Drop, "extension header");
    }

    #[test]
    fn parse_reads_lists_and_refuses_what_it_cannot_read() {
        let p = Policy::parse("22,tcp:443,udp:53,22").unwrap();
        assert_eq!(&p.tcp[..3], &[22, 443, 0]);
        assert_eq!(p.udp[0], 53);
        assert_eq!(Policy::parse("none"), Ok(Policy::deny_all()));
        assert_eq!(Policy::parse(""), Ok(Policy::deny_all()));
        for bad in ["ssh", "22,", "0", "65536", "tcp:", "udp:x", "22;80"] {
            assert_eq!(Policy::parse(bad), Err(ParseError::BadEntry), "{bad}");
        }
        let many = "1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17";
        assert_eq!(Policy::parse(many), Err(ParseError::TooMany));
    }
}
