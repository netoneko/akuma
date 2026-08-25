//! Boot-time SNTP fallback for platforms with no working RTC.
//!
//! `kernel_main` already tried the PL031 RTC (`timer::init_utc_from_rtc`) —
//! QEMU `virt` has one, so that's the end of it there. Firecracker's aarch64
//! microVM exposes no PL031 at all, which is why
//! `docs/archive/MISSING_NTP_SYSCALLS.md` found the guest permanently stuck
//! at epoch 0 with no way to even correct it (`clock_settime` was missing
//! too — see `akuma-time`). By the time `run_async_main` reaches network
//! init, `timer::utc_time_us()` still being `None` IS that platform signal —
//! no separate "which board am I on" check needed — so this runs one
//! best-effort SNTP round trip instead. Never fatal: the caller logs the
//! `Result` and boot continues with the clock unset on failure, exactly as
//! it did before this existed.
//!
//! All the protocol/retry logic lives in `akuma_time::{sntp, boot}` (host
//! tested there); this module is just the wiring — DNS resolve, UDP socket,
//! and handing `akuma_net::smoltcp_net`'s calls to `akuma_time::boot`'s
//! effects. Returns `Result<(), &'static str>` (same shape as
//! `akuma_net::init`) rather than logging itself, so the caller in
//! `main.rs` — which decided to attempt this in the first place — is the
//! one place that reports success or failure.

#[cfg(feature = "smoltcp")]
pub fn try_bootstrap_clock() -> Result<(), &'static str> {
    let ip = akuma_net::smoltcp_net::dns_query(crate::config::NTP_SERVER_HOSTNAME).map_err(|e| {
        use akuma_net::smoltcp_net::DnsQueryError;
        match e {
            DnsQueryError::StartFailed => "DNS query failed to start",
            DnsQueryError::QueryFailed => "DNS query failed",
            DnsQueryError::NoRecords => "DNS query returned no records",
            DnsQueryError::Timeout => "DNS query timed out",
        }
    })?;

    let handle =
        akuma_net::smoltcp_net::udp_socket_create().ok_or("no free UDP socket for the boot-time sync")?;
    // Fixed local port: this is the only UDP socket alive this early in boot
    // (network init just finished, IRQs aren't unmasked yet), so there's no
    // ephemeral-port allocator to share and no conflict to avoid.
    const NTP_CLIENT_LOCAL_PORT: u16 = 49123;
    akuma_net::smoltcp_net::udp_socket_bind(handle, NTP_CLIENT_LOCAL_PORT)
        .map_err(|()| "could not bind the boot-time sync socket")?;

    let remote = smoltcp::wire::IpEndpoint::new(
        smoltcp::wire::IpAddress::Ipv4(ip),
        akuma_time::sntp::NTP_PORT,
    );

    let mut send = |req: &[u8]| akuma_net::smoltcp_net::udp_socket_send(handle, req, remote).is_ok();
    let mut recv = |buf: &mut [u8]| akuma_net::smoltcp_net::udp_socket_recv(handle, buf).ok().map(|(n, _from)| n);
    let mut poll_network = || {
        akuma_net::smoltcp_net::poll();
    };
    let mut uptime_us = akuma_timer::uptime_us;
    let mut yield_now = akuma_exec::threading::yield_now;

    let mut effects = akuma_time::boot::BootstrapEffects {
        send: &mut send,
        recv: &mut recv,
        poll_network: &mut poll_network,
        uptime_us: &mut uptime_us,
        yield_now: &mut yield_now,
    };

    let result = akuma_time::boot::bootstrap_over_udp(&mut effects, crate::config::NTP_BOOTSTRAP_TIMEOUT_US)
        .map_err(|e| {
            use akuma_time::boot::BootstrapError;
            use akuma_time::sntp::SntpError;
            match e {
                BootstrapError::SendFailed => "UDP send failed",
                BootstrapError::Timeout => "no response within timeout",
                BootstrapError::Protocol(SntpError::ShortPacket) => "response too short",
                BootstrapError::Protocol(SntpError::NotServerMode) => "response not in server mode",
                BootstrapError::Protocol(SntpError::KissOfDeath) => {
                    "server sent kiss-of-death (stratum 0)"
                }
                BootstrapError::Protocol(SntpError::OriginMismatch) => {
                    "response origin mismatch (stale or spoofed reply)"
                }
            }
        })?;

    akuma_timer::set_utc_time_us(result.unix_epoch_us, result.anchor_uptime_us);
    Ok(())
}

#[cfg(not(feature = "smoltcp"))]
pub fn try_bootstrap_clock() -> Result<(), &'static str> {
    Err("built without the smoltcp feature; no network stack to query")
}
