//! The BOT failure/recovery decision, pinned by the 2026-09-11 metal incident.
//!
//! The photograph this suite encodes (`docs/archive/AKUMA_AMD64_USB_XHCI.md`
//! § 2026-09-11): a data phase *timed out* — its `cc=6` event arriving just
//! after the poll budget — and because the timeout path ran no recovery, the
//! device stayed halted and every later CBW timed out until reboot. The boot
//! before it had recovered 57 stalls out of 57, because those arrived inside
//! the budget. The difference between survivable and fatal was one code
//! path; these tests make that path impossible to reintroduce silently.

use akuma_xhci::recovery::{recovery_plan, retry_decision, AttemptOutcome, Decision, OutcomeKind, Phase, RecoveryStep};
use akuma_xhci::trb::cc;

const BULK_IN_DCI: u8 = 3; // EP1 IN (0x81)
const BULK_OUT_DCI: u8 = 4; // EP2 OUT (0x02)

#[test]
fn a_stall_recovers_and_retries_once() {
    let d = retry_decision(
        0,
        &AttemptOutcome::Stalled { code: cc::STALL_ERROR, phase: Phase::Cbw, dci: BULK_OUT_DCI },
    );
    assert_eq!(d, Decision::RecoverAndRetry);
}

#[test]
fn a_timeout_recovers_and_retries_once() {
    // The photograph's first line: `transfer timeout: data`. Before this was
    // tested, the timeout path returned without recovery, the device stayed
    // halted, and the disk died for the rest of the boot.
    for phase in [Phase::Cbw, Phase::Data, Phase::Csw] {
        let d = retry_decision(0, &AttemptOutcome::TimedOut { phase, dci: BULK_IN_DCI });
        assert_eq!(d, Decision::RecoverAndRetry, "phase {phase:?}");
    }
}

#[test]
fn a_desynced_csw_recovers_too() {
    // CSW signature mismatch is the classic BOT pipe desync — exactly what
    // class reset recovery exists for.
    let d = retry_decision(0, &AttemptOutcome::Desynced { phase: Phase::Csw, dci: BULK_IN_DCI });
    assert_eq!(d, Decision::RecoverAndRetry);
}

#[test]
fn a_non_stall_completion_code_is_not_recovery_disease() {
    // Babble / transaction errors: recovery would reset a healthy endpoint
    // and hide a real problem. Give up.
    for code in [cc::BABBLE_DETECTED, cc::USB_TRANSACTION_ERROR, cc::DATA_BUFFER_ERROR] {
        let d = retry_decision(
            0,
            &AttemptOutcome::Stalled { code, phase: Phase::Data, dci: BULK_IN_DCI },
        );
        assert_eq!(d, Decision::GiveUp, "cc={code}");
    }
}

#[test]
fn never_retries_past_the_budget() {
    // Stalls: one recovery cycle, then dead — a device that stalls again is
    // refusing the command, not being slow.
    let stalled = AttemptOutcome::Stalled { code: cc::STALL_ERROR, phase: Phase::Cbw, dci: BULK_OUT_DCI };
    assert_eq!(retry_decision(0, &stalled), Decision::RecoverAndRetry);
    assert_eq!(retry_decision(1, &stalled), Decision::Dead);
    assert_eq!(retry_decision(2, &stalled), Decision::Dead, "past the budget is a caller bug");

    // Timeouts: two cycles — the second covers a device leaving a low-power
    // state (idle spin-down / USB link exit), which needs seconds, not one
    // re-issue. The 2026-09-11 stall cadence tracked the operator's poll gap
    // exactly, and the box died after a long idle on its first access back.
    let timed_out = AttemptOutcome::TimedOut { phase: Phase::Data, dci: BULK_IN_DCI };
    assert_eq!(retry_decision(0, &timed_out), Decision::RecoverAndRetry);
    assert_eq!(retry_decision(1, &timed_out), Decision::RecoverAndRetry);
    assert_eq!(retry_decision(2, &timed_out), Decision::Dead);
    assert_eq!(retry_decision(3, &timed_out), Decision::Dead, "past the budget is a caller bug");

    // Desyncs: one cycle, like stalls.
    let desynced = AttemptOutcome::Desynced { phase: Phase::Csw, dci: BULK_IN_DCI };
    assert_eq!(retry_decision(0, &desynced), Decision::RecoverAndRetry);
    assert_eq!(retry_decision(1, &desynced), Decision::Dead);
}

#[test]
fn phase_log_spellings_match_the_console() {
    // The glue prints these verbatim; the archive doc quotes them.
    assert_eq!(Phase::Cbw.as_str(), "CBW");
    assert_eq!(Phase::Data.as_str(), "data");
    assert_eq!(Phase::Csw.as_str(), "CSW");
}

#[test]
fn recovery_plan_touches_the_failed_ring_only_controller_side() {
    // The failure was on bulk OUT. Reset Endpoint and Set TR Dequeue Pointer
    // are controller-side commands that are only legal on a halted/stopped
    // endpoint — issuing them against the *other*, running ring is the
    // spec-illegal gamble the 2026-09-11 photograph caught the tail of.
    let plan = recovery_plan(OutcomeKind::Stalled, true, BULK_OUT_DCI, 0x81, 0x02);
    assert_eq!(
        plan.iter().filter(|s| matches!(s, RecoveryStep::ResetEndpoint { .. })).count(),
        1,
        "exactly one controller-side endpoint reset"
    );
    assert_eq!(plan[0], RecoveryStep::ResetEndpoint { dci: BULK_OUT_DCI });
    assert_eq!(
        plan.iter().filter(|s| matches!(s, RecoveryStep::SetTrDequeuePointer { .. })).count(),
        1,
        "exactly one Set TR Dequeue Pointer"
    );
    assert_eq!(plan[4], RecoveryStep::SetTrDequeuePointer { dci: BULK_OUT_DCI });
}

#[test]
fn recovery_plan_clears_the_device_halt_on_both_bulk_endpoints() {
    // A controller-side reset does not un-halt the *device*; the device may
    // have halted either endpoint. BOT reset once, then CLEAR_FEATURE on
    // both — IN first, matching `usb_stor_reset_common`'s order.
    let plan = recovery_plan(OutcomeKind::Stalled, true, BULK_OUT_DCI, 0x81, 0x02);
    let halts: Vec<u8> =
        plan.iter().filter_map(|s| match s {
            RecoveryStep::ClearHalt { ep_addr } => Some(*ep_addr),
            _ => None,
        }).collect();
    assert_eq!(halts, [0x81, 0x02]);
    assert!(
        matches!(plan[1], RecoveryStep::BotMassStorageReset),
        "the BOT Mass Storage Reset sits between the controller reset and the clear-halts"
    );
}

#[test]
fn recovery_plan_is_order_sensitive() {
    // The order is the substance: controller reset, device reset, clear
    // halts, THEN move the dequeue — a plan that STDP'd before the endpoint
    // was reset would program a running endpoint; one that retried before
    // the clear-halts would run straight back into the device halt.
    let plan = recovery_plan(OutcomeKind::Stalled, true, BULK_IN_DCI, 0x81, 0x02);
    let expected = [
        RecoveryStep::ResetEndpoint { dci: BULK_IN_DCI },
        RecoveryStep::BotMassStorageReset,
        RecoveryStep::ClearHalt { ep_addr: 0x81 },
        RecoveryStep::ClearHalt { ep_addr: 0x02 },
        RecoveryStep::SetTrDequeuePointer { dci: BULK_IN_DCI },
    ];
    assert_eq!(plan, expected);
}

#[test]
fn a_stall_on_either_ring_produces_its_own_plan() {
    for (dci, addr) in [(BULK_IN_DCI, 0x81), (BULK_OUT_DCI, 0x02)] {
        let plan = recovery_plan(OutcomeKind::Stalled, true, dci, 0x81, 0x02);
        assert_eq!(plan[0], RecoveryStep::ResetEndpoint { dci });
        assert_eq!(plan[4], RecoveryStep::SetTrDequeuePointer { dci });
        let _ = addr;
    }
}

#[test]
fn a_timeout_on_a_running_endpoint_aborts_the_live_td() {
    // Until 2026-09-12 this was the empty plan — "a slow endpoint, leave it
    // alone, just retry". The retry's TRBs then sat BEHIND the still-live TD
    // in a strictly ordered ring: the late completion was for the old TRB
    // (`discarded transfer event: cc=1` on the metal), the second CBW hit the
    // device mid-command, and the device's 13-byte CSW could land in the
    // retry's data TRB as a "successful" short data phase. The `ls`
    // regression blamed on the BOT reset was this. Now: stop the endpoint
    // (the only legal abort of a running TD), reset the device's BOT state,
    // clear both halts, move the dequeue past the dead TD — then retry.
    let plan = recovery_plan(OutcomeKind::TimedOut, false, BULK_OUT_DCI, 0x81, 0x02);
    let expected = [
        RecoveryStep::StopEndpoint { dci: BULK_OUT_DCI },
        RecoveryStep::BotMassStorageReset,
        RecoveryStep::ClearHalt { ep_addr: 0x81 },
        RecoveryStep::ClearHalt { ep_addr: 0x02 },
        RecoveryStep::SetTrDequeuePointer { dci: BULK_OUT_DCI },
    ];
    assert_eq!(plan, expected);
    // No Reset Endpoint: the endpoint is not halted, and Reset Endpoint on a
    // running one is a Context State Error (the 2026-09-11 `cc=0x13`).
    assert!(plan.iter().all(|s| !matches!(s, RecoveryStep::ResetEndpoint { .. })));
}

#[test]
fn only_a_timeout_on_a_running_endpoint_stops_it() {
    // Stop Endpoint is legal on Running only. A halted endpoint (stall, or a
    // timeout whose context reads Halted) takes Reset Endpoint instead; a
    // desync touches no controller state at all.
    for (kind, halted) in [
        (OutcomeKind::Stalled, true),
        (OutcomeKind::TimedOut, true),
        (OutcomeKind::Desynced, false),
    ] {
        let plan = recovery_plan(kind, halted, BULK_IN_DCI, 0x81, 0x02);
        assert!(
            plan.iter().all(|s| !matches!(s, RecoveryStep::StopEndpoint { .. })),
            "{kind:?}/halted={halted}: Stop Endpoint only belongs to a running-endpoint timeout"
        );
    }
}

#[test]
fn a_timeout_with_a_real_halt_still_gets_the_full_sequence() {
    let plan = recovery_plan(OutcomeKind::TimedOut, true, BULK_IN_DCI, 0x81, 0x02);
    assert_eq!(plan[0], RecoveryStep::ResetEndpoint { dci: BULK_IN_DCI });
    assert_eq!(plan[4], RecoveryStep::SetTrDequeuePointer { dci: BULK_IN_DCI });
    assert!(matches!(plan[1], RecoveryStep::BotMassStorageReset));
}

#[test]
fn a_desync_gets_the_device_half_only() {
    // CSW desync: the pipe needs the BOT reset + clear-halts, but no
    // endpoint is halted, so the controller-side commands stay out.
    let plan = recovery_plan(OutcomeKind::Desynced, false, BULK_IN_DCI, 0x81, 0x02);
    assert_eq!(
        plan.iter().filter(|s| matches!(s, RecoveryStep::BotMassStorageReset)).count(),
        1,
    );
    assert!(plan.iter().all(|s| !matches!(s, RecoveryStep::ResetEndpoint { .. })));
    assert!(plan.iter().all(|s| !matches!(s, RecoveryStep::SetTrDequeuePointer { .. })));
}
