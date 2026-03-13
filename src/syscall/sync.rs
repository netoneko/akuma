use super::*;
use akuma_exec::mmu::user_access::copy_from_user_safe;

static FUTEX_WAITERS: Spinlock<BTreeMap<usize, Vec<usize>>> = Spinlock::new(BTreeMap::new());

fn futex_do_wake(uaddr: usize, max_wake: u32) -> u64 {
    let mut waiters = FUTEX_WAITERS.lock();
    let woken = if let Some(queue) = waiters.get_mut(&uaddr) {
        let count = (max_wake as usize).min(queue.len());
        let to_wake: Vec<usize> = queue.drain(..count).collect();
        if queue.is_empty() {
            waiters.remove(&uaddr);
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

pub fn futex_wake(uaddr: usize, max_wake: i32) {
    futex_do_wake(uaddr, max_wake as u32);
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

    let cmd = op & !(FUTEX_PRIVATE_FLAG | FUTEX_CLOCK_REALTIME);

    // Validate uaddr - must be 4-byte aligned and in user space
    if uaddr == 0 || uaddr & 3 != 0 {
        return EINVAL;
    }
    if !validate_user_ptr(uaddr as u64, 4) {
        return EFAULT;
    }

    match cmd {
        FUTEX_WAIT | FUTEX_WAIT_BITSET => {
            // Read the futex value atomically
            let mut current_val: u32 = 0;
            if unsafe { copy_from_user_safe(&mut current_val as *mut u32 as *mut u8, uaddr as *const u8, 4).is_err() } {
                return EFAULT;
            }
            if current_val != val {
                return EAGAIN;
            }

            let tid = akuma_exec::threading::current_thread_id();

            {
                let mut waiters = FUTEX_WAITERS.lock();
                let queue = waiters.entry(uaddr).or_insert_with(Vec::new);
                queue.push(tid);
            }

            let deadline = if timeout_ptr != 0 && validate_user_ptr(timeout_ptr, 16) {
                let mut ts = Timespec { tv_sec: 0, tv_nsec: 0 };
                if unsafe { copy_from_user_safe(&mut ts as *mut Timespec as *mut u8, timeout_ptr as *const u8, 16).is_err() } {
                    return EFAULT;
                }
                let timeout_us = (ts.tv_sec as u64) * 1_000_000 + (ts.tv_nsec as u64) / 1000;
                if cmd == FUTEX_WAIT_BITSET {
                    // FUTEX_WAIT_BITSET uses absolute time if CLOCK_REALTIME flag set
                    // For now, treat as relative since we use monotonic time
                    crate::timer::uptime_us() + timeout_us
                } else {
                    crate::timer::uptime_us() + timeout_us
                }
            } else {
                u64::MAX
            };

            akuma_exec::threading::schedule_blocking(deadline);

            {
                let mut waiters = FUTEX_WAITERS.lock();
                if let Some(queue) = waiters.get_mut(&uaddr) {
                    queue.retain(|&t| t != tid);
                    if queue.is_empty() {
                        waiters.remove(&uaddr);
                    }
                }
            }

            if deadline != u64::MAX && crate::timer::uptime_us() >= deadline {
                return ETIMEDOUT;
            }

            0
        }
        FUTEX_WAKE | FUTEX_WAKE_BITSET => {
            futex_do_wake(uaddr, val)
        }
        FUTEX_REQUEUE => {
            // Wake up to val waiters, requeue rest to uaddr2
            // val2 (passed as timeout_ptr) is max to requeue
            let max_requeue = timeout_ptr as u32;
            
            if uaddr2 != 0 && !validate_user_ptr(uaddr2 as u64, 4) {
                return EFAULT;
            }
            
            let mut waiters = FUTEX_WAITERS.lock();
            
            // Extract waiters from uaddr
            let (to_wake, to_requeue) = if let Some(queue) = waiters.remove(&uaddr) {
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
                    waiters.insert(uaddr, remaining);
                }
                
                (to_wake, to_requeue)
            } else {
                (Vec::new(), Vec::new())
            };
            
            // Add requeued waiters to uaddr2
            if !to_requeue.is_empty() && uaddr2 != 0 {
                let queue2 = waiters.entry(uaddr2).or_insert_with(Vec::new);
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
            
            let (to_wake, to_requeue) = if let Some(queue) = waiters.remove(&uaddr) {
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
                    waiters.insert(uaddr, remaining);
                }
                
                (to_wake, to_requeue)
            } else {
                (Vec::new(), Vec::new())
            };
            
            if !to_requeue.is_empty() && uaddr2 != 0 {
                let queue2 = waiters.entry(uaddr2).or_insert_with(Vec::new);
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
            futex_do_wake(uaddr, val)
        }
        FUTEX_LOCK_PI | FUTEX_UNLOCK_PI | FUTEX_TRYLOCK_PI => ENOSYS,
        FUTEX_WAIT_REQUEUE_PI | FUTEX_CMP_REQUEUE_PI => ENOSYS,
        _ => {
            crate::tprint!(96, "[futex] unsupported op={} (cmd={})\n", op, cmd);
            ENOSYS
        }
    }
}
