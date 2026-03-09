use super::*;

const SIG_DFL: usize = 0;
const SIG_IGN: usize = 1;

#[repr(C)]
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
        let old = &proc.signal_actions[idx];
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
        unsafe { core::ptr::write_unaligned(oldact_ptr as *mut KernelSigaction, out); }
    }

    if act_ptr != 0 && validate_user_ptr(act_ptr as u64, 32) {
        let sa = unsafe { core::ptr::read_unaligned(act_ptr as *const KernelSigaction) };
        let handler = match sa.sa_handler {
            SIG_DFL => akuma_exec::process::SignalHandler::Default,
            SIG_IGN => akuma_exec::process::SignalHandler::Ignore,
            addr => akuma_exec::process::SignalHandler::UserFn(addr),
        };
        proc.signal_actions[idx] = akuma_exec::process::SignalAction {
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

pub(super) fn sys_tkill(tid: u32, sig: u32) -> u64 {
    if sig == 0 { return 0; }
    if sig as usize > akuma_exec::process::MAX_SIGNALS { return EINVAL; }

    crate::safe_print!(96, "[signal] tkill(tid={}, sig={})\n", tid, sig);

    if sig == 9 {
        super::proc::sys_exit_group(-(sig as i32));
        return 0;
    }

    let handler = akuma_exec::process::current_process()
        .map(|p| {
            let idx = (sig - 1) as usize;
            p.signal_actions[idx].handler
        })
        .unwrap_or(akuma_exec::process::SignalHandler::Default);

    match handler {
        akuma_exec::process::SignalHandler::Ignore => 0,
        akuma_exec::process::SignalHandler::Default => {
            if signal_is_fatal_default(sig) {
                super::proc::sys_exit_group(-(sig as i32));
            }
            0
        }
        akuma_exec::process::SignalHandler::UserFn(_) => {
            if sig == 6 {
                super::proc::sys_exit_group(-(sig as i32));
            }
            0
        }
    }
}
