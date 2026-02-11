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
    /// DNS query timed out
    Timeout,
}

// ============================================================================
// Host Resolution
// ============================================================================

/// Check if a host is a loopback address
pub fn is_loopback(host: &str) -> bool {
    host == "localhost" || host == "127.0.0.1"
}

/// Resolve a hostname to an IP address.
///
/// Handles localhost, IPv4 literals, and real DNS queries via smoltcp.
pub async fn resolve_host(
    host: &str,
) -> Result<IpAddress, DnsError> {
    // "localhost" resolves to 127.0.0.1
    if host == "localhost" {
        return Ok(IpAddress::Ipv4(LOOPBACK_IP));
    }

    // Try to parse as IPv4 literal (including 127.0.0.1)
    if let Ok(ipv4) = host.parse::<Ipv4Address>() {
        return Ok(IpAddress::Ipv4(ipv4));
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
    if host == "localhost" {
        return Ok(LOOPBACK_IP);
    }
    if let Ok(ipv4) = host.parse::<Ipv4Address>() {
        return Ok(ipv4);
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
    pub fn as_str(&self) -> &'static str {
        match self {
            DnsError::LookupFailed => "DNS lookup failed",
            DnsError::NoConfig => "Network not configured",
            DnsError::InvalidHost => "Invalid hostname",
            DnsError::Timeout => "DNS query timed out",
        }
    }
}
