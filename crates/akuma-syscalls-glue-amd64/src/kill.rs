//! `kill(2)` for the x86 kernels.
//!
//! A signal is pended on the target's thread group and taken by each thread at its own next
//! return to ring 3 (`akuma_exec::process::pend_signal_to_group`), never by marking a running
//! thread dead from another core.

const EPERM: u64 = (-1i64) as u64;
const ESRCH: u64 = (-3i64) as u64;
const EINVAL: u64 = (-22i64) as u64;

/// `kill(2)` — x86_64 62, for the x86 kernels (amd64 and Taoxichuan), not glue's (2026-10-09;
/// moved here from `amd64/src/signal.rs` 2026-11-01 so Taoxichuan delivers it the same way).
///
/// Glue's `sys_kill` hands SIGKILL to `akuma_exec::process::deliver_signal`,
/// whose SIGKILL arm is `kill_thread_group` + `kill_process_with_signal`: the
/// AArch64 teardown, which marks every sibling's task slot `TERMINATED` **from
/// the killer's core** and expects the victims to have consumed a deferred
/// kill at their EL1→EL0 boundary first. Nothing on this target consumes that
/// request (`take_thread_kill_request` has no x86 caller), and a thread here
/// leaves ring 3 only through its own syscall return or a tick. So on amd64
/// that path was, measured with `userspace/forktest/c_stress/killtree.c` under
/// Firecracker:
///
/// * **2 s per `kill -9` of any threaded process** — the full `KILL_GRACE_US`,
///   every time, because the siblings never die on their own; a three-process
///   tree took 6.1 s against 4 ms on Linux;
/// * **a cross-core hard kill of RUNNING threads** (`[kill] … victim_state=2`),
///   which skips `thread::teardown` and `run_process`'s epilogue: the
///   `THREADS` row, `clear_child_tid`, the `MAP_SHARED` writeback and the
///   `SPAWN` row are all left to whoever reuses the slot;
/// * **leaked rows that compound**: the next process in that slot found 17,
///   then 41, then 65 "live" threads at its exit (`DRAIN INCOMPLETE`), spun
///   the 100 000-round drain, and `wake_group` posted kill requests to task
///   slots those rows no longer owned;
/// * **`kill(-pgid, …)` answered `ESRCH`** (the register was read as a `u32`),
///   so kami's group kill never reached anything, and **`kill(<thread tid>,
///   9)` answered `ESRCH`** too.
///
/// The two hard hangs on the ryzen box (2026-10-09, `[BKL] stuck … tag=501`
/// right after SIGKILLing a Chromium tree) are this class — a thread
/// hard-terminated mid-kernel on another core — and the same stuck line
/// reproduced under Firecracker with eight vCPUs in the probe's fifth round.
/// Record: `docs/archive/AKUMA_AMD64_SIGKILL_NATIVE_PATH.md`.
///
/// Here every signal, SIGKILL included, is **pended and the wait interrupted**
/// (`pend_signal_to_group`), and the death happens on the dying thread:
/// [`deliver_pending`] takes `Next::Fatal`, `exit_current_from_signal` sets
/// `leave`, a thread unwinds through `run_thread` → `teardown`, the leader
/// through `run_process`, whose epilogue drains the rest. That is the path a
/// SIGTERM already took on this target, and it was sound; SIGKILL differs only
/// in that no handler can be installed for it, which `next_delivery` already
/// knows. A thread in a compute loop is reached by the tick
/// (`deliver_pending_on_tick`), as it is for any other fatal signal.
///
/// Targets follow Linux: `pid > 0` one process, `0` the caller's group, `-1`
/// everyone but init and the caller, `pid < -1` the group `-pid`. Group
/// delivery walks the process table once into a fixed array — no `Vec` on a
/// path that runs while the machine is in trouble — and addresses each thread
/// group through its leader, so a thread's own row (`pid != tgid`) is never a
/// second delivery to the same group.
#[must_use]
pub fn kill(pid: u64, sig: u64) -> u64 {
    use akuma_exec::process::{kill_target, pend_signal_to_group, KillTarget};
    if sig as usize > akuma_exec::process::MAX_SIGNALS {
        return EINVAL;
    }
    let sig = sig as u32;
    // `pid_t` is 32 bits in a 64-bit register: `kill(-53, 9)` arrives as
    // `0xffff_ffff_ffff_ffcb`, and the truncation is what recovers `-53`.
    let target = kill_target((pid as u32).cast_signed());
    match target {
        KillTarget::Pid(target) => {
            // init is not a target, as it was not through glue (`EPERM`).
            if target == 1 && sig != 0 {
                return EPERM;
            }
            // Box, then uid — before the `sig == 0` probe, so probing cannot
            // find a process in a box the caller cannot see.
            if let Err(e) = akuma_exec::process::check_signal_pid(target) {
                return akuma_syscalls_glue::signal::deny_errno(e);
            }
            if sig == 0 {
                return 0;
            }
            if pend_signal_to_group(target, sig) { 0 } else { ESRCH }
        }
        KillTarget::OwnGroup | KillTarget::Group(_) | KillTarget::All => {
            let me = akuma_exec::process::current_process_shared();
            let (my_tgid, my_pgid) = me.map_or((0, 0), |p| (p.tgid, p.pgid));
            let group = match target {
                KillTarget::Group(g) => Some(g),
                KillTarget::OwnGroup => Some(my_pgid),
                _ => None,
            };
            // Leaders only (`pid == tgid`): one delivery per thread group.
            // Fixed array, like `deliver_signal`'s — the callback runs with
            // IRQs disabled and may not allocate.
            let mut targets = [0u32; akuma_exec::process::MAX_PROCESSES];
            let mut count = 0usize;
            akuma_exec::process::for_each_process(|p| {
                if p.pid != p.tgid || count >= targets.len() {
                    return;
                }
                let wanted = match group {
                    Some(g) => p.pgid == g,
                    // `kill(-1)`: everything but init and the caller's group.
                    None => p.pid > 1 && p.tgid != my_tgid,
                };
                if wanted {
                    targets[count] = p.pid;
                    count += 1;
                }
            });
            // Filter outside the callback (it runs IRQs-off; `check_signal`
            // takes the box-registry lock). Linux's rule for a group or
            // `kill(-1)`: signal every member you may, succeed if that was at
            // least one, else EPERM if something was only refused by uid and
            // ESRCH if nothing reachable existed. Without this, `kill(-1, 9)`
            // from inside a box killed every process on the machine.
            let (mut sent, mut forbidden) = (0usize, false);
            for &t in &targets[..count] {
                match akuma_exec::process::check_signal_pid(t) {
                    Ok(()) => {
                        if sig != 0 {
                            pend_signal_to_group(t, sig);
                        }
                        sent += 1;
                    }
                    Err(akuma_exec::process::SignalDeny::Forbidden) => forbidden = true,
                    Err(akuma_exec::process::SignalDeny::Invisible) => {}
                }
            }
            if sent > 0 {
                0
            } else if forbidden {
                EPERM
            } else {
                ESRCH
            }
        }
    }
}

/// `tkill(tid, sig)` / `tgkill(tgid, tid, sig)` — x86_64 200 and 234.
///
/// **Not folded to `akuma_syscalls_glue::sys_tkill`, and the reason is a
/// divergence worth keeping visible.** That function decides fatality *inline*:
/// a `SIG_DFL` signal whose default action is termination calls
/// `sys_exit_group` from inside the `tkill`, which on this target would leave
/// ring 3 through glue's epilogue instead of `run_process`'s — no `SPAWN` row
/// stamped, no `thread::drain`, the thread parked in a `yield_now` loop. Here
/// every signal is *pended* and every fatality decision belongs to
/// [`deliver_pending`], which is the one place that knows how this kernel
/// leaves ring 3.
///
/// The dispositions it does reproduce are glue's, because they are POSIX:
/// `SIG_IGN` drops the signal, `SIGKILL` is unconditional, and a blocked signal
/// pends rather than acting.
#[must_use]
pub fn tkill(tid: u32, sig: u32) -> u64 {
    if let Err(e) = akuma_syscalls_glue::signal::check_signal_tid(tid) {
        return akuma_syscalls_glue::signal::deny_errno(e);
    }
    if sig == 0 {
        return 0;
    }
    if sig as usize > akuma_exec::process::MAX_SIGNALS {
        return EINVAL;
    }
    let tid = tid as usize;
    if tid >= akuma_exec::threading::MAX_THREADS {
        return ESRCH;
    }

    let handler = akuma_exec::process::find_pid_by_thread(tid)
        .and_then(akuma_exec::process::lookup_process_shared)
        .map_or(akuma_exec::process::SignalHandler::Default, |p| {
            p.signal_actions.actions.lock()[(sig - 1) as usize].handler
        });
    // `SIG_IGN` drops the signal — except SIGKILL and SIGSTOP, which cannot be
    // ignored (`sigaction` refuses them, but a stale table entry must not win).
    if matches!(handler, akuma_exec::process::SignalHandler::Ignore) && sig != 9 && sig != 19 {
        return 0;
    }

    // **SIGKILL is pended like every other signal** (2026-10-09). Until then
    // this arm called `exit_current_from_signal(9)` — on the *caller*, whatever
    // `tid` named, which was only right for `tkill(gettid(), SIGKILL)`. Pending
    // it is enough: `take_pending_signal` cannot mask bit 9, `next_delivery`
    // reaches `fatal_default` for it, and the target leaves at its own next
    // syscall return or tick, which is the only way a thread leaves ring 3 on
    // this target. For `tid == self` that return is the one this syscall is
    // about to make, so the effect is immediate either way.
    //
    // A fatal default to a *parked* thread needs the interrupt bit as well as
    // the pend: `pend_signal_for_thread` wakes the slot, but a park loop that
    // finds no `UserFn` handler re-parks (`current_thread_has_pending_interrupt`
    // reports only handled signals), and the thread never reaches the return
    // where the default action runs. `kill(2)` sets the same bit for the same
    // reason (`pend_signal_to_group`).
    if matches!(handler, akuma_exec::process::SignalHandler::Default)
        && akuma_syscalls_glue::signal::signal_is_fatal_default(sig)
    {
        akuma_exec::process::interrupt_thread(tid);
    }

    // **Pend, and do not touch `ProcessChannel::interrupted`.**
    //
    // That flag is the Ctrl-C sledgehammer, not a signal: glue's dispatch
    // prologue reads it on *every* syscall, marks the process a `Zombie(130)`
    // and returns `EINTR` — `SA_RESTART`-blind and disposition-blind by design,
    // because Ctrl-C's job is to end the foreground job. Setting it here made
    // `raise(3)` fail in the most confusing possible way: musl's `raise` is
    // `block-all` / `tgkill` / `restore`, and the `EINTR` landed on the
    // **restore**, so the signal stayed blocked forever and the handler never
    // ran. Glue's own `sys_tkill` does not set it either; the per-thread `EINTR`
    // decision belongs to `current_thread_has_pending_interrupt`, which reads
    // the pending set and honours `SA_RESTART`.
    akuma_exec::threading::pend_signal_for_thread(tid, sig);
    0
}

/// See [`sys_tkill`]. The `tgid` check is what stops a recycled tid taking a
/// signal meant for the thread that used to own the slot.
#[must_use]
pub fn tgkill(tgid: u32, tid: u32, sig: u32) -> u64 {
    if let Some(pid) = akuma_exec::process::find_pid_by_thread(tid as usize)
        && let Some(proc) = akuma_exec::process::lookup_process_shared(pid)
        && proc.tgid != tgid
    {
        return ESRCH;
    }
    tkill(tid, sig)
}
