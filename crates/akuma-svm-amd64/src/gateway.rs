//! The far end of the guest's virtual wire: a tiny user-mode gateway, in the spirit of
//! QEMU's "slirp" and with its addresses, so a guest that falls back to
//! `10.0.2.15` agrees with what it is talking to.
//!
//! It is a [`NetBackend`] and nothing else — no sockets, no host NIC. It answers:
//!
//! * **ARP** requests for the gateway (`10.0.2.2`) and the DNS proxy (`10.0.2.3`);
//! * **DHCP** (RFC 2131): DISCOVER → OFFER and REQUEST → ACK, always for `10.0.2.15`,
//!   with the gateway as router, the proxy as resolver, a day's lease;
//! * **ICMP echo** to the gateway (the payload comes back as sent);
//! * **UDP echo** to the gateway's port 7 (RFC 862);
//! * **TCP** ([`crate::tcp`]): an **echo** server on the gateway's port 7, and
//!   **forward slots** — connections the *gateway opens to the guest* (the hypervisor's
//!   relay: a host listener's clients reach the guest's sshd this way). A closed port is
//!   answered with a reset.
//!
//! Everything else the guest sends is counted and dropped. That is enough for a guest
//! to bring its stack up, reach "its gateway" and prove both directions of the device
//! against an independent implementation of the protocols; it is deliberately not a
//! route to anywhere.
//!
//! No allocation: replies are built in place into a queue of [`DEPTH`] slots of
//! [`SLOT`] bytes, in storage the caller lends ([`STORAGE`] bytes: the queue and the
//! rings of the TCP connections). A reply that finds the queue full is dropped (and
//! counted): the guest retransmits. TCP segments are *pulled* only while the queue has
//! room, so the TCP side never loses a frame ([`crate::tcp`]'s contract).

use crate::tcp::{self, Segment, Tcp};
use crate::virtio_net::NetBackend;

/// The guest's hardware address (QEMU's default for the first NIC).
pub const GUEST_MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];
/// The gateway's hardware address.
pub const GATEWAY_MAC: [u8; 6] = [0x52, 0x55, 0x0A, 0x00, 0x02, 0x02];
/// The address the gateway hands the guest.
pub const GUEST_IP: [u8; 4] = [10, 0, 2, 15];
/// The gateway.
pub const GATEWAY_IP: [u8; 4] = [10, 0, 2, 2];
/// The DNS proxy (answers ARP only: nothing resolves).
pub const DNS_IP: [u8; 4] = [10, 0, 2, 3];
/// The subnet mask, and the broadcast address.
pub const NETMASK: [u8; 4] = [255, 255, 255, 0];
pub const BROADCAST_IP: [u8; 4] = [10, 0, 2, 255];

/// Bytes of one reply slot: a full Ethernet frame.
pub const SLOT: usize = 1536;
/// Reply slots.
pub const DEPTH: usize = 8;
/// Bytes of each TCP ring (every connection has two).
pub const RING: usize = 8192;
/// Connections the gateway opens to the guest.
pub const FWD_SLOTS: usize = 2;
/// The gateway's source port for forward slot `i`, generation `g`: `FWD_PORT_BASE + i + FWD_SLOTS * g`.
///
/// Every [`Gateway::fwd_open`] takes the slot's next
/// generation, so a connection never reuses the 4-tuple of the one before it: a guest that still
/// holds that one half-open (a reset lost to a full queue, TIME-WAIT) would ignore the new SYN,
/// and a SYN is sent once.
pub const FWD_PORT_BASE: u16 = 40_000;
/// Source-port generations per slot.
pub const FWD_GENS: usize = 8;
/// The TCP and UDP echo port (RFC 862).
pub const ECHO_PORT: u16 = 7;
const CONNS: usize = 1 + FWD_SLOTS;
/// Storage the gateway needs lent to it, in bytes.
pub const STORAGE: usize = DEPTH * SLOT + CONNS * 2 * RING;

const ETH_ARP: u16 = 0x0806;
const ETH_IP: u16 = 0x0800;
const BROADCAST_MAC: [u8; 6] = [0xFF; 6];
const DHCP_COOKIE: [u8; 4] = [0x63, 0x82, 0x53, 0x63];
const DHCP_DISCOVER: u8 = 1;
const DHCP_OFFER: u8 = 2;
const DHCP_REQUEST: u8 = 3;
const DHCP_ACK: u8 = 5;
const LEASE_SECS: u32 = 86_400;

/// What the gateway has seen and done.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Frames the guest sent.
    pub frames: u64,
    pub arp_replies: u64,
    pub dhcp_offers: u64,
    pub dhcp_acks: u64,
    pub icmp_echoes: u64,
    pub udp_echoes: u64,
    /// TCP connections accepted by the echo server, and segments it echoed.
    pub tcp_accepts: u64,
    /// Resets sent to segments for ports nothing listens on.
    pub tcp_resets: u64,
    /// Frames ignored: not for the gateway, malformed, or a protocol it does not speak.
    pub ignored: u64,
    /// Replies lost to a full queue or a frame too big for a slot.
    pub lost: u64,
    /// The forward connections' life, in order: SYNs built for the guest, SYNs handed to the
    /// guest's receive buffers, and what came back for a live connection (SYN-ACK, RST) or for
    /// an old one (stale). The 2026-10-22 stall was a SYN that vanished between these.
    pub fwd_syn_out: u64,
    pub rx_syn_delivered: u64,
    pub fwd_synack_in: u64,
    pub fwd_rst_in: u64,
    pub fwd_stale_in: u64,
    /// Frames handed to the guest's receive buffers.
    pub rx_delivered: u64,
}

/// The frames waiting for the guest: a ring of [`DEPTH`] slots in borrowed storage.
#[derive(Debug)]
struct Queue<'a> {
    slots: &'a mut [u8],
    lens: [usize; DEPTH],
    head: usize,
    count: usize,
}

impl Queue<'_> {
    const fn has_room(&self) -> bool {
        self.count < DEPTH
    }

    /// Build a frame in the next free slot with `build` (which returns its length).
    fn push_with(&mut self, build: impl FnOnce(&mut [u8]) -> Option<usize>) -> bool {
        if !self.has_room() {
            return false;
        }
        let at = (self.head + self.count) % DEPTH;
        let Some(n) = build(&mut self.slots[at * SLOT..(at + 1) * SLOT]) else { return false };
        self.lens[at] = n;
        self.count += 1;
        true
    }
}

impl Stats {
    const fn new() -> Self {
        Self {
            frames: 0,
            arp_replies: 0,
            dhcp_offers: 0,
            dhcp_acks: 0,
            icmp_echoes: 0,
            udp_echoes: 0,
            tcp_accepts: 0,
            tcp_resets: 0,
            ignored: 0,
            lost: 0,
            fwd_syn_out: 0,
            rx_syn_delivered: 0,
            fwd_synack_in: 0,
            fwd_rst_in: 0,
            fwd_stale_in: 0,
            rx_delivered: 0,
        }
    }
}

/// The gateway: a queue of frames for the guest, its TCP connections and the counters.
#[derive(Debug)]
pub struct Gateway<'a> {
    q: Queue<'a>,
    /// The guest's hardware address, as last seen on a frame from it.
    guest_mac: [u8; 6],
    /// The TCP echo server's connection, and the guest port it is bound to.
    echo: Tcp<'a>,
    echo_peer: u16,
    /// Connections the gateway opened to the guest, and the guest port each targets.
    fwd: [Tcp<'a>; FWD_SLOTS],
    fwd_peer: [u16; FWD_SLOTS],
    /// The source-port generation each slot's current (or last) connection uses.
    fwd_gen: [u8; FWD_SLOTS],
    /// Next initial sequence number.
    iss: u32,
    pub stats: Stats,
}

/// The Internet checksum (RFC 1071) over `data`, seeded with `seed` (a pseudo-header sum).
fn checksum(data: &[u8], seed: u32) -> u16 {
    let mut sum = seed;
    let mut i = 0;
    while i + 1 < data.len() {
        sum += u32::from(u16::from_be_bytes([data[i], data[i + 1]]));
        i += 2;
    }
    if i < data.len() {
        sum += u32::from(data[i]) << 8;
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    !(sum as u16)
}

/// The sum of the UDP pseudo-header (RFC 768) for a segment of `len` bytes.
fn pseudo_sum(src: [u8; 4], dst: [u8; 4], proto: u8, len: usize) -> u32 {
    let w = |a: [u8; 4]| u32::from(u16::from_be_bytes([a[0], a[1]])) + u32::from(u16::from_be_bytes([a[2], a[3]]));
    w(src) + w(dst) + u32::from(proto) + len as u32
}

fn be16(b: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([b[at], b[at + 1]])
}

/// Write an Ethernet + IPv4 (no options) + UDP frame into `out`; its length, or `None`
/// if it does not fit. The UDP checksum is real, not the optional zero.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_udp(
    out: &mut [u8],
    dst_mac: [u8; 6],
    src_ip: [u8; 4],
    dst_ip: [u8; 4],
    sport: u16,
    dport: u16,
    payload: &[u8],
) -> Option<usize> {
    let udp_len = 8 + payload.len();
    let ip_len = 20 + udp_len;
    let total = 14 + ip_len;
    if total > out.len() {
        return None;
    }
    out[..6].copy_from_slice(&dst_mac);
    out[6..12].copy_from_slice(&GATEWAY_MAC);
    out[12..14].copy_from_slice(&ETH_IP.to_be_bytes());
    let ip = &mut out[14..14 + 20];
    ip[0] = 0x45;
    ip[1] = 0;
    ip[2..4].copy_from_slice(&(ip_len as u16).to_be_bytes());
    ip[4..8].copy_from_slice(&[0, 0, 0x40, 0]); // id 0, don't fragment
    ip[8] = 64;
    ip[9] = 17;
    ip[10..12].fill(0);
    ip[12..16].copy_from_slice(&src_ip);
    ip[16..20].copy_from_slice(&dst_ip);
    let c = checksum(ip, 0);
    ip[10..12].copy_from_slice(&c.to_be_bytes());
    let udp = &mut out[34..34 + udp_len];
    udp[0..2].copy_from_slice(&sport.to_be_bytes());
    udp[2..4].copy_from_slice(&dport.to_be_bytes());
    udp[4..6].copy_from_slice(&(udp_len as u16).to_be_bytes());
    udp[6..8].fill(0);
    udp[8..].copy_from_slice(payload);
    let mut c = checksum(udp, pseudo_sum(src_ip, dst_ip, 17, udp_len));
    // A computed zero is sent as all ones (RFC 768).
    if c == 0 {
        c = 0xFFFF;
    }
    udp[6..8].copy_from_slice(&c.to_be_bytes());
    Some(total)
}

/// Write an Ethernet + IPv4 + TCP frame into `out` for `o`, whose payload (if any) is
/// already at `out[54..54 + o.len]` (where [`Tcp::poll_tx`] was told to put it). Returns
/// the frame's length.
#[allow(clippy::too_many_arguments)]
fn build_tcp(out: &mut [u8], dst_mac: [u8; 6], src_ip: [u8; 4], dst_ip: [u8; 4], sport: u16, dport: u16, o: &tcp::Out) -> usize {
    let hdr = if o.mss_option { 24 } else { 20 };
    let tcp_len = hdr + o.len;
    let ip_len = 20 + tcp_len;
    // The payload sits after a 20-byte TCP header; a SYN (no payload) carries its option
    // in the four bytes after that.
    out[..6].copy_from_slice(&dst_mac);
    out[6..12].copy_from_slice(&GATEWAY_MAC);
    out[12..14].copy_from_slice(&ETH_IP.to_be_bytes());
    {
        let ip = &mut out[14..34];
        ip[0] = 0x45;
        ip[1] = 0;
        ip[2..4].copy_from_slice(&(ip_len as u16).to_be_bytes());
        ip[4..8].copy_from_slice(&[0, 0, 0x40, 0]);
        ip[8] = 64;
        ip[9] = 6;
        ip[10..12].fill(0);
        ip[12..16].copy_from_slice(&src_ip);
        ip[16..20].copy_from_slice(&dst_ip);
        let c = checksum(ip, 0);
        ip[10..12].copy_from_slice(&c.to_be_bytes());
    }
    if o.mss_option {
        out[54..58].copy_from_slice(&[2, 4, (tcp::MSS >> 8) as u8, tcp::MSS as u8]);
    }
    let t = &mut out[34..34 + tcp_len];
    t[0..2].copy_from_slice(&sport.to_be_bytes());
    t[2..4].copy_from_slice(&dport.to_be_bytes());
    t[4..8].copy_from_slice(&o.seq.to_be_bytes());
    t[8..12].copy_from_slice(&o.ack.to_be_bytes());
    t[12] = ((hdr / 4) as u8) << 4;
    t[13] = o.flags;
    t[14..16].copy_from_slice(&o.window.to_be_bytes());
    t[16..20].fill(0);
    let c = checksum(t, pseudo_sum(src_ip, dst_ip, 6, tcp_len));
    t[16..18].copy_from_slice(&c.to_be_bytes());
    14 + ip_len
}

/// The DHCP option with `code` in `opts` (after the magic cookie), as its value bytes.
fn dhcp_option(opts: &[u8], code: u8) -> Option<&[u8]> {
    let mut i = 0;
    while i < opts.len() {
        match opts[i] {
            0 => i += 1,
            255 => return None,
            c => {
                let len = usize::from(*opts.get(i + 1)?);
                let val = opts.get(i + 2..i + 2 + len)?;
                if c == code {
                    return Some(val);
                }
                i += 2 + len;
            }
        }
    }
    None
}

impl<'a> Gateway<'a> {
    /// A gateway with nothing queued, in `storage` ([`STORAGE`] bytes or more; the excess
    /// is unused). `None` if it is too small.
    #[must_use]
    pub fn new(storage: &'a mut [u8]) -> Option<Self> {
        if storage.len() < STORAGE {
            return None;
        }
        let (slots, mut rest) = storage.split_at_mut(DEPTH * SLOT);
        let mut ring = || {
            let (r, tail) = core::mem::take(&mut rest).split_at_mut(RING);
            rest = tail;
            r
        };
        let mut conn = || {
            let rx = ring();
            let tx = ring();
            Tcp::new(rx, tx)
        };
        let echo = conn();
        let fwd = [conn(), conn()];
        Some(Self {
            q: Queue { slots, lens: [0; DEPTH], head: 0, count: 0 },
            guest_mac: GUEST_MAC,
            echo,
            echo_peer: 0,
            fwd,
            fwd_peer: [0; FWD_SLOTS],
            fwd_gen: [0; FWD_SLOTS],
            iss: 0x4B1D_0000,
            stats: Stats::new(),
        })
    }

    /// Build a reply in the next free slot with `build`, which returns its length.
    fn enqueue(&mut self, build: impl FnOnce(&mut [u8]) -> Option<usize>) -> bool {
        let ok = self.q.push_with(build);
        if !ok {
            self.stats.lost += 1;
        }
        ok
    }

    fn next_iss(&mut self) -> u32 {
        self.iss = self.iss.wrapping_add(0x1_0007);
        self.iss
    }

    fn arp(&mut self, f: &[u8]) {
        // Ethernet/IPv4 request: hardware 1, protocol 0x0800, sizes 6 and 4, opcode 1.
        if f.len() < 42 || be16(f, 14) != 1 || be16(f, 16) != ETH_IP || f[18] != 6 || f[19] != 4 || be16(f, 20) != 1 {
            self.stats.ignored += 1;
            return;
        }
        let target = [f[38], f[39], f[40], f[41]];
        if target != GATEWAY_IP && target != DNS_IP {
            self.stats.ignored += 1;
            return;
        }
        let (sender_mac, sender_ip) = ([f[22], f[23], f[24], f[25], f[26], f[27]], [f[28], f[29], f[30], f[31]]);
        let ok = self.enqueue(|o| {
            o[..6].copy_from_slice(&sender_mac);
            o[6..12].copy_from_slice(&GATEWAY_MAC);
            o[12..14].copy_from_slice(&ETH_ARP.to_be_bytes());
            o[14..20].copy_from_slice(&[0, 1, 8, 0, 6, 4]);
            o[20..22].copy_from_slice(&2u16.to_be_bytes());
            o[22..28].copy_from_slice(&GATEWAY_MAC);
            o[28..32].copy_from_slice(&target);
            o[32..38].copy_from_slice(&sender_mac);
            o[38..42].copy_from_slice(&sender_ip);
            Some(42)
        });
        if ok {
            self.stats.arp_replies += 1;
        }
    }

    fn ipv4(&mut self, f: &[u8]) {
        if f.len() < 34 || f[14] >> 4 != 4 {
            self.stats.ignored += 1;
            return;
        }
        let ihl = usize::from(f[14] & 0xF) * 4;
        let total = usize::from(be16(f, 16));
        // A header that does not check, a length that overruns the frame (or is shorter
        // than the header), or a fragment: not for us.
        if ihl < 20 || total < ihl || 14 + total > f.len() || checksum(&f[14..14 + ihl], 0) != 0 || be16(f, 20) & 0x3FFF != 0 {
            self.stats.ignored += 1;
            return;
        }
        let dst = [f[30], f[31], f[32], f[33]];
        let src = [f[26], f[27], f[28], f[29]];
        let payload = &f[14 + ihl..14 + total];
        match f[23] {
            1 if dst == GATEWAY_IP => self.icmp(f, ihl, total),
            17 => self.udp(f, src, dst, payload),
            6 if dst == GATEWAY_IP => self.tcp_in(src, payload),
            _ => self.stats.ignored += 1,
        }
    }

    fn icmp(&mut self, f: &[u8], ihl: usize, total: usize) {
        let icmp = &f[14 + ihl..14 + total];
        // Echo request, code 0, with a good checksum.
        if icmp.len() < 8 || icmp[0] != 8 || icmp[1] != 0 || checksum(icmp, 0) != 0 {
            self.stats.ignored += 1;
            return;
        }
        let ok = self.enqueue(|o| {
            let n = 14 + total;
            if n > o.len() {
                return None;
            }
            o[..n].copy_from_slice(&f[..n]);
            // Back to the sender: Ethernet and IP addresses swap, the TTL is ours.
            o[..6].copy_from_slice(&f[6..12]);
            o[6..12].copy_from_slice(&GATEWAY_MAC);
            o[26..30].copy_from_slice(&f[30..34]);
            o[30..34].copy_from_slice(&f[26..30]);
            o[22] = 64;
            o[24..26].fill(0);
            let c = checksum(&o[14..14 + ihl], 0);
            o[24..26].copy_from_slice(&c.to_be_bytes());
            // Type 0 (echo reply); the identifier, sequence and data are echoed.
            let icmp = &mut o[14 + ihl..n];
            icmp[0] = 0;
            icmp[2..4].fill(0);
            let c = checksum(icmp, 0);
            icmp[2..4].copy_from_slice(&c.to_be_bytes());
            Some(n)
        });
        if ok {
            self.stats.icmp_echoes += 1;
        }
    }

    fn udp(&mut self, f: &[u8], src: [u8; 4], dst: [u8; 4], seg: &[u8]) {
        if seg.len() < 8 || usize::from(be16(seg, 4)) != seg.len() {
            self.stats.ignored += 1;
            return;
        }
        // A nonzero UDP checksum must verify.
        if be16(seg, 6) != 0 && checksum(seg, pseudo_sum(src, dst, 17, seg.len())) != 0 {
            self.stats.ignored += 1;
            return;
        }
        let (sport, dport, data) = (be16(seg, 0), be16(seg, 2), &seg[8..]);
        let client_mac = [f[6], f[7], f[8], f[9], f[10], f[11]];
        match dport {
            67 => self.dhcp(data),
            7 if dst == GATEWAY_IP => {
                let ok = self.enqueue(|o| build_udp(o, client_mac, GATEWAY_IP, src, 7, sport, data));
                if ok {
                    self.stats.udp_echoes += 1;
                }
            }
            _ => self.stats.ignored += 1,
        }
    }

    fn dhcp(&mut self, d: &[u8]) {
        // BOOTP request, Ethernet, then the magic cookie at 236.
        if d.len() < 240 || d[0] != 1 || d[1] != 1 || d[2] != 6 || d[236..240] != DHCP_COOKIE {
            self.stats.ignored += 1;
            return;
        }
        let reply = match dhcp_option(&d[240..], 53) {
            Some([DHCP_DISCOVER]) => DHCP_OFFER,
            Some([DHCP_REQUEST]) => DHCP_ACK,
            _ => {
                self.stats.ignored += 1;
                return;
            }
        };
        let (xid, flags, chaddr) = ([d[4], d[5], d[6], d[7]], [d[10], d[11]], &d[28..44]);
        let ok = self.enqueue(|o| {
            // The BOOTP body is built apart: `build_udp` copies it into the frame.
            let mut body = [0u8; 300];
            body[0] = 2;
            body[1] = 1;
            body[2] = 6;
            body[4..8].copy_from_slice(&xid);
            body[10..12].copy_from_slice(&flags);
            body[16..20].copy_from_slice(&GUEST_IP); // yiaddr
            body[20..24].copy_from_slice(&GATEWAY_IP); // siaddr
            body[28..44].copy_from_slice(chaddr);
            body[236..240].copy_from_slice(&DHCP_COOKIE);
            let mut n = 240;
            let mut opt = |code: u8, val: &[u8]| {
                body[n] = code;
                body[n + 1] = val.len() as u8;
                body[n + 2..n + 2 + val.len()].copy_from_slice(val);
                n += 2 + val.len();
            };
            opt(53, &[reply]);
            opt(54, &GATEWAY_IP);
            opt(51, &LEASE_SECS.to_be_bytes());
            opt(1, &NETMASK);
            opt(3, &GATEWAY_IP);
            opt(6, &DNS_IP);
            opt(28, &BROADCAST_IP);
            body[n] = 255;
            // The client has no address yet: broadcast, as every server does.
            build_udp(o, BROADCAST_MAC, GATEWAY_IP, [255; 4], 67, 68, &body)
        });
        if ok {
            if reply == DHCP_OFFER {
                self.stats.dhcp_offers += 1;
            } else {
                self.stats.dhcp_acks += 1;
            }
        }
    }
    /// A TCP segment for the gateway (`seg` is the IP payload; the IP header was checked).
    fn tcp_in(&mut self, src: [u8; 4], seg: &[u8]) {
        if seg.len() < 20 || checksum(seg, pseudo_sum(src, GATEWAY_IP, 6, seg.len())) != 0 {
            self.stats.ignored += 1;
            return;
        }
        let off = usize::from(seg[12] >> 4) * 4;
        if off < 20 || off > seg.len() {
            self.stats.ignored += 1;
            return;
        }
        let (sport, dport) = (be16(seg, 0), be16(seg, 2));
        let s = Segment {
            seq: u32::from_be_bytes([seg[4], seg[5], seg[6], seg[7]]),
            ack: u32::from_be_bytes([seg[8], seg[9], seg[10], seg[11]]),
            flags: seg[13],
            window: be16(seg, 14),
            payload: &seg[off..],
        };
        if dport == ECHO_PORT {
            let fresh = self.echo.state() == tcp::State::Closed && s.flags & tcp::flag::SYN != 0;
            if fresh {
                let iss = self.next_iss();
                if self.echo.accept(&s, iss) {
                    self.echo_peer = sport;
                    self.stats.tcp_accepts += 1;
                }
            } else if self.echo.state() != tcp::State::Closed && sport == self.echo_peer {
                self.echo.input(&s);
            } else {
                self.reset_for(src, sport, dport, &s);
            }
        } else if (FWD_PORT_BASE..FWD_PORT_BASE + (FWD_SLOTS * FWD_GENS) as u16).contains(&dport) {
            let off = usize::from(dport - FWD_PORT_BASE);
            let i = off % FWD_SLOTS;
            // Only the slot's current generation is live; an older one is a stale connection.
            if off / FWD_SLOTS == usize::from(self.fwd_gen[i]) && self.fwd[i].state() != tcp::State::Closed && sport == self.fwd_peer[i] {
                if s.flags & tcp::flag::RST != 0 {
                    self.stats.fwd_rst_in += 1;
                } else if s.flags & (tcp::flag::SYN | tcp::flag::ACK) == (tcp::flag::SYN | tcp::flag::ACK) {
                    self.stats.fwd_synack_in += 1;
                }
                self.fwd[i].input(&s);
            } else {
                self.stats.fwd_stale_in += 1;
                self.reset_for(src, sport, dport, &s);
            }
        } else {
            self.reset_for(src, sport, dport, &s);
        }
    }

    /// Answer a segment for a connection that does not exist with a reset (RFC 793 p.65),
    /// unless it is itself a reset.
    fn reset_for(&mut self, src: [u8; 4], sport: u16, dport: u16, s: &Segment<'_>) {
        if s.flags & tcp::flag::RST != 0 {
            return;
        }
        let o = if s.flags & tcp::flag::ACK != 0 {
            tcp::Out { seq: s.ack, ack: 0, flags: tcp::flag::RST, window: 0, len: 0, mss_option: false }
        } else {
            let seg_len = s.payload.len() as u32 + u32::from(s.flags & tcp::flag::SYN != 0) + u32::from(s.flags & tcp::flag::FIN != 0);
            tcp::Out { seq: 0, ack: s.seq.wrapping_add(seg_len), flags: tcp::flag::RST | tcp::flag::ACK, window: 0, len: 0, mss_option: false }
        };
        let mac = self.guest_mac;
        if self.enqueue(|out| Some(build_tcp(out, mac, GATEWAY_IP, src, dport, sport, &o))) {
            self.stats.tcp_resets += 1;
        }
    }

    /// Move the echo connection's received bytes back to its sender, and close it when the
    /// peer does.
    fn echo_step(&mut self) {
        let mut tmp = [0u8; 512];
        loop {
            let room = self.echo.write_room().min(tmp.len());
            if room == 0 || self.echo.readable() == 0 {
                break;
            }
            let n = self.echo.read(&mut tmp[..room]);
            if n == 0 {
                break;
            }
            self.echo.write(&tmp[..n]);
        }
        if self.echo.peer_finished() {
            self.echo.close();
        }
        if self.echo.is_closed() {
            self.echo.clear();
        }
    }

    /// Send what the TCP connections have to say, while the queue has room. The hypervisor
    /// calls this once per turn (and after every transmit); the TCP side is pulled, so it
    /// never loses a frame to a full queue.
    pub fn pump(&mut self) {
        self.echo_step();
        let mac = self.guest_mac;
        let echo_peer = self.echo_peer;
        while self.q.has_room() && self.echo.wants_tx() {
            let echo = &mut self.echo;
            let pushed = self.q.push_with(|out| {
                let o = echo.poll_tx(&mut out[54..54 + tcp::MSS])?;
                Some(build_tcp(out, mac, GATEWAY_IP, GUEST_IP, ECHO_PORT, echo_peer, &o))
            });
            if !pushed {
                break;
            }
        }
        for i in 0..FWD_SLOTS {
            let peer = self.fwd_peer[i];
            let sport = self.fwd_port(i);
            while self.q.has_room() && self.fwd[i].wants_tx() {
                let conn = &mut self.fwd[i];
                let mut syn = false;
                let pushed = self.q.push_with(|out| {
                    let o = conn.poll_tx(&mut out[54..54 + tcp::MSS])?;
                    syn = o.flags & (tcp::flag::SYN | tcp::flag::ACK) == tcp::flag::SYN;
                    Some(build_tcp(out, mac, GATEWAY_IP, GUEST_IP, sport, peer, &o))
                });
                if !pushed {
                    break;
                }
                if syn {
                    self.stats.fwd_syn_out += 1;
                }
            }
        }
    }

    /// Open forward slot `slot` to `guest_port` on the guest: the SYN goes out at the next
    /// [`Gateway::pump`]. False if the slot is in use.
    pub fn fwd_open(&mut self, slot: usize, guest_port: u16) -> bool {
        if slot >= FWD_SLOTS || self.fwd[slot].state() != tcp::State::Closed || !self.fwd[slot].is_closed() {
            return false;
        }
        let iss = self.next_iss();
        self.fwd_peer[slot] = guest_port;
        self.fwd_gen[slot] = ((usize::from(self.fwd_gen[slot]) + 1) % FWD_GENS) as u8;
        self.fwd[slot].connect(iss);
        true
    }

    /// The source port forward slot `slot`'s current connection uses.
    fn fwd_port(&self, slot: usize) -> u16 {
        FWD_PORT_BASE + (slot + FWD_SLOTS * usize::from(self.fwd_gen[slot])) as u16
    }

    /// Start slot `slot`'s connection over, from a new source port, to the same guest port: the
    /// SYN is sent once, so one the guest never answered is retried by the caller. The old
    /// attempt is reset (the guest may be holding it half-open). False if the slot is idle.
    pub fn fwd_reopen(&mut self, slot: usize) -> bool {
        if slot >= FWD_SLOTS || self.fwd[slot].state() == tcp::State::Closed {
            return false;
        }
        let peer = self.fwd_peer[slot];
        self.fwd_release(slot);
        self.fwd_open(slot, peer)
    }

    /// Frames waiting for the guest.
    #[must_use]
    pub fn queued(&self) -> usize {
        self.q.count
    }

    /// Forward slot `slot`'s connection, for reading, writing, closing and asking its state.
    pub fn fwd(&mut self, slot: usize) -> &mut Tcp<'a> {
        &mut self.fwd[slot]
    }

    /// Free slot `slot` once its connection is closed (or abandon it with a reset).
    pub fn fwd_release(&mut self, slot: usize) {
        self.fwd[slot].abort();
        self.pump();
        if self.fwd[slot].is_closed() {
            self.fwd[slot].clear();
        }
    }
}

impl NetBackend for Gateway<'_> {
    fn transmit(&mut self, frame: &[u8]) {
        self.stats.frames += 1;
        if frame.len() < 14 {
            self.stats.ignored += 1;
            return;
        }
        // The guest's address, for frames the gateway starts (forward connections).
        self.guest_mac = [frame[6], frame[7], frame[8], frame[9], frame[10], frame[11]];
        match be16(frame, 12) {
            ETH_ARP => self.arp(frame),
            ETH_IP => self.ipv4(frame),
            _ => self.stats.ignored += 1,
        }
        self.pump();
    }

    fn has_frame(&self) -> bool {
        self.q.count > 0
    }

    fn receive(&mut self, dst: &mut [u8]) -> Option<usize> {
        if self.q.count == 0 {
            return None;
        }
        let at = self.q.head;
        let n = self.q.lens[at];
        let fits = n <= dst.len();
        if fits {
            dst[..n].copy_from_slice(&self.q.slots[at * SLOT..at * SLOT + n]);
        }
        self.q.head = (at + 1) % DEPTH;
        self.q.count -= 1;
        if fits {
            self.stats.rx_delivered += 1;
            // An IPv4 TCP segment with SYN set and ACK clear (Ethernet 14 + IP 20, flags at 47).
            if n >= 48 && dst[12] == 0x08 && dst[13] == 0x00 && dst[23] == 6 && dst[47] & 0x12 == 0x02 {
                self.stats.rx_syn_delivered += 1;
            }
        }
        Some(if fits { n } else { 0 })
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::vec;
    use std::vec::Vec;

    fn gw() -> Gateway<'static> {
        let storage: &'static mut [u8] = std::boxed::Box::leak(vec![0u8; STORAGE].into_boxed_slice());
        Gateway::new(storage).expect("enough storage")
    }

    /// RFC 1071 verification by an independent route: the ones-complement sum of the
    /// words, checksum included, is 0xFFFF.
    fn sums_to_ffff(data: &[u8], seed: u32) -> bool {
        let mut s = u64::from(seed);
        for pair in data.chunks(2) {
            s += u64::from(pair[0]) << 8 | pair.get(1).map_or(0, |&b| u64::from(b));
        }
        while s >> 16 != 0 {
            s = (s & 0xFFFF) + (s >> 16);
        }
        s == 0xFFFF
    }

    fn frame_from_guest(proto_payload: &[u8], proto: u8, dst_ip: [u8; 4], dst_mac: [u8; 6]) -> Vec<u8> {
        let mut f = Vec::new();
        f.extend_from_slice(&dst_mac);
        f.extend_from_slice(&GUEST_MAC);
        f.extend_from_slice(&ETH_IP.to_be_bytes());
        let total = 20 + proto_payload.len();
        let mut ip = vec![0x45, 0, (total >> 8) as u8, total as u8, 0, 1, 0, 0, 64, proto, 0, 0];
        ip.extend_from_slice(&[0, 0, 0, 0]);
        ip.extend_from_slice(&dst_ip);
        let c = checksum(&ip, 0);
        ip[10..12].copy_from_slice(&c.to_be_bytes());
        f.extend_from_slice(&ip);
        f.extend_from_slice(proto_payload);
        f
    }

    fn set_src(f: &mut [u8], src: [u8; 4]) {
        f[26..30].copy_from_slice(&src);
        f[24..26].fill(0);
        let c = checksum(&f[14..34], 0);
        f[24..26].copy_from_slice(&c.to_be_bytes());
    }

    fn udp_from_guest(src: [u8; 4], dst: [u8; 4], sport: u16, dport: u16, data: &[u8]) -> Vec<u8> {
        let mut u = vec![];
        u.extend_from_slice(&sport.to_be_bytes());
        u.extend_from_slice(&dport.to_be_bytes());
        u.extend_from_slice(&((8 + data.len()) as u16).to_be_bytes());
        u.extend_from_slice(&[0, 0]);
        u.extend_from_slice(data);
        let c = checksum(&u, pseudo_sum(src, dst, 17, u.len()));
        u[6..8].copy_from_slice(&c.to_be_bytes());
        let mut f = frame_from_guest(&u, 17, dst, if dst == [255; 4] { BROADCAST_MAC } else { GATEWAY_MAC });
        set_src(&mut f, src);
        f
    }

    fn arp_request(target: [u8; 4]) -> Vec<u8> {
        let mut f = vec![];
        f.extend_from_slice(&BROADCAST_MAC);
        f.extend_from_slice(&GUEST_MAC);
        f.extend_from_slice(&ETH_ARP.to_be_bytes());
        f.extend_from_slice(&[0, 1, 8, 0, 6, 4, 0, 1]);
        f.extend_from_slice(&GUEST_MAC);
        f.extend_from_slice(&GUEST_IP);
        f.extend_from_slice(&[0; 6]);
        f.extend_from_slice(&target);
        f
    }

    fn take(g: &mut Gateway) -> Vec<u8> {
        let mut b = [0u8; 2000];
        let n = g.receive(&mut b).expect("a frame is queued");
        b[..n].to_vec()
    }

    /// A DHCP client message: DISCOVER or REQUEST, with the options real clients send.
    fn dhcp_msg(kind: u8, xid: u32) -> Vec<u8> {
        let mut d = vec![0u8; 240];
        d[0] = 1;
        d[1] = 1;
        d[2] = 6;
        d[4..8].copy_from_slice(&xid.to_be_bytes());
        d[10] = 0x80; // broadcast flag
        d[28..34].copy_from_slice(&GUEST_MAC);
        d[236..240].copy_from_slice(&DHCP_COOKIE);
        d.extend_from_slice(&[53, 1, kind]);
        if kind == DHCP_REQUEST {
            d.extend_from_slice(&[50, 4, 10, 0, 2, 15, 54, 4, 10, 0, 2, 2]);
        }
        d.extend_from_slice(&[55, 3, 1, 3, 6, 255]);
        d
    }

    fn parse_reply_udp(f: &[u8]) -> (u16, u16, Vec<u8>) {
        assert_eq!(&f[12..14], &ETH_IP.to_be_bytes());
        assert!(sums_to_ffff(&f[14..34], 0), "IP header checksum");
        let total = usize::from(be16(f, 16));
        assert_eq!(f.len(), 14 + total);
        let udp = &f[34..14 + total];
        let src = [f[26], f[27], f[28], f[29]];
        let dst = [f[30], f[31], f[32], f[33]];
        assert_ne!(be16(udp, 6), 0, "a real UDP checksum");
        assert!(sums_to_ffff(udp, pseudo_sum(src, dst, 17, udp.len())), "UDP checksum");
        (be16(udp, 0), be16(udp, 2), udp[8..].to_vec())
    }

    #[test]
    fn arp_for_the_gateway_and_the_proxy_is_answered_others_are_not() {
        let mut g = gw();
        for target in [GATEWAY_IP, DNS_IP] {
            g.transmit(&arp_request(target));
            let r = take(&mut g);
            assert_eq!(r.len(), 42);
            assert_eq!(&r[..6], &GUEST_MAC);
            assert_eq!(&r[6..12], &GATEWAY_MAC);
            assert_eq!(&r[20..22], &[0, 2], "reply");
            assert_eq!(&r[22..28], &GATEWAY_MAC, "sender hardware address");
            assert_eq!(&r[28..32], &target, "sender protocol address is the asked-for one");
            assert_eq!(&r[32..38], &GUEST_MAC);
            assert_eq!(&r[38..42], &GUEST_IP);
        }
        g.transmit(&arp_request([10, 0, 2, 99]));
        assert!(!g.has_frame());
        assert_eq!((g.stats.arp_replies, g.stats.ignored), (2, 1));
    }

    #[test]
    fn dhcp_discover_gets_an_offer_and_request_an_ack_with_the_slirp_parameters() {
        let mut g = gw();
        for (kind, reply, xid) in [(DHCP_DISCOVER, DHCP_OFFER, 0xDEAD_BEEFu32), (DHCP_REQUEST, DHCP_ACK, 0x1234_5678)] {
            g.transmit(&udp_from_guest([0; 4], [255; 4], 68, 67, &dhcp_msg(kind, xid)));
            let f = take(&mut g);
            assert_eq!(&f[..6], &BROADCAST_MAC);
            assert_eq!(&f[30..34], &[255; 4]);
            assert_eq!(&f[26..30], &GATEWAY_IP);
            let (sport, dport, body) = parse_reply_udp(&f);
            assert_eq!((sport, dport), (67, 68));
            assert!(body.len() >= 300, "BOOTP minimum");
            assert_eq!(body[0], 2, "BOOTREPLY");
            assert_eq!(u32::from_be_bytes(body[4..8].try_into().unwrap()), xid);
            assert_eq!(&body[10..12], &[0x80, 0], "flags echoed");
            assert_eq!(&body[16..20], &GUEST_IP, "yiaddr");
            assert_eq!(&body[28..34], &GUEST_MAC, "chaddr");
            assert_eq!(&body[236..240], &DHCP_COOKIE);
            let o = &body[240..];
            assert_eq!(dhcp_option(o, 53), Some(&[reply][..]));
            assert_eq!(dhcp_option(o, 54), Some(&GATEWAY_IP[..]));
            assert_eq!(dhcp_option(o, 1), Some(&NETMASK[..]));
            assert_eq!(dhcp_option(o, 3), Some(&GATEWAY_IP[..]));
            assert_eq!(dhcp_option(o, 6), Some(&DNS_IP[..]));
            assert_eq!(dhcp_option(o, 51), Some(&LEASE_SECS.to_be_bytes()[..]));
        }
        assert_eq!((g.stats.dhcp_offers, g.stats.dhcp_acks), (1, 1));
    }

    #[test]
    fn dhcp_that_is_not_a_discover_or_request_or_is_malformed_is_ignored() {
        let mut g = gw();
        for kind in [4u8, 7, 8] {
            g.transmit(&udp_from_guest([0; 4], [255; 4], 68, 67, &dhcp_msg(kind, 1)));
        }
        let mut bad_cookie = dhcp_msg(DHCP_DISCOVER, 1);
        bad_cookie[236] = 0;
        g.transmit(&udp_from_guest([0; 4], [255; 4], 68, 67, &bad_cookie));
        g.transmit(&udp_from_guest([0; 4], [255; 4], 68, 67, &[1u8; 100]));
        assert!(!g.has_frame());
        assert_eq!(g.stats.ignored, 5);
    }

    #[test]
    fn dhcp_option_walks_padding_and_stops_at_end() {
        let o = [0, 0, 53, 1, 3, 12, 2, 7, 7, 255, 99, 1, 1];
        assert_eq!(dhcp_option(&o, 53), Some(&[3u8][..]));
        assert_eq!(dhcp_option(&o, 12), Some(&[7u8, 7][..]));
        assert_eq!(dhcp_option(&o, 99), None, "past the end option");
        assert_eq!(dhcp_option(&[53, 9, 1], 53), None, "a length that overruns");
    }

    #[test]
    fn an_echo_request_to_the_gateway_comes_back_as_a_valid_reply() {
        let mut g = gw();
        let data: Vec<u8> = (0..56).collect();
        for len in [0usize, 1, 56] {
            let mut icmp = vec![8, 0, 0, 0, 0x12, 0x34, 0x00, 0x07];
            icmp.extend_from_slice(&data[..len]);
            let c = checksum(&icmp, 0);
            icmp[2..4].copy_from_slice(&c.to_be_bytes());
            let mut f = frame_from_guest(&icmp, 1, GATEWAY_IP, GATEWAY_MAC);
            set_src(&mut f, GUEST_IP);
            g.transmit(&f);
            let r = take(&mut g);
            assert_eq!(&r[..6], &GUEST_MAC);
            assert_eq!(&r[6..12], &GATEWAY_MAC);
            assert_eq!(&r[26..30], &GATEWAY_IP);
            assert_eq!(&r[30..34], &GUEST_IP);
            assert!(sums_to_ffff(&r[14..34], 0), "IP checksum");
            let reply = &r[34..];
            assert_eq!(reply.len(), 8 + len);
            assert_eq!(reply[0], 0, "echo reply");
            assert_eq!(&reply[4..8], &[0x12, 0x34, 0, 7], "identifier and sequence");
            assert_eq!(&reply[8..], &data[..len]);
            assert!(sums_to_ffff(reply, 0), "ICMP checksum");
        }
        assert_eq!(g.stats.icmp_echoes, 3);
    }

    #[test]
    fn a_corrupt_or_misaddressed_echo_is_ignored() {
        let mut g = gw();
        let mut icmp = vec![8, 0, 0, 0, 0, 1, 0, 1];
        let c = checksum(&icmp, 0);
        icmp[2..4].copy_from_slice(&c.to_be_bytes());
        // To some other host.
        let mut f = frame_from_guest(&icmp, 1, [10, 0, 2, 77], GATEWAY_MAC);
        set_src(&mut f, GUEST_IP);
        g.transmit(&f);
        // A bad ICMP checksum.
        let mut bad = icmp.clone();
        bad[2] ^= 0xFF;
        let mut f = frame_from_guest(&bad, 1, GATEWAY_IP, GATEWAY_MAC);
        set_src(&mut f, GUEST_IP);
        g.transmit(&f);
        // A bad IP header checksum.
        let mut f = frame_from_guest(&icmp, 1, GATEWAY_IP, GATEWAY_MAC);
        set_src(&mut f, GUEST_IP);
        f[24] ^= 0xFF;
        g.transmit(&f);
        // A length field that overruns the frame.
        let mut f = frame_from_guest(&icmp, 1, GATEWAY_IP, GATEWAY_MAC);
        set_src(&mut f, GUEST_IP);
        f[16] = 0x10;
        g.transmit(&f);
        assert!(!g.has_frame());
        assert_eq!(g.stats.ignored, 4);
    }

    #[test]
    fn udp_echo_returns_the_payload_from_port_7_to_the_senders_port() {
        let mut g = gw();
        g.transmit(&udp_from_guest(GUEST_IP, GATEWAY_IP, 40_000, 7, b"hello, akuma"));
        let f = take(&mut g);
        assert_eq!(&f[..6], &GUEST_MAC);
        let (sport, dport, body) = parse_reply_udp(&f);
        assert_eq!((sport, dport), (7, 40_000));
        assert_eq!(body, b"hello, akuma");
        // Other ports, and other hosts' port 7, are not ours.
        g.transmit(&udp_from_guest(GUEST_IP, GATEWAY_IP, 1, 9, b"x"));
        g.transmit(&udp_from_guest(GUEST_IP, [10, 0, 2, 3], 1, 7, b"x"));
        assert!(!g.has_frame());
        // A UDP checksum that does not verify is dropped; zero means "none" and is accepted.
        let mut f = udp_from_guest(GUEST_IP, GATEWAY_IP, 1, 7, b"abc");
        f[40] ^= 0x55;
        g.transmit(&f);
        assert!(!g.has_frame());
        let mut f = udp_from_guest(GUEST_IP, GATEWAY_IP, 1, 7, b"abc");
        f[40..42].fill(0);
        g.transmit(&f);
        assert!(g.has_frame());
    }

    #[test]
    fn everything_else_is_counted_and_dropped() {
        let mut g = gw();
        g.transmit(&[1, 2, 3]); // runt
        let mut v6 = arp_request(GATEWAY_IP);
        v6[12..14].copy_from_slice(&0x86DDu16.to_be_bytes());
        g.transmit(&v6);
        let tcp = frame_from_guest(&[0u8; 20], 6, GATEWAY_IP, GATEWAY_MAC);
        g.transmit(&tcp);
        assert!(!g.has_frame());
        assert_eq!((g.stats.frames, g.stats.ignored), (3, 3));
    }

    #[test]
    fn the_queue_is_fifo_bounded_and_a_small_buffer_costs_only_that_frame() {
        let mut g = gw();
        for i in 0..DEPTH + 2 {
            g.transmit(&udp_from_guest(GUEST_IP, GATEWAY_IP, 5000 + i as u16, 7, &[i as u8; 4]));
        }
        assert_eq!(g.stats.udp_echoes, DEPTH as u64);
        assert_eq!(g.stats.lost, 2, "the replies that found no slot");
        // First in, first out, in the order the guest sent them.
        let mut small = [0u8; 10];
        assert_eq!(g.receive(&mut small), Some(0), "too big for the buffer: dropped");
        let (_, dport, _) = parse_reply_udp(&take(&mut g));
        assert_eq!(dport, 5001, "the first reply went with the small buffer");
        // A slot freed, a new reply fits again.
        g.transmit(&udp_from_guest(GUEST_IP, GATEWAY_IP, 6000, 7, b"z"));
        assert_eq!(g.stats.lost, 2);
        let mut last = 0;
        while g.has_frame() {
            last = parse_reply_udp(&take(&mut g)).1;
        }
        assert_eq!(last, 6000);
        assert_eq!(g.receive(&mut small), None);
    }

    #[test]
    fn the_internet_checksum_matches_the_rfc_1071_example() {
        // RFC 1071 §3: the words 0001 f203 f4f5 f6f7 sum to ddf2; the checksum is its complement.
        assert_eq!(checksum(&[0x00, 0x01, 0xF2, 0x03, 0xF4, 0xF5, 0xF6, 0xF7], 0), !0xDDF2u16);
        // An odd length pads with a zero byte.
        assert_eq!(checksum(&[0x12], 0), !0x1200u16);
    }
}
