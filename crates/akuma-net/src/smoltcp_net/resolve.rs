//! `dns_query` over smoltcp's DNS socket.

use super::*;

// DNS Resolution
// ============================================================================

/// Blocking DNS query - resolves a hostname to an IPv4 address.
///
/// Polls the network stack and yields the current thread until a result is available.
/// Used by the syscall handler for userspace programs and by kernel services.
pub fn dns_query(hostname: &str) -> Result<smoltcp::wire::Ipv4Address, DnsQueryError> {
    // Fast path: try parsing as IP literal first
    if let Ok(ip) = hostname.parse::<smoltcp::wire::Ipv4Address>() {
        return Ok(ip);
    }
    if hostname == "localhost" {
        return Ok(smoltcp::wire::Ipv4Address::LOCALHOST);
    }

    // Start a DNS query
    let query_handle = with_network(|net| {
        let dns_socket = net.sockets.get_mut::<dns::Socket>(net.dns_handle);
        let cx = net.iface.context();
        dns_socket.start_query(cx, hostname, smoltcp::wire::DnsQueryType::A).ok()
    }).flatten().ok_or(DnsQueryError::StartFailed)?;

    // Poll until we get a result or timeout (10 seconds)
    let start = (runtime().uptime_us)();
    let timeout_us = 10_000_000u64;

    loop {
        poll();

        let result = with_network(|net| {
            let dns_socket = net.sockets.get_mut::<dns::Socket>(net.dns_handle);
            match dns_socket.get_query_result(query_handle) {
                Ok(addrs) => {
                    Some(
                        addrs.first().map_or(Err(DnsQueryError::NoRecords), |addr| {
                            let IpAddress::Ipv4(v4) = addr;
                            Ok(*v4)
                        }),
                    )
                }
                Err(dns::GetQueryResultError::Pending) => None,
                Err(dns::GetQueryResultError::Failed) => Some(Err(DnsQueryError::QueryFailed)),
            }
        }).flatten();

        match result {
            Some(Ok(addr)) => return Ok(addr),
            Some(Err(e)) => return Err(e),
            None => {
                if (runtime().uptime_us)() - start > timeout_us {
                    return Err(DnsQueryError::Timeout);
                }
                // Wait for the DNS response, DROPPING the Big Kernel Lock across the wait
                // under shared-kernel SMP. This loop does not poll itself — it relies on
                // the async-main poller (on a peer core) to drive the DNS RX, which cannot
                // happen if we spin holding the BKL. Same freeze as the socket wait; fires
                // first, on any connect-by-hostname. See docs/runbooks/debug-smp.md.
                (runtime().blocking_relax)();
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum DnsQueryError {
    StartFailed,
    QueryFailed,
    NoRecords,
    Timeout,
}
