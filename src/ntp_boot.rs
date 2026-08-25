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
//! best-effort SNTP round trip instead. Never fatal: on any failure, boot
//! continues with the clock unset exactly as it did before this existed.
//!
//! All the protocol/retry logic lives in `akuma_time::{sntp, boot}` (host
//! tested there); this module is just the wiring — DNS resolve, UDP socket,
//! and handing `akuma_net::smoltcp_net`'s calls to `akuma_time::boot`'s
//! effects.

#[cfg(feature = "smoltcp")]
pub fn try_bootstrap_clock() {
    if !crate::config::ENABLE_NTP_BOOTSTRAP {
        return;
    }

    let ip = match akuma_net::smoltcp_net::dns_query(crate::config::NTP_SERVER_HOSTNAME) {
        Ok(ip) => ip,
        Err(e) => {
            log::warn!(
                "[NTP] resolving {} failed: {e:?}",
                crate::config::NTP_SERVER_HOSTNAME
            );
            return;
        }
    };

    let Some(handle) = akuma_net::smoltcp_net::udp_socket_create() else {
        log::warn!("[NTP] no free UDP socket for the boot-time sync");
        return;
    };
    // Fixed local port: this is the only UDP socket alive this early in boot
    // (network init just finished, IRQs aren't unmasked yet), so there's no
    // ephemeral-port allocator to share and no conflict to avoid.
    const NTP_CLIENT_LOCAL_PORT: u16 = 49123;
    if akuma_net::smoltcp_net::udp_socket_bind(handle, NTP_CLIENT_LOCAL_PORT).is_err() {
        log::warn!("[NTP] could not bind the boot-time sync socket");
        return;
    }

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

    match akuma_time::boot::bootstrap_over_udp(&mut effects, crate::config::NTP_BOOTSTRAP_TIMEOUT_US) {
        Ok(result) => {
            akuma_timer::set_utc_time_us(result.unix_epoch_us, result.anchor_uptime_us);
            log::info!(
                "[NTP] boot-time clock set from {} ({})",
                crate::config::NTP_SERVER_HOSTNAME,
                crate::timer::utc_iso8601()
            );
        }
        Err(e) => log::warn!(
            "[NTP] boot-time sync against {} failed: {e:?}",
            crate::config::NTP_SERVER_HOSTNAME
        ),
    }
}

#[cfg(not(feature = "smoltcp"))]
pub fn try_bootstrap_clock() {}
