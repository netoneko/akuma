// This crate is unsafe-free by design and stays that way: `forbid` (not
// `deny`) so no module can opt back in with a local `allow`.
#![forbid(unsafe_code)]
#![no_std]
//! A single-question DNS A-record client, as pure logic.
//!
//! Two halves, both host-tested:
//!
//! * the **wire format** — [`build_query`] and [`parse_response`]; and
//! * the **loop** — [`resolve_a`], which walks a resolver list, retransmits an
//!   unanswered query, and moves on at once when a server answers "no such
//!   record" instead of waiting out the timeout. It takes only effects
//!   ([`Effects`]), the shape `akuma-sntp`'s `bootstrap_over_udp` and
//!   `akuma-net-yarn`'s wait loop already use, so the caller wires the real
//!   sockets and clock and this crate never names a network stack.
//!
//! # Why it is a crate
//!
//! It began as `amd64/src/dns.rs`, written 2026-09-05 to route around
//! `smoltcp_net::dns_query`, which had hung on its first amd64 caller. That
//! left two resolvers: smoltcp's DNS socket (AArch64, and every kernel-side
//! caller) and this one (amd64), with different behaviour where it matters —
//! smoltcp's returns `Failed` on the first `NXDOMAIN` and has one configured
//! server, while a resolver that answers some names and `NXDOMAIN`s others is a
//! measured case on the HP box (`example.com` through its uplink, 2026-09-06)
//! and again on QEMU slirp (2026-10-18). Two implementations of one protocol,
//! one of them with no host test, is the drift the shared crates exist to stop;
//! `akuma_net::dns::resolve_ipv4` now drives this on every target.

/// Largest query or response this client will build or accept.
///
/// A hostname plus the fixed header/question/answer overhead comfortably fits; a
/// bound rather than trust, like every other length taken off the wire.
pub const MAX_PACKET: usize = 256;

/// How often an unanswered query is re-sent to the same server.
///
/// One send-and-pray lost the boot's only resolution roughly every other boot
/// (the first packet after DHCP, the gateway's ARP entry not yet resolved).
pub const RETRANSMIT_US: u64 = 1_500_000;

/// The least time any one server is given, however small the caller's total.
pub const MIN_PER_SERVER_US: u64 = 500_000;

/// Encode `hostname` into DNS label form (`length, bytes, …, 0`) at
/// `buf[pos..]`. Returns the new `pos`, or `None` if it does not fit or a label
/// is empty or longer than DNS's 63-byte limit.
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
/// tagged `id` (echoed back in the response — the only thing standing between
/// this client and a stale or spoofed datagram).
///
/// Returns the query's length, or `None` if `hostname` does not encode.
pub fn build_query(id: u16, hostname: &str, buf: &mut [u8; MAX_PACKET]) -> Option<usize> {
    // Header: ID, flags (0x0100 = recursion desired, standard query),
    // QDCOUNT=1, ANCOUNT=NSCOUNT=ARCOUNT=0.
    buf[0..2].copy_from_slice(&id.to_be_bytes());
    buf[2..4].copy_from_slice(&0x0100u16.to_be_bytes());
    buf[4..6].copy_from_slice(&1u16.to_be_bytes());
    buf[6..12].fill(0);

    let mut pos = encode_qname(hostname, buf, 12)?;
    if pos + 4 > buf.len() {
        return None;
    }
    buf[pos..pos + 2].copy_from_slice(&1u16.to_be_bytes()); // QTYPE = A
    buf[pos + 2..pos + 4].copy_from_slice(&1u16.to_be_bytes()); // QCLASS = IN
    pos += 4;
    Some(pos)
}

/// Skip one (possibly compressed) name at `resp[pos]`, returning the offset
/// just past it. A compression pointer is two bytes in a response and is not
/// followed — the caller wants where the name *ends*, not what it says.
fn skip_name(resp: &[u8], mut pos: usize) -> Option<usize> {
    loop {
        let len = *resp.get(pos)?;
        if len == 0 {
            return Some(pos + 1);
        }
        if len & 0xC0 == 0xC0 {
            resp.get(pos + 1)?;
            return Some(pos + 2);
        }
        pos = pos.checked_add(1 + usize::from(len))?;
    }
}

/// What a received datagram means for the query in flight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reply {
    /// An A record — resolution is done.
    Answer([u8; 4]),
    /// A valid response from this server with no usable A record: `NXDOMAIN`,
    /// `NOERROR` with an empty or AAAA-only answer, `SERVFAIL`, `REFUSED`.
    /// Nothing more will come from *this* server.
    NoRecord,
    /// Not an answer to this query (wrong id, QR clear, truncated or malformed).
    /// Ignore it and keep waiting.
    Ignore,
}

/// Classify a response datagram against the query `id`.
#[must_use]
pub fn parse_response(resp: &[u8], id: u16) -> Reply {
    let bad = Reply::Ignore;
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
        return Reply::NoRecord; // NXDOMAIN / SERVFAIL / REFUSED
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
                return Reply::Answer([a[0], a[1], a[2], a[3]]);
            }
            return bad;
        }
        pos = rdata_start + rdlength;
    }
    Reply::NoRecord // a valid NOERROR response, just no A record
}

/// What the loop reports about one server as it goes.
///
/// For a caller that wants to say *which* resolver let it down: a failed lookup
/// is a slow path, so the print is free, and "DNS failed" names nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Note {
    /// The send to this server failed (no route, no socket buffer).
    SendFailed([u8; 4]),
    /// This server answered, with no usable A record; moving to the next.
    NoRecord([u8; 4]),
    /// This server did not answer within its share of the budget.
    NoReply([u8; 4]),
}

/// The effects [`resolve_a`] needs. All borrowed mutably so the caller's
/// closures can capture a socket handle by value.
pub struct Effects<'a> {
    /// Send one datagram to the resolver at port 53. `false` on any failure.
    pub send: &'a mut dyn FnMut([u8; 4], &[u8]) -> bool,
    /// Non-blocking receive: `Some(n)` with the datagram at the front of the
    /// buffer, `None` if nothing is there. Must not block.
    pub recv: &'a mut dyn FnMut(&mut [u8]) -> Option<usize>,
    /// Drive the network stack's poll/RX-drain step, once per iteration.
    pub poll_network: &'a mut dyn FnMut(),
    /// Monotonic microseconds.
    pub uptime_us: &'a mut dyn FnMut() -> u64,
    /// Give up the CPU for one iteration.
    pub yield_now: &'a mut dyn FnMut(),
    /// Told about each server that did not work out. Optional in spirit: pass a
    /// closure that ignores it.
    pub note: &'a mut dyn FnMut(Note),
}

/// Why [`resolve_a`] gave up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolveError {
    /// The name does not encode: an empty or over-long label, or too long to fit
    /// [`MAX_PACKET`].
    BadName,
    /// The resolver list was empty.
    NoResolvers,
    /// Every server was reached and none had an A record for the name.
    NoRecord,
    /// At least one server never answered and none produced a record.
    Timeout,
    /// Every send failed, so nothing was ever asked.
    SendFailed,
}

/// Resolve `hostname` to an IPv4 address.
///
/// `timeout_us` is the **total** budget, split evenly across `resolvers` with a
/// floor of [`MIN_PER_SERVER_US`] each. Within a server the query is
/// retransmitted every [`RETRANSMIT_US`]. Across servers the walk advances on a
/// timeout **or** an answer without a record — a configured resolver that
/// `NXDOMAIN`s a name another resolves is a real case, and waiting out the
/// timeout on it would turn a one-second lookup into a ten-second one.
///
/// `id` tags the query; the caller supplies it so this crate needs no entropy
/// source (the uptime reading is enough to refuse a stale reply to a different
/// query). A literal address or `localhost` is **not** handled here: those are
/// answers, not questions, and the caller's `resolve_literal` owns them.
///
/// # Errors
///
/// See [`ResolveError`]. When servers disagree, [`ResolveError::Timeout`]
/// outranks [`ResolveError::NoRecord`]: a server that never spoke leaves the
/// name's existence open, and "does not exist" would claim more than was learned.
pub fn resolve_a(
    effects: &mut Effects,
    hostname: &str,
    resolvers: &[[u8; 4]],
    timeout_us: u64,
    id: u16,
) -> Result<[u8; 4], ResolveError> {
    if resolvers.is_empty() {
        return Err(ResolveError::NoResolvers);
    }
    let mut query = [0u8; MAX_PACKET];
    let len = build_query(id, hostname, &mut query).ok_or(ResolveError::BadName)?;
    let per_server = (timeout_us / resolvers.len() as u64).max(MIN_PER_SERVER_US);

    let mut resp = [0u8; MAX_PACKET];
    let mut any_sent = false;
    let mut any_timeout = false;
    for &server in resolvers {
        if !(effects.send)(server, &query[..len]) {
            (effects.note)(Note::SendFailed(server));
            continue;
        }
        any_sent = true;
        let start = (effects.uptime_us)();
        let mut last_send = start;
        loop {
            (effects.poll_network)();
            if let Some(n) = (effects.recv)(&mut resp) {
                match parse_response(&resp[..n.min(resp.len())], id) {
                    Reply::Answer(ip) => return Ok(ip),
                    Reply::NoRecord => {
                        (effects.note)(Note::NoRecord(server));
                        break;
                    }
                    Reply::Ignore => {}
                }
            }
            let now = (effects.uptime_us)();
            if now.saturating_sub(start) > per_server {
                (effects.note)(Note::NoReply(server));
                any_timeout = true;
                break;
            }
            if now.saturating_sub(last_send) > RETRANSMIT_US {
                let _ = (effects.send)(server, &query[..len]);
                last_send = now;
            }
            (effects.yield_now)();
        }
    }

    Err(if !any_sent {
        ResolveError::SendFailed
    } else if any_timeout {
        ResolveError::Timeout
    } else {
        ResolveError::NoRecord
    })
}

#[cfg(test)]
mod tests;
