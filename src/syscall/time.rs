use super::*;
use akuma_exec::mmu::user_access::{copy_from_user_safe, copy_to_user_safe};
use akuma_exec::threading::MAX_THREADS;
use core::sync::atomic::{AtomicU64, Ordering};

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct LocalTimespec {
    tv_sec: u64,
    tv_nsec: u64,
}

// ---------------------------------------------------------------------------
// ITIMER_REAL / alarm() support
//
// musl on aarch64 implements alarm() via setitimer(ITIMER_REAL, ...).  This
// module stores the per-thread-slot deadline and periodic interval, checked
// every timer tick from `kernel_timer::on_timer_interrupt`.
// ---------------------------------------------------------------------------

/// Per-thread ITIMER_REAL deadline in uptime microseconds (0 = disarmed).
/// Only the thread-group leader's slot is meaningful (alarm is per-process).
static ITIMER_DEADLINE: [AtomicU64; MAX_THREADS] = {
    #[allow(clippy::declare_interior_mutable_const)]
    const INIT: AtomicU64 = AtomicU64::new(0);
    [INIT; MAX_THREADS]
};

/// Per-thread ITIMER_REAL periodic interval in microseconds (0 = one-shot).
static ITIMER_INTERVAL: [AtomicU64; MAX_THREADS] = {
    #[allow(clippy::declare_interior_mutable_const)]
    const INIT: AtomicU64 = AtomicU64::new(0);
    [INIT; MAX_THREADS]
};

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct LocalTimeval {
    tv_sec: u64,
    tv_usec: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct LocalItimerval {
    it_interval: LocalTimeval,
    it_value: LocalTimeval,
}

/// Check and fire expired ITIMER_REAL timers. Called from the timer tick
/// (`kernel_timer::on_timer_interrupt`). Sends SIGALRM (14) to each thread
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
pub fn check_itimers() {
    let now = crate::timer::uptime_us();
    for tid in 0..MAX_THREADS {
        let deadline = ITIMER_DEADLINE[tid].load(Ordering::Relaxed);
        if deadline > 0 && now >= deadline {
            // Fire SIGALRM
            akuma_exec::process::interrupt_thread(tid);
            if let Some(pid) = akuma_exec::process::find_pid_by_thread(tid)
                && let Some(proc) = akuma_exec::process::lookup_process_shared(pid)
                && let Some(ch) = proc.channel.as_ref()
            {
                ch.set_interrupted();
            }
            akuma_exec::threading::pend_signal_for_thread(tid, 14);
            // Re-arm if periodic, else disarm
            let interval = ITIMER_INTERVAL[tid].load(Ordering::Relaxed);
            if interval > 0 {
                ITIMER_DEADLINE[tid].store(
                    now.saturating_add(interval),
                    Ordering::Relaxed,
                );
            } else {
                ITIMER_DEADLINE[tid].store(0, Ordering::Relaxed);
            }
        }
    }
}

/// setitimer(ITIMER_REAL) — arms/disarms the real-time interval timer that
/// delivers SIGALRM on expiration. musl's alarm() is a thin wrapper around this.
pub(super) fn sys_setitimer(which: u32, new_ptr: u64, old_ptr: u64) -> u64 {
    const ITIMER_REAL: u32 = 0;
    if which != ITIMER_REAL {
        // ITIMER_VIRTUAL (1) and ITIMER_PROF (2) are not implemented.
        return crate::syscall::ENOSYS;
    }

    let tid = akuma_exec::threading::current_thread_id();
    if tid >= MAX_THREADS {
        return crate::syscall::EINVAL;
    }

    // Write old timer state if requested
    if old_ptr != 0 {
        let old_deadline = ITIMER_DEADLINE[tid].load(Ordering::Relaxed);
        let old_interval = ITIMER_INTERVAL[tid].load(Ordering::Relaxed);
        let now = crate::timer::uptime_us();
        let remaining = old_deadline.saturating_sub(now);
        let old = LocalItimerval {
            it_interval: LocalTimeval {
                tv_sec: old_interval / 1_000_000,
                tv_usec: old_interval % 1_000_000,
            },
            it_value: LocalTimeval {
                tv_sec: remaining / 1_000_000,
                tv_usec: remaining % 1_000_000,
            },
        };
        let _ = unsafe {
            copy_to_user_safe(
                old_ptr as *mut u8,
                (&raw const old).cast::<u8>(),
                core::mem::size_of::<LocalItimerval>(),
            )
        };
    }

    // Read and apply new timer state if requested
    if new_ptr != 0 {
        let mut new_val = LocalItimerval::default();
        if unsafe {
            copy_from_user_safe(
                (&raw mut new_val).cast::<u8>(),
                new_ptr as *const u8,
                core::mem::size_of::<LocalItimerval>(),
            )
        }
        .is_err()
        {
            return crate::syscall::EFAULT;
        }

        let interval_us =
            new_val.it_interval.tv_sec.saturating_mul(1_000_000) + new_val.it_interval.tv_usec;
        let value_us =
            new_val.it_value.tv_sec.saturating_mul(1_000_000) + new_val.it_value.tv_usec;

        let now = crate::timer::uptime_us();
        if value_us > 0 {
            ITIMER_DEADLINE[tid].store(now.saturating_add(value_us), Ordering::Relaxed);
            ITIMER_INTERVAL[tid].store(interval_us, Ordering::Relaxed);
        } else {
            // Disarm
            ITIMER_DEADLINE[tid].store(0, Ordering::Relaxed);
            ITIMER_INTERVAL[tid].store(0, Ordering::Relaxed);
        }
    }

    0
}

pub(super) fn sys_clock_gettime(clock_id_arg: u64, tp_ptr: u64) -> u64 {
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
            let ok_before = elr >= 4 && unsafe {
                akuma_exec::mmu::user_access::copy_from_user_safe(
                    instr_before.as_mut_ptr(), (elr - 4) as *const u8, 4).is_ok()
            };
            let ok_at = unsafe {
                akuma_exec::mmu::user_access::copy_from_user_safe(
                    instr_at.as_mut_ptr(), elr as *const u8, 4).is_ok()
            };
            let before_word = u32::from_le_bytes(instr_before);
            let at_word = u32::from_le_bytes(instr_at);
            crate::safe_print!(192,
                "[clock-diag] large clock_id={:#x} tp={:#x} ELR={:#x}\n  instr[ELR-4]={:#010x}({}) instr[ELR]={:#010x}({})\n",
                clock_id_arg, tp_ptr, elr,
                before_word, if ok_before { "ok" } else { "err" },
                at_word, if ok_at { "ok" } else { "err" });
        }
        return EINVAL;
    }
    let clock_id = clock_id_arg as u32;

    if !validate_user_ptr(tp_ptr, 16) { return EFAULT; }

    let (sec, nsec) = if clock_id == 0 {
        let us = crate::timer::utc_time_us().unwrap_or(0);
        ((us / 1_000_000) as u64, ((us % 1_000_000) * 1_000) as u64)
    } else {
        let us = crate::timer::uptime_us();
        ((us / 1_000_000), ((us % 1_000_000) * 1_000))
    };

    let ts = LocalTimespec { tv_sec: sec, tv_nsec: nsec };
    if unsafe { copy_to_user_safe(tp_ptr as *mut u8, (&raw const ts).cast::<u8>(), 16).is_err() } {
        return EFAULT;
    }
    0
}

pub(super) fn sys_clock_getres(clock_id: u32, res_ptr: usize) -> u64 {
    let _ = clock_id;
    if res_ptr != 0 && validate_user_ptr(res_ptr as u64, 16) {
        let ts = LocalTimespec { tv_sec: 0, tv_nsec: 1 };
        let _ = unsafe { copy_to_user_safe(res_ptr as *mut u8, (&raw const ts).cast::<u8>(), 16) };
    }
    0
}

pub(super) fn sys_nanosleep(a0: u64, a1: u64) -> u64 {
    // Support two ABIs:
    // - Linux/musl: a0 = pointer to struct timespec {tv_sec, tv_nsec}
    // - libakuma:   a0 = seconds (raw), a1 = nanoseconds (raw)
    // Distinguish by checking if a0 looks like a user-space pointer (>= PAGE_SIZE).
    let (sec, nsec) = if a0 >= 4096 && validate_user_ptr(a0, 16) {
        let mut ts = LocalTimespec::default();
        if unsafe { copy_from_user_safe((&raw mut ts).cast::<u8>(), a0 as *const u8, 16).is_ok() } {
            (ts.tv_sec, ts.tv_nsec)
        } else {
            (a0, a1)
        }
    } else {
        (a0, a1)
    };
    let total_us = sec.saturating_mul(1_000_000).saturating_add(nsec / 1_000);
    if total_us == 0 { return 0; }
    let deadline = crate::timer::uptime_us().saturating_add(total_us);
    loop {
        if crate::timer::uptime_us() >= deadline { return 0; }
        if akuma_exec::process::should_interrupt_blocking_syscall() { return EINTR; }
        akuma_exec::threading::schedule_blocking(deadline);
    }
}

/// `clock_nanosleep(2)`. Missing entirely until 2026-08-07
/// (`docs/archive/THREAD_SLEEP_MISSING_CLOCK_NANOSLEEP.md`): every unimplemented
/// syscall falls through to the generic `ENOSYS` handler, and `std::thread::sleep`
/// on any `target_os = "linux"` build calls this syscall specifically (not plain
/// `nanosleep`) — so every `thread::sleep()` call, on any thread, panicked instead
/// of sleeping. Modeled directly on [`sys_nanosleep`], plus `clockid`/`TIMER_ABSTIME`
/// handling to cover the full POSIX contract, not just std's relative-sleep case.
///
/// Returns the ordinary Linux syscall convention (0 / `-errno`) — the POSIX
/// library function's unusual "return the positive error number" contract is a
/// userspace libc wrapper detail (musl negates the raw syscall's `-errno` back to
/// positive), not something this syscall itself needs to implement.
pub(super) fn sys_clock_nanosleep(clock_id: u32, flags: i32, request_ptr: u64, remain_ptr: u64) -> u64 {
    const TIMER_ABSTIME: i32 = 1;
    let _ = remain_ptr; // Linux only fills this on an interrupted *relative* sleep; sys_nanosleep doesn't either.

    if !validate_user_ptr(request_ptr, 16) { return EFAULT; }
    let mut ts = LocalTimespec::default();
    if unsafe { copy_from_user_safe((&raw mut ts).cast::<u8>(), request_ptr as *const u8, 16).is_err() } {
        return EFAULT;
    }
    let req_us = (ts.tv_sec.saturating_mul(1_000_000)).saturating_add(ts.tv_nsec / 1_000);

    let deadline = if flags & TIMER_ABSTIME != 0 {
        // Absolute deadline in `clock_id`'s own time base. Mirrors `sys_clock_gettime`'s
        // clock_id==0 (CLOCK_REALTIME) vs everything-else (uptime-based) split, and
        // `sys_futex`'s FUTEX_CLOCK_REALTIME absolute-deadline conversion in
        // `src/syscall/sync.rs` — same wall-clock-to-uptime math, different caller.
        if clock_id == 0 {
            match crate::timer::utc_time_us() {
                Some(utc_now) if req_us > utc_now => crate::timer::uptime_us() + (req_us - utc_now),
                Some(_) => crate::timer::uptime_us(), // already past -> immediate return
                None => req_us,
            }
        } else {
            req_us
        }
    } else {
        // Relative sleep, same as plain nanosleep.
        if req_us == 0 { return 0; }
        crate::timer::uptime_us().saturating_add(req_us)
    };

    loop {
        if crate::timer::uptime_us() >= deadline { return 0; }
        if akuma_exec::process::should_interrupt_blocking_syscall() { return EINTR; }
        akuma_exec::threading::schedule_blocking(deadline);
    }
}

pub(super) fn sys_times(buf_ptr: usize) -> u64 {
    if buf_ptr != 0 {
        const TMS_SIZE: usize = 32;
        if !validate_user_ptr(buf_ptr as u64, TMS_SIZE) { return EFAULT; }
        let zero = [0u8; TMS_SIZE];
        let _ = unsafe { copy_to_user_safe(buf_ptr as *mut u8, zero.as_ptr(), TMS_SIZE) };
    }
    let uptime_us = crate::timer::uptime_us();
    uptime_us / 10_000 
}

pub(super) fn sys_getrusage(who: i32, usage_ptr: usize) -> u64 {
    const RUSAGE_SIZE: usize = 144;
    if !validate_user_ptr(usage_ptr as u64, RUSAGE_SIZE) { return EFAULT; }
    let zero = [0u8; RUSAGE_SIZE];
    let _ = unsafe { copy_to_user_safe(usage_ptr as *mut u8, zero.as_ptr(), RUSAGE_SIZE) };
    let _ = who;
    0
}

pub(super) fn sys_time() -> u64 { crate::timer::utc_time_us().unwrap_or(0) }

pub(super) fn sys_uptime() -> u64 { crate::timer::uptime_us() }
