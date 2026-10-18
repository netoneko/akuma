//! DNS Resolution Module
//!
//! Provides DNS resolution with:
//! - Loopback address handling (localhost -> 127.0.0.1)
//! - IP literal parsing
//! - Real DNS queries over this crate's UDP sockets, through `akuma-dns`
//!
//! **One resolver for every target.** Until 2026-10-18 this drove smoltcp's
//! dedicated DNS socket (`smoltcp_net::dns_query`) while amd64 carried its own
//! hand-rolled client (`amd64/src/dns.rs`), written when the former hung on
//! amd64's first kernel-side caller. The two differed where it matters: smoltcp
//! fails a query on the first `NXDOMAIN`, with one configured server, and a
//! resolver that `NXDOMAIN`s a name another answers is a measured case (the HP
//! box's uplink, QEMU slirp). Both now go through [`resolve_ipv4`].

use akuma_dns::{Effects, Note, ResolveError};
use core::sync::atomic::{AtomicU32, Ordering};
use smoltcp::wire::{IpAddress, Ipv4Address};

// ============================================================================
// Constants
// ============================================================================

/// Loopback IP address
pub const LOOPBACK_IP: Ipv4Address = Ipv4Address::LOCALHOST;

// ============================================================================
// Error Types
// ============================================================================

/// DNS resolution error
#[derive(Debug, Clone, Copy)]
pub enum DnsError {
    /// DNS query failed
    LookupFailed,
    /// Network stack not configured
    NoConfig,
    /// Invalid hostname
    InvalidHost,
    /// DNS query timed out
    Timeout,
}

// ============================================================================
// Host Resolution
// ============================================================================

/// The part of resolution that never touches the network: `localhost` and a
/// dotted-quad IPv4 literal.
///
/// Split out so there is **one** of it. There were two — one inside
/// [`resolve_host`], one inside [`resolve_host_blocking`] — and amd64's
/// `crate::dns::resolve_a` (a separate A-record client, written because
/// `smoltcp_net::dns_query` hung on that target) had neither. So
/// `resolve_host("192.168.1.203")` returned the literal on AArch64 and asked a
/// resolver for an A record *named* `192.168.1.203` on amd64, which NXDOMAINs:
/// every `no_std` program whose provider URL is an IP literal — `meow` pointed
/// at a LAN inference server is the one that found this — reported "DNS
/// resolution failed" against a host `busybox wget` reached perfectly, because
/// busybox is musl and resolves for itself.
///
/// Returns `None` for anything that needs a query, including a malformed quad:
/// the caller should ask the resolver rather than fail, since a name may
/// legitimately look almost like an address.
#[must_use]
pub fn resolve_literal(host: &str) -> Option<[u8; 4]> {
    if host == "localhost" {
        return Some(LOOPBACK_IP.octets());
    }
    host.parse::<Ipv4Address>().ok().map(|ip| ip.octets())
}

/// Total budget for [`resolve_host`] and [`resolve_host_blocking`].
pub const DEFAULT_TIMEOUT_US: u64 = 10_000_000;

/// How many resolvers [`set_fallback_resolvers`] can hold.
pub const MAX_FALLBACK_RESOLVERS: usize = 4;

/// Resolvers tried after the one the interface was configured with, as
/// big-endian `u32`s; `0` is an empty slot (`0.0.0.0` is not a resolver).
static FALLBACKS: [AtomicU32; MAX_FALLBACK_RESOLVERS] =
    [const { AtomicU32::new(0) }; MAX_FALLBACK_RESOLVERS];

/// Name resolvers to fall back to when the configured one times out or answers
/// without a record. At most [`MAX_FALLBACK_RESOLVERS`]; the rest are dropped.
///
/// **A policy, so a kernel sets it** — the default is none, which keeps a name
/// from leaving the configured resolver unless the kernel said it may. amd64
/// sets Cloudflare and Google: bare metal has no VMM-provided forwarder to trust
/// and `NXDOMAIN`s from an uplink resolver are the measured failure it covers.
pub fn set_fallback_resolvers(list: &[[u8; 4]]) {
    for (i, slot) in FALLBACKS.iter().enumerate() {
        let v = list.get(i).map_or(0, |ip| u32::from_be_bytes(*ip));
        slot.store(v, Ordering::Relaxed);
    }
}

/// The resolvers to try, in order: the configured one, then the fallbacks,
/// without repeats.
fn resolvers() -> ([[u8; 4]; 1 + MAX_FALLBACK_RESOLVERS], usize) {
    let mut list = [[0u8; 4]; 1 + MAX_FALLBACK_RESOLVERS];
    let mut n = 0;
    let configured = crate::smoltcp_net::static_ipv4().dns;
    let fallbacks = FALLBACKS.iter().map(|f| f.load(Ordering::Relaxed).to_be_bytes());
    for r in core::iter::once(configured).chain(fallbacks) {
        if r != [0; 4] && !list[..n].contains(&r) {
            list[n] = r;
            n += 1;
        }
    }
    (list, n)
}

/// Resolve `host` to an IPv4 address, blocking for up to `timeout_us` in total.
///
/// `localhost` and a dotted-quad literal are answered without a query. Anything
/// else is asked of each resolver in turn ([`resolvers`]), the budget split
/// between them. Yields through `blocking_relax` while it waits, which drops the
/// BKL under shared-kernel SMP: this loop drives the poll itself, but a peer
/// core's async-main poller must be able to run too.
///
/// # Errors
///
/// [`DnsError::Timeout`] if a resolver never answered and none had a record;
/// [`DnsError::LookupFailed`] if every resolver answered without one.
pub fn resolve_ipv4(host: &str, timeout_us: u64) -> Result<Ipv4Address, DnsError> {
    use crate::socket::{self, SocketAddrV4, socket_const::SOCK_DGRAM};

    if let Some(ip) = resolve_literal(host) {
        return Ok(Ipv4Address::from(ip));
    }
    let rt = crate::runtime::runtime();
    let Some(idx) = socket::alloc_socket(SOCK_DGRAM) else {
        return Err(DnsError::LookupFailed);
    };

    let (list, count) = resolvers();
    // The uptime reading doubles as the query id: enough entropy to refuse a
    // stale reply to a different query, which is all the id is for here.
    let now = (rt.uptime_us)();
    let id = (now ^ (now >> 16)) as u16;

    let mut send = |to: [u8; 4], q: &[u8]| socket::socket_send_udp(idx, q, SocketAddrV4::new(to, 53)).is_ok();
    let mut recv = |buf: &mut [u8]| socket::socket_recv_udp(idx, buf, true).ok().map(|(n, _from)| n);
    let mut poll = || {
        crate::smoltcp_net::poll();
    };
    let mut uptime = || (rt.uptime_us)();
    let mut relax = || (rt.blocking_relax)();
    let mut note = |n: Note| match n {
        Note::SendFailed(ip) => crate::safe_print!(
            64, "  dns: {}.{}.{}.{}: send failed\n", ip[0], ip[1], ip[2], ip[3]),
        Note::NoRecord(ip) => crate::safe_print!(
            64, "  dns: {}.{}.{}.{}: no record\n", ip[0], ip[1], ip[2], ip[3]),
        Note::NoReply(ip) => crate::safe_print!(
            64, "  dns: {}.{}.{}.{}: no reply before timeout\n", ip[0], ip[1], ip[2], ip[3]),
    };
    let mut effects = Effects {
        send: &mut send,
        recv: &mut recv,
        poll_network: &mut poll,
        uptime_us: &mut uptime,
        yield_now: &mut relax,
        note: &mut note,
    };
    let result = akuma_dns::resolve_a(&mut effects, host, &list[..count], timeout_us, id);
    socket::remove_socket(idx);

    result.map(Ipv4Address::from).map_err(|e| match e {
        ResolveError::BadName => DnsError::InvalidHost,
        ResolveError::NoResolvers => DnsError::NoConfig,
        ResolveError::Timeout => DnsError::Timeout,
        ResolveError::NoRecord | ResolveError::SendFailed => DnsError::LookupFailed,
    })
}

/// Resolve a hostname to an IP address.
///
/// Handles localhost, IPv4 literals, and real DNS queries ([`resolve_ipv4`]).
pub fn resolve_host(host: &str) -> Result<IpAddress, DnsError> {
    resolve_ipv4(host, DEFAULT_TIMEOUT_US).map(IpAddress::Ipv4)
}

/// Blocking DNS resolution (for synchronous contexts like syscalls)
pub fn resolve_host_blocking(host: &str) -> Result<Ipv4Address, DnsError> {
    resolve_ipv4(host, DEFAULT_TIMEOUT_US)
}

// ============================================================================
// Helper for displaying errors
// ============================================================================

impl DnsError {
    #[must_use] 
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::LookupFailed => "DNS lookup failed",
            Self::NoConfig => "Network not configured",
            Self::InvalidHost => "Invalid hostname",
            Self::Timeout => "DNS query timed out",
        }
    }
}


#[cfg(test)]
mod tests {
    use super::resolve_literal;

    #[test]
    fn localhost_is_loopback() {
        assert_eq!(resolve_literal("localhost"), Some([127, 0, 0, 1]));
    }

    #[test]
    fn dotted_quads_resolve_without_a_query() {
        // The case that was broken on amd64: a LAN peer named by address.
        assert_eq!(resolve_literal("192.168.1.203"), Some([192, 168, 1, 203]));
        assert_eq!(resolve_literal("127.0.0.1"), Some([127, 0, 0, 1]));
        assert_eq!(resolve_literal("0.0.0.0"), Some([0, 0, 0, 0]));
        assert_eq!(resolve_literal("255.255.255.255"), Some([255, 255, 255, 255]));
    }

    #[test]
    fn names_still_need_a_resolver() {
        // `None` means "ask DNS", not "fail" — so anything that is not exactly
        // an address must land here, including the near misses.
        for host in [
            "api.z.ai",
            "example.com",
            "192.168.1",          // too few octets
            "192.168.1.203.4",    // too many
            "192.168.1.256",      // out of range
            "192.168.1.203:8080", // host_port() strips the port; this is a name
            "",
            "localhost.",
        ] {
            assert_eq!(resolve_literal(host), None, "{host} should need a query");
        }
    }
}
