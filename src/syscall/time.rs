use super::*;

pub(super) fn sys_clock_gettime(clock_id: u32, tp_ptr: u64) -> u64 {
    if !validate_user_ptr(tp_ptr, core::mem::size_of::<Timespec>()) { return EFAULT; }

    let (sec, nsec) = match clock_id {
        0 => {
            let us = crate::timer::utc_time_us().unwrap_or(0);
            ((us / 1_000_000) as i64, ((us % 1_000_000) * 1_000) as i64)
        }
        1 | _ => {
            let us = crate::timer::uptime_us();
            ((us / 1_000_000) as i64, ((us % 1_000_000) * 1_000) as i64)
        }
    };

    unsafe {
        *(tp_ptr as *mut Timespec) = Timespec { tv_sec: sec, tv_nsec: nsec };
    }
    0
}

pub(super) fn sys_clock_getres(clock_id: u32, res_ptr: usize) -> u64 {
    let _ = clock_id;
    if res_ptr != 0 && validate_user_ptr(res_ptr as u64, 16) {
        unsafe {
            let ptr = res_ptr as *mut u64;
            core::ptr::write(ptr, 0);  // tv_sec
            core::ptr::write(ptr.add(1), 1); // tv_nsec = 1
        }
    }
    0
}

pub(super) fn sys_nanosleep(a0: u64, a1: u64) -> u64 {
    // Support two ABIs:
    // - Linux/musl: a0 = pointer to struct timespec {tv_sec, tv_nsec}
    // - libakuma:   a0 = seconds (raw), a1 = nanoseconds (raw)
    // Distinguish by checking if a0 looks like a user-space pointer (>= PAGE_SIZE).
    let (sec, nsec) = if a0 >= 4096 && validate_user_ptr(a0, 16) {
        unsafe {
            let p = a0 as *const u64;
            (core::ptr::read(p), core::ptr::read(p.add(1)))
        }
    } else {
        (a0, a1)
    };
    let total_us = sec.saturating_mul(1_000_000).saturating_add(nsec / 1_000);
    if total_us == 0 { return 0; }
    let deadline = crate::timer::uptime_us().saturating_add(total_us);
    loop {
        if crate::timer::uptime_us() >= deadline { return 0; }
        if akuma_exec::process::is_current_interrupted() { return EINTR; }
        akuma_exec::threading::schedule_blocking(deadline);
    }
}

pub(super) fn sys_times(buf_ptr: usize) -> u64 {
    if buf_ptr != 0 {
        const TMS_SIZE: usize = 32;
        if !validate_user_ptr(buf_ptr as u64, TMS_SIZE) { return EFAULT; }
        unsafe { core::ptr::write_bytes(buf_ptr as *mut u8, 0, TMS_SIZE); }
    }
    let uptime_us = crate::timer::uptime_us();
    (uptime_us / 10_000) as u64
}

pub(super) fn sys_getrusage(who: i32, usage_ptr: usize) -> u64 {
    const RUSAGE_SIZE: usize = 144;
    if !validate_user_ptr(usage_ptr as u64, RUSAGE_SIZE) { return EFAULT; }
    unsafe { core::ptr::write_bytes(usage_ptr as *mut u8, 0, RUSAGE_SIZE); }
    let _ = who;
    0
}

pub(super) fn sys_time() -> u64 { crate::timer::utc_time_us().unwrap_or(0) }

pub(super) fn sys_uptime() -> u64 { crate::timer::uptime_us() }
