// This crate is unsafe-free by design and stays that way: `forbid` (not
// `deny`) so no module can opt back in with a local `allow`. Cargo cannot
// host this per-crate — `[lints] workspace = true` and crate-local lints are
// mutually exclusive ("cannot override `workspace.lints` in `lints`"), and
// spelling the ban in Cargo.toml would mean duplicating the whole workspace
// lint table into every crate that wants it.
#![forbid(unsafe_code)]
//! Time syscalls: itimers, `clock_gettime`/`clock_settime`/`clock_getres`,
//! `nanosleep`/`clock_nanosleep`, `adjtimex`/`clock_adjtime`, and the
//! boot-time SNTP fallback ([`boot`]) for platforms with no RTC.
//!
//! Extracted from `src/syscall/time.rs` 2026-08-25
//! (`docs/archive/MISSING_NTP_SYSCALLS.md`). Everything here used to be
//! bin-crate-private, but nothing in it actually needs the bin crate: the two
//! `crate::timer::{utc_time_us, uptime_us}` wrappers it used to call are thin
//! forwarders to `akuma_timer` (kept as bin-crate re-exports for the ~190
//! other call sites, see `src/timer.rs`), so this crate calls `akuma_timer`
//! directly instead; the one diagnostic print goes through the `log` facade
//! (`akuma-net`'s pattern — see `src/klog.rs`) instead of `crate::safe_print!`.
//! Everything else it touches (`akuma_exec::threading`, `akuma_exec::process`,
//! `akuma_exec::mmu::user_access`, `akuma_primitives::errno`) was already a
//! regular crate dependency.
//!
//! # Closing the missing-syscall gap
//!
//! `MISSING_NTP_SYSCALLS.md` found `clock_gettime`/`clock_getres` implemented
//! but no way to ever *set* the clock: `clock_settime` (112), `adjtimex`
//! (171), `clock_adjtime` (266) were all unimplemented, so nothing — not
//! `date -s`, not `rdate`, not `ntpd` — could correct a wrong clock. Those
//! three are implemented here now. `adjtimex`/`clock_adjtime` apply
//! `ADJ_OFFSET`/`ADJ_SETOFFSET` as an immediate step rather than a gradual PLL
//! slew (documented at the call site) — good enough for `rdate`/`ntpd -q`
//! (`sntp`-style one-shot correction), not a full `ntpd` daemon.

#![no_std]

pub mod boot;
pub mod sntp;

use akuma_exec::mmu::user_access::{copy_to_user, read_user_into, write_user_val};
use akuma_exec::threading::MAX_THREADS;
use akuma_primitives::errno::negated::{EFAULT, EINVAL};
// The four `Local*` ABI structs this module used to declare — `LocalTimespec`,
// `LocalTimeval`, `LocalItimerval`, `LocalTimex` — moved to
// `akuma-syscalls-linux` on 2026-08-27. The `Local` in their names was the
// problem, not a description: this crate *is* the timespec syscalls, and it
// spelled `struct timespec` a second time only because the bin crate's
// definition was unreachable from a library crate.
//
// The two time structs are signed there (Linux's `time_t`/`long` both are);
// they were unsigned here. That difference is real — the sleep and settime
// paths below do unsigned saturating arithmetic on the raw bits — so those
// sites keep doing exactly that, now through the explicit `bits()`/`from_bits()`
// reinterpretation rather than through a struct definition that hid it.
use akuma_syscalls_linux::{Itimerval, Timespec, Timeval, Timex};

// ---------------------------------------------------------------------------
// ITIMER_REAL / alarm() support
//
// musl on aarch64 implements alarm() via setitimer(ITIMER_REAL, ...). The
// per-thread-slot deadline and periodic interval are stored in
// `akuma_exec::threading` (via `get_itimer`/`set_itimer`), not here, so that
// slot recycling (`scrub_thread_slot`) resets them like every other per-slot
// register — see docs/archive/GIT_CLONE_STALE_ITIMER_SIGALRM.md for the bug
// that motivated moving this out of a syscall-module-local static.
// ---------------------------------------------------------------------------

/// Check and fire expired ITIMER_REAL timers. Called from the timer tick
/// (`akuma_exec::alarms::on_timer_interrupt`). Sends SIGALRM (14) to each thread
/// whose deadline has passed.
///
/// Runs in timer-IRQ context on whichever core took the interrupt — "current
/// process" there is whatever happened to be interrupted, not the itimer's
/// owner, so this must NOT apply SIGALRM's default (fatal) action inline the
/// way `sys_tkill` does for a same-thread caller. It follows `sys_kill`'s
/// cross-context pattern instead: `interrupt_thread` sets the target's
/// `ProcessChannel::interrupted` flag, which `is_current_interrupted()` (the
/// unconditional first half of `should_interrupt_blocking_syscall`) checks
/// regardless of signal disposition — unlike the mask-and-handler-gated second
/// half, `current_thread_has_pending_interrupt`, which by design only reports
/// signals with a registered handler (see its doc comment) and would never
/// break a `pause()`/`ppoll(NULL, 0, NULL)` out of an infinite block for a
/// handler-less SIGALRM. The actual default action then applies safely at the
/// target thread's own next syscall-return dispatch, where "current process"
/// is correct.
///
/// `interrupt_thread` alone is not enough: it sets the flag via `get_channel`,
/// keyed by `PROCESS_CHANNELS[tid]` — populated once at the original spawn
/// point and never re-registered on each subsequent fork/exec. What the
/// target thread will actually read back is `current_channel()`, which tries
/// `Process::channel` FIRST (inherited by value through fork, so it follows
/// the process down as many fork/exec generations as it likes) and only
/// falls back to `get_channel`. A process several generations removed from
/// the original registration point — e.g. anything an SSH session execs —
/// has a populated `Process::channel` but no `PROCESS_CHANNELS[tid]` entry,
/// so `interrupt_thread` alone silently no-ops for it. Set both.
///
/// Gated by [`wants_force_interrupt`]: the Ctrl-C-style flag this sets is
/// unconditional and ignores `SA_RESTART`, so applying it for every itimer
/// tick regardless of disposition breaks the *other* legitimate use of a
/// periodic `ITIMER_REAL` — a heartbeat/low-speed-limit handler installed
/// with `SA_RESTART` that expects its own blocking syscalls to keep running
/// after each tick — docs/archive/GIT_CLONE_STALE_ITIMER_SIGALRM.md.
pub fn check_itimers() {
    let now = akuma_timer::uptime_us();
    for tid in 0..MAX_THREADS {
        let (deadline, interval) = akuma_exec::threading::get_itimer(tid);
        if deadline > 0 && now >= deadline {
            // Fire SIGALRM
            if wants_force_interrupt(tid) {
                akuma_exec::process::interrupt_thread(tid);
                if let Some(pid) = akuma_exec::process::find_pid_by_thread(tid)
                    && let Some(proc) = akuma_exec::process::lookup_process_shared(pid)
                    && let Some(ch) = proc.channel.as_ref()
                {
                    ch.set_interrupted();
                }
            }
            akuma_exec::threading::pend_signal_for_thread(tid, 14);
            // Re-arm if periodic, else disarm
            let new_deadline = if interval > 0 { now.saturating_add(interval) } else { 0 };
            akuma_exec::threading::set_itimer(tid, new_deadline, interval);
        }
    }
}

/// Should an expired `ITIMER_REAL` force-interrupt `tid`'s current blocking
/// syscall via the Ctrl-C-style flag (bypassing `SA_RESTART`)? No process
/// context defaults to yes (the conservative, pre-existing behavior). The
/// actual decision, keyed on SIGALRM's disposition, is
/// [`SignalAction::wants_itimer_force_interrupt`] — host-tested there since
/// this module isn't (kernel-binary-only, no `cargo test` target).
fn wants_force_interrupt(tid: usize) -> bool {
    let Some(pid) = akuma_exec::process::find_pid_by_thread(tid) else { return true };
    let Some(proc) = akuma_exec::process::lookup_process_shared(pid) else { return true };
    let action = { let actions = proc.signal_actions.actions.lock(); actions[13] }; // SIGALRM(14) - 1
    action.wants_itimer_force_interrupt()
}

/// setitimer(ITIMER_REAL) — arms/disarms the real-time interval timer that
/// delivers SIGALRM on expiration. musl's alarm() is a thin wrapper around this.
#[must_use] 
pub fn sys_setitimer(which: u32, new_ptr: u64, old_ptr: u64) -> u64 {
    const ITIMER_REAL: u32 = 0;
    if which != ITIMER_REAL {
        // ITIMER_VIRTUAL (1) and ITIMER_PROF (2) are not implemented.
        return akuma_primitives::errno::negated::ENOSYS;
    }

    let tid = akuma_exec::threading::current_thread_id();
    if tid >= MAX_THREADS {
        return EINVAL;
    }

    // Write old timer state if requested
    if old_ptr != 0 {
        let (old_deadline, old_interval) = akuma_exec::threading::get_itimer(tid);
        let now = akuma_timer::uptime_us();
        let remaining = old_deadline.saturating_sub(now);
        let old = Itimerval {
            it_interval: Timeval::from_bits(old_interval / 1_000_000, old_interval % 1_000_000),
            it_value: Timeval::from_bits(remaining / 1_000_000, remaining % 1_000_000),
        };
        let _ = write_user_val(old_ptr, &old);
    }

    // Read and apply new timer state if requested
    if new_ptr != 0 {
        let mut new_val = Itimerval::default();
        if read_user_into(&mut new_val, new_ptr).is_err() {
            return EFAULT;
        }

        // Unsigned arithmetic over the raw bits, which is what this path did
        // when the struct itself was `u64` — see the `bits()` note at the top.
        let (int_sec, int_usec) = new_val.it_interval.bits();
        let (val_sec, val_usec) = new_val.it_value.bits();
        let interval_us = int_sec.saturating_mul(1_000_000) + int_usec;
        let value_us = val_sec.saturating_mul(1_000_000) + val_usec;

        let now = akuma_timer::uptime_us();
        if value_us > 0 {
            akuma_exec::threading::set_itimer(tid, now.saturating_add(value_us), interval_us);
        } else {
            // Disarm
            akuma_exec::threading::set_itimer(tid, 0, 0);
        }
    }

    0
}

/// `CLOCK_REALTIME`, the only clock id `clock_settime`/`clock_adjtime` accept
/// — mirrors `sys_clock_gettime`'s clock_id==0 special case.
const CLOCK_REALTIME: u32 = 0;

#[must_use] 
pub fn sys_clock_gettime(clock_id_arg: u64, tp_ptr: u64) -> u64 {
    // Linux clock_id is a small integer or a compact CPU-clock encoding
    // (~(pid << 3) | CPUCLOCK_*).  Pointer-sized values in x0 (e.g. Go heap) are
    // EINVAL on Linux.  Do not copy out a timespec to such an x0: serial crash5.log
    // showed `clock_gettime_recover`-style writes immediately before WILD-DA at
    // FAR=0x10 with memclr ELR (see docs/GO_FORKTEST_DEBUG.md).
    const MAX_REASONABLE_CLOCK_ID: u64 = 0x1000_0000;
    if clock_id_arg > MAX_REASONABLE_CLOCK_ID {
        // Diagnostic: read instruction bytes at ELR and ELR-4 to identify the caller
        if let Some(elr) = akuma_exec::threading::current_trap_frame_elr() {
            let mut instr_before = [0u8; 4];
            let mut instr_at = [0u8; 4];
            let ok_before = elr >= 4 && read_user_into(&mut instr_before, elr - 4).is_ok();
            let ok_at = read_user_into(&mut instr_at, elr).is_ok();
            let before_word = u32::from_le_bytes(instr_before);
            let at_word = u32::from_le_bytes(instr_at);
            log::warn!(
                "[clock-diag] large clock_id={clock_id_arg:#x} tp={tp_ptr:#x} ELR={elr:#x}\n  instr[ELR-4]={before_word:#010x}({}) instr[ELR]={at_word:#010x}({})",
                if ok_before { "ok" } else { "err" },
                if ok_at { "ok" } else { "err" },
            );
        }
        return EINVAL;
    }
    let clock_id = clock_id_arg as u32;

    let (sec, nsec) = if clock_id == 0 {
        let us = akuma_timer::utc_time_us(akuma_timer::uptime_us()).unwrap_or(0);
        ((us / 1_000_000) as u64, ((us % 1_000_000) * 1_000) as u64)
    } else {
        let us = akuma_timer::uptime_us();
        ((us / 1_000_000), ((us % 1_000_000) * 1_000))
    };

    let ts = Timespec::from_bits(sec, nsec);
    if write_user_val(tp_ptr, &ts).is_err() {
        return EFAULT;
    }
    0
}

/// `clock_settime(2)`, `CLOCK_REALTIME` only (Linux also errors on the other
/// clock ids — `CLOCK_MONOTONIC` etc. aren't settable there either).
///
/// The missing half of `sys_clock_gettime`: closes
/// `docs/archive/MISSING_NTP_SYSCALLS.md`'s "no way to set the clock at all"
/// gap. `date -s`, `rdate`, and `ntpd`'s initial step all bottom out here.
#[must_use] 
pub fn sys_clock_settime(clock_id: u32, tp_ptr: u64) -> u64 {
    if clock_id != CLOCK_REALTIME {
        return EINVAL;
    }
    let mut ts = Timespec::default();
    if read_user_into(&mut ts, tp_ptr).is_err() {
        return EFAULT;
    }
    let (sec, nsec) = ts.bits();
    let unix_epoch_us = sec.saturating_mul(1_000_000).saturating_add(nsec / 1_000);
    akuma_timer::set_utc_time_us(unix_epoch_us, akuma_timer::uptime_us());
    0
}

const ADJ_OFFSET: u32 = 0x0001;
const ADJ_STATUS: u32 = 0x0010;
const ADJ_SETOFFSET: u32 = 0x0100;
const ADJ_NANO: u32 = 0x2000;
/// `STA_UNSYNC` (`<linux/timex.h>`) — reported set until a real sync source
/// (this bootstrap, or a future full `ntpd`) has actually run. `adjtimex`
/// query mode is how `chronyd`/`ntpd -gq` decide whether to trust the clock at
/// all, so always answering "synced" here would be a lie a caller might act on.
const STA_UNSYNC: i32 = 0x0040;

/// `clock_adjtime(2)` / `adjtimex(2)` (the latter is
/// `clock_adjtime(CLOCK_REALTIME, buf)`).
///
/// Honors `ADJ_SETOFFSET` and
/// `ADJ_OFFSET` as an immediate step of the wall clock — there is no PLL here,
/// so a "slew" request lands all at once instead of gradually. That is enough
/// for `rdate`/`ntpd -q`/`sntp`-style one-shot correction (the case
/// `docs/archive/MISSING_NTP_SYSCALLS.md` needed), not a full `ntpd` daemon
/// doing continuous frequency discipline. Every other `modes` bit
/// (`ADJ_FREQUENCY`, `ADJ_TICK`, ...) is accepted and ignored: there is no
/// frequency/tick state to adjust, and rejecting them would just break a
/// caller that sets several bits at once for a step it also wants.
#[must_use] 
pub fn sys_clock_adjtime(clock_id: u32, buf_ptr: u64) -> u64 {
    if clock_id != CLOCK_REALTIME {
        return EINVAL;
    }
    let mut tx = Timex::default();
    if read_user_into(&mut tx, buf_ptr).is_err() {
        return EFAULT;
    }

    let nano = tx.modes & ADJ_NANO != 0;
    if tx.modes & ADJ_SETOFFSET != 0 {
        // `time` holds a delta to add to the current time, applied immediately.
        let delta_us = if nano {
            tx.time_sec.saturating_mul(1_000_000).saturating_add(tx.time_usec / 1000)
        } else {
            tx.time_sec.saturating_mul(1_000_000).saturating_add(tx.time_usec)
        };
        step_utc_by(delta_us);
    } else if tx.modes & ADJ_OFFSET != 0 {
        // `offset` is in usec unless ADJ_NANO, then nsec.
        let offset_us = if nano { tx.offset / 1000 } else { tx.offset };
        step_utc_by(offset_us);
    }

    // Report current state back. No leap-second/frequency tracking, so every
    // read-only field besides `time` stays at its zeroed default.
    let now_us = akuma_timer::utc_time_us(akuma_timer::uptime_us()).unwrap_or(0);
    tx.time_sec = (now_us / 1_000_000).cast_signed();
    tx.time_usec = if nano {
        ((now_us % 1_000_000) * 1000).cast_signed()
    } else {
        (now_us % 1_000_000).cast_signed()
    };
    tx.offset = 0;
    tx.status = if tx.modes & ADJ_STATUS != 0 { tx.status } else { STA_UNSYNC };
    let _ = write_user_val(buf_ptr, &tx);
    0 // TIME_OK — no leap-second state machine, so this is the only code ever returned.
}

/// `adjtimex(2)` — always `CLOCK_REALTIME`, see [`sys_clock_adjtime`].
#[must_use] 
pub fn sys_adjtimex(buf_ptr: u64) -> u64 {
    sys_clock_adjtime(CLOCK_REALTIME, buf_ptr)
}

/// Step the wall clock by `delta_us` (positive or negative), immediately.
/// No-ops if the clock has never been set — there is nothing to offset from,
/// and guessing an anchor would fabricate a wrong absolute time instead of
/// leaving it honestly unset.
fn step_utc_by(delta_us: i64) {
    let uptime = akuma_timer::uptime_us();
    if let Some(now_us) = akuma_timer::utc_time_us(uptime) {
        let new_us = now_us.cast_signed().saturating_add(delta_us).max(0).cast_unsigned();
        akuma_timer::set_utc_time_us(new_us, uptime);
    }
}

#[must_use] 
pub fn sys_clock_getres(clock_id: u32, res_ptr: usize) -> u64 {
    let _ = clock_id;
    if res_ptr != 0 {
        let ts = Timespec { tv_sec: 0, tv_nsec: 1 };
        let _ = write_user_val(res_ptr as u64, &ts);
    }
    0
}

#[must_use] 
pub fn sys_nanosleep(a0: u64, a1: u64) -> u64 {
    // Support two ABIs:
    // - Linux/musl: a0 = pointer to struct timespec {tv_sec, tv_nsec}
    // - libakuma:   a0 = seconds (raw), a1 = nanoseconds (raw)
    // Distinguish by checking if a0 looks like a user-space pointer (>= PAGE_SIZE).
    let mut ts = Timespec::default();
    let (sec, nsec) = if a0 >= 4096 && read_user_into(&mut ts, a0).is_ok() {
        ts.bits()
    } else {
        (a0, a1)
    };
    let total_us = sec.saturating_mul(1_000_000).saturating_add(nsec / 1_000);
    if total_us == 0 { return 0; }
    let deadline = akuma_timer::uptime_us().saturating_add(total_us);
    loop {
        if akuma_timer::uptime_us() >= deadline { return 0; }
        if akuma_exec::process::should_interrupt_blocking_syscall() {
            return akuma_primitives::errno::negated::EINTR;
        }
        akuma_exec::threading::schedule_blocking(deadline);
    }
}

/// `clock_nanosleep(2)`. Missing entirely until 2026-08-07
/// (`docs/archive/THREAD_SLEEP_MISSING_CLOCK_NANOSLEEP.md`): every
/// unimplemented syscall falls through to the generic `ENOSYS` handler.
///
/// That matters here because `std::thread::sleep` on any `target_os =
/// "linux"` build calls this syscall specifically (not plain `nanosleep`) —
/// so every `thread::sleep()` call, on any thread, panicked instead of
/// sleeping.
///
/// Modeled directly on [`sys_nanosleep`], plus `clockid`/`TIMER_ABSTIME`
/// handling to cover the full POSIX contract, not just std's relative-sleep case.
///
/// Returns the ordinary Linux syscall convention (0 / `-errno`) — the POSIX
/// library function's unusual "return the positive error number" contract is a
/// userspace libc wrapper detail (musl negates the raw syscall's `-errno` back to
/// positive), not something this syscall itself needs to implement.
#[must_use] 
pub fn sys_clock_nanosleep(clock_id: u32, flags: i32, request_ptr: u64, remain_ptr: u64) -> u64 {
    const TIMER_ABSTIME: i32 = 1;
    let _ = remain_ptr; // Linux only fills this on an interrupted *relative* sleep; sys_nanosleep doesn't either.

    let mut ts = Timespec::default();
    if read_user_into(&mut ts, request_ptr).is_err() {
        return EFAULT;
    }
    let (sec, nsec) = ts.bits();
    let req_us = (sec.saturating_mul(1_000_000)).saturating_add(nsec / 1_000);

    let deadline = if flags & TIMER_ABSTIME != 0 {
        // Absolute deadline in `clock_id`'s own time base. Mirrors `sys_clock_gettime`'s
        // clock_id==0 (CLOCK_REALTIME) vs everything-else (uptime-based) split, and
        // `sys_futex`'s FUTEX_CLOCK_REALTIME absolute-deadline conversion in
        // `src/syscall/sync.rs` — same wall-clock-to-uptime math, different caller.
        if clock_id == 0 {
            match akuma_timer::utc_time_us(akuma_timer::uptime_us()) {
                Some(utc_now) if req_us > utc_now => akuma_timer::uptime_us() + (req_us - utc_now),
                Some(_) => akuma_timer::uptime_us(), // already past -> immediate return
                None => req_us,
            }
        } else {
            req_us
        }
    } else {
        // Relative sleep, same as plain nanosleep.
        if req_us == 0 { return 0; }
        akuma_timer::uptime_us().saturating_add(req_us)
    };

    loop {
        if akuma_timer::uptime_us() >= deadline { return 0; }
        if akuma_exec::process::should_interrupt_blocking_syscall() {
            return akuma_primitives::errno::negated::EINTR;
        }
        akuma_exec::threading::schedule_blocking(deadline);
    }
}

#[must_use] 
pub fn sys_times(buf_ptr: usize) -> u64 {
    if buf_ptr != 0 {
        const TMS_SIZE: usize = 32;
        let zero = [0u8; TMS_SIZE];
        if copy_to_user(buf_ptr as u64, &zero).is_err() { return EFAULT; }
    }
    let uptime_us = akuma_timer::uptime_us();
    uptime_us / 10_000
}

#[must_use] 
pub fn sys_getrusage(who: i32, usage_ptr: usize) -> u64 {
    const RUSAGE_SIZE: usize = 144;
    let zero = [0u8; RUSAGE_SIZE];
    if copy_to_user(usage_ptr as u64, &zero).is_err() { return EFAULT; }
    let _ = who;
    0
}

#[must_use] 
pub fn sys_time() -> u64 { akuma_timer::utc_time_us(akuma_timer::uptime_us()).unwrap_or(0) }

#[must_use] 
pub fn sys_uptime() -> u64 { akuma_timer::uptime_us() }
