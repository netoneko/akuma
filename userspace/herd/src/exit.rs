//! What to do with a service that just exited.
//!
//! Split out from `check_process_exits` so the restart policy can be tested
//! without a VM. See `docs/SIGNAL_EXIT_HANDLING.md` for the bug this exists to
//! close: herd reaped with `waitpid`, which reports `WEXITSTATUS` only, so a
//! service killed by a signal looked exactly like `exit 0` and took the
//! clean-exit branch — respawning with no `restart_delay_ms`, no
//! `restart_count`, and no reachable `Failed` state.

/// The restart policy fields of a service's config that bear on this decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    /// Run once; never restarted regardless of how it ended.
    pub oneshot: bool,
    /// Restart on failure.
    pub restart: bool,
    /// Restart ceiling; 0 means unlimited.
    pub max_retries: u32,
}

/// How a reaped service died. Two fields rather than one exit code because a
/// signal death carries `WEXITSTATUS == 0`: `signaled` is the only thing
/// separating a SIGSEGV from a clean success.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Exit {
    /// `WIFSIGNALED`.
    pub signaled: bool,
    /// `WEXITSTATUS`.
    pub exit_code: i32,
}

impl Exit {
    /// A clean `exit 0`.
    pub const CLEAN: Self = Self { signaled: false, exit_code: 0 };

    /// A normal termination with `code`.
    #[must_use]
    pub const fn code(code: i32) -> Self {
        Self { signaled: false, exit_code: code }
    }

    /// Death by signal. `WEXITSTATUS` is 0 for these, which is the whole problem.
    #[must_use]
    pub const fn signal() -> Self {
        Self { signaled: true, exit_code: 0 }
    }

    /// Whether this counts as a failure for restart purposes.
    #[must_use]
    pub const fn failed(&self) -> bool {
        self.signaled || self.exit_code != 0
    }
}

/// The state transition `check_process_exits` should apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// `oneshot` ran its single time. Terminal until a reboot.
    Completed,
    /// Schedule a `PendingRestart`: honour `restart_delay_ms` and increment
    /// `restart_count`.
    Restart,
    /// The retry ceiling is spent.
    Failed,
    /// Nothing more to do: the service stays down (herd's `Exited` state) until
    /// `herd start` or a reboot. It must **not** land in the "not started yet"
    /// state, which `start_stopped_services` revives on the next pass — that
    /// was a no-delay restart loop for every clean or `restart = false` exit.
    Stopped,
}

/// Decide what happens to a service that just exited.
///
/// `restart_count` is the count *before* this exit, so the ceiling comparison
/// is `<`: with `max_retries = 3`, counts 0/1/2 restart and 3 fails — exactly
/// three restarts.
#[must_use]
pub fn classify(policy: Policy, restart_count: u32, _exit: Exit) -> Outcome {
    // Checked first, and independent of how it ended: a oneshot that crashes is
    // still a oneshot that has had its turn.
    if policy.oneshot {
        return Outcome::Completed;
    }

    // `restart = true` means "keep this running", not "restart it if it
    // crashes": a clean exit is restarted too. Treating `exit 0` as final left
    // `sshd` and `netwatch` down for good on ryzen (2026-10-08): both were
    // reported `exited with code 0` seconds after a healthy start, and nothing
    // brought them back. The loop a clean exit could cause is bounded by
    // `restart_delay_ms`, and by `max_retries` when set.
    if !policy.restart {
        return Outcome::Stopped;
    }

    if policy.max_retries == 0 || restart_count < policy.max_retries {
        Outcome::Restart
    } else {
        Outcome::Failed
    }
}

/// Decide what happens to a service whose *spawn* failed: it never ran, so
/// there is no exit to classify.
///
/// A supervised service (`restart = true`) is retried exactly as if it had
/// crashed — `restart_delay_ms` between tries, `max_retries` as the ceiling.
/// Before this, a failed spawn went straight to `Failed`, which nothing ever
/// retries (`start_stopped_services` only revives "not started yet"), so one
/// transient failure at boot left an enabled `sshd` down until somebody typed
/// `herd start sshd` at the console (2026-10-08, the trashcan's metal). A
/// oneshot, or `restart = false`, keeps `Failed`: it asked not to be retried.
#[must_use]
pub fn classify_spawn_failure(policy: Policy, restart_count: u32) -> Outcome {
    if policy.oneshot {
        return Outcome::Failed;
    }
    // 127 is the shell's "command could not be run"; any failing exit reads the same.
    match classify(policy, restart_count, Exit::code(127)) {
        Outcome::Stopped | Outcome::Completed => Outcome::Failed,
        other => other,
    }
}

/// A run at least this long was healthy: the next failure starts the backoff over.
pub const HEALTHY_RUN_MS: u64 = 30_000;

/// The longest a quick-failing service waits between tries.
pub const MAX_BACKOFF_MS: u64 = 60_000;

/// Quick failures in a row, after a service that ran for `ran_ms` ended (a spawn
/// that failed ran for 0). A healthy run clears the streak; a quick one adds to it.
#[must_use]
pub const fn next_streak(streak: u32, ran_ms: u64) -> u32 {
    if ran_ms >= HEALTHY_RUN_MS { 0 } else { streak.saturating_add(1) }
}

/// How long to wait before the restart that follows `streak` quick failures in a
/// row: `base_ms` for the first, doubling for each one after it, never more than
/// [`MAX_BACKOFF_MS`] (or `base_ms`, if that is configured longer).
///
/// Without it a service that cannot start was retried every `restart_delay_ms`
/// (1 s by default) for as long as herd ran: 69 tries a minute for a service the
/// kernel refuses outright, each one a spawn, a log line and a reap.
#[must_use]
pub fn restart_delay_ms(base_ms: u64, streak: u32) -> u64 {
    let doublings = if streak == 0 { 0 } else { (streak - 1).min(16) };
    base_ms.saturating_mul(1u64 << doublings).min(MAX_BACKOFF_MS.max(base_ms))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_quick_failure_waits_the_configured_delay() {
        assert_eq!(restart_delay_ms(1000, next_streak(0, 0)), 1000);
    }

    #[test]
    fn quick_failures_in_a_row_double_up_to_the_cap() {
        let mut streak = 0;
        let mut delays = [0u64; 9];
        for d in &mut delays {
            streak = next_streak(streak, 5);
            *d = restart_delay_ms(1000, streak);
        }
        assert_eq!(delays, [1000, 2000, 4000, 8000, 16_000, 32_000, 60_000, 60_000, 60_000]);
    }

    #[test]
    fn a_healthy_run_starts_the_backoff_over() {
        assert_eq!(next_streak(7, HEALTHY_RUN_MS), 0);
        assert_eq!(restart_delay_ms(1000, next_streak(7, HEALTHY_RUN_MS)), 1000);
        assert_eq!(next_streak(7, HEALTHY_RUN_MS - 1), 8);
    }

    #[test]
    fn a_configured_delay_longer_than_the_cap_is_kept() {
        assert_eq!(restart_delay_ms(120_000, 5), 120_000);
    }

    #[test]
    fn a_long_streak_does_not_overflow() {
        assert_eq!(restart_delay_ms(1000, u32::MAX), MAX_BACKOFF_MS);
        assert_eq!(restart_delay_ms(u64::MAX, 40), u64::MAX);
        assert_eq!(next_streak(u32::MAX, 0), u32::MAX);
    }

    const SUPERVISED: Policy = Policy { oneshot: false, restart: true, max_retries: 0 };

    #[test]
    fn signal_death_is_a_failure_even_though_the_exit_code_is_zero() {
        // The whole point: `waitpid` reports 0 for this, and reading only that
        // sent a crashed service down the clean-exit path.
        assert!(Exit::signal().failed());
        assert_eq!(Exit::signal().exit_code, 0);
        assert_eq!(classify(SUPERVISED, 0, Exit::signal()), Outcome::Restart);
    }

    #[test]
    fn a_supervised_service_is_restarted_whether_it_exits_cleanly_or_not() {
        assert_eq!(classify(SUPERVISED, 0, Exit::CLEAN), Outcome::Restart);
        assert_eq!(classify(SUPERVISED, 0, Exit::code(1)), Outcome::Restart);
    }

    #[test]
    fn clean_exits_count_against_max_retries() {
        let policy = Policy { max_retries: 2, ..SUPERVISED };
        assert_eq!(classify(policy, 1, Exit::CLEAN), Outcome::Restart);
        assert_eq!(classify(policy, 2, Exit::CLEAN), Outcome::Failed);
    }

    #[test]
    fn max_retries_is_reachable_for_a_crashing_service() {
        // The regression that mattered most: a service that segfaults on startup
        // used to respawn forever because it never reached this branch.
        let policy = Policy { max_retries: 3, ..SUPERVISED };
        assert_eq!(classify(policy, 0, Exit::signal()), Outcome::Restart);
        assert_eq!(classify(policy, 1, Exit::signal()), Outcome::Restart);
        assert_eq!(classify(policy, 2, Exit::signal()), Outcome::Restart);
        assert_eq!(classify(policy, 3, Exit::signal()), Outcome::Failed);
        assert_eq!(classify(policy, 9, Exit::signal()), Outcome::Failed);
    }

    #[test]
    fn zero_max_retries_means_unlimited() {
        let policy = Policy { max_retries: 0, ..SUPERVISED };
        assert_eq!(classify(policy, 10_000, Exit::code(1)), Outcome::Restart);
    }

    #[test]
    fn oneshot_completes_however_it_ended() {
        let policy = Policy { oneshot: true, restart: true, max_retries: 3 };
        assert_eq!(classify(policy, 0, Exit::CLEAN), Outcome::Completed);
        assert_eq!(classify(policy, 0, Exit::code(1)), Outcome::Completed);
        assert_eq!(classify(policy, 0, Exit::signal()), Outcome::Completed);
    }

    #[test]
    fn restart_disabled_never_schedules_a_restart() {
        let policy = Policy { restart: false, ..SUPERVISED };
        assert_eq!(classify(policy, 0, Exit::CLEAN), Outcome::Stopped);
        assert_eq!(classify(policy, 0, Exit::code(1)), Outcome::Stopped);
        assert_eq!(classify(policy, 0, Exit::signal()), Outcome::Stopped);
    }

    #[test]
    fn nonzero_exit_and_signal_death_are_classified_alike() {
        // They differ only in what gets *reported* (shell_code), not in policy.
        for count in [0u32, 1, 5] {
            let policy = Policy { max_retries: 2, ..SUPERVISED };
            assert_eq!(
                classify(policy, count, Exit::signal()),
                classify(policy, count, Exit::code(1)),
            );
        }
    }

    #[test]
    fn a_failed_spawn_of_a_supervised_service_is_retried() {
        // The sshd-never-started case: restart = true, spawn failed at boot.
        assert_eq!(classify_spawn_failure(SUPERVISED, 0), Outcome::Restart);
        assert_eq!(classify_spawn_failure(SUPERVISED, 10_000), Outcome::Restart);
    }

    #[test]
    fn a_failed_spawn_honours_max_retries() {
        let policy = Policy { max_retries: 3, ..SUPERVISED };
        assert_eq!(classify_spawn_failure(policy, 2), Outcome::Restart);
        assert_eq!(classify_spawn_failure(policy, 3), Outcome::Failed);
    }

    #[test]
    fn a_failed_spawn_without_restart_stays_failed() {
        // Not `Stopped`/`Exited`: it never ran, and the old behaviour was `Failed`.
        let policy = Policy { restart: false, ..SUPERVISED };
        assert_eq!(classify_spawn_failure(policy, 0), Outcome::Failed);
    }

    #[test]
    fn a_failed_spawn_of_a_oneshot_stays_failed() {
        // `classify` would say Completed, which is a lie: it never ran.
        let policy = Policy { oneshot: true, ..SUPERVISED };
        assert_eq!(classify_spawn_failure(policy, 0), Outcome::Failed);
    }
}
