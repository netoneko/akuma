//! The recovery loop, simulated against a scripted enclosure until it gives
//! a reason to believe it cannot get stuck.
//!
//! Three test families:
//!
//! 1. **The decode is the spec's** — `EpState::decode` reads DW0 bits [2:0]
//!    with no shift, and the shift the glue carried for its first week (the
//!    2026-09-12 finding: RUNNING read as Disabled, `halted` unreachable,
//!    Reset Endpoint never once executed on the metal) is pinned as the
//!    wedge it was.
//! 2. **Every reachable behavior terminates** — the scripted-enclosure sweep
//!    drives `drive_command` over the whole answer matrix (3 answers over
//!    3 attempts x 3 BOT phases = 19 683 scripts) and bounds every run.
//! 3. **Halts clear** — when the device honors the class recovery, the plan
//!    returns the endpoint to Running with legal steps only; when the state
//!    read lies, the same disease wedges.

use akuma_xhci::device::{drive_command, EpState, SimAnswer, SimDevice, Verdict, EP_IN, EP_OUT};
use akuma_xhci::recovery::Phase;

const BULK_IN_DCI: u8 = 3;
const BULK_OUT_DCI: u8 = 4;

#[test]
fn the_decode_is_bits_two_zero_no_shift() {
    // The exact metal values from the 2026-09-12 timeout diagnostic.
    assert_eq!(EpState::decode(0x0000_0001), EpState::Running); // ep ctx dw0
    assert_eq!(EpState::decode(0x0000_0000), EpState::Disabled);
    assert_eq!(EpState::decode(0x0000_0002), EpState::Halted);
    assert_eq!(EpState::decode(0x0000_0003), EpState::Stopped);
    assert_eq!(EpState::decode(0x0000_0004), EpState::Error);
    // Noise above bit 2 is other fields (mult, max P-streams, interval...) —
    // a real context dword never has the state field to itself.
    assert_eq!(EpState::decode(0x0008_0001), EpState::Running);
}

#[test]
fn the_old_shift_read_every_live_state_wrong() {
    // Regression pin: this arithmetic is what the glue shipped with. From any
    // legal state dword (0..=4) it never once produced Halted — which is why
    // the full recovery path never ran on the metal.
    let old = |dw0: u32| (dw0 >> 2) & 0x7;
    for dw0 in 0u32..=4 {
        assert_ne!(
            EpState::decode(old(dw0)),
            EpState::Halted,
            "dw0={dw0}: the old shift would have detected a real halt"
        );
    }
    // And its signature failure: the RUNNING endpoint the diagnostic dumped.
    assert_eq!(EpState::decode(old(0x1)), EpState::Disabled);
}

#[test]
fn a_stall_clears_and_the_command_completes() {
    // The enclosure holds the OUT endpoint; the first CBW stalls, the plan
    // clears device + controller sides, the retry serves. This is the path
    // that never executed on the metal for a week.
    let dev = &mut SimDevice::new();
    dev.ep[EP_OUT].device_halted = true;
    let v = drive_command(dev, &mut |attempt, _| match attempt {
        0 => SimAnswer::Stalled,
        _ => SimAnswer::Done,
    }, BULK_OUT_DCI);
    assert_eq!(v, Verdict::Served);
    assert_eq!(dev.ep[EP_OUT].state, EpState::Running);
    assert!(!dev.ep[EP_OUT].device_halted);
    assert_eq!(dev.illegal_steps, 0, "every controller-side step was legal");
    assert_eq!(dev.dequeues_moved, 1, "the dequeue moved past the stalled TD");
    // And the transitions the glue should have printed:
    let seen: Vec<_> = dev.recorded().collect();
    assert_eq!((seen[0].from, seen[0].to), (EpState::Running, EpState::Halted));
    assert_eq!((seen[1].from, seen[1].to), (EpState::Halted, EpState::Stopped));
    assert_eq!((seen[2].from, seen[2].to), (EpState::Stopped, EpState::Running));
}

#[test]
fn a_slow_device_that_never_answers_still_terminates() {
    // The idle-wake disease: every phase late. The driver must give up in a
    // bounded number of attempts — never loop, never touch a live ring.
    let dev = &mut SimDevice::new();
    let calls = core::cell::Cell::new(0);
    let v = drive_command(dev, &mut |_, _| {
        calls.set(calls.get() + 1);
        SimAnswer::Nothing
    }, BULK_IN_DCI);
    assert_eq!(v, Verdict::Dead);
    assert_eq!(
        dev.ep[EP_IN].state,
        EpState::Running,
        "the last retry rang the doorbell; the cap gives up without touching the ring again"
    );
    assert_eq!(dev.illegal_steps, 0, "Stop Endpoint on Running, Set TR Dequeue on Stopped: all legal");
    // Two recovery cycles (timeouts get two), and each one aborted the live
    // TD and moved the dequeue past it before the retry.
    assert_eq!(dev.dequeues_moved, 2);
    // The loop stops at the first failing phase (Cbw), so a phase is only
    // ever scripted as far as it got: 3 attempts, one phase each.
    assert_eq!(calls.get(), 3);
}

#[test]
fn a_timeout_aborts_the_live_td_before_the_retry() {
    // The 2026-09-12 double-TD bug, as a transition trace. A running endpoint
    // never answers the data phase (a drive waking from standby); the plan
    // must take it Running -> Stopped (Stop Endpoint: the abort), move the
    // dequeue while Stopped, and the retry's doorbell takes it back to
    // Running — on a ring that no longer carries the dead TD.
    let dev = &mut SimDevice::new();
    let v = drive_command(dev, &mut |attempt, phase| match (attempt, phase) {
        (0, Phase::Data) => SimAnswer::Nothing,
        _ => SimAnswer::Done,
    }, BULK_IN_DCI);
    assert_eq!(v, Verdict::Served);
    assert_eq!(dev.illegal_steps, 0);
    assert_eq!(dev.dequeues_moved, 1, "the dequeue moved past the timed-out TD exactly once");
    let seen: Vec<_> = dev.recorded().map(|t| (t.from, t.to)).collect();
    assert_eq!(
        seen,
        [(EpState::Running, EpState::Stopped), (EpState::Stopped, EpState::Running)],
        "stop (abort), then the retry's doorbell — no halt, no reset"
    );
}

#[test]
fn a_dequeue_move_on_a_halted_endpoint_is_illegal() {
    // Figure 4-4: Halted's only exit is Reset Endpoint. A Set TR Dequeue
    // Pointer there is a Context State Error on the controller — and the
    // model must say so, or a plan that skipped Reset Endpoint would look
    // like it moved the ring.
    let dev = &mut SimDevice::new();
    dev.set_state(EP_OUT, EpState::Halted);
    dev.set_tr_dequeue_pointer(EP_OUT);
    assert_eq!(dev.illegal_steps, 1);
    assert_eq!(dev.dequeues_moved, 0);
    // Stop Endpoint is Running-only too.
    dev.stop_endpoint(EP_OUT);
    assert_eq!(dev.illegal_steps, 2);
    // The legal path: Reset Endpoint (Halted -> Stopped), then the move.
    dev.reset_endpoint(EP_OUT);
    dev.set_tr_dequeue_pointer(EP_OUT);
    assert_eq!(dev.illegal_steps, 2, "no new illegal step");
    assert_eq!(dev.dequeues_moved, 1);
}

#[test]
fn every_scripted_behavior_terminates_within_the_cap() {
    // The "won't get stuck" sweep: every answer matrix of 3 attempts x 3
    // phases, driven to a verdict. No script can run away — drive_command
    // has no loop beyond its attempt cap — and no script may record an
    // illegal step (a controller-side op against a forbidding state).
    let answers = [SimAnswer::Done, SimAnswer::Stalled, SimAnswer::Nothing];
    let mut scripts = 0;
    for c0 in answers {
        for c1 in answers {
            for c2 in answers {
                for d0 in answers {
                    for d1 in answers {
                        for d2 in answers {
                            for s0 in answers {
                                for s1 in answers {
                                    for s2 in answers {
                                        let cells =
                                            [c0, c1, c2, d0, d1, d2, s0, s1, s2];
                                        let dev = &mut SimDevice::new();
                                        let v = drive_command(
                                            dev,
                                            &mut move |attempt, phase| {
                                                let pi = match phase {
                                                    Phase::Cbw => 0,
                                                    Phase::Data => 1,
                                                    Phase::Csw => 2,
                                                };
                                                cells[attempt.min(2) * 3 + pi]
                                            },
                                            BULK_OUT_DCI,
                                        );
                                        assert!(matches!(
                                            v,
                                            Verdict::Served | Verdict::Dead | Verdict::GivenUp
                                        ));
                                        assert_eq!(
                                            dev.illegal_steps, 0,
                                            "script {cells:?}: a step hit a forbidding state"
                                        );
                                        scripts += 1;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    assert_eq!(scripts, 19_683, "the whole matrix ran");
}

#[test]
fn a_honoring_device_is_always_recovered_from_a_halt() {
    // Whatever the first attempt answers, if the enclosure honors the class
    // recovery (BOT reset + clear-halts) the next attempt serves — checked
    // from both failure openings. (A second *deliberate* STALL after
    // recovery is the other rule: terminal, not retried — a device refusing
    // a command twice is dead, by `retry_decision`'s cap.)
    for first in [SimAnswer::Stalled, SimAnswer::Nothing] {
        let dev = &mut SimDevice::new();
        if first == SimAnswer::Stalled {
            dev.ep[EP_IN].device_halted = true;
        }
        let v = drive_command(dev, &mut move |attempt, _| match attempt {
            0 => first,
            _ => SimAnswer::Done,
        }, BULK_IN_DCI);
        assert_eq!(v, Verdict::Served, "first={first:?}");
        assert_eq!(dev.ep[EP_IN].state, EpState::Running);
        assert!(!dev.ep[EP_IN].device_halted);
        assert_eq!(dev.illegal_steps, 0);
    }
}

#[test]
fn misreading_the_state_wedges_what_the_fix_recovers() {
    // The differential proof that the decode is load-bearing. Same disease,
    // same enclosure, same script — the ONLY difference is what the glue's
    // state read reports. With the truth (the fix): Served, dequeue moved.
    // With the lie the `>> 2` bug produced (observed "not halted" for a
    // genuinely halted endpoint): the plan loses its controller-side half —
    // no Reset Endpoint, no Set TR Dequeue Pointer — the device-side steps
    // clear the enclosure but the controller endpoint stays Halted and
    // delivers nothing, every retry times out, and the command dies at the
    // cap. That was the box, for a week.
    // The script: one STALL, then the enclosure is willing. Both runs get
    // the identical enclosure; only the state read differs.
    let mut script = |attempt: usize, _phase: Phase| {
        if attempt == 0 { SimAnswer::Stalled } else { SimAnswer::Done }
    };

    let dev = &mut SimDevice::new();
    dev.ep[EP_OUT].device_halted = true;
    let v = drive_command(dev, &mut script, BULK_OUT_DCI);
    assert_eq!(v, Verdict::Served, "the honest read serves");
    assert_eq!(dev.dequeues_moved, 1);

    let dev = &mut SimDevice::new();
    dev.ep[EP_OUT].device_halted = true;
    dev.state_read_override = Some(false); // the 2026-09-12 bug, expressed
    let v = drive_command(dev, &mut script, BULK_OUT_DCI);
    assert_eq!(v, Verdict::Dead, "the misread disease ends in the cap");
    assert_eq!(dev.ep[EP_OUT].state, EpState::Halted, "still halted");
    assert_eq!(dev.dequeues_moved, 0, "no dequeue was ever moved");
}
