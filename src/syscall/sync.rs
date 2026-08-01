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
/// # Why every access below masks local IRQs
///
/// `FUTEX_WAITERS` is reachable from a BKL-free syscall window (Phase 7f). A nested IRQ
/// taken on a core that holds this table runs `enter_kernel()` and hard-spins for the
/// BKL; if a peer core holds the BKL and is inside `futex_do_wake` waiting on this
/// table, neither can advance — the AB-BA shape `locking.md`'s "Correctness rules
/// learned the hard way" describes, and the reason `PreemptGuard` masks IRQs. Masking
/// makes each hold un-interruptible on its own core, which is the discipline `PIPES`
/// (`syscall/pipe.rs`) and `Process::fault_mutex` (`process/children.rs`) already use.
///
/// Two of the critical sections below read the futex word from user memory *inside* the
/// hold, which the lost-wakeup argument requires (see `sys_futex`'s FUTEX_WAIT arm).
/// That is safe under masked IRQs because the word is already mapped: `sys_futex`
/// validates `uaddr` with `validate_user_ptr` (which demand-pages via
/// `ensure_user_pages_mapped`) before any lock op, and a futex word is writable
/// anonymous memory, so `reclaim_clean_file_pages` — which only evicts clean RO *file*
/// pages — can never unmap it underneath us. A fault there therefore means userspace
/// raced an `munmap` against its own `FUTEX_WAIT`, and with the lazy region gone
/// `try_resolve_el1_user_copy_lazy_fault` declines the fault, so it resolves through
/// `copy_from_user_safe`'s fixup to `EFAULT` rather than demand-paging under the hold.
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

pub fn futex_do_wake(tgid: u32, uaddr: usize, max_wake: u32) -> u64 {
    let key = (tgid, uaddr);

    // The wakes deliberately run *outside* the hold: `Waker::wake` touches the
    // scheduler, which must not be entered with the futex table held.
    let to_wake: Vec<usize> = crate::irq::with_irqs_disabled(|| {
        let mut waiters = FUTEX_WAITERS.lock();
        let Some(queue) = waiters.get_mut(&key) else { return Vec::new() };
        let count = (max_wake as usize).min(queue.len());
        let drained: Vec<usize> = queue.drain(..count).collect();
        if queue.is_empty() {
            waiters.remove(&key);
        }
        drained
    });

    for tid in &to_wake {
        akuma_exec::threading::get_waker_for_thread(*tid).wake();
    }
    to_wake.len() as u64
}

/// Kernel-internal futex wake (clear_child_tid, robust futex).
/// Wakes both tgid=0 (shared futex waiters) and tgid=tgid (FUTEX_PRIVATE waiters such
/// as pthread_join), since we cannot know which variant the waiter used.
pub fn futex_wake(tgid: u32, uaddr: usize, max_wake: i32) {
    let n0 = futex_do_wake(0, uaddr, max_wake as u32);
    let n1 = if tgid != 0 {
        futex_do_wake(tgid, uaddr, max_wake as u32)
    } else {
        0
    };
    if crate::config::FUTEX_DBG_ENABLED {
        tprint!(128, "[clear_child_tid] tgid={} addr={:#x} woke shared={} private={}\n", tgid, uaddr, n0, n1);
    }
}

/// Test helper: insert the current thread into the futex waiter table at an
/// explicit (tgid, uaddr) key and block until woken.
///
/// `FUTEX_WAIT_PRIVATE` in the test environment always resolves to tgid=0
/// (because `read_current_pid()` returns None with no user address space).
/// This helper lets tests place a waiter at a non-zero tgid so we can
/// verify that `futex_wake(tgid, ...)` correctly reaches private-futex
/// queues (the fix for the `clear_child_tid` / `pthread_join` hang).
#[allow(dead_code)]
pub fn futex_wait_at_tgid_for_test(tgid: u32, uaddr: usize) {
    let tid = akuma_exec::threading::current_thread_id();
    let key = (tgid, uaddr);
    crate::irq::with_irqs_disabled(|| {
        let mut waiters = FUTEX_WAITERS.lock();
        waiters.entry(key).or_default().push(tid);
    });
    akuma_exec::threading::schedule_blocking(u64::MAX);
    // futex_do_wake removed us from the queue before calling wake()
}

/// Atomically re-check the futex word and enqueue `tid` as a waiter on `key`.
///
/// The user read happens INSIDE the hold on purpose — that is what makes it atomic with
/// respect to `futex_do_wake`. A concurrent wake either runs before we take the table
/// (and changes the futex value, so we observe the new value and report `EAGAIN`) or
/// after we insert our tid (so it finds us and wakes us). Splitting the read out would
/// reopen the lost-wakeup window. See the `FUTEX_WAITERS` doc comment for why doing the
/// read under masked IRQs cannot demand-page.
///
/// `Err` carries the errno the caller must return; `Ok` means we are enqueued.
fn futex_check_and_enqueue(key: (u32, usize), tid: usize, uaddr: usize, val: u32) -> Result<(), u64> {
    crate::irq::with_irqs_disabled(|| {
        let mut waiters = FUTEX_WAITERS.lock();
        let mut current_val: u32 = 0;
        if unsafe { copy_from_user_safe((&raw mut current_val).cast::<u8>(), uaddr as *const u8, 4).is_err() } {
            return Err(EFAULT);
        }
        if current_val != val {
            return Err(EAGAIN);
        }
        waiters.entry(key).or_default().push(tid);
        Ok(())
    })
}

/// Move waiters off `key1`: take up to `max_wake` of them for the caller to wake, and
/// requeue up to `max_requeue` of the rest onto `key2` (skipped when `key2`'s uaddr is
/// 0). Shared verbatim by FUTEX_REQUEUE and FUTEX_CMP_REQUEUE, which differ only in the
/// value pre-check they do before calling.
///
/// Returns the tids to wake and how many were requeued. The wakes are deliberately left
/// to the caller so they happen outside the hold.
fn futex_requeue_table(
    key1: (u32, usize),
    key2: (u32, usize),
    max_wake: u32,
    max_requeue: u32,
) -> (Vec<usize>, usize) {
    let has_requeue_target = key2.1 != 0;
    crate::irq::with_irqs_disabled(|| {
        let mut waiters = FUTEX_WAITERS.lock();

        let (to_wake, to_requeue) = if let Some(queue) = waiters.remove(&key1) {
            let wake_count = (max_wake as usize).min(queue.len());
            let mut remaining: Vec<usize> = queue;
            let to_wake: Vec<usize> = remaining.drain(..wake_count).collect();

            let requeue_count = if has_requeue_target {
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

        if !to_requeue.is_empty() && has_requeue_target {
            waiters.entry(key2).or_default().extend(to_requeue.iter().copied());
        }

        (to_wake, to_requeue.len())
    })
}

/// Remove `tid` from `key`'s waiter queue, dropping the queue if it empties.
fn futex_dequeue(key: (u32, usize), tid: usize) {
    crate::irq::with_irqs_disabled(|| {
        let mut waiters = FUTEX_WAITERS.lock();
        if let Some(queue) = waiters.get_mut(&key) {
            queue.retain(|&t| t != tid);
            if queue.is_empty() { waiters.remove(&key); }
        }
    });
}

/// Test hooks for the boot self-test in `src/process_tests.rs`.
///
/// The requeue table logic below was factored out of two byte-identical copies
/// (FUTEX_REQUEUE / FUTEX_CMP_REQUEUE) while masking IRQs, so it is exactly the kind of
/// change that wants a deterministic test rather than a log grep. The waiter table needs
/// no user address space, so it is fully drivable from the boot suite — unlike
/// `futex_check_and_enqueue`, whose in-hold user read has no valid `uaddr` there.
#[cfg(not(any(feature = "no-tests", kernel_profile_size)))]
pub mod test_hooks {
    use super::{FUTEX_WAITERS, futex_dequeue, futex_requeue_table};
    use alloc::vec::Vec;

    pub fn enqueue(key: (u32, usize), tid: usize) {
        crate::irq::with_irqs_disabled(|| {
            FUTEX_WAITERS.lock().entry(key).or_default().push(tid);
        });
    }

    /// `None` when no queue exists for `key` (distinct from an empty one, which the
    /// table never stores — every removal path drops an emptied queue).
    pub fn queue(key: (u32, usize)) -> Option<Vec<usize>> {
        crate::irq::with_irqs_disabled(|| FUTEX_WAITERS.lock().get(&key).cloned())
    }

    pub fn requeue(key1: (u32, usize), key2: (u32, usize), max_wake: u32, max_requeue: u32) -> (Vec<usize>, usize) {
        futex_requeue_table(key1, key2, max_wake, max_requeue)
    }

    pub fn dequeue(key: (u32, usize), tid: usize) {
        futex_dequeue(key, tid);
    }

    pub fn drop_key(key: (u32, usize)) {
        crate::irq::with_irqs_disabled(|| { FUTEX_WAITERS.lock().remove(&key); });
    }
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
        // For WAKE operations on unmapped addresses: there can't be any
        // waiters, so return 0 (none woken).  Go's runtime calls
        // futex(0xfffffffffffffffc, FUTEX_WAKE) during exit coordination —
        // returning EFAULT breaks Go's exit path and leaves goroutine
        // threads stuck.
        if cmd == FUTEX_WAKE || cmd == FUTEX_WAKE_BITSET || cmd == FUTEX_WAKE_OP {
            return 0; // no waiters on unmapped address
        }
        if cmd == FUTEX_WAIT || cmd == FUTEX_WAIT_BITSET {
            return EAGAIN; // "value changed" — Go retries and proceeds with exit
        }
        return EFAULT;
    }

    match cmd {
        FUTEX_WAIT | FUTEX_WAIT_BITSET => {
            let tid = akuma_exec::threading::current_thread_id();
            let tgid = futex_key_tgid(is_private);
            let key = (tgid, uaddr);

            if crate::config::FUTEX_DBG_ENABLED {
                let ts = crate::timer::uptime_us();
                tprint!(128, "[futex-dbg] WAIT tid={} tgid={} addr={:#x} val={} ts={}us\n", tid, tgid, uaddr, val, ts);
            }

            // FUTEX_WAIT_BITSET with val3==0 is invalid per spec.
            if cmd == FUTEX_WAIT_BITSET && val3 == 0 {
                return EINVAL;
            }

            if let Err(errno) = futex_check_and_enqueue(key, tid, uaddr, val) {
                return errno;
            }

            let is_realtime = (op & FUTEX_CLOCK_REALTIME) != 0;
            let deadline = if timeout_ptr != 0 && validate_user_ptr(timeout_ptr, 16) {
                let mut ts = Timespec { tv_sec: 0, tv_nsec: 0 };
                if unsafe { copy_from_user_safe((&raw mut ts).cast::<u8>(), timeout_ptr as *const u8, 16).is_err() } {
                    // Remove ourselves from the waiter queue before returning.
                    futex_dequeue(key, tid);
                    return EFAULT;
                }
                let timeout_us = (ts.tv_sec as u64) * 1_000_000 + (ts.tv_nsec as u64) / 1000;
                // Timeout interpretation per Linux semantics, NOT op-flag-agnostic:
                //   - FUTEX_WAIT (plain): timeout is RELATIVE to now.
                //   - FUTEX_WAIT_BITSET: timeout is ABSOLUTE. Default clock is
                //     CLOCK_MONOTONIC; the CLOCK_REALTIME flag selects wall-clock.
                // The wait loop below compares deadlines against `uptime_us()`, and our
                // CLOCK_MONOTONIC == uptime_us (src/syscall/time.rs), so an absolute
                // monotonic deadline is used directly.  This is exactly what Rust std
                // emits for *every* timed wait (Condvar::wait_timeout, park_timeout,
                // Mutex/Once contention): it computes `CLOCK_MONOTONIC::now() + dur`
                // and passes FUTEX_WAIT_BITSET *without* CLOCK_REALTIME.  Treating that
                // already-absolute value as relative (adding uptime again) made every
                // std timed wait sleep ~2x current-uptime — growing the longer the VM
                // runs — which manifested as the rustc "futex deadlock" (see
                // docs/AKUMA_SELF_HOSTING.md §7d).
                if cmd == FUTEX_WAIT_BITSET {
                    if is_realtime {
                        // Absolute CLOCK_REALTIME (wall-clock) deadline.  Convert into
                        // uptime terms so the wait loop's uptime comparison is correct:
                        // remaining = abs_realtime - utc_now; deadline = uptime_now + remaining.
                        match crate::timer::utc_time_us() {
                            Some(utc_now) if timeout_us > utc_now => {
                                crate::timer::uptime_us() + (timeout_us - utc_now)
                            }
                            Some(_) => crate::timer::uptime_us(), // already past → immediate timeout
                            // No wall clock available: fall back to treating the absolute
                            // value as uptime microseconds (imprecise but bounded).
                            None => timeout_us,
                        }
                    } else {
                        // Absolute CLOCK_MONOTONIC deadline == absolute uptime.
                        timeout_us
                    }
                } else {
                    // Plain FUTEX_WAIT: relative timeout.
                    crate::timer::uptime_us() + timeout_us
                }
            } else {
                u64::MAX
            };

            // Main wait loop — handles spurious wakeups from schedule_blocking.
            //
            // We distinguish genuine FUTEX_WAKE from spurious by checking queue
            // membership after schedule_blocking returns:
            //   - NOT in queue → removed by futex_do_wake → genuine wake → return 0
            //   - Still in queue → spurious wakeup → check deadline/signal, retry
            loop {
                akuma_exec::threading::schedule_blocking(deadline);

                // Check whether we were genuinely woken (removed from queue by FUTEX_WAKE).
                let woken_by_futex = crate::irq::with_irqs_disabled(|| {
                    let mut waiters = FUTEX_WAITERS.lock();
                    let in_queue = waiters.get(&key).is_some_and(|q| q.contains(&tid));
                    if in_queue {
                        // Spurious: remove ourselves so we can re-check / re-enqueue below.
                        if let Some(queue) = waiters.get_mut(&key) {
                            queue.retain(|&t| t != tid);
                            if queue.is_empty() { waiters.remove(&key); }
                        }
                    }
                    !in_queue
                });

                // If we were woken by a pending signal, return EINTR (Linux spec).
                if akuma_exec::threading::peek_pending_signal(tid) != 0 {
                    if crate::config::FUTEX_DBG_ENABLED {
                        tprint!(128, "[futex-dbg] WOKE tid={} addr={:#x} result=EINTR ts={}us\n", tid, uaddr, crate::timer::uptime_us());
                    }
                    return EINTR;
                }

                if woken_by_futex {
                    if crate::config::FUTEX_DBG_ENABLED {
                        tprint!(128, "[futex-dbg] WOKE tid={} addr={:#x} result=0 ts={}us\n", tid, uaddr, crate::timer::uptime_us());
                    }
                    return 0;
                }

                // Not a genuine FUTEX_WAKE — check terminal conditions.
                if deadline != u64::MAX && crate::timer::uptime_us() >= deadline {
                    if crate::config::FUTEX_DBG_ENABLED {
                        tprint!(128, "[futex-dbg] WOKE tid={} addr={:#x} result=ETIMEDOUT ts={}us\n", tid, uaddr, crate::timer::uptime_us());
                    }
                    return ETIMEDOUT;
                }

                // Spurious wakeup before deadline.  Re-check the futex value
                // (under the lock to avoid lost-wakeup races) and re-enqueue. A changed
                // value is reported as EAGAIN so the caller re-evaluates its own
                // condition variable.
                if let Err(errno) = futex_check_and_enqueue(key, tid, uaddr, val) {
                    return errno;
                }
            }
        }
        FUTEX_WAKE | FUTEX_WAKE_BITSET => {
            let tgid = futex_key_tgid(is_private);
            let woken = futex_do_wake(tgid, uaddr, val);
            if crate::config::FUTEX_DBG_ENABLED {
                tprint!(128, "[futex-dbg] WAKE addr={:#x} max={} woken={} ts={}us\n", uaddr, val, woken, crate::timer::uptime_us());
            }
            woken
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

            let (to_wake, requeued) = futex_requeue_table(key1, key2, val, max_requeue);
            let woken = to_wake.len();

            for tid in &to_wake {
                akuma_exec::threading::get_waker_for_thread(*tid).wake();
            }

            if crate::config::FUTEX_DBG_ENABLED {
                tprint!(128, "[futex-dbg] REQUEUE addr={:#x} addr2={:#x} woken={} requeued={} ts={}us\n", uaddr, uaddr2, woken, requeued, crate::timer::uptime_us());
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
            if unsafe { copy_from_user_safe((&raw mut current_val).cast::<u8>(), uaddr as *const u8, 4).is_err() } {
                return EFAULT;
            }
            if current_val != val3 {
                return EAGAIN;
            }

            if uaddr2 != 0 && !validate_user_ptr(uaddr2 as u64, 4) {
                return EFAULT;
            }

            let (to_wake, requeued) = futex_requeue_table(key1, key2, val, max_requeue);
            let woken = to_wake.len();

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
            // §7k investigation: a corrupt futex op (e.g. -1) reaching here means the
            // op register (x1) held garbage at the `svc`. Dump the user instruction
            // stream at the syscall so a recurrence tells us WHICH mechanism:
            //   - `svc #0` at [elr-4] AND a sane `mov w1,#<op>` just before, yet op is
            //     garbage  → the register was corrupted after it was set (preemption /
            //     context-switch save-restore bug);
            //   - garbage/wrong instruction at [elr-4]/[elr-8] → stale I-cache mis-decode.
            // ELR (trap frame) points just past the `svc`. Cheap; only the rare
            // corruption path hits it.
            if let Some(elr) = akuma_exec::threading::current_trap_frame_elr() {
                let mut buf = [0u8; 12];
                let read_ok = unsafe {
                    akuma_exec::mmu::user_access::copy_from_user_safe(
                        buf.as_mut_ptr(),
                        elr.wrapping_sub(8) as *const u8,
                        12,
                    )
                    .is_ok()
                };
                if read_ok {
                    let pre = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]); // elr-8
                    let svc = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]); // elr-4
                    let nxt = u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]); // elr
                    let tid = akuma_exec::threading::current_thread_id();
                    crate::safe_print!(
                        224,
                        "[futex-diag] tid={} elr={:#x} op={:#x} uaddr={:#x} val={} val3={} insn[-8]={:#010x} insn[-4]={:#010x}({}) insn[0]={:#010x}\n",
                        tid, elr, op as u32, uaddr, val, val3, pre, svc,
                        if svc == 0xd400_0001 { "svc#0" } else { "NOT-SVC" }, nxt,
                    );
                } else {
                    crate::safe_print!(96, "[futex-diag] elr={:#x} user read failed\n", elr);
                }
            }
            ENOSYS
        }
    }
}
