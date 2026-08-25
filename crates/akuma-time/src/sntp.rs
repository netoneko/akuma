//! Pure SNTP (RFC 4330) client protocol: request/response packet shape and
//! the offset computation. No I/O — [`boot`](super::boot) supplies that.
//!
//! # Why the offset formula uses uptime, not wall-clock, for T1/T4
//!
//! The classic four-timestamp NTP offset formula —
//! `offset = ((T2 - T1) + (T3 - T4)) / 2` — assumes all four timestamps share
//! one clock domain, where the client's own clock may carry a fixed error
//! (the offset being solved for). That doesn't fit here: this client's clock
//! has no absolute epoch at all yet (that is the entire reason it's running —
//! see `docs/archive/MISSING_NTP_SYSCALLS.md`), so "T1 in the client's wrong
//! absolute time" isn't a meaningful input.
//!
//! What the client DOES have is a correct-rate monotonic uptime counter
//! (`akuma_timer::uptime_us`) — wrong epoch, right tick rate. Two uptime
//! reads a `t4_up - t1_up` apart span exactly that many real microseconds,
//! even though neither reading means anything as an absolute timestamp. So:
//!
//! ```text
//! round_trip_us   = t4_up_us - t1_up_us          (client-measured, real duration)
//! server_proc_us  = T3_srv_us - T2_srv_us         (server-measured, real duration)
//! one_way_us      = (round_trip_us - server_proc_us) / 2
//! unix_us_at_t4   = T3_srv_us + one_way_us
//! ```
//!
//! `unix_us_at_t4` is the estimated wall-clock time at the instant `t4_up_us`
//! was read, so the caller can hand `(unix_us_at_t4, t4_up_us)` straight to
//! `akuma_timer::set_utc_time_us` — that function's contract is exactly "this
//! uptime reading corresponds to this unix time", so no `is_none() `/`now -
//! t4` skew correction is needed at the call site.

#![allow(clippy::doc_markdown)]

/// NTP/SNTP well-known port.
pub const NTP_PORT: u16 = 123;

/// Fixed 48-byte SNTP packet size (no extension fields, no MAC).
pub const PACKET_LEN: usize = 48;

/// Seconds between the NTP epoch (1900-01-01) and the Unix epoch
/// (1970-01-01). NTP timestamps are seconds-since-1900; subtract this to land
/// on Unix time.
const NTP_UNIX_EPOCH_DELTA_SECS: u64 = 2_208_988_800;

/// LI=0 (no warning), VN=4 (NTPv4), Mode=3 (client) — byte 0 of every SNTP
/// client request.
const LI_VN_MODE_CLIENT: u8 = 0b00_100_011;

/// Build an SNTP client request.
///
/// `marker` is an opaque 64-bit value placed in
/// the Transmit Timestamp field (bytes 40..48); a well-behaved server echoes
/// it back verbatim as the response's Origin Timestamp (bytes 24..32), which
/// [`parse_response`] checks — the only thing standing between this client
/// and accepting a stale or off-path-spoofed UDP datagram, since SNTP has no
/// other authentication. `marker` doesn't need to be a real timestamp; boot()
/// passes a `uptime_us` reading, but any locally-unique value works.
#[must_use]
pub fn build_request(marker: u64) -> [u8; PACKET_LEN] {
    let mut pkt = [0u8; PACKET_LEN];
    pkt[0] = LI_VN_MODE_CLIENT;
    pkt[40..48].copy_from_slice(&marker.to_be_bytes());
    pkt
}

/// Why a response was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SntpError {
    /// Fewer than [`PACKET_LEN`] bytes — not a well-formed SNTP packet.
    ShortPacket,
    /// Mode field isn't 4 (server) — e.g. an echoed-back client packet, or
    /// noise on the port.
    NotServerMode,
    /// Stratum 0: RFC 4330's "kiss of death" — the server is refusing to
    /// serve time (rate limiting, deny-listing, "server not available" —
    /// see the `Reference Identifier` kiss codes). Never a time source.
    KissOfDeath,
    /// Origin Timestamp didn't match the marker this client sent — either a
    /// stale reply to an earlier request, or a spoofed/off-path packet.
    OriginMismatch,
}

/// A successful SNTP round trip's result: the estimated Unix time, anchored
/// to the uptime reading it corresponds to. Hand both straight to
/// `akuma_timer::set_utc_time_us(unix_epoch_us, anchor_uptime_us)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SntpResult {
    pub unix_epoch_us: u64,
    pub anchor_uptime_us: u64,
}

/// Parse and validate a server response, computing the estimated Unix time.
///
/// See the module doc for the offset formula. `t1_up_us`/`t4_up_us` are the
/// caller's own `uptime_us()` readings taken immediately around the send and
/// the receive.
pub fn parse_response(
    resp: &[u8],
    marker: u64,
    t1_up_us: u64,
    t4_up_us: u64,
) -> Result<SntpResult, SntpError> {
    if resp.len() < PACKET_LEN {
        return Err(SntpError::ShortPacket);
    }
    let mode = resp[0] & 0x7;
    if mode != 4 {
        return Err(SntpError::NotServerMode);
    }
    let stratum = resp[1];
    if stratum == 0 {
        return Err(SntpError::KissOfDeath);
    }
    let origin = be_u64(&resp[24..32]);
    if origin != marker {
        return Err(SntpError::OriginMismatch);
    }
    let t2_srv_us = ntp_to_unix_us(be_u64(&resp[32..40]));
    let t3_srv_us = ntp_to_unix_us(be_u64(&resp[40..48]));

    let round_trip_us = t4_up_us.saturating_sub(t1_up_us);
    let server_proc_us = t3_srv_us.saturating_sub(t2_srv_us);
    let one_way_us = round_trip_us.saturating_sub(server_proc_us) / 2;
    let unix_epoch_us = t3_srv_us.saturating_add(one_way_us);

    Ok(SntpResult { unix_epoch_us, anchor_uptime_us: t4_up_us })
}

fn be_u64(bytes: &[u8]) -> u64 {
    u64::from_be_bytes(bytes.try_into().expect("caller passes exactly 8 bytes"))
}

/// Convert an NTP 64-bit fixed-point timestamp (32-bit seconds since 1900 +
/// 32-bit fraction) to microseconds since the Unix epoch.
fn ntp_to_unix_us(ntp64: u64) -> u64 {
    let secs = (ntp64 >> 32).saturating_sub(NTP_UNIX_EPOCH_DELTA_SECS);
    let frac = ntp64 & 0xFFFF_FFFF;
    secs.saturating_mul(1_000_000) + ((frac * 1_000_000) >> 32)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a synthetic server response for a given origin/T2/T3, in NTP
    /// 64-bit fixed-point form (whole seconds only — fractional-second
    /// precision is exercised separately by `ntp_to_unix_us_fraction`).
    fn make_response(origin_marker: u64, t2_unix_secs: u64, t3_unix_secs: u64) -> [u8; PACKET_LEN] {
        let mut pkt = [0u8; PACKET_LEN];
        pkt[0] = 0b00_100_100; // LI=0, VN=4, Mode=4 (server)
        pkt[1] = 2; // stratum 2 (not kiss-of-death)
        pkt[24..32].copy_from_slice(&origin_marker.to_be_bytes());
        let t2_ntp = (t2_unix_secs + NTP_UNIX_EPOCH_DELTA_SECS) << 32;
        let t3_ntp = (t3_unix_secs + NTP_UNIX_EPOCH_DELTA_SECS) << 32;
        pkt[32..40].copy_from_slice(&t2_ntp.to_be_bytes());
        pkt[40..48].copy_from_slice(&t3_ntp.to_be_bytes());
        pkt
    }

    #[test]
    fn build_request_sets_client_mode_and_marker() {
        let req = build_request(0x1122_3344_5566_7788);
        assert_eq!(req[0], 0b00_100_011);
        assert_eq!(&req[40..48], &0x1122_3344_5566_7788u64.to_be_bytes());
    }

    #[test]
    fn happy_path_computes_expected_unix_time() {
        // Server's clock reads T2=T3=1_000_000 unix seconds (zero processing
        // time). Client's own round trip (t4_up - t1_up) is 2ms, symmetric ->
        // one-way delay 1ms. Estimated unix time = T3 + 1ms.
        let marker = 42;
        let resp = make_response(marker, 1_000_000, 1_000_000);
        let t1_up_us = 500_000;
        let t4_up_us = t1_up_us + 2_000; // 2ms round trip
        let result = parse_response(&resp, marker, t1_up_us, t4_up_us).unwrap();
        assert_eq!(result.unix_epoch_us, 1_000_000_000_000 + 1_000);
        assert_eq!(result.anchor_uptime_us, t4_up_us);
    }

    #[test]
    fn server_processing_time_is_subtracted_from_round_trip() {
        let marker: u64 = 7;
        // Server took 4ms between T2 and T3.
        let mut resp = [0u8; PACKET_LEN];
        resp[0] = 0b00_100_100;
        resp[1] = 2;
        resp[24..32].copy_from_slice(&marker.to_be_bytes());
        let base_ntp_secs = 1_000_000 + NTP_UNIX_EPOCH_DELTA_SECS;
        let t2_ntp = base_ntp_secs << 32;
        // T3 = T2 + 4ms, encoded via the fractional field.
        let four_ms_frac = (4_000u64 << 32) / 1_000_000; // 4ms as a 32-bit NTP fraction
        let t3_ntp = (base_ntp_secs << 32) | four_ms_frac;
        resp[32..40].copy_from_slice(&t2_ntp.to_be_bytes());
        resp[40..48].copy_from_slice(&t3_ntp.to_be_bytes());

        let t1_up_us = 0;
        let t4_up_us = 8_000; // 8ms round trip, 4ms of which was server processing.
        let result = parse_response(&resp, marker, t1_up_us, t4_up_us).unwrap();
        // one_way = (8ms - 4ms)/2 = 2ms; unix_at_t4 = T3 + 2ms.
        let expected: u64 = (1_000_000 * 1_000_000) + 4_000 + 2_000;
        assert!(
            (result.unix_epoch_us as i64 - expected as i64).abs() < 50,
            "expected ~{expected}, got {}",
            result.unix_epoch_us
        );
    }

    #[test]
    fn short_packet_rejected() {
        let resp = [0u8; 10];
        assert_eq!(parse_response(&resp, 0, 0, 0), Err(SntpError::ShortPacket));
    }

    #[test]
    fn non_server_mode_rejected() {
        let mut resp = make_response(1, 1_000_000, 1_000_000);
        resp[0] = 0b00_100_011; // mode 3 (client) instead of 4 (server)
        assert_eq!(parse_response(&resp, 1, 0, 0), Err(SntpError::NotServerMode));
    }

    #[test]
    fn kiss_of_death_rejected() {
        let mut resp = make_response(1, 1_000_000, 1_000_000);
        resp[1] = 0; // stratum 0
        assert_eq!(parse_response(&resp, 1, 0, 0), Err(SntpError::KissOfDeath));
    }

    #[test]
    fn origin_mismatch_rejected() {
        let resp = make_response(999, 1_000_000, 1_000_000);
        assert_eq!(parse_response(&resp, 1, 0, 0), Err(SntpError::OriginMismatch));
    }

    #[test]
    fn ntp_to_unix_us_matches_known_epoch() {
        // 1970-01-01T00:00:00Z in NTP form: exactly NTP_UNIX_EPOCH_DELTA_SECS
        // seconds since 1900, zero fraction.
        assert_eq!(ntp_to_unix_us(NTP_UNIX_EPOCH_DELTA_SECS << 32), 0);
        // Half a second later.
        assert_eq!(
            ntp_to_unix_us((NTP_UNIX_EPOCH_DELTA_SECS << 32) | (1u64 << 31)),
            500_000
        );
    }
}
