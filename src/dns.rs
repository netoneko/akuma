//! DNS Resolution Module
//!
//! Provides DNS resolution with:
//! - Loopback address handling (localhost -> 127.0.0.1)
//! - IP literal parsing
//! - Timed DNS queries

use embassy_net::{IpAddress, Ipv4Address, Stack};
use embassy_time::{Duration, Instant};

use crate::async_net;

// ============================================================================
// Constants
// ============================================================================

/// Loopback IP address
pub const LOOPBACK_IP: Ipv4Address = Ipv4Address::new(127, 0, 0, 1);

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
}

// ============================================================================
// Host Resolution
// ============================================================================

/// Check if a host is a loopback address
pub fn is_loopback(host: &str) -> bool {
    host == "localhost" || host == "127.0.0.1"
}

/// Resolve a hostname to an IP address with timing information.
///
/// Resolution order:
/// 1. "localhost" -> 127.0.0.1 (0ms)
/// 2. IP literal -> parsed IP (0ms)
/// 3. Hostname -> DNS query with timing
///
/// For loopback addresses, use `async_net::get_loopback_stack()` when connecting.
/// For other addresses, use `async_net::get_global_stack()`.
pub async fn resolve_host(
    host: &str,
    stack: &Stack<'static>,
) -> Result<(IpAddress, Duration), DnsError> {
    let start = Instant::now();

    // "localhost" resolves to 127.0.0.1
    if host == "localhost" {
        return Ok((IpAddress::Ipv4(LOOPBACK_IP), Duration::from_ticks(0)));
    }

    // Try to parse as IPv4 literal (including 127.0.0.1)
    if let Ok(ipv4) = host.parse::<Ipv4Address>() {
        return Ok((IpAddress::Ipv4(ipv4), Duration::from_ticks(0)));
    }

    // Perform DNS query using the provided stack
    let result = stack
        .dns_query(host, embassy_net::dns::DnsQueryType::A)
        .await;

    let elapsed = start.elapsed();

    match result {
        Ok(addrs) if !addrs.is_empty() => Ok((addrs[0], elapsed)),
        _ => Err(DnsError::LookupFailed),
    }
}

/// Get the configured DNS server address (if any) from the main stack
pub fn get_dns_server(stack: &Stack<'static>) -> Option<Ipv4Address> {
    stack
        .config_v4()
        .and_then(|config| config.dns_servers.first().copied())
}

/// Get the appropriate stack for connecting to a resolved IP address.
/// Returns the loopback stack for 127.x.x.x addresses, main stack otherwise.
pub fn get_stack_for_ip(ip: IpAddress) -> Option<Stack<'static>> {
    match ip {
        IpAddress::Ipv4(v4) if v4.octets()[0] == 127 => async_net::get_loopback_stack(),
        _ => async_net::get_global_stack(),
    }
}

// ============================================================================
// Helper for displaying errors
// ============================================================================

impl DnsError {
    pub fn as_str(&self) -> &'static str {
        match self {
            DnsError::LookupFailed => "DNS lookup failed",
            DnsError::NoConfig => "Network not configured",
            DnsError::InvalidHost => "Invalid hostname",
        }
    }
}
