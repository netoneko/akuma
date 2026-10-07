//! Resilience policy for the `ssh` client: the pure, host-testable
//! bookkeeping behind keepalive liveness detection and auto-reconnect
//! backoff. No syscalls, no sockets, no time reads — the caller passes
//! `now`, so the logic is fully deterministic under test. The client binary
//! (`src/client/protocol.rs`) owns all the I/O; this module is only the
//! decision math.

/// Liveness bookkeeping for the client keepalive
/// (`keepalive@openssh.com`, OpenSSH `ServerAliveInterval` semantics).
///
/// The caller drives it: `due()` when deciding whether to send a probe,
/// `on_probe_sent()` afterwards, `on_bytes()` whenever any packet arrives
/// (a probe reply is just another packet — no reply-specific tracking is
/// needed, per RFC 4254 §4 a server answers an unknown `want_reply` request
/// with `SSH_MSG_REQUEST_FAILURE`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Keepalive {
    /// Probe after this much inbound silence. `0` disables keepalive.
    pub interval_ms: u64,
    /// Give up after this many consecutive unanswered probes.
    pub count_max: u32,
    /// Consecutive probes with no inbound bytes since the last one.
    missed: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeOutcome {
    /// Probe sent; keep waiting. Includes how many have been missed so far.
    Waiting(u32),
    /// Silence exceeded `interval * (count_max + 1)` — declare the peer dead.
    Dead,
}

impl Keepalive {
    pub fn new(interval_ms: u64, count_max: u32) -> Self {
        Self {
            interval_ms,
            count_max,
            missed: 0,
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.interval_ms > 0
    }

    /// Whether a probe is due: keepalive enabled and the peer has been
    /// silent for a full interval. The caller owns `last_recv_ms` so this
    /// stays a pure function of its arguments.
    pub fn due(&self, now_ms: u64, last_recv_ms: u64) -> bool {
        self.is_enabled() && now_ms.saturating_sub(last_recv_ms) >= self.interval_ms
    }

    /// Record that a probe went out. Returns `Dead` once more than
    /// `count_max` consecutive probes have gone unanswered.
    pub fn on_probe_sent(&mut self) -> ProbeOutcome {
        self.missed += 1;
        if self.missed > self.count_max {
            ProbeOutcome::Dead
        } else {
            ProbeOutcome::Waiting(self.missed)
        }
    }

    /// Record inbound bytes from the peer: full liveness reset.
    pub fn on_bytes(&mut self) {
        self.missed = 0;
    }
}

/// How long to wait before reconnect attempt `attempt` (1-based: the number
/// of the attempt that just failed). Linear backoff — `base * attempt`,
/// capped — so a briefly-flapping link reconnects fast without hammering a
/// server that's actually down.
pub fn reconnect_delay_ms(attempt: u64, base_ms: u64, cap_ms: u64) -> u64 {
    base_ms.saturating_mul(attempt).min(cap_ms).max(base_ms)
}

/// Total-attempts check for the reconnect loop: attempt numbers are 1-based
/// and include the initial connection, so `should_retry(1, 3)` is true (one
/// failure left, one retry remaining) and `should_retry(3, 3)` is not.
pub fn should_retry(attempt: u64, max_attempts: u64) -> bool {
    attempt < max_attempts
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Keepalive ---

    #[test]
    fn probe_not_due_before_interval() {
        let k = Keepalive::new(15_000, 3);
        assert!(!k.due(14_999, 0));
        assert!(!k.due(15_000 - 1, 0));
    }

    #[test]
    fn probe_due_at_interval_boundary() {
        let k = Keepalive::new(15_000, 3);
        assert!(k.due(15_000, 0));
        assert!(k.due(20_000, 5_000));
    }

    #[test]
    fn disabled_when_interval_zero() {
        let k = Keepalive::new(0, 3);
        assert!(!k.is_enabled());
        assert!(!k.due(u64::MAX, 0));
    }

    #[test]
    fn dead_only_after_count_max_plus_one_probes() {
        let mut k = Keepalive::new(15_000, 3);
        assert_eq!(k.on_probe_sent(), ProbeOutcome::Waiting(1));
        assert_eq!(k.on_probe_sent(), ProbeOutcome::Waiting(2));
        assert_eq!(k.on_probe_sent(), ProbeOutcome::Waiting(3));
        // count_max = 3 means three chances before giving up...
        assert_eq!(k.on_probe_sent(), ProbeOutcome::Dead);
    }

    #[test]
    fn bytes_reset_missed_count() {
        let mut k = Keepalive::new(15_000, 3);
        assert_eq!(k.on_probe_sent(), ProbeOutcome::Waiting(1));
        assert_eq!(k.on_probe_sent(), ProbeOutcome::Waiting(2));
        k.on_bytes();
        assert_eq!(k.on_probe_sent(), ProbeOutcome::Waiting(1));
    }

    #[test]
    fn count_max_zero_dies_on_first_unanswered_probe() {
        let mut k = Keepalive::new(15_000, 0);
        assert_eq!(k.on_probe_sent(), ProbeOutcome::Dead);
    }

    #[test]
    fn silence_span_is_interval_times_count_max_plus_one() {
        // 15 s interval, 3 max → dead after 60 s of total silence.
        let mut k = Keepalive::new(15_000, 3);
        let last_recv = 0u64;
        let mut now = 15_000u64; // first probe becomes due after one interval
        let mut probes = 0u32;
        loop {
            assert!(k.due(now, last_recv));
            probes += 1;
            if k.on_probe_sent() == ProbeOutcome::Dead {
                break;
            }
            now += 15_000;
        }
        // count_max = 3 → dead on the 4th probe, i.e. after 60 s of silence.
        assert_eq!(probes, 4);
        assert_eq!(now, 60_000);
    }

    // --- Reconnect backoff ---

    #[test]
    fn backoff_is_linear_then_capped() {
        assert_eq!(reconnect_delay_ms(1, 2_000, 30_000), 2_000);
        assert_eq!(reconnect_delay_ms(2, 2_000, 30_000), 4_000);
        assert_eq!(reconnect_delay_ms(5, 2_000, 30_000), 10_000);
        assert_eq!(reconnect_delay_ms(15, 2_000, 30_000), 30_000);
        assert_eq!(reconnect_delay_ms(100, 2_000, 30_000), 30_000);
    }

    #[test]
    fn backoff_never_below_base() {
        assert_eq!(reconnect_delay_ms(1, 5_000, 30_000), 5_000);
    }

    #[test]
    fn should_retry_respects_max_attempts() {
        assert!(should_retry(1, 3));
        assert!(should_retry(2, 3));
        assert!(!should_retry(3, 3));
        assert!(!should_retry(4, 3));
        // Reconnect disabled entirely: a single attempt never retries.
        assert!(!should_retry(1, 1));
    }
}
