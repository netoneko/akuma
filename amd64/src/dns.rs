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

/// Public fallback resolvers, tried in order after the configured one.
/// Cloudflare then Google — the two that a `-netdev user` slirp NATs straight
/// through and that a household router almost never blocks.
const FALLBACK_RESOLVERS: [[u8; 4]; 2] = [[1, 1, 1, 1], [8, 8, 8, 8]];

/// The resolvers to try, in order: the one the interface was brought up with
/// first, then [`FALLBACK_RESOLVERS`] (skipping a duplicate of the first).
///
/// The configured resolver used to be a hardcoded `10.0.2.3` — QEMU usermode's
/// fixed DNS proxy, the address `amd64/mkdisk.sh` writes into the guest's
/// `/etc/resolv.conf`. Correct for a VMM, a **black hole on bare metal**, where
/// `amd64/src/net.rs`'s `BARE_METAL_STATIC_V4` seeds it as `1.1.1.1` instead.
/// Now it comes from `akuma_net::smoltcp_net::static_ipv4()` — the single source
/// of truth the smoltcp DNS socket is seeded from too — and the fallbacks cover
/// the other half of the problem: a *configured* resolver that answers some
/// names and `NXDOMAIN`s others (measured 2026-09-06: the HP box's own uplink
/// resolver, reached through slirp, does exactly this for `example.com` while
/// resolving `pool.ntp.org` fine — so `resolve_a` for one name has to be able
/// to walk past it to a server that will answer).
fn resolvers() -> ([[u8; 4]; 3], usize) {
    let configured = akuma_net::smoltcp_net::static_ipv4().dns;
    let mut list = [[0u8; 4]; 3];
    let mut n = 0;
    for r in core::iter::once(configured).chain(FALLBACK_RESOLVERS) {
        if !list[..n].contains(&r) {
            list[n] = r;
            n += 1;
        }
    }
    (list, n)
}

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

/// What a received datagram means for the query in flight.
enum ParseResult {
    /// An A record — resolution is done.
    Answer([u8; 4]),
    /// A valid response from this server with no usable A record: `NXDOMAIN`,
    /// `NOERROR` with an empty/AAAA-only answer, or `SERVFAIL`. Nothing more
    /// will come from *this* server — move to the next resolver rather than
    /// waiting out the timeout.
    NoRecord,
    /// Not an answer to this query (wrong id, QR clear, truncated). Ignore it
    /// and keep waiting.
    Ignore,
}

/// Classify a response datagram against the query `id`.
fn parse_response(resp: &[u8], id: u16) -> ParseResult {
    let bad = ParseResult::Ignore;
    if resp.len() < 12 {
        return bad;
    }
    let Ok(rid) = resp[0..2].try_into().map(u16::from_be_bytes) else { return bad };
    if rid != id {
        return bad;
    }
    let Ok(flags) = resp[2..4].try_into().map(u16::from_be_bytes) else { return bad };
    if flags & 0x8000 == 0 {
        return bad; // QR clear: not a response
    }
    if flags & 0x000F != 0 {
        return ParseResult::NoRecord; // NXDOMAIN / SERVFAIL / REFUSED
    }
    let Ok(qdcount) = resp[4..6].try_into().map(u16::from_be_bytes) else { return bad };
    let Ok(ancount) = resp[6..8].try_into().map(u16::from_be_bytes) else { return bad };

    let mut pos = 12;
    for _ in 0..qdcount {
        let Some(p) = skip_name(resp, pos) else { return bad };
        pos = p + 4; // QTYPE + QCLASS
    }
    for _ in 0..ancount {
        let Some(p) = skip_name(resp, pos) else { return bad };
        pos = p;
        let Some(rtype) = resp.get(pos..pos + 2).and_then(|b| b.try_into().ok()).map(u16::from_be_bytes)
        else {
            return bad;
        };
        let Some(rdlength) = resp
            .get(pos + 8..pos + 10)
            .and_then(|b| b.try_into().ok())
            .map(|b| u16::from_be_bytes(b) as usize)
        else {
            return bad;
        };
        let rdata_start = pos + 10;
        if rtype == 1 && rdlength == 4 {
            if let Some(a) = resp.get(rdata_start..rdata_start + 4) {
                return ParseResult::Answer([a[0], a[1], a[2], a[3]]);
            }
            return bad;
        }
        pos = rdata_start + rdlength;
    }
    ParseResult::NoRecord // a valid NOERROR response, just no A record
}

/// Resolve `hostname` to an IPv4 address, blocking (via `poll`+non-blocking
/// receive, like every other wait loop on this target) for up to `timeout_us`
/// of [`crate::net::uptime_us`] **total**, split evenly across the resolvers in
/// [`resolvers`].
///
/// Within one resolver the query is **retransmitted** every [`RETRANSMIT_US`]:
/// a single lost datagram used to be a failed resolution, which for
/// [`crate::clock::sync_via_sntp`] meant no wall clock and every TLS handshake
/// failing date validation. Across resolvers it walks the list on a timeout or
/// an `NXDOMAIN` — see [`resolvers`] for why a configured resolver that answers
/// some names and not others is a real case here.
pub fn resolve_a(hostname: &str, timeout_us: u64) -> Option<[u8; 4]> {
    let mut query_buf = [0u8; MAX_PACKET];
    // The uptime reading doubles as the query ID's low bits — good enough
    // entropy for "do not accept a stale reply to a different query".
    let id = crate::net::uptime_us() as u16;
    let Some(len) = build_query(id, hostname, &mut query_buf) else {
        fail("name does not encode");
        return None;
    };

    let Some(idx) = akuma_net::socket::alloc_socket(SOCK_DGRAM) else {
        fail("no UDP socket free");
        return None;
    };

    let (list, count) = resolvers();
    let per_server = (timeout_us / count as u64).max(500_000);

    let mut answer = None;
    let mut resp_buf = [0u8; MAX_PACKET];
    'servers: for dns in &list[..count] {
        let server = SocketAddrV4::new(*dns, 53);
        let send_query = || akuma_net::socket::socket_send_udp(idx, &query_buf[..len], server).is_ok();
        if !send_query() {
            continue;
        }
        let start = crate::net::uptime_us();
        let mut last_send = start;
        loop {
            akuma_net::smoltcp_net::poll();
            if let Ok((n, _from)) = akuma_net::socket::socket_recv_udp(idx, &mut resp_buf, true) {
                match parse_response(&resp_buf[..n], id) {
                    ParseResult::Answer(ip) => {
                        answer = Some(ip);
                        break 'servers;
                    }
                    ParseResult::NoRecord => continue 'servers,
                    ParseResult::Ignore => {}
                }
            }
            let now = crate::net::uptime_us();
            if now.saturating_sub(start) > per_server {
                fail_at(server.ip, "no reply before timeout");
                continue 'servers;
            }
            if now.saturating_sub(last_send) > RETRANSMIT_US {
                let _ = send_query();
                last_send = now;
            }
            crate::sched::yield_now();
        }
    }

    akuma_net::socket::remove_socket(idx);
    if answer.is_none() {
        fail("no resolver answered");
    }
    answer
}

/// One line naming why a resolution did not land — the difference between
/// "DNS resolution failed" telling you nothing and telling you where to look.
/// A failed DNS query is already a slow path, so the print is free.
fn fail(why: &str) {
    crate::serial::puts("  dns: ");
    crate::serial::puts(why);
    crate::serial::puts("\n");
}

/// [`fail`] naming the resolver that did not work out.
fn fail_at(ip: [u8; 4], why: &str) {
    crate::serial::puts("  dns: ");
    for (i, o) in ip.iter().enumerate() {
        if i > 0 {
            crate::serial::puts(".");
        }
        crate::serial::put_dec(u64::from(*o));
    }
    crate::serial::puts(": ");
    crate::serial::puts(why);
    crate::serial::puts("\n");
}
