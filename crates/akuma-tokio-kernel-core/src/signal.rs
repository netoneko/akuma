//! Signal delivery's decisions, shared by both machines (B21 A5.7).
//!
//! Which pending signal runs, whether a handler restarts the call it
//! interrupted, and which default actions end the group. What a machine contributes is the
//! frame itself ([`Deliver`]): how a handler is entered on its stack and how
//! `rt_sigreturn` puts the interrupted state back. Moved from the amd64 half's
//! `child.rs`.

use akuma_exec::process::{MAX_SIGNALS, SignalAction, SignalHandler};

use crate::proc::{self, NSLOTS, Proc};

/// Signals no mask can block: `SIGKILL` (9) and `SIGSTOP` (19).
pub const UNBLOCKABLE: u64 = (1 << 8) | (1 << 18);

pub const SA_SIGINFO: u64 = 4;
pub const SA_RESTORER: u64 = 0x0400_0000;
pub const SA_RESTART: u64 = 0x1000_0000;
pub const SA_NODEFER: u64 = 0x4000_0000;

/// `-EINTR`, the result `SA_RESTART` acts on.
const EINTR_RESULT: u64 = 0u64.wrapping_sub(4);

/// The machine's half: a trapped thread's register file, about to return to
/// user mode.
pub trait Deliver {
    /// Push a signal frame for `sig` and redirect the return into `entry`.
    /// `result` is what the interrupted call answered; `restart` (the call's
    /// number) asks for the call to run again after the handler instead (the
    /// machine backs its pc over the call instruction and puts the call's
    /// number or first argument back). `false`
    /// when the handler cannot be entered (an unwritable stack, no way back),
    /// with nothing changed.
    #[allow(clippy::too_many_arguments)]
    fn push_handler(
        &mut self,
        sig: u32,
        entry: u64,
        act: &SignalAction,
        result: u64,
        restart: Option<u64>,
        cur_mask: u64,
        restore_mask: u64,
    ) -> bool;
}

/// The mask a handler runs under: the thread's, the action's, and the signal
/// itself unless `SA_NODEFER`.
#[must_use]
pub fn mask_during(sig: u32, act: &SignalAction, cur_mask: u64) -> u64 {
    let bit = 1u64 << (sig - 1);
    cur_mask | act.mask | if act.flags & SA_NODEFER == 0 { bit } else { 0 }
}

/// Deliver the running thread's pending signals at its return to user mode.
///
/// From syscall `nr`, whose result is `r`. One handler at most (the next
/// pending signal waits for this one's `rt_sigreturn`, itself a syscall
/// return); ignored signals are dropped; a default action that kills ends the
/// group (status `-signal`). `no_restart` lists the calls Linux never restarts
/// (the sleeps and the waits with a timeout), in the machine's numbering.
/// Returns what the call answers.
pub fn deliver_at_return<M: Proc, F: Deliver>(f: &mut F, nr: u64, r: u64, no_restart: &[u64]) -> u64 {
    // The interrupt flag exists to break a call that is *blocking*; on the way
    // back nothing is. Left set, the thread's next glue call answers EINTR
    // without running: musl's `raise` and `abort` then lost the mask restore
    // after their `tkill`, so the signal stayed blocked.
    let me = M::current_slot();
    if me < NSLOTS {
        let _ = akuma_threading::take_thread_interrupted(me);
    }
    let mask = akuma_exec::threading::thread_signal_mask();
    // `rt_sigsuspend` and `pause` armed the restore-sigmask slot: a handler
    // entered here records the mask from before the call for its
    // `rt_sigreturn`; with none entered, that mask is the thread's again now.
    let restore = akuma_exec::threading::take_restore_sigmask();
    if !deliver_one::<M, F>(f, nr, r, mask, restore.unwrap_or(mask), no_restart)
        && let Some(m) = restore
    {
        akuma_exec::threading::set_thread_signal_mask(m);
    }
    r
}

/// Take pending signals the mask `cur` lets through until one runs a handler
/// (`true`) or none is left (`false`).
fn deliver_one<M: Proc, F: Deliver>(f: &mut F, nr: u64, r: u64, cur: u64, restore: u64, no_restart: &[u64]) -> bool {
    if M::current_slot() >= NSLOTS {
        return false;
    }
    let Some(p) = akuma_exec::process::current_process_shared() else {
        return false;
    };
    while let Some(sig) = akuma_threading::take_pending_signal(cur & !UNBLOCKABLE) {
        let idx = (sig as usize).wrapping_sub(1);
        if idx >= MAX_SIGNALS {
            continue;
        }
        let act = p.signal_actions.actions.lock()[idx];
        match act.handler {
            SignalHandler::Ignore => {}
            SignalHandler::UserFn(entry) => {
                let restart = (r == EINTR_RESULT && act.flags & SA_RESTART != 0 && !no_restart.contains(&nr)).then_some(nr);
                if f.push_handler(sig, entry as u64, &act, r, restart, cur, restore) {
                    return true;
                }
                // A handler that cannot be entered must not swallow a fatal signal.
                if akuma_syscalls_glue::signal::signal_is_fatal_default(sig) {
                    proc::task_exit_code::<M>(-sig.cast_signed());
                }
            }
            SignalHandler::Default => {
                if akuma_syscalls_glue::signal::signal_is_fatal_default(sig) {
                    akuma_primitives::safe_print!(96, "[signal] pid={} killed by signal {}\n", p.pid, sig);
                    proc::task_exit_code::<M>(-sig.cast_signed());
                }
            }
        }
    }
    false
}

/// At a user-mode tick: a pending signal whose action kills ends the group now.
///
/// So a thread in a loop that makes no syscall still dies of `SIGKILL`
/// (or `SIGTERM` with no handler). Handlers wait for the next syscall return.
pub fn deliver_fatal_on_tick<M: Proc>() {
    let me = M::current_slot();
    if me >= NSLOTS {
        return;
    }
    let pend = akuma_threading::pending_signals_raw(me) & !(akuma_exec::threading::thread_signal_mask() & !UNBLOCKABLE);
    if pend == 0 {
        return;
    }
    let Some(p) = akuma_exec::process::current_process_shared() else {
        return;
    };
    for sig in 1..=64u32 {
        if pend & (1 << (sig - 1)) == 0 || !akuma_syscalls_glue::signal::signal_is_fatal_default(sig) {
            continue;
        }
        let idx = (sig - 1) as usize;
        let default = idx >= MAX_SIGNALS || matches!(p.signal_actions.actions.lock()[idx].handler, SignalHandler::Default);
        if sig == 9 || default {
            akuma_threading::clear_pending_signal(me, sig);
            proc::task_exit_code::<M>(-sig.cast_signed());
        }
    }
}
