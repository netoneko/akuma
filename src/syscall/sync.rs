use super::*;

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

pub(super) fn sys_futex(uaddr: usize, op: i32, val: u32, timeout_ptr: u64, _uaddr2: usize, _val3: u32) -> u64 {
    const FUTEX_WAIT: i32 = 0;
    const FUTEX_WAKE: i32 = 1;
    const FUTEX_WAIT_BITSET: i32 = 9;
    const FUTEX_WAKE_BITSET: i32 = 10;
    const FUTEX_PRIVATE_FLAG: i32 = 128;
    const FUTEX_CLOCK_REALTIME: i32 = 256;

    let cmd = op & !(FUTEX_PRIVATE_FLAG | FUTEX_CLOCK_REALTIME);

    match cmd {
        FUTEX_WAIT | FUTEX_WAIT_BITSET => {
            let current = unsafe { (uaddr as *const AtomicU32).as_ref() };
            if let Some(atomic) = current {
                if atomic.load(Ordering::SeqCst) != val {
                    return EAGAIN;
                }
            } else {
                return EFAULT;
            }

            let tid = akuma_exec::threading::current_thread_id();

            {
                let mut waiters = FUTEX_WAITERS.lock();
                let queue = waiters.entry(uaddr).or_insert_with(Vec::new);
                queue.push(tid);
            }

            let deadline = if timeout_ptr != 0 && validate_user_ptr(timeout_ptr, 16) {
                let ts = unsafe { &*(timeout_ptr as *const Timespec) };
                let timeout_us = (ts.tv_sec as u64) * 1_000_000 + (ts.tv_nsec as u64) / 1000;
                if cmd == FUTEX_WAIT_BITSET {
                    let now_us = crate::timer::uptime_us();
                    if timeout_us > now_us { timeout_us } else { now_us }
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
        _ => 0,
    }
}
