//! The bulk-Only Mass Storage failure/recovery **decision**, extracted from the
//! amd64 glue so it can be host-tested.
//!
//! The metal incident of 2026-09-11 (the framebuffer photograph in
//! `docs/archive/AKUMA_AMD64_USB_XHCI.md` § 2026-09-11) had exactly one root:
//! the first failing phase *timed out* instead of *stalling*, and the timeout
//! path ran no recovery — so the device stayed halted, every later command
//! timed out, and the disk was dead until reboot. On the previous boot every
//! stall had delivered its `cc=STALL_ERROR` inside the poll budget and was
//! recovered 57/57. The difference between survivable and fatal was one code
//! path; this module pins both to the same decision, with tests.
//!
//! Two rules, in order:
//!
//! 1. **Both failure shapes recover.** A completion code of `STALL_ERROR` and
//!    a poll-budget timeout are the same situation seen at different speeds —
//!    a halted endpoint that answers late is indistinguishable from one that
//!    never answers. Both get the class-standard recovery. Any *other*
//!    completion code (babble, transaction error, …) is a different disease
//!    and gets no recovery.
//! 2. **Stalls retry once; timeouts twice.** A command that fails again after
//!    recovery has a real problem and must say so — with one exception: the
//!    metal evidence points at idle power management (the stall cadence on
//!    the 2026-09-11 run tracked the operator's 15-second poll gap exactly,
//!    and the box died *after a long idle*, on its first access back). A
//!    device coming out of a low-power state needs seconds, not one
//!    re-issue, so a *timeout* gets a second recovery cycle. A stall after
//!    recovery stays terminal — that is a device refusing the command, not a
//!    slow one.

/// Which BOT phase failed — the log's phase string and the endpoint the
/// failure belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Cbw,
    Data,
    Csw,
}

impl Phase {
    /// The glue's log spelling, so host tests pin what the console shows.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cbw => "CBW",
            Self::Data => "data",
            Self::Csw => "CSW",
        }
    }
}

/// How one BOT command attempt came back, with what recovery needs to know.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttemptOutcome {
    /// The controller completed the TD with a completion code the BOT layer
    /// cannot use — `STALL_ERROR` being the recoverable one.
    Stalled { code: u8, phase: Phase, dci: u8 },
    /// The controller never completed the TD within the poll budget. The TD
    /// is still live in the ring; the endpoint may be halted and answer late.
    TimedOut { phase: Phase, dci: u8 },
    /// The CSW was unparseable or its tag mismatched — the pipe is desynced.
    Desynced { phase: Phase, dci: u8 },
}

/// What to do after an attempt failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Run the class-standard recovery (see [`recovery_plan`]) and re-issue
    /// the whole command once.
    RecoverAndRetry,
    /// This outcome is not recovery's disease (a non-STALL completion code):
    /// report the failure, touch nothing.
    GiveUp,
    /// The command failed again after recovery — report and stop.
    Dead,
}

/// `attempt` counts failures of one command.
///
/// 0 = first failure, 1 = failed again after recovery, 2 = failed a third
/// time (timeouts only). Past that — or at any point for a non-timeout — is
/// [`Decision::Dead`], which is also the safe answer for a caller bug that
/// keeps asking.
#[must_use]
pub fn retry_decision(attempt: u8, outcome: &AttemptOutcome) -> Decision {
    let max_attempt = match outcome {
        AttemptOutcome::TimedOut { .. } => 2,
        _ => 1,
    };
    if attempt >= max_attempt {
        return Decision::Dead;
    }
    match outcome {
        AttemptOutcome::Stalled { code, .. } if *code == crate::trb::cc::STALL_ERROR => {
            Decision::RecoverAndRetry
        }
        AttemptOutcome::Stalled { .. } => Decision::GiveUp,
        AttemptOutcome::TimedOut { .. } | AttemptOutcome::Desynced { .. } => {
            Decision::RecoverAndRetry
        }
    }
}

/// One step of the class-standard recovery sequence, in the order the spec
/// and Linux perform them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryStep {
    /// Controller-side: clear the halted state of the one endpoint that
    /// failed. Issued for the failed ring ONLY — a Set TR Dequeue Pointer on
    /// a *running* endpoint is spec-illegal (xHCI §4.6.9), and both bulk
    /// rings are not necessarily halted.
    ResetEndpoint { dci: u8 },
    /// Device-side: the Bulk-Only Mass Storage Reset class request
    /// (`bmRequestType 0x21`, `bRequest 0xFF`, `wIndex` = BOT interface).
    /// A controller-side reset does not un-halt the device.
    BotMassStorageReset,
    /// Device-side: `CLEAR_FEATURE(ENDPOINT_HALT)` — the device may have
    /// halted either bulk endpoint, and both must be cleared at the device
    /// before either ring restarts.
    ClearHalt { ep_addr: u8 },
    /// Controller-side: move the failed ring's dequeue past the failed TD so
    /// the retry does not land on it. The failed ring ONLY, and only after
    /// its Reset Endpoint.
    SetTrDequeuePointer { dci: u8 },
}

/// Build the recovery plan for a failure on `failed_dci` (the timed-out or
/// stalled ring). `bulk_in_addr`/`bulk_out_addr` are the USB endpoint
/// addresses (`0x80 | n` for IN) the CLEAR_FEATURE steps need.
#[must_use]
pub fn recovery_plan(
    failed_dci: u8,
    bulk_in_addr: u8,
    bulk_out_addr: u8,
) -> [RecoveryStep; 5] {
    [
        RecoveryStep::ResetEndpoint { dci: failed_dci },
        RecoveryStep::BotMassStorageReset,
        RecoveryStep::ClearHalt { ep_addr: bulk_in_addr },
        RecoveryStep::ClearHalt { ep_addr: bulk_out_addr },
        RecoveryStep::SetTrDequeuePointer { dci: failed_dci },
    ]
}
