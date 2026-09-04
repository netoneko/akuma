//! Boot-time SNTP bootstrap loop: pure orchestration over caller-supplied
//! effects, so the round-trip/retry/timeout logic is host-testable without a
//! devbox boot.
//!
//! Same shape as `akuma-net-yarn`'s `wait_until` state machine
//! (`WaitPolicy` takes only effects; the caller wires the real socket/timer
//! calls). **`amd64/src/clock.rs`** is the one real caller today, wiring
//! `akuma_net::socket::{socket_send_udp, socket_recv_udp}` and its own
//! iteration-counted `uptime_us` stand-in (see that module's doc — this
//! target has no calibrated timer to feed the real thing) into
//! [`BootstrapEffects`]. The main aarch64 kernel's own wiring — `akuma_net::
//! smoltcp_net::{udp_socket_send, udp_socket_recv, poll}` and the real,
//! hardware-calibrated `akuma_timer::uptime_us` — is still the open gap
//! `docs/archive/MISSING_NTP_SYSCALLS.md` describes: this module (and
//! `sntp`) were extracted from `src/syscall/time.rs` in anticipation of that
//! wiring, but nothing in `src/` calls `bootstrap_over_udp` yet.
//!
//! This crate deliberately has no `akuma-net` dependency: the socket/DNS
//! calls only exist behind a caller's own network-stack feature, and keeping
//! this crate free of that means the whole SNTP protocol + retry logic gets
//! host tests independent of any network-stack feature flag.

use crate::sntp::{self, SntpResult};

/// The effects [`bootstrap_over_udp`] needs from its caller. All borrowed
/// mutably so the caller's closures can capture a socket handle by value.
pub struct BootstrapEffects<'a> {
    /// Send one UDP datagram to the already-resolved NTP server. Returns
    /// `false` on any send failure (no route, socket full, ...).
    pub send: &'a mut dyn FnMut(&[u8]) -> bool,
    /// Non-blocking receive attempt: `Some(n)` with the datagram copied into
    /// the front of the caller's scratch buffer if one arrived, `None`
    /// otherwise. Must not block — this loop drives its own polling.
    pub recv: &'a mut dyn FnMut(&mut [u8]) -> Option<usize>,
    /// Drive the network stack's own poll/RX-drain step. Called once per
    /// loop iteration before checking `recv`.
    pub poll_network: &'a mut dyn FnMut(),
    /// Monotonic microseconds since boot (`akuma_timer::uptime_us`).
    pub uptime_us: &'a mut dyn FnMut() -> u64,
    /// Cooperatively yield the current thread for one iteration.
    pub yield_now: &'a mut dyn FnMut(),
}

/// Why [`bootstrap_over_udp`] gave up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootstrapError {
    /// [`BootstrapEffects::send`] returned `false`.
    SendFailed,
    /// No valid response within `timeout_us`.
    Timeout,
    /// A response arrived but failed [`sntp::parse_response`]'s validation.
    Protocol(sntp::SntpError),
}

/// Send one SNTP request and wait up to `timeout_us` for a valid response,
/// driving `effects.poll_network`/`yield_now` between attempts.
///
/// Never retries a second request — one shot, like the caller's other
/// best-effort boot steps (`docs/archive/MISSING_NTP_SYSCALLS.md`'s
/// fallback is meant to beat "stuck at 1970", not to be a full `ntpd`).
pub fn bootstrap_over_udp(
    effects: &mut BootstrapEffects,
    timeout_us: u64,
) -> Result<SntpResult, BootstrapError> {
    let marker = (effects.uptime_us)();
    let request = sntp::build_request(marker);
    let t1_up_us = (effects.uptime_us)();
    if !(effects.send)(&request) {
        return Err(BootstrapError::SendFailed);
    }
    let deadline_us = t1_up_us.saturating_add(timeout_us);

    let mut buf = [0u8; sntp::PACKET_LEN];
    loop {
        (effects.poll_network)();
        if let Some(n) = (effects.recv)(&mut buf) {
            let t4_up_us = (effects.uptime_us)();
            return sntp::parse_response(&buf[..n], marker, t1_up_us, t4_up_us)
                .map_err(BootstrapError::Protocol);
        }
        if (effects.uptime_us)() >= deadline_us {
            return Err(BootstrapError::Timeout);
        }
        (effects.yield_now)();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::cell::Cell;

    /// Scripted network: `poll_network` advances a shared clock by
    /// `poll_advance_us` each call (modeling a real poll loop taking real
    /// time), and `recv` returns the canned `response` only once `arrives_at`
    /// has been reached — or never, if `arrives_at` is `None`.
    struct Script {
        now_us: Cell<u64>,
        poll_advance_us: u64,
        arrives_at: Option<u64>,
        response: [u8; sntp::PACKET_LEN],
        polls: Cell<u32>,
        sends: Cell<u32>,
        send_ok: bool,
    }

    fn run(script: &Script, timeout_us: u64) -> Result<SntpResult, BootstrapError> {
        let mut send = |_req: &[u8]| {
            script.sends.set(script.sends.get() + 1);
            script.send_ok
        };
        let mut recv = |buf: &mut [u8]| {
            let now = script.now_us.get();
            match script.arrives_at {
                Some(t) if now >= t => {
                    buf[..sntp::PACKET_LEN].copy_from_slice(&script.response);
                    Some(sntp::PACKET_LEN)
                }
                _ => None,
            }
        };
        let mut poll_network = || {
            script.polls.set(script.polls.get() + 1);
            script.now_us.set(script.now_us.get() + script.poll_advance_us);
        };
        let mut uptime_us = || script.now_us.get();
        let mut yield_now = || {};

        let mut effects = BootstrapEffects {
            send: &mut send,
            recv: &mut recv,
            poll_network: &mut poll_network,
            uptime_us: &mut uptime_us,
            yield_now: &mut yield_now,
        };
        bootstrap_over_udp(&mut effects, timeout_us)
    }

    fn canned_response(marker_getter: impl Fn() -> u64) -> [u8; sntp::PACKET_LEN] {
        // Built after the fact once we know the marker `bootstrap_over_udp`
        // will use (it's `uptime_us()` at the first call, i.e. `now_us` at
        // t=0), so tests construct the response with that same value.
        let marker = marker_getter();
        let mut pkt = [0u8; sntp::PACKET_LEN];
        pkt[0] = 0b00_100_100;
        pkt[1] = 2;
        pkt[24..32].copy_from_slice(&marker.to_be_bytes());
        let ntp_secs = 2_000_000_000u64 + 2_208_988_800;
        pkt[32..40].copy_from_slice(&(ntp_secs << 32).to_be_bytes());
        pkt[40..48].copy_from_slice(&(ntp_secs << 32).to_be_bytes());
        pkt
    }

    #[test]
    fn happy_path_returns_result_and_stops_polling() {
        let script = Script {
            now_us: Cell::new(0),
            poll_advance_us: 100,
            arrives_at: Some(300),
            response: canned_response(|| 0), // marker == uptime_us() at t1 == 0
            polls: Cell::new(0),
            sends: Cell::new(0),
            send_ok: true,
        };
        let result = run(&script, 10_000).unwrap();
        // T2==T3 in the canned response (zero server processing time), so the
        // whole 300us round trip (t1=0, t4=300) becomes a 150us one-way delay
        // added to the server's transmit timestamp.
        assert_eq!(result.unix_epoch_us, 2_000_000_000_000_150);
        assert_eq!(script.sends.get(), 1);
        assert!(script.polls.get() >= 3, "should have polled until the response arrived");
    }

    #[test]
    fn send_failure_is_reported_without_polling() {
        let script = Script {
            now_us: Cell::new(0),
            poll_advance_us: 100,
            arrives_at: None,
            response: [0u8; sntp::PACKET_LEN],
            polls: Cell::new(0),
            sends: Cell::new(0),
            send_ok: false,
        };
        assert_eq!(run(&script, 10_000), Err(BootstrapError::SendFailed));
        assert_eq!(script.polls.get(), 0);
    }

    #[test]
    fn no_response_times_out() {
        let script = Script {
            now_us: Cell::new(0),
            poll_advance_us: 1_000,
            arrives_at: None,
            response: [0u8; sntp::PACKET_LEN],
            polls: Cell::new(0),
            sends: Cell::new(0),
            send_ok: true,
        };
        assert_eq!(run(&script, 5_000), Err(BootstrapError::Timeout));
    }

    #[test]
    fn malformed_response_surfaces_protocol_error() {
        let mut bad = canned_response(|| 0);
        bad[1] = 0; // stratum 0 -> kiss of death
        let script = Script {
            now_us: Cell::new(0),
            poll_advance_us: 100,
            arrives_at: Some(100),
            response: bad,
            polls: Cell::new(0),
            sends: Cell::new(0),
            send_ok: true,
        };
        assert_eq!(
            run(&script, 10_000),
            Err(BootstrapError::Protocol(sntp::SntpError::KissOfDeath))
        );
    }
}
