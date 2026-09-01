use super::*;
use alloc::collections::{BTreeMap, BTreeSet};

struct TimerFdState {
    armed_at_us: u64,
    initial_us: u64,
    interval_us: u64,
    expirations_consumed: u64,
    pollers: BTreeSet<usize>,
}

static TIMERFD_TABLE: Spinlock<BTreeMap<u32, TimerFdState>> = Spinlock::new(BTreeMap::new());
static TIMERFD_NEXT_ID: AtomicU32 = AtomicU32::new(1);

pub fn timerfd_add_poller(id: u32, tid: usize) {
    akuma_primitives::irq::with_irqs_disabled(|| {
        if let Some(state) = TIMERFD_TABLE.lock().get_mut(&id) {
            state.pollers.insert(tid);
        }
    });
}

// `LocalTimespec` (a second `struct timespec`, spelled `u64`) is
// `akuma_syscalls_linux::Timespec` since 2026-08-27, reached through
// `use super::*`. The `bits()`/`from_bits()` pair below is what keeps this
// path's arithmetic unsigned, exactly as it was — see that type's doc comment.

fn timespec_to_us_safe(ptr: usize) -> Result<u64, u64> {
    if ptr == 0 { return Ok(0); }
    let mut ts = Timespec::default();
    if read_user_into(&mut ts, ptr as u64).is_err() {
        return Err(EFAULT);
    }
    Ok(ts.to_us())
}

fn us_to_timespec_safe(us: u64, ptr: usize) -> Result<(), u64> {
    let ts = Timespec::from_bits(us / 1_000_000, (us % 1_000_000) * 1_000);
    if write_user_val(ptr as u64, &ts).is_err() {
        return Err(EFAULT);
    }
    Ok(())
}

pub(super) fn timerfd_can_read(timer_id: u32) -> bool {
    let now = akuma_primitives::clock::uptime_us();
    TIMERFD_TABLE.lock().get(&timer_id).is_some_and(|state| {
        if state.initial_us == 0 { return false; }
        let elapsed = now.saturating_sub(state.armed_at_us);
        if elapsed < state.initial_us { return false; }
        let total =
            1 + (elapsed - state.initial_us).checked_div(state.interval_us).unwrap_or(0);
        total > state.expirations_consumed
    })
}

pub(super) fn sys_timerfd_create(clockid: i32, flags: i32) -> u64 {
    let timer_id = TIMERFD_NEXT_ID.fetch_add(1, Ordering::Relaxed);
    if let Some(proc) = akuma_exec::process::current_process_shared() {
        let fd = proc.alloc_fd(akuma_exec::process::FileDescriptor::TimerFd(timer_id));
        if crate::config::SYSCALL_DEBUG_NET_ENABLED {
            akuma_primitives::safe_print!(96, "[timerfd] create id={} fd={} clk={} fl={}\n", timer_id, fd, clockid, flags);
        }
        u64::from(fd)
    } else {
        EBADF
    }
}

pub(super) fn sys_timerfd_settime(fd_num: u32, flags: i32, new_value: usize, old_value: usize) -> SysResult {
    let timer_id = match akuma_exec::process::current_process_shared().and_then(|p| p.get_fd(fd_num)) {
        Some(akuma_exec::process::FileDescriptor::TimerFd(id)) => id,
        _ => return Err(EBADF),
    };

    let mut table = TIMERFD_TABLE.lock();

    if old_value != 0 {
        if let Some(state) = table.get(&timer_id) {
            let now = akuma_primitives::clock::uptime_us();
            let elapsed = now.saturating_sub(state.armed_at_us);
            let remaining = state.initial_us.saturating_sub(elapsed);
            // struct itimerspec { it_interval at 0, it_value at 16 }
            if us_to_timespec_safe(state.interval_us, old_value).is_err() { return Err(EFAULT); }      // it_interval
            if us_to_timespec_safe(remaining, old_value + 16).is_err() { return Err(EFAULT); }         // it_value (remaining time)
        } else {
            let zero = [0u8; 32];
            if copy_to_user(old_value as u64, &zero).is_err() {
                return Err(EFAULT);
            }
        }
    }

    // struct itimerspec { struct timespec it_interval; struct timespec it_value; }
    // it_interval is at offset 0, it_value (initial) is at offset 16
    let interval_us = timespec_to_us_safe(new_value)?;       // it_interval
    let initial_us = timespec_to_us_safe(new_value + 16)?;   // it_value (initial expiration)

    const TFD_TIMER_ABSTIME: i32 = 1;
    let now = akuma_primitives::clock::uptime_us();
    let effective_initial = if flags & TFD_TIMER_ABSTIME != 0 {
        initial_us.saturating_sub(now)
    } else {
        initial_us
    };

    if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
        akuma_primitives::safe_print!(128, "[timerfd] settime id={} initial={}us interval={}us\n",
        timer_id, effective_initial, interval_us);
    }

    if initial_us == 0 && interval_us == 0 {
        table.remove(&timer_id);
    } else {
        table.insert(timer_id, TimerFdState {
            armed_at_us: now,
            initial_us: effective_initial,
            interval_us,
            expirations_consumed: 0,
            pollers: BTreeSet::new(),
        });
    }

    Ok(0)
}

pub(super) fn sys_timerfd_gettime(fd_arg0: u64, out_ptr: u64) -> u64 {
    let timer_id = match akuma_exec::process::current_process_shared().and_then(|p| p.get_fd(fd_arg0 as u32)) {
        Some(akuma_exec::process::FileDescriptor::TimerFd(id)) => id,
        _ => return EBADF,
    };
    let out = out_ptr as usize;
    if out != 0 {
        if !validate_user_ptr(out_ptr, 32) {
            return EFAULT;
        }
        let table = TIMERFD_TABLE.lock();
        if let Some(state) = table.get(&timer_id) {
            let now = akuma_primitives::clock::uptime_us();
            let elapsed = now.saturating_sub(state.armed_at_us);
            let remaining = state.initial_us.saturating_sub(elapsed);
            if us_to_timespec_safe(state.interval_us, out).is_err() { return EFAULT; }
            if us_to_timespec_safe(remaining, out + 16).is_err() { return EFAULT; }
        } else {
            let zero = [0u8; 32];
            if copy_to_user(out as u64, &zero).is_err() {
                return EFAULT;
            }
        }
    }
    0
}

pub(super) fn timerfd_read(timer_id: u32) -> u64 {
    let now = akuma_primitives::clock::uptime_us();
    let mut table = TIMERFD_TABLE.lock();
    let state = match table.get_mut(&timer_id) {
        Some(s) => s,
        None => return EAGAIN,
    };

    if state.initial_us == 0 { return EAGAIN; }

    let elapsed = now.saturating_sub(state.armed_at_us);
    if elapsed < state.initial_us { return EAGAIN; }

    let total_expirations =
        1 + (elapsed - state.initial_us).checked_div(state.interval_us).unwrap_or(0);

    let new_expirations = total_expirations.saturating_sub(state.expirations_consumed);
    if new_expirations == 0 { return EAGAIN; }

    state.expirations_consumed = total_expirations;
    new_expirations
}
