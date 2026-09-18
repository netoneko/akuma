//! DNS Resolution Module
//!
//! Provides DNS resolution with:
//! - Loopback address handling (localhost -> 127.0.0.1)
//! - IP literal parsing
//! - Real DNS queries via smoltcp DNS socket

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

/// Resolve a hostname to an IP address.
///
/// Handles localhost, IPv4 literals, and real DNS queries via smoltcp.
pub fn resolve_host(host: &str) -> Result<IpAddress, DnsError> {
    // localhost and IPv4 literals (including 127.0.0.1) never go out
    if let Some(ip) = resolve_literal(host) {
        return Ok(IpAddress::Ipv4(Ipv4Address::from(ip)));
    }

    // Real DNS resolution via smoltcp
    match crate::smoltcp_net::dns_query(host) {
        Ok(ipv4) => Ok(IpAddress::Ipv4(ipv4)),
        Err(crate::smoltcp_net::DnsQueryError::Timeout) => Err(DnsError::Timeout),
        Err(_) => Err(DnsError::LookupFailed),
    }
}

/// Blocking DNS resolution (for synchronous contexts like syscalls)
pub fn resolve_host_blocking(host: &str) -> Result<Ipv4Address, DnsError> {
    if let Some(ip) = resolve_literal(host) {
        return Ok(Ipv4Address::from(ip));
    }
    crate::smoltcp_net::dns_query(host).map_err(|e| match e {
        crate::smoltcp_net::DnsQueryError::Timeout => DnsError::Timeout,
        _ => DnsError::LookupFailed,
    })
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
