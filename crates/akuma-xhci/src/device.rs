//! The endpoint-state decode, and a host simulation of the recovery loop
//! against a scripted enclosure — the proof that the driver's failure path
//! cannot get stuck.
//!
//! Two things live here, for one reason:
//!
//! 1. [`EpState::decode`] pins the endpoint-context state field to the spec
//!    (xHCI §6.2.3, DW0 bits **[2:0]**, no shift). The 2026-09-12 metal run
//!    found the glue computing `(dw0 >> 2) & 0x7`, which read the RUNNING
//!    endpoint's `0x1` as `0` = Disabled — and made `halted` unreachable, so
//!    Reset Endpoint + Set TR Dequeue Pointer had *never once executed* on
//!    real silicon. The decode is load-bearing, and this is where a wrong
//!    shift is caught without a boot.
//! 2. [`SimDevice`] + [`drive_command`] model the whole failure loop — a BOT
//!    attempt answers, the glue decides (`recovery::retry_decision`), the
//!    plan's steps execute against the model with their spec legality rules,
//!    the command re-issues — so the property that matters is a test: every
//!    scripted device behavior terminates in bounded attempts, a halt that
//!    the device honors recovery for is always cleared, and no step is ever
//!    issued against a state that forbids it.

use crate::recovery::{recovery_plan, retry_decision, AttemptOutcome, Decision, OutcomeKind, Phase, RecoveryStep};

/// xHCI Endpoint Context state (§6.2.3, Table 6-9) — the value in DW0 bits
/// [2:0] of the endpoint context the controller keeps at DCBAA[slot].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EpState {
    /// 0 — the endpoint is not configured (or a buggy read shifted it here).
    Disabled,
    /// 1 — primed and able to run.
    Running,
    /// 2 — halted by the controller (a STALL, or the device'shalt condition);
    /// transfers are ignored until Reset Endpoint + Set TR Dequeue Pointer.
    Halted,
    /// 3 — primed but not running (after Reset Endpoint, awaiting a doorbell).
    Stopped,
    /// 4 — the controller detected a protocol error.
    Error,
}

impl EpState {
    /// Decode from an endpoint context's first dword. The field is bits
    /// [2:0] — `dw0 & 0x7`, NO shift.
    #[must_use]
    pub fn decode(dw0: u32) -> Self {
        match dw0 & 0x7 {
            0 => Self::Disabled,
            1 => Self::Running,
            2 => Self::Halted,
            3 => Self::Stopped,
            _ => Self::Error,
        }
    }

    /// The glue's log spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "dis",
            Self::Running => "run",
            Self::Halted => "halt",
            Self::Stopped => "stop",
            Self::Error => "err",
        }
    }
}

/// One bulk endpoint as the simulation sees it: the controller-side context
/// state plus the device-side halt condition behind it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SimEndpoint {
    /// What the controller's context reads (what the glue's `ep_state` sees).
    pub state: EpState,
    /// Whether the *enclosure* holds the endpoint halted (refuses transfers
    /// with STALL). Only a device-side Mass Storage Reset + CLEAR_FEATURE
    /// clears this; controller-side steps cannot.
    pub device_halted: bool,
}

/// Index of a bulk endpoint in the simulation: `0` = IN, `1` = OUT.
pub const EP_IN: usize = 0;
pub const EP_OUT: usize = 1;

/// One recorded state transition, so tests (and the glue's log line) can pin
/// the path a recovery took.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Transition {
    pub ep: usize,
    pub from: EpState,
    pub to: EpState,
}

/// A simulated enclosure + controller pair: two bulk endpoint contexts, a
/// transition log, and the answers a scripted behavior makes it give.
pub struct SimDevice {
    pub ep: [SimEndpoint; 2],
    /// (ep, from, to) for every controller-visible state change, oldest
    /// first. Fixed-size: a plan touches at most a handful of states.
    pub transitions: [Option<Transition>; 8],
    transition_count: usize,
    /// Steps issued against a state that forbids them — must stay empty for
    /// every reachable behavior.
    pub illegal_steps: usize,
    /// Times Set TR Dequeue Pointer re-pointed a ring (the 2026-09-11 gap:
    /// Reset Endpoint without this parks the dequeue on the stalled TRB).
    pub dequeues_moved: usize,
    /// What the glue observes instead of the truth, when set — the seam the
    /// 2026-09-12 `>> 2` decode bug sat on (it observed `false` — not
    /// halted — for a genuinely halted endpoint). `None` reads the model.
    pub state_read_override: Option<bool>,
}

impl Default for SimDevice {
    fn default() -> Self {
        Self::new()
    }
}

impl SimDevice {
    /// Both endpoints running, device healthy.
    #[must_use]
    pub fn new() -> Self {
        Self {
            ep: [
                SimEndpoint { state: EpState::Running, device_halted: false },
                SimEndpoint { state: EpState::Running, device_halted: false },
            ],
            transitions: [None; 8],
            transition_count: 0,
            illegal_steps: 0,
            dequeues_moved: 0,
            state_read_override: None,
        }
    }

    /// Force a state (bring-up, or a scripted fault) — recorded.
    pub fn set_state(&mut self, ep: usize, to: EpState) {
        self.transition(ep, to);
    }

    /// Simulate the endpoint answering the BOT phase a transfer is on.
    /// The enclosure answers with a STALL when it holds the halt; a halted
    /// controller endpoint delivers nothing at all (the glue times out).
    #[must_use]
    pub fn phase_answer(&mut self, ep: usize) -> SimAnswer {
        if self.ep[ep].state == EpState::Halted {
            return SimAnswer::Nothing;
        }
        if self.ep[ep].device_halted {
            // The device refuses: it STALLs the phase, the controller marks
            // the endpoint halted, the event carries cc=STALL_ERROR.
            self.transition(ep, EpState::Halted);
            return SimAnswer::Stalled;
        }
        SimAnswer::Done
    }

    /// What the simulated enclosure answers for one BOT phase, *enacted on
    /// the model*: a refusal (the enclosure holds the halt) STALLs the phase
    /// and the controller marks the context Halted; a context that is
    /// already Halted delivers nothing at all — that is what a timeout on
    /// the metal is. `Done` from a healthy enclosure just completes.
    pub fn enact(&mut self, ep: usize, intent: SimAnswer) -> SimAnswer {
        if self.ep[ep].state == EpState::Halted {
            return SimAnswer::Nothing;
        }
        match intent {
            SimAnswer::Done => {
                if self.ep[ep].device_halted {
                    self.transition(ep, EpState::Halted);
                    SimAnswer::Stalled
                } else {
                    SimAnswer::Done
                }
            }
            SimAnswer::Stalled => {
                self.transition(ep, EpState::Halted);
                SimAnswer::Stalled
            }
            SimAnswer::Nothing => SimAnswer::Nothing,
        }
    }

    /// Controller-side: clear a halt. Legal only on a HALTED endpoint
    /// (xHCI §4.6.9 — resetting a running endpoint deranges a live ring).
    /// Halted -> Stopped; the doorbell (the retry) moves it to Running.
    pub fn reset_endpoint(&mut self, ep: usize) {
        if self.ep[ep].state == EpState::Halted {
            self.transition(ep, EpState::Stopped);
        } else {
            self.illegal_steps += 1;
        }
    }

    /// Controller-side: abort a *running* endpoint's live TD (spec §4.6.9
    /// Stop Endpoint). Running -> Stopped; on any other state the controller
    /// answers Context State Error, counted here as an illegal step. This is
    /// the step a timeout on a running endpoint needs before its dequeue can
    /// legally move — a plain retry behind the live TD was the 2026-09-12
    /// double-TD bug.
    pub fn stop_endpoint(&mut self, ep: usize) {
        if self.ep[ep].state == EpState::Running {
            self.transition(ep, EpState::Stopped);
        } else {
            self.illegal_steps += 1;
        }
    }

    /// Controller-side: move the dequeue past the failed TD. Legal only in
    /// the Stopped or Error state (spec §4.6.10, endpoint state machine
    /// Figure 4-4): on Running the ring is live, and on **Halted** the only
    /// exit is Reset Endpoint — a Set TR Dequeue Pointer there is a Context
    /// State Error, not a move. The ring restart is what a "Reset Endpoint
    /// without this" recovery forgot.
    pub fn set_tr_dequeue_pointer(&mut self, ep: usize) {
        if matches!(self.ep[ep].state, EpState::Stopped | EpState::Error) {
            self.dequeues_moved += 1;
        } else {
            self.illegal_steps += 1;
        }
    }

    /// Device-side: the BOT Mass Storage Reset — clears the *device's* halt
    /// condition on both bulk endpoints.
    pub fn mass_storage_reset(&mut self) {
        self.ep[EP_IN].device_halted = false;
        self.ep[EP_OUT].device_halted = false;
    }

    /// Device-side: `CLEAR_FEATURE(ENDPOINT_HALT)` on one bulk endpoint.
    pub fn clear_halt(&mut self, ep: usize) {
        self.ep[ep].device_halted = false;
    }

    /// The glue's retry: ring the doorbell and run the phase. A Stopped
    /// endpoint starts; the answer is what the enclosure gives.
    pub fn retry_phase(&mut self, ep: usize) -> SimAnswer {
        if self.ep[ep].state == EpState::Stopped {
            self.transition(ep, EpState::Running);
        }
        self.phase_answer(ep)
    }

    fn transition(&mut self, ep: usize, to: EpState) {
        let from = self.ep[ep].state;
        if from != to && self.transition_count < self.transitions.len() {
            self.transitions[self.transition_count] = Some(Transition { ep, from, to });
            self.transition_count += 1;
        }
        self.ep[ep].state = to;
    }

    /// The log so far, as a slice of recorded transitions.
    pub fn recorded(&self) -> impl Iterator<Item = Transition> + '_ {
        self.transitions.iter().filter_map(|t| *t)
    }
}

/// What the simulated enclosure answers for one BOT phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SimAnswer {
    /// The phase completed inside the budget.
    Done,
    /// The device STALLed the phase (cc=STALL_ERROR).
    Stalled,
    /// Nothing arrived within the budget — the glue times out.
    Nothing,
}

/// Verdict of a driven command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The command completed.
    Served,
    /// The retry cap was reached — reported, no further attempts. Bounded.
    Dead,
    /// A non-STALL completion code: not recovery's disease, reported.
    GivenUp,
}

/// The scripted enclosure: what it answers for each phase of each attempt.
/// A behavior is a small fixed script; past its end it repeats its last
/// answer (so "always Late" and "stalls forever" are two-line scripts).
type Script<'a> = &'a mut dyn FnMut(usize, Phase) -> SimAnswer;

const BULK_IN_ADDR: u8 = 0x81;
const BULK_OUT_ADDR: u8 = 0x02;

fn ep_of(dci: u8) -> usize {
    if dci == 3 { EP_IN } else { EP_OUT }
}

/// Drive one BOT command the way the glue does — attempt, decide
/// ([`retry_decision`]), execute the plan ([`recovery_plan`]) against the
/// model with its legality rules, re-issue — and report the verdict.
///
/// `answer(attempt, phase)` is the scripted enclosure. The first phase that
/// does not answer `Done` is the failure the recovery sees, exactly like
/// `bot_run_once` walking CBW / data / CSW.
#[must_use]
pub fn drive_command(dev: &mut SimDevice, answer: Script<'_>, failed_dci: u8) -> Verdict {
    let ep = ep_of(failed_dci);
    for attempt in 0..=2u8 {
        // A retry rings the doorbell first: a Stopped endpoint (Reset
        // Endpoint's result) starts here, Halted -> Stopped -> Running.
        if attempt > 0 {
            dev.retry_phase(ep);
        }
        // One attempt: run the three BOT phases until one fails. The
        // enclosure's intent is enacted on the model — a STALL marks the
        // context Halted; a Halted context answers Nothing (a timeout).
        let mut outcome = None;
        for phase in [Phase::Cbw, Phase::Data, Phase::Csw] {
            match dev.enact(ep, answer(usize::from(attempt), phase)) {
                SimAnswer::Done => {}
                SimAnswer::Stalled => {
                    outcome = Some(AttemptOutcome::Stalled {
                        code: crate::trb::cc::STALL_ERROR,
                        phase,
                        dci: failed_dci,
                    });
                    break;
                }
                SimAnswer::Nothing => {
                    outcome = Some(AttemptOutcome::TimedOut { phase, dci: failed_dci });
                    break;
                }
            }
        }
        let Some(outcome) = outcome else {
            return Verdict::Served;
        };

        match retry_decision(attempt, &outcome) {
            Decision::GiveUp => return Verdict::GivenUp,
            Decision::Dead => return Verdict::Dead,
            Decision::RecoverAndRetry => {}
        }

        // The glue reads the live endpoint state to pick the plan's
        // controller-side half — the read the 2026-09-12 bug corrupted.
        let observed_halted = dev
            .state_read_override
            .unwrap_or_else(|| dev.ep[ep].state == EpState::Halted);
        let kind = match outcome {
            AttemptOutcome::Stalled { .. } => OutcomeKind::Stalled,
            AttemptOutcome::TimedOut { .. } => OutcomeKind::TimedOut,
            AttemptOutcome::Desynced { .. } => OutcomeKind::Desynced,
        };
        for step in recovery_plan(kind, observed_halted, failed_dci, BULK_IN_ADDR, BULK_OUT_ADDR) {
            exec_step(dev, ep, step);
        }
    }
    Verdict::Dead
}

/// Execute one plan step against the simulation, with the spec legality the
/// hardware enforces by ignoring (or deranging) illegal ones.
fn exec_step(dev: &mut SimDevice, failed_ep: usize, step: RecoveryStep) {
    match step {
        RecoveryStep::ResetEndpoint { .. } => dev.reset_endpoint(failed_ep),
        RecoveryStep::StopEndpoint { .. } => dev.stop_endpoint(failed_ep),
        RecoveryStep::SetTrDequeuePointer { .. } => dev.set_tr_dequeue_pointer(failed_ep),
        RecoveryStep::BotMassStorageReset => dev.mass_storage_reset(),
        RecoveryStep::ClearHalt { ep_addr } => {
            if ep_addr == BULK_IN_ADDR {
                dev.clear_halt(EP_IN);
            } else {
                dev.clear_halt(EP_OUT);
            }
        }
        RecoveryStep::None => {}
    }
}
