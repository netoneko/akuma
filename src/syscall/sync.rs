use super::*;
use akuma_exec::mmu::user_access::copy_from_user_safe;

/// Futex waiter table.
///
/// Key is `(tgid, uaddr)`:
/// - For FUTEX_PRIVATE operations, `tgid` is the thread-group leader's PID (from
///   `PROCESS_INFO_ADDR`), scoping the futex to the process. This prevents cross-process
///   VA collisions when different processes have the same virtual address (no ASLR).
/// - For FUTEX_SHARED (non-private) operations, `tgid = 0`.
/// - For kernel-internal wakes (clear_child_tid, robust futex), `tgid = 0`.
static FUTEX_WAITERS: Spinlock<BTreeMap<(u32, usize), Vec<usize>>> = Spinlock::new(BTreeMap::new());

/// Returns the TGID to use as the futex key namespace.
/// For private futex: returns the current process's PID (shared among CLONE_VM threads via
/// `PROCESS_INFO_ADDR`). For non-private (shared): returns 0.
fn futex_key_tgid(is_private: bool) -> u32 {
    if is_private {
        akuma_exec::process::read_current_pid().unwrap_or(0)
    } else {
        0
    }
}

fn futex_do_wake(tgid: u32, uaddr: usize, max_wake: u32) -> u64 {
    let mut waiters = FUTEX_WAITERS.lock();
    let key = (tgid, uaddr);
    let woken = if let Some(queue) = waiters.get_mut(&key) {
        let count = (max_wake as usize).min(queue.len());
        let to_wake: Vec<usize> = queue.drain(..count).collect();
        if queue.is_empty() {
            waiters.remove(&key);
        }
        drop(waiters);
        for tid in &to_wake {
            akuma_exec::threading::get_waker_for_thread(*tid).wake();
        }
        to_wake.len() as u64
    } else {
        0
    };
    woken
}

/// Kernel-internal futex wake (clear_child_tid, robust futex). Uses tgid=0 (shared).
pub fn futex_wake(uaddr: usize, max_wake: i32) {
    futex_do_wake(0, uaddr, max_wake as u32);
}

pub(super) fn sys_futex(uaddr: usize, op: i32, val: u32, timeout_ptr: u64, uaddr2: usize, val3: u32) -> u64 {
    const FUTEX_WAIT: i32 = 0;
    const FUTEX_WAKE: i32 = 1;
    #[allow(dead_code)]
    const FUTEX_FD: i32 = 2;  // Deprecated, returns ENOSYS
    const FUTEX_REQUEUE: i32 = 3;
    const FUTEX_CMP_REQUEUE: i32 = 4;
    const FUTEX_WAKE_OP: i32 = 5;
    const FUTEX_LOCK_PI: i32 = 6;
    const FUTEX_UNLOCK_PI: i32 = 7;
    const FUTEX_TRYLOCK_PI: i32 = 8;
    const FUTEX_WAIT_BITSET: i32 = 9;
    const FUTEX_WAKE_BITSET: i32 = 10;
    const FUTEX_WAIT_REQUEUE_PI: i32 = 11;
    const FUTEX_CMP_REQUEUE_PI: i32 = 12;
    const FUTEX_PRIVATE_FLAG: i32 = 128;
    const FUTEX_CLOCK_REALTIME: i32 = 256;

    let is_private = (op & FUTEX_PRIVATE_FLAG) != 0;
    let cmd = op & !(FUTEX_PRIVATE_FLAG | FUTEX_CLOCK_REALTIME);

    // Validate uaddr - must be 4-byte aligned and in user space
    if uaddr == 0 || uaddr & 3 != 0 {
        return EINVAL;
    }
    if !validate_user_ptr(uaddr as u64, 4) {
        crate::tprint!(128, "[futex] EFAULT: uaddr={:#x} not mapped\n", uaddr);
        return EFAULT;
    }

    match cmd {
        FUTEX_WAIT | FUTEX_WAIT_BITSET => {
            let tid = akuma_exec::threading::current_thread_id();
            let tgid = futex_key_tgid(is_private);
            let key = (tgid, uaddr);

            // FUTEX_WAIT_BITSET with val3==0 is invalid per spec.
            if cmd == FUTEX_WAIT_BITSET && val3 == 0 {
                return EINVAL;
            }

            {
                let mut waiters = FUTEX_WAITERS.lock();
                // Read value INSIDE the lock — atomic with respect to futex_do_wake.
                // A concurrent wake either runs before we lock (and changes the futex
                // value, so we see the new value and return EAGAIN) or after we insert
                // our TID (so it finds us and calls wake, setting the sticky flag).
                let mut current_val: u32 = 0;
                if unsafe { copy_from_user_safe(&mut current_val as *mut u32 as *mut u8, uaddr as *const u8, 4).is_err() } {
                    return EFAULT;
                }
                if current_val != val {
                    return EAGAIN;
                }
                let queue = waiters.entry(key).or_insert_with(Vec::new);
                queue.push(tid);
            }

            let is_realtime = (op & FUTEX_CLOCK_REALTIME) != 0;
            let deadline = if timeout_ptr != 0 && validate_user_ptr(timeout_ptr, 16) {
                let mut ts = Timespec { tv_sec: 0, tv_nsec: 0 };
                if unsafe { copy_from_user_safe(&mut ts as *mut Timespec as *mut u8, timeout_ptr as *const u8, 16).is_err() } {
                    // Remove ourselves from the waiter queue before returning.
                    let mut waiters = FUTEX_WAITERS.lock();
                    if let Some(queue) = waiters.get_mut(&key) {
                        queue.retain(|&t| t != tid);
                        if queue.is_empty() { waiters.remove(&key); }
                    }
                    return EFAULT;
                }
                let timeout_us = (ts.tv_sec as u64) * 1_000_000 + (ts.tv_nsec as u64) / 1000;
                if cmd == FUTEX_WAIT_BITSET && is_realtime {
                    // FUTEX_WAIT_BITSET + CLOCK_REALTIME: timeout is an absolute wall-clock
                    // value. We treat it as absolute uptime microseconds (imprecise but safe:
                    // prevents sleeping far into the future compared to adding uptime).
                    timeout_us
                } else {
                    crate::timer::uptime_us() + timeout_us
                }
            } else {
                u64::MAX
            };

            akuma_exec::threading::schedule_blocking(deadline);

            {
                let mut waiters = FUTEX_WAITERS.lock();
                if let Some(queue) = waiters.get_mut(&key) {
                    queue.retain(|&t| t != tid);
                    if queue.is_empty() {
                        waiters.remove(&key);
                    }
                }
            }

            // If we were woken by a pending signal, return EINTR (Linux spec).
            if akuma_exec::threading::peek_pending_signal(tid) != 0 {
                return EINTR;
            }

            if deadline != u64::MAX && crate::timer::uptime_us() >= deadline {
                return ETIMEDOUT;
            }

            0
        }
        FUTEX_WAKE | FUTEX_WAKE_BITSET => {
            let tgid = futex_key_tgid(is_private);
            futex_do_wake(tgid, uaddr, val)
        }
        FUTEX_REQUEUE => {
            // Wake up to val waiters, requeue rest to uaddr2
            // val2 (passed as timeout_ptr) is max to requeue
            let max_requeue = timeout_ptr as u32;
            let tgid = futex_key_tgid(is_private);
            let key1 = (tgid, uaddr);
            let key2 = (tgid, uaddr2);

            if uaddr2 != 0 && !validate_user_ptr(uaddr2 as u64, 4) {
                return EFAULT;
            }

            let mut waiters = FUTEX_WAITERS.lock();

            // Extract waiters from uaddr
            let (to_wake, to_requeue) = if let Some(queue) = waiters.remove(&key1) {
                let wake_count = (val as usize).min(queue.len());
                let mut remaining: Vec<usize> = queue;
                let to_wake: Vec<usize> = remaining.drain(..wake_count).collect();

                let requeue_count = if uaddr2 != 0 {
                    (max_requeue as usize).min(remaining.len())
                } else {
                    0
                };
                let to_requeue: Vec<usize> = remaining.drain(..requeue_count).collect();

                // Put back any remaining waiters
                if !remaining.is_empty() {
                    waiters.insert(key1, remaining);
                }

                (to_wake, to_requeue)
            } else {
                (Vec::new(), Vec::new())
            };

            // Add requeued waiters to uaddr2
            if !to_requeue.is_empty() && uaddr2 != 0 {
                let queue2 = waiters.entry(key2).or_insert_with(Vec::new);
                queue2.extend(to_requeue.iter().copied());
            }

            let woken = to_wake.len();
            let requeued = to_requeue.len();

            drop(waiters);

            for tid in &to_wake {
                akuma_exec::threading::get_waker_for_thread(*tid).wake();
            }

            (woken + requeued) as u64
        }
        FUTEX_CMP_REQUEUE => {
            // Like FUTEX_REQUEUE but also checks val3 against uaddr value
            let max_requeue = timeout_ptr as u32;
            let tgid = futex_key_tgid(is_private);
            let key1 = (tgid, uaddr);
            let key2 = (tgid, uaddr2);

            // Check current value matches expected
            let mut current_val: u32 = 0;
            if unsafe { copy_from_user_safe(&mut current_val as *mut u32 as *mut u8, uaddr as *const u8, 4).is_err() } {
                return EFAULT;
            }
            if current_val != val3 {
                return EAGAIN;
            }

            if uaddr2 != 0 && !validate_user_ptr(uaddr2 as u64, 4) {
                return EFAULT;
            }

            let mut waiters = FUTEX_WAITERS.lock();

            let (to_wake, to_requeue) = if let Some(queue) = waiters.remove(&key1) {
                let wake_count = (val as usize).min(queue.len());
                let mut remaining: Vec<usize> = queue;
                let to_wake: Vec<usize> = remaining.drain(..wake_count).collect();

                let requeue_count = if uaddr2 != 0 {
                    (max_requeue as usize).min(remaining.len())
                } else {
                    0
                };
                let to_requeue: Vec<usize> = remaining.drain(..requeue_count).collect();

                if !remaining.is_empty() {
                    waiters.insert(key1, remaining);
                }

                (to_wake, to_requeue)
            } else {
                (Vec::new(), Vec::new())
            };

            if !to_requeue.is_empty() && uaddr2 != 0 {
                let queue2 = waiters.entry(key2).or_insert_with(Vec::new);
                queue2.extend(to_requeue.iter().copied());
            }

            let woken = to_wake.len();
            let requeued = to_requeue.len();

            drop(waiters);

            for tid in &to_wake {
                akuma_exec::threading::get_waker_for_thread(*tid).wake();
            }

            (woken + requeued) as u64
        }
        FUTEX_WAKE_OP => {
            // Complex operation: wake waiters at uaddr, optionally wake at uaddr2
            // based on atomic operation result. For now, just wake at uaddr.
            let tgid = futex_key_tgid(is_private);
            futex_do_wake(tgid, uaddr, val)
        }
        FUTEX_LOCK_PI | FUTEX_UNLOCK_PI | FUTEX_TRYLOCK_PI => ENOSYS,
        FUTEX_WAIT_REQUEUE_PI | FUTEX_CMP_REQUEUE_PI => ENOSYS,
        _ => {
            crate::tprint!(96, "[futex] unsupported op={} (cmd={})\n", op, cmd);
            ENOSYS
        }
    }
}
