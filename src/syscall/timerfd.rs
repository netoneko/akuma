use super::*;
use akuma_exec::mmu::user_access::{copy_from_user_safe, copy_to_user_safe};

struct TimerFdState {
    armed_at_us: u64,
    initial_us: u64,
    interval_us: u64,
    expirations_consumed: u64,
}

static TIMERFD_TABLE: Spinlock<BTreeMap<u32, TimerFdState>> = Spinlock::new(BTreeMap::new());
static TIMERFD_NEXT_ID: AtomicU32 = AtomicU32::new(1);

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct LocalTimespec {
    tv_sec: u64,
    tv_nsec: u64,
}

fn timespec_to_us_safe(ptr: usize) -> Result<u64, u64> {
    if ptr == 0 { return Ok(0); }
    let mut ts = LocalTimespec::default();
    if unsafe { copy_from_user_safe(&mut ts as *mut LocalTimespec as *mut u8, ptr as *const u8, 16).is_err() } {
        return Err(EFAULT);
    }
    Ok(ts.tv_sec * 1_000_000 + ts.tv_nsec / 1_000)
}

fn us_to_timespec_safe(us: u64, ptr: usize) -> Result<(), u64> {
    let ts = LocalTimespec {
        tv_sec: us / 1_000_000,
        tv_nsec: (us % 1_000_000) * 1_000,
    };
    if unsafe { copy_to_user_safe(ptr as *mut u8, &ts as *const LocalTimespec as *const u8, 16).is_err() } {
        return Err(EFAULT);
    }
    Ok(())
}

pub(super) fn timerfd_can_read(timer_id: u32) -> bool {
    let now = crate::timer::uptime_us();
    TIMERFD_TABLE.lock().get(&timer_id).map_or(false, |state| {
        if state.initial_us == 0 { return false; }
        let elapsed = now.saturating_sub(state.armed_at_us);
        if elapsed < state.initial_us { return false; }
        let total = if state.interval_us > 0 {
            1 + (elapsed - state.initial_us) / state.interval_us
        } else {
            1
        };
        total > state.expirations_consumed
    })
}

pub(super) fn sys_timerfd_create(clockid: i32, flags: i32) -> u64 {
    let timer_id = TIMERFD_NEXT_ID.fetch_add(1, Ordering::Relaxed);
    if let Some(proc) = akuma_exec::process::current_process() {
        let fd = proc.alloc_fd(akuma_exec::process::FileDescriptor::TimerFd(timer_id));
        crate::safe_print!(96, "[timerfd] create id={} fd={} clk={} fl={}\n", timer_id, fd, clockid, flags);
        fd as u64
    } else {
        EBADF
    }
}

pub(super) fn sys_timerfd_settime(fd_num: u32, flags: i32, new_value: usize, old_value: usize) -> u64 {
    let timer_id = match akuma_exec::process::current_process().and_then(|p| p.get_fd(fd_num)) {
        Some(akuma_exec::process::FileDescriptor::TimerFd(id)) => id,
        _ => return EBADF,
    };

    let mut table = TIMERFD_TABLE.lock();

    if old_value != 0 && validate_user_ptr(old_value as u64, 32) {
        if let Some(state) = table.get(&timer_id) {
            let now = crate::timer::uptime_us();
            let elapsed = now.saturating_sub(state.armed_at_us);
            let remaining = state.initial_us.saturating_sub(elapsed);
            // struct itimerspec { it_interval at 0, it_value at 16 }
            let _ = us_to_timespec_safe(state.interval_us, old_value);      // it_interval
            let _ = us_to_timespec_safe(remaining, old_value + 16);         // it_value (remaining time)
        } else {
            let zero = [0u8; 32];
            let _ = unsafe { copy_to_user_safe(old_value as *mut u8, zero.as_ptr(), 32) };
        }
    }

    if !validate_user_ptr(new_value as u64, 32) { return EFAULT; }

    // struct itimerspec { struct timespec it_interval; struct timespec it_value; }
    // it_interval is at offset 0, it_value (initial) is at offset 16
    let interval_us = match timespec_to_us_safe(new_value) { Ok(v) => v, Err(e) => return e };       // it_interval
    let initial_us = match timespec_to_us_safe(new_value + 16) { Ok(v) => v, Err(e) => return e };   // it_value (initial expiration)

    const TFD_TIMER_ABSTIME: i32 = 1;
    let now = crate::timer::uptime_us();
    let effective_initial = if flags & TFD_TIMER_ABSTIME != 0 {
        initial_us.saturating_sub(now)
    } else {
        initial_us
    };

    crate::safe_print!(128, "[timerfd] settime id={} initial={}us interval={}us\n",
        timer_id, effective_initial, interval_us);

    if initial_us == 0 && interval_us == 0 {
        table.remove(&timer_id);
    } else {
        table.insert(timer_id, TimerFdState {
            armed_at_us: now,
            initial_us: effective_initial,
            interval_us,
            expirations_consumed: 0,
        });
    }

    0
}

pub(super) fn sys_timerfd_gettime(fd_arg0: u64, out_ptr: u64) -> u64 {
    let timer_id = match akuma_exec::process::current_process().and_then(|p| p.get_fd(fd_arg0 as u32)) {
        Some(akuma_exec::process::FileDescriptor::TimerFd(id)) => id,
        _ => return EBADF,
    };
    let out = out_ptr as usize;
    if out != 0 && validate_user_ptr(out_ptr, 32) {
        let table = TIMERFD_TABLE.lock();
        if let Some(state) = table.get(&timer_id) {
            let now = crate::timer::uptime_us();
            let elapsed = now.saturating_sub(state.armed_at_us);
            let remaining = state.initial_us.saturating_sub(elapsed);
            let _ = us_to_timespec_safe(state.interval_us, out);
            let _ = us_to_timespec_safe(remaining, out + 16);
        } else {
            let zero = [0u8; 32];
            let _ = unsafe { copy_to_user_safe(out as *mut u8, zero.as_ptr(), 32) };
        }
    }
    0
}

pub(super) fn timerfd_read(timer_id: u32) -> u64 {
    let now = crate::timer::uptime_us();
    let mut table = TIMERFD_TABLE.lock();
    let state = match table.get_mut(&timer_id) {
        Some(s) => s,
        None => return EAGAIN,
    };

    if state.initial_us == 0 { return EAGAIN; }

    let elapsed = now.saturating_sub(state.armed_at_us);
    if elapsed < state.initial_us { return EAGAIN; }

    let total_expirations = if state.interval_us > 0 {
        1 + (elapsed - state.initial_us) / state.interval_us
    } else {
        1
    };

    let new_expirations = total_expirations.saturating_sub(state.expirations_consumed);
    if new_expirations == 0 { return EAGAIN; }

    state.expirations_consumed = total_expirations;
    new_expirations
}
