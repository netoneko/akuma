//! A minimal, hand-rolled DNS A-record client over raw UDP.
//!
//! # Why this exists instead of `akuma_net::dns::resolve_host_blocking`
//!
//! That function (`smoltcp_net::dns_query`, driving smoltcp's own dedicated
//! `dns::Socket`) is real, shared, already-tested code — and the first thing
//! tried here. It hung: `clock.rs`'s SNTP bootstrap needs to resolve
//! `pool.ntp.org` before it can do anything, and measured 2026-09-05, a call
//! to `resolve_host_blocking` never returned even after 90 real seconds,
//! with no timeout firing. The **same hostname resolves in under 3 seconds**
//! through this target's *other*, already-proven DNS path — musl's own
//! `sendmsg`/`recvmsg`-based stub resolver in userspace
//! (`docs/archive/AKUMA_FIRECRACKER_AMD64.md` §3.29.3) — so the DNS server
//! and the network are not the problem. Nothing on amd64 had ever called
//! `smoltcp_net::dns_query` before this feature (checked: zero prior call
//! sites in `amd64/src/`), which makes "this specific code path has a real,
//! unexercised bug on this target" a live possibility, not fixed here —
//! tracked in `docs/archive/AKUMA_FIRECRACKER_AMD64.md` §3.30.
//!
//! This module sidesteps it entirely: a single-question A-record query, sent
//! and received over the exact same `akuma_net::socket::{socket_send_udp,
//! socket_recv_udp}` primitives `clock.rs`'s SNTP fetch already uses and
//! already trusts, rather than smoltcp's separate DNS-socket machinery.

use akuma_net::socket::socket_const::SOCK_DGRAM;
use akuma_net::socket::SocketAddrV4;

/// QEMU usermode `-netdev user`'s fixed DNS proxy address — the same one
/// `amd64/mkdisk.sh` writes into the guest's own `/etc/resolv.conf`
/// (`nameserver 10.0.2.3`), and Firecracker's `net-setup.sh` dnsmasq answers
/// on by the same convention.
const DNS_SERVER: SocketAddrV4 = SocketAddrV4::new([10, 0, 2, 3], 53);

/// Largest query or response this client will build or accept. A hostname
/// plus the fixed header/question/answer overhead comfortably fits; a bound
/// rather than trust, like every other length this kernel takes off the wire.
const MAX_PACKET: usize = 256;

/// How often an unanswered query is re-sent within the caller's timeout.
/// One send-and-pray lost the boot's only DNS resolution roughly every other
/// boot (first packet after DHCP, gateway ARP entry not yet resolved) — see
/// [`resolve_a`].
const RETRANSMIT_US: u64 = 1_500_000;

/// Encode `hostname` into DNS label form (`length, bytes, length, bytes, …,
/// 0`) at `buf[pos..]`. Returns the new `pos`, or `None` if it does not fit
/// or a label is too long (63 bytes — DNS's own limit).
fn encode_qname(hostname: &str, buf: &mut [u8], mut pos: usize) -> Option<usize> {
    for label in hostname.split('.') {
        let len = label.len();
        if len == 0 || len > 63 || pos + 1 + len >= buf.len() {
            return None;
        }
        buf[pos] = len as u8;
        buf[pos + 1..pos + 1 + len].copy_from_slice(label.as_bytes());
        pos += 1 + len;
    }
    if pos >= buf.len() {
        return None;
    }
    buf[pos] = 0; // root label
    Some(pos + 1)
}

/// Build a standard recursive single-question A-record query for `hostname`,
/// tagged with `id` (echoed back in the response — the only thing standing
/// between this client and accepting a stale or spoofed UDP datagram, same
/// role `akuma_sntp::sntp`'s marker plays for SNTP).
///
/// Returns the query bytes, or `None` if `hostname` does not fit.
fn build_query(id: u16, hostname: &str, buf: &mut [u8; MAX_PACKET]) -> Option<usize> {
    // Header: ID, flags (0x0100 = recursion desired, standard query),
    // QDCOUNT=1, ANCOUNT=NSCOUNT=ARCOUNT=0.
    buf[0..2].copy_from_slice(&id.to_be_bytes());
    buf[2..4].copy_from_slice(&0x0100u16.to_be_bytes());
    buf[4..6].copy_from_slice(&1u16.to_be_bytes());
    buf[6..8].copy_from_slice(&0u16.to_be_bytes());
    buf[8..10].copy_from_slice(&0u16.to_be_bytes());
    buf[10..12].copy_from_slice(&0u16.to_be_bytes());

    let mut pos = encode_qname(hostname, buf, 12)?;
    if pos + 4 > buf.len() {
        return None;
    }
    buf[pos..pos + 2].copy_from_slice(&1u16.to_be_bytes()); // QTYPE = A
    buf[pos + 2..pos + 4].copy_from_slice(&1u16.to_be_bytes()); // QCLASS = IN
    pos += 4;
    Some(pos)
}

/// Skip one (possibly compressed) DNS name starting at `resp[pos]`, returning
/// the offset just past it. A compression pointer (`0xC0` in the top two
/// bits of the first byte) is a two-byte name in a response and does not
/// need following — the caller only wants where the name *ends* in `resp`,
/// not what it says.
fn skip_name(resp: &[u8], mut pos: usize) -> Option<usize> {
    loop {
        let len = *resp.get(pos)?;
        if len == 0 {
            return Some(pos + 1);
        }
        if len & 0xC0 == 0xC0 {
            // Compression pointer: two bytes total, does not recurse further
            // in the caller's cursor.
            resp.get(pos + 1)?;
            return Some(pos + 2);
        }
        pos = pos.checked_add(1 + usize::from(len))?;
    }
}

/// Parse a response, validating it answers `id`, and return the first A
/// record's address.
fn parse_response(resp: &[u8], id: u16) -> Option<[u8; 4]> {    if resp.len() < 12 {
        return None;
    }
    if u16::from_be_bytes(resp[0..2].try_into().ok()?) != id {
        return None;
    }
    let flags = u16::from_be_bytes(resp[2..4].try_into().ok()?);
    if flags & 0x8000 == 0 {
        return None; // QR bit clear: not a response
    }
    if flags & 0x000F != 0 {
        return None; // RCODE != 0: server-side error (NXDOMAIN and friends)
    }
    let qdcount = u16::from_be_bytes(resp[4..6].try_into().ok()?);
    let ancount = u16::from_be_bytes(resp[6..8].try_into().ok()?);

    // Skip the question section (echoed back) to reach the answers.
    let mut pos = 12;
    for _ in 0..qdcount {
        pos = skip_name(resp, pos)?;
        pos += 4; // QTYPE + QCLASS
    }

    for _ in 0..ancount {
        pos = skip_name(resp, pos)?;
        let rtype = u16::from_be_bytes(resp.get(pos..pos + 2)?.try_into().ok()?);
        let rdlength = u16::from_be_bytes(resp.get(pos + 8..pos + 10)?.try_into().ok()?) as usize;
        let rdata_start = pos + 10;
        if rtype == 1 && rdlength == 4 {
            // TYPE A, a real 4-byte IPv4 address.
            let addr = resp.get(rdata_start..rdata_start + 4)?;
            return Some([addr[0], addr[1], addr[2], addr[3]]);
        }
        pos = rdata_start + rdlength;
    }
    None
}

/// Resolve `hostname` to an IPv4 address, blocking (via `poll`+non-blocking
/// receive, like every other wait loop on this target) for up to
/// `timeout_us` of [`crate::net::uptime_us`].
///
/// The query is **retransmitted** every [`RETRANSMIT_US`] until the timeout:
/// this used to be a single send, and a single lost datagram was a failed
/// resolution — which for [`crate::clock::sync_via_sntp`] meant "no wall
/// clock this boot", which meant every TLS certificate in sight failed date
/// validation. The likeliest loss is the very first packet after boot, while
/// the gateway's ARP entry is still being resolved; a 1.5 s retransmit rides
/// through that and through ordinary datagram loss alike.
pub fn resolve_a(hostname: &str, timeout_us: u64) -> Option<[u8; 4]> {
    let mut query_buf = [0u8; MAX_PACKET];
    // The uptime reading doubles as the query ID's low bits — good enough
    // entropy for "do not accept a stale reply to a different query", which
    // is all a 16-bit ID buys against an off-path guess anyway.
    let id = crate::net::uptime_us() as u16;
    let len = build_query(id, hostname, &mut query_buf)?;

    let idx = akuma_net::socket::alloc_socket(SOCK_DGRAM)?;
    let send_query = || akuma_net::socket::socket_send_udp(idx, &query_buf[..len], DNS_SERVER).is_ok();

    let result = if send_query() {
        let start = crate::net::uptime_us();
        let mut last_send = start;
        let mut resp_buf = [0u8; MAX_PACKET];
        loop {
            akuma_net::smoltcp_net::poll();
            if let Ok((n, _from)) = akuma_net::socket::socket_recv_udp(idx, &mut resp_buf, true) {
                break parse_response(&resp_buf[..n], id);
            }
            let now = crate::net::uptime_us();
            if now.saturating_sub(start) > timeout_us {
                break None;
            }
            if now.saturating_sub(last_send) > RETRANSMIT_US {
                let _ = send_query();
                last_send = now;
            }
            crate::sched::yield_now();
        }
    } else {
        None
    };

    akuma_net::socket::remove_socket(idx);
    result
}
