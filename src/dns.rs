//! DNS Resolution Module (Stub)
//!
//! Provides DNS resolution with:
//! - Loopback address handling (localhost -> 127.0.0.1)
//! - IP literal parsing

use smoltcp::wire::{IpAddress, Ipv4Address};

// ============================================================================
// Constants
// ============================================================================

/// Loopback IP address
pub const LOOPBACK_IP: Ipv4Address = Ipv4Address([127, 0, 0, 1]);

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

/// Resolve a hostname to an IP address (Literal only for now)
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

    // Real DNS resolution not implemented yet
    Err(DnsError::LookupFailed)
}

// ============================================================================
// Helper for displaying errors
// ============================================================================

impl DnsError {
    pub fn as_str(&self) -> &'static str {
        match self {
            DnsError::LookupFailed => "DNS lookup failed (literals only)",
            DnsError::NoConfig => "Network not configured",
            DnsError::InvalidHost => "Invalid hostname",
        }
    }
}