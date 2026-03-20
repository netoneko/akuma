use super::*;
use akuma_exec::mmu::user_access::{copy_from_user_safe, copy_to_user_safe};

const SIG_DFL: usize = 0;
const SIG_IGN: usize = 1;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct KernelSigaction {
    sa_handler: usize,
    sa_flags: u64,
    sa_restorer: usize,
    sa_mask: u64,
}

pub(super) fn sys_rt_sigaction(sig: u32, act_ptr: usize, oldact_ptr: usize, sigsetsize: usize) -> u64 {
    if sig == 0 || sig as usize > akuma_exec::process::MAX_SIGNALS { return EINVAL; }
    if sig == 9 || sig == 19 { return EINVAL; }
    let sigset_ok = sigsetsize == 8;

    let proc = match akuma_exec::process::current_process() {
        Some(p) => p,
        None => return ENOSYS,
    };

    let idx = (sig - 1) as usize;

    if oldact_ptr != 0 && validate_user_ptr(oldact_ptr as u64, 32) {
        let actions = proc.signal_actions.actions.lock();
        let old = &actions[idx];
        let handler_val = match old.handler {
            akuma_exec::process::SignalHandler::Default => SIG_DFL,
            akuma_exec::process::SignalHandler::Ignore => SIG_IGN,
            akuma_exec::process::SignalHandler::UserFn(addr) => addr,
        };
        let out = KernelSigaction {
            sa_handler: handler_val,
            sa_flags: old.flags,
            sa_restorer: old.restorer,
            sa_mask: if sigset_ok { old.mask } else { 0 },
        };
        if unsafe { copy_to_user_safe(oldact_ptr as *mut u8, &out as *const KernelSigaction as *const u8, 32).is_err() } {
            return EFAULT;
        }
    }

    if act_ptr != 0 && validate_user_ptr(act_ptr as u64, 32) {
        let mut sa = KernelSigaction::default();
        if unsafe { copy_from_user_safe(&mut sa as *mut KernelSigaction as *mut u8, act_ptr as *const u8, 32).is_err() } {
            return EFAULT;
        }
        let handler = match sa.sa_handler {
            SIG_DFL => akuma_exec::process::SignalHandler::Default,
            SIG_IGN => akuma_exec::process::SignalHandler::Ignore,
            addr => akuma_exec::process::SignalHandler::UserFn(addr),
        };
        let mut actions = proc.signal_actions.actions.lock();
        actions[idx] = akuma_exec::process::SignalAction {
            handler,
            flags: sa.sa_flags,
            mask: if sigset_ok { sa.sa_mask } else { 0 },
            restorer: sa.sa_restorer,
        };
    }

    0
}

fn signal_is_fatal_default(sig: u32) -> bool {
    matches!(sig, 1 | 2 | 3 | 4 | 5 | 6 | 7 | 8 | 9 | 11 | 13 | 14 | 15 | 24 | 25 | 26 | 27 | 31)
}

/// rt_sigprocmask - change the signal mask
/// how: SIG_BLOCK (0), SIG_UNBLOCK (1), SIG_SETMASK (2)
/// set: pointer to new signal mask
/// oldset: pointer to store old signal mask
/// sigsetsize: size of signal set (must be 8)
pub(super) fn sys_rt_sigprocmask(how: u32, set_ptr: u64, oldset_ptr: u64, sigsetsize: usize) -> u64 {
    const SIG_BLOCK: u32 = 0;
    const SIG_UNBLOCK: u32 = 1;
    const SIG_SETMASK: u32 = 2;

    if sigsetsize != 8 {
        return EINVAL;
    }

    let proc = match akuma_exec::process::current_process() {
        Some(p) => p,
        None => return ENOSYS,
    };

    // Return old mask if requested
    if oldset_ptr != 0 {
        if !validate_user_ptr(oldset_ptr, 8) {
            return EFAULT;
        }
        if unsafe { copy_to_user_safe(oldset_ptr as *mut u8, &proc.signal_mask as *const u64 as *const u8, 8).is_err() } {
            return EFAULT;
        }
    }

    // Apply new mask if provided
    if set_ptr != 0 {
        if !validate_user_ptr(set_ptr, 8) {
            return EFAULT;
        }
        let mut new_mask: u64 = 0;
        if unsafe { copy_from_user_safe(&mut new_mask as *mut u64 as *mut u8, set_ptr as *const u8, 8).is_err() } {
            return EFAULT;
        }

        // SIGKILL (9) and SIGSTOP (19) cannot be blocked
        let allowed_mask = new_mask & !((1u64 << 8) | (1u64 << 18));

        match how {
            SIG_BLOCK => {
                proc.signal_mask |= allowed_mask;
            }
            SIG_UNBLOCK => {
                proc.signal_mask &= !new_mask;
            }
            SIG_SETMASK => {
                proc.signal_mask = allowed_mask;
            }
            _ => return EINVAL,
        }
    }

    0
}

/// sigaltstack - set/get alternate signal stack
/// ss: pointer to new stack_t structure
/// old_ss: pointer to store old stack_t structure
pub(super) fn sys_sigaltstack(ss_ptr: u64, old_ss_ptr: u64) -> u64 {
    // stack_t structure:
    // void *ss_sp;     // Base address of stack (8 bytes)
    // int ss_flags;    // Flags (4 bytes)
    // size_t ss_size;  // Size of stack (8 bytes)
    const STACK_T_SIZE: usize = 24;
    const SS_DISABLE: i32 = 2;

    let slot = akuma_exec::threading::current_thread_id();

    // Return old stack if requested
    if old_ss_ptr != 0 {
        if !validate_user_ptr(old_ss_ptr, STACK_T_SIZE) {
            return EFAULT;
        }
        #[repr(C)]
        struct StackT { sp: u64, flags: i32, _pad: i32, size: u64 }
        let (sp, size, flags) = akuma_exec::threading::get_sigaltstack(slot);
        let out = StackT { sp, flags, _pad: 0, size };
        if unsafe { copy_to_user_safe(old_ss_ptr as *mut u8, &out as *const StackT as *const u8, STACK_T_SIZE).is_err() } {
            return EFAULT;
        }
    }

    // Set new stack if provided
    if ss_ptr != 0 {
        if !validate_user_ptr(ss_ptr, STACK_T_SIZE) {
            return EFAULT;
        }
        #[repr(C)]
        struct StackT { sp: u64, flags: i32, _pad: i32, size: u64 }
        let mut ss = StackT { sp: 0, flags: 0, _pad: 0, size: 0 };
        if unsafe { copy_from_user_safe(&mut ss as *mut StackT as *mut u8, ss_ptr as *const u8, STACK_T_SIZE).is_err() } {
            return EFAULT;
        }

        // SS_DISABLE disables the alternate stack
        if ss.flags & SS_DISABLE != 0 {
            akuma_exec::threading::set_sigaltstack(slot, 0, 0, SS_DISABLE);
        } else {
            // Minimum stack size check (MINSIGSTKSZ = 2048 on most systems)
            if ss.size < 2048 {
                return ENOMEM;
            }
            akuma_exec::threading::set_sigaltstack(slot, ss.sp, ss.size, ss.flags);
        }
    }

    0
}

/// rt_sigtimedwait - synchronously wait for queued signals
/// set: signals to wait for
/// info: pointer to siginfo_t structure (ignored for now)
/// timeout: pointer to timespec structure
/// sigsetsize: size of signal set (must be 8)
pub fn sys_rt_sigtimedwait(set_ptr: u64, info_ptr: u64, timeout_ptr: u64, sigsetsize: usize) -> u64 {
    if sigsetsize != 8 {
        return EINVAL;
    }

    let _proc = match akuma_exec::process::current_process() {
        Some(p) => p,
        None => return ENOSYS,
    };

    if !validate_user_ptr(set_ptr, 8) {
        return EFAULT;
    }

    let mut wait_mask: u64 = 0;
    if unsafe { copy_from_user_safe(&mut wait_mask as *mut u64 as *mut u8, set_ptr as *const u8, 8).is_err() } {
        return EFAULT;
    }

    let mut timeout_us = u64::MAX;
    if timeout_ptr != 0 {
        if !validate_user_ptr(timeout_ptr, 16) {
            return EFAULT;
        }
        #[repr(C)]
        struct Timespec { tv_sec: i64, tv_nsec: i64 }
        let mut ts = Timespec { tv_sec: 0, tv_nsec: 0 };
        if unsafe { copy_from_user_safe(&mut ts as *mut Timespec as *mut u8, timeout_ptr as *const u8, 16).is_err() } {
            return EFAULT;
        }
        timeout_us = (ts.tv_sec as u64) * 1_000_000 + (ts.tv_nsec as u64) / 1000;
    }

    let start_time = crate::timer::uptime_us();

    loop {
        // Check for pending signals.  We pass ~wait_mask to take_pending_signal
        // because take_pending_signal takes a mask of BLOCKED signals, but
        // rt_sigtimedwait waits for the signals in wait_mask (so anything NOT
        // in wait_mask is effectively "blocked" for this wait).
        if let Some(sig) = akuma_exec::threading::take_pending_signal(!wait_mask) {
            // Fill siginfo if requested (minimal implementation)
            if info_ptr != 0 && validate_user_ptr(info_ptr, 128) {
                #[repr(C)]
                struct Siginfo { si_signo: i32, si_errno: i32, si_code: i32, _pad: [i32; 29] }
                let info = Siginfo { si_signo: sig as i32, si_errno: 0, si_code: 0, _pad: [0; 29] };
                let _ = unsafe { copy_to_user_safe(info_ptr as *mut u8, &info as *const Siginfo as *const u8, 128) };
            }
            return sig as u64;
        }

        let now = crate::timer::uptime_us();
        let elapsed = now.saturating_sub(start_time);
        if timeout_ptr != 0 && elapsed >= timeout_us {
            return EAGAIN; // Timeout
        }

        if akuma_exec::process::is_current_interrupted() {
            return EINTR;
        }

        // Sleep until next tick or timeout
        let deadline = if timeout_ptr != 0 {
            start_time + timeout_us
        } else {
            u64::MAX
        };
        
        // Cap sleep to 10ms to keep system responsive
        let sleep_deadline = deadline.min(now + 10_000);
        akuma_exec::threading::schedule_blocking(sleep_deadline);
    }
}

pub(super) fn sys_tkill(tid: u32, sig: u32) -> u64 {
    if sig == 0 { return 0; }
    if sig as usize > akuma_exec::process::MAX_SIGNALS { return EINVAL; }

    crate::safe_print!(96, "[signal] tkill(tid={}, sig={})\n", tid, sig);

    // Get the target process (the one that owns the thread tid)
    let (target_handler, target_mask) = if let Some(pid) = akuma_exec::process::find_pid_by_thread(tid as usize) {
        if let Some(proc) = akuma_exec::process::lookup_process(pid) {
            let idx = (sig - 1) as usize;
            let actions = proc.signal_actions.actions.lock();
            (actions[idx].handler, proc.signal_mask)
        } else {
            (akuma_exec::process::SignalHandler::Default, 0)
        }
    } else {
        (akuma_exec::process::SignalHandler::Default, 0)
    };

    // SIGKILL (9) is always fatal and cannot be blocked/handled
    if sig == 9 {
        super::proc::sys_exit_group(-(sig as i32));
        return 0;
    }

    match target_handler {
        akuma_exec::process::SignalHandler::Ignore => 0,
        akuma_exec::process::SignalHandler::Default => {
            let bit = 1u64 << (sig - 1);
            let blocked = (target_mask & bit) != 0;
            
            if signal_is_fatal_default(sig) && !blocked {
                super::proc::sys_exit_group(-(sig as i32));
            } else if signal_is_fatal_default(sig) && blocked {
                // If fatal but blocked, we still pend it so it fires when unmasked
                akuma_exec::threading::pend_signal_for_thread(tid as usize, sig);
            }
            0
        }
        akuma_exec::process::SignalHandler::UserFn(_) => {
            // Pend the signal on the target thread so it is delivered at the
            // next syscall return for that thread.
            akuma_exec::threading::pend_signal_for_thread(tid as usize, sig);
            0
        }
    }
}

/// tgkill(tgid, tid, sig) — like tkill but checks the thread group id.
/// We don't track thread groups separately so we just forward to tkill.
pub(super) fn sys_tgkill(_tgid: u32, tid: u32, sig: u32) -> u64 {
    sys_tkill(tid, sig)
}

/// Helper for other syscalls (like pipe write) to send SIGPIPE
pub(crate) fn send_sigpipe() {
    let tid = akuma_exec::threading::current_thread_id() as u32;
    // SIGPIPE is signal 13
    sys_tkill(tid, 13);
}
