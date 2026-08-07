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
    /// Nothing more to do. `start_stopped_services` may revive it later.
    Stopped,
}

/// Decide what happens to a service that just exited.
///
/// `restart_count` is the count *before* this exit, so the ceiling comparison
/// is `<`: with `max_retries = 3`, counts 0/1/2 restart and 3 fails — exactly
/// three restarts.
#[must_use]
pub fn classify(policy: Policy, restart_count: u32, exit: Exit) -> Outcome {
    // Checked first, and independent of how it ended: a oneshot that crashes is
    // still a oneshot that has had its turn.
    if policy.oneshot {
        return Outcome::Completed;
    }

    if !exit.failed() || !policy.restart {
        return Outcome::Stopped;
    }

    if policy.max_retries == 0 || restart_count < policy.max_retries {
        Outcome::Restart
    } else {
        Outcome::Failed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn clean_exit_stops_and_nonzero_exit_restarts() {
        assert_eq!(classify(SUPERVISED, 0, Exit::CLEAN), Outcome::Stopped);
        assert_eq!(classify(SUPERVISED, 0, Exit::code(1)), Outcome::Restart);
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
}
