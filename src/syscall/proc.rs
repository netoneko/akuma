use super::*;

// Maps child PID → the parent thread's generation-tagged WakeHandle for processes
// created with CLONE_VFORK. The parent blocks after fork until the child calls
// execve (success) or exits. Without this, both parent and child run the Go
// runtime concurrently in the same address space, causing memory corruption.
// A WakeHandle rather than a bare tid: the parent registers itself (live by
// definition), and if it is killed and its slot recycled before the child ever
// execs, the eventual vfork_complete wakes nobody instead of the new occupant.
static VFORK_WAITERS: Spinlock<BTreeMap<u32, akuma_exec::threading::WakeHandle>> =
    Spinlock::new(BTreeMap::new());

/// Mark the child channel as exited so `pidfd_can_read` and `find_exited_child`
/// return immediately, AND raise SIGCHLD on the parent.  Called from
/// `sys_exit` / `sys_exit_group` — before `return_to_kernel` runs — so the
/// parent's epoll + pidfd / wait4 path sees the exit without a 10ms polling
/// delay, and a parent parked in `sigsuspend` (busybox ash `wait`) is woken.
/// `set_exited` is idempotent; the second call from `return_to_kernel` is a
/// no-op for SIGCHLD purposes (the `has_exited` guard in `publish_child_exit`
/// suppresses the duplicate signal).
fn notify_child_channel_exited(pid: u32, code: i32) {
    akuma_exec::process::publish_child_exit(pid, code);
}

/// Public wrapper for crash paths in exceptions.rs that need to notify the
/// parent before `return_to_kernel` runs.  Idempotent — the second call from
/// `return_to_kernel::remove_channel` is harmless.
pub fn notify_child_channel_exited_pub(pid: u32, code: i32) {
    notify_child_channel_exited(pid, code);
}

/// Wake the vfork parent (if any) of the given child PID.
/// Called from do_execve (on successful image replacement), sys_exit_group/sys_exit,
/// and fault exit paths in exceptions.rs.
pub fn vfork_complete(child_pid: u32) {
    let parent = crate::irq::with_irqs_disabled(|| {
        VFORK_WAITERS.lock().remove(&child_pid)
    });
    if let Some(handle) = parent {
        akuma_exec::threading::wake_by_handle(handle);
    }
}

/// Number of entries currently in VFORK_WAITERS.  Used only by kernel tests.
#[cfg(kernel_tests)]
pub fn vfork_waiters_len() -> usize {
    crate::irq::with_irqs_disabled(|| VFORK_WAITERS.lock().len())
}

/// Kernel test helper: insert a fake pending vfork for `child_pid`, invoke
/// `vfork_complete`, and return whether the entry was cleanly removed.
#[cfg(kernel_tests)]
pub fn test_vfork_complete_mechanism(child_pid: u32) -> bool {
    crate::irq::with_irqs_disabled(|| {
        VFORK_WAITERS.lock().insert(child_pid, akuma_exec::threading::current_wake_handle());
    });
    vfork_complete(child_pid);
    let still_present = crate::irq::with_irqs_disabled(|| {
        VFORK_WAITERS.lock().contains_key(&child_pid)
    });
    !still_present
}

/// Kernel test helper: insert a fake vfork entry without invoking vfork_complete.
#[cfg(kernel_tests)]
pub fn vfork_waiters_insert_for_test(child_pid: u32) {
    VFORK_WAITERS.lock().insert(child_pid, akuma_exec::threading::current_wake_handle());
}

/// Kernel test helper: check whether a child PID is still in VFORK_WAITERS.
#[cfg(kernel_tests)]
pub fn vfork_waiters_contains_for_test(child_pid: u32) -> bool {
    VFORK_WAITERS.lock().contains_key(&child_pid)
}

/// Linux `wait*status`: normal exit is `(code & 0xff) << 8` (WIFEXITED / WEXITSTATUS).
/// Negative `code` is treated as stopped-by-signal: low 7 bits = signal number.
pub fn encode_wait_status(code: i32) -> u32 {
    if code < 0 {
        
        (-code) as u32 & 0x7F
    } else {
        ((code as u32) & 0xFF) << 8
    }
}

pub(super) fn sys_set_tpidr_el0(address: u64) -> u64 {
    unsafe {
        core::arch::asm!("msr tpidr_el0, {}", "isb", in(reg) address);
    }
    0
}

pub(super) fn sys_setpgid(pid: u32, pgid: u32) -> u64 {
    let target_pid = if pid == 0 {
        match akuma_exec::process::read_current_pid() { Some(p) => p, None => return ESRCH }
    } else {
        pid
    };

    let target_pgid = if pgid == 0 { target_pid } else { pgid };

    if let Some(old_pgid) = akuma_exec::process::with_process(target_pid, |p| {
        let old = p.pgid;
        p.pgid = target_pgid;
        old
    }) {
        if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
            crate::safe_print!(128, "[syscall] setpgid(pid={}, pgid={}): old={}, new={}\n", target_pid, pgid, old_pgid, target_pgid);
        }
        0
    } else {
        // Linux: setpgid(2) returns ESRCH for a pid that is not a child of the
        // caller or the caller itself (no such process). ENOENT here was wrong
        // — userspace expects "no such process".
        ESRCH
    }
}

pub(super) fn sys_getpgid(pid: u32) -> u64 {
    let target_pid = if pid == 0 {
        match akuma_exec::process::read_current_pid() {
            Some(p) => p,
            // Caller has no registered process; matches Linux behavior of
            // returning ESRCH for an unknown pid rather than a bogus TID/PGID.
            None => return ESRCH,
        }
    } else {
        pid
    };

    if let Some(proc) = akuma_exec::process::lookup_process_shared(target_pid) {
        if crate::config::SYSCALL_DEBUG_INFO_ENABLED && pid == 0 {
            crate::safe_print!(128, "[syscall] getpgid(0) for PID {}: returning PGID {}\n", target_pid, proc.pgid);
        }
        u64::from(proc.pgid)
    } else {
        if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
            crate::safe_print!(128, "[syscall] getpgid({}) not found: ESRCH\n", target_pid);
        }
        ESRCH
    }
}

pub(super) fn sys_setsid() -> u64 {
    if let Some(pid) = akuma_exec::process::with_current_process(|p| {
        p.pgid = p.pid;
        p.pid
    }) {
        u64::from(pid)
    } else {
        // Per setsid(2): EPERM if the calling process is already a process
        // group leader. Here the caller has no current process at all, so
        // ESRCH is the most accurate Linux-compatible code.
        ESRCH
    }
}

/// One `utsname` field. The Linux ABI is six of these, NUL-padded.
const UTS_FIELD_LEN: usize = 65;
/// The whole `struct utsname`: 390 bytes, of which ~30 are ever non-zero. That
/// is the ABI's shape, not this kernel's — Linux's `new_utsname` is the same
/// size and copies the same 390 bytes.
const UTS_LEN: usize = UTS_FIELD_LEN * 6;

/// Write one NUL-padded field into a `utsname` image at compile time.
///
/// A `const fn` taking and returning the array rather than a `&mut` one,
/// because that is what `static` initialization can use. Hand-rolled byte loop
/// because `copy_from_slice` is not `const`.
const fn uts_field(mut buf: [u8; UTS_LEN], index: usize, value: &[u8]) -> [u8; UTS_LEN] {
    let start = index * UTS_FIELD_LEN;
    // Leave at least one byte for the terminator, exactly as the runtime
    // version did — a field that filled all 65 bytes would come back unterminated.
    let max = UTS_FIELD_LEN - 1;
    let len = if value.len() < max { value.len() } else { max };
    let mut i = 0;
    while i < len {
        buf[start + i] = value[i];
        i += 1;
    }
    buf
}

/// The answer `uname(2)` gives, built once at compile time and copied out
/// verbatim.
///
/// Every field is a compile-time constant — two literals, `CARGO_PKG_VERSION`,
/// and the `<git-sha>-<profile>` identity `build.rs` embeds — so there has
/// never been anything to compute per call. It used to be assembled anyway:
/// a 390-byte stack memset plus six `copy_from_slice`s on every `uname`, and
/// then a second 390-byte pass into userspace. That is ~780 bytes moved and a
/// 390-byte stack frame to deliver ~30 bytes of static text.
///
/// As a `static` it lives in `.rodata` and the syscall is one `copy_to_user`,
/// which is what Linux does (it copies straight out of `init_uts_ns.name`).
///
/// `release` tracks the kernel crate version, and `version` carries the build
/// identity `<git-sha>-<profile>` (e.g. `a1b2c3d-release-smp-shared`) — enough
/// for `uname -a` to say which commit and build target is running. See
/// docs/archive/UNAME.md. sysname/nodename/domainname stay static literals:
/// sethostname/setdomainname are not wired into the dispatch table, so there is
/// no write path for them to track — which is also what makes baking the whole
/// image into `.rodata` correct rather than merely cheaper.
static UTSNAME: [u8; UTS_LEN] = {
    let b = [0u8; UTS_LEN];
    let b = uts_field(b, 0, b"Akuma");
    let b = uts_field(b, 1, b"akuma");
    let b = uts_field(b, 2, env!("CARGO_PKG_VERSION").as_bytes());
    let b = uts_field(
        b,
        3,
        concat!(env!("AKUMA_GIT_SHA"), "-", env!("AKUMA_BUILD_PROFILE")).as_bytes(),
    );
    let b = uts_field(b, 4, b"aarch64");
    uts_field(b, 5, b"(none)")
};

pub(super) fn sys_uname(buf: u64) -> u64 {
    if !validate_user_ptr(buf, UTS_LEN) { return EFAULT; }
    if copy_to_user(buf, &UTSNAME).is_err() {
        return EFAULT;
    }
    0
}

/// set_tid_address(2) — record the clear_child_tid futex address and return the
/// caller's **TID**.
///
/// Returning the PID here is not harmless bookkeeping: musl's `__init_tp` caches
/// this value in the initial thread's `pthread_self()->tid`, and every later
/// `raise`/`pthread_kill`/`abort` passes it to `tkill`, which indexes the
/// per-thread arrays by kernel thread slot. A PID sent a self-signal to whatever
/// unrelated thread happened to occupy that slot. Same namespace rule as
/// `clone_thread`'s CLONE_PARENT_SETTID write and `gettid()`.
pub(super) fn sys_set_tid_address(tidptr: u64) -> u64 {
    akuma_exec::process::with_current_process(|p| {
        p.clear_child_tid = tidptr;
    });
    akuma_exec::threading::current_thread_id() as u64
}

pub(super) fn sys_set_robust_list(head: u64, len: usize) -> u64 {
    if len != 24 { return EINVAL; }
    if akuma_exec::process::with_current_process(|p| {
        p.robust_list_head = head;
        p.robust_list_len = len;
    }).is_some() {
        return 0;
    }
    ENOSYS
}

pub(super) fn sys_exit(code: i32) -> u64 {
    if let Some(proc) = akuma_exec::process::current_process_shared() {
        if crate::config::FUTEX_DBG_ENABLED {
            crate::tprint!(96, "[exit93] tid={} pid={} tgid={}\n",
                akuma_exec::threading::current_thread_id(), proc.pid, proc.tgid);
        }
        if crate::config::SYSCALL_DEBUG_NET_ENABLED {
            let elapsed_us = crate::timer::uptime_us().saturating_sub(proc.start_time_us);
            let secs = elapsed_us / 1_000_000;
            let frac = (elapsed_us % 1_000_000) / 10_000;
            crate::tprint!(128, "[exit] tid={} pid={} name={} code={} after {}.{:02}s\n",
                akuma_exec::threading::current_thread_id(), proc.pid, proc.name, code, secs, frac);
        }
        let pid = proc.pid;
        let proc_tid = proc.thread_id;
        akuma_exec::process::with_current_process(|p| {
            p.exited = true;
            p.exit_code = code;
            p.state = akuma_exec::process::ProcessState::Zombie(code);
        });
        if crate::config::PROC_SYSCALL_LOG_ENABLED {
            crate::syscall::log::mark_exited(pid);
        }
        // Close all fds NOW so the SharedFdTable is empty before the thread
        // terminates.  on_thread_cleanup → unregister_process → Box drop runs
        // in scheduler context; if close_all runs there, it can deadlock.
        //
        // CLONE_THREAD (pthread) siblings share the parent's Arc<FdTable>.
        // Calling close_all() here would drain the shared table and close
        // every pipe and socket visible to the entire thread group — observed
        // as git's sideband thread destroying all of fetch-pack's pipes on
        // exit, so git-index-pack never receives pack data.  On Linux,
        // sys_exit() for a non-leader thread must NOT close the shared FD
        // table; only sys_exit_group() (or the last thread) does that.
        if proc.tgid == proc.pid {
            // Flush any writable MAP_SHARED file mappings to disk before teardown,
            // so a process that exits without munmap still persists its writes.
            super::mem::flush_and_clear_shared_file_mappings(proc.tgid);
            proc.fds.close_all();
        }
        // CLONE_CHILD_CLEARTID: write 0 to the TID address and wake any
        // pthread_join waiters.  This must happen here, while the user address
        // space is still active, because return_to_kernel is never reached from
        // the sys_exit path (the thread loops in yield_now instead of returning
        // through the normal EL0→EL1→EL0 trampoline).
        let tid_addr = proc.clear_child_tid;
        if tid_addr != 0 {
            let mapped = crate::mmu::is_current_user_page_mapped(tid_addr as usize);
            if crate::config::FUTEX_DBG_ENABLED {
                tprint!(160, "[cct-exit] pid={} tgid={} tid_addr={:#x} mapped={} (sys_exit)\n",
                    pid, proc.tgid, tid_addr, mapped);
            }
            if mapped {
                unsafe { core::ptr::write(tid_addr as *mut u32, 0); }
            }
            crate::syscall::futex_wake(proc.tgid, tid_addr as usize, i32::MAX);
        }

        notify_child_channel_exited(pid, code);
        vfork_complete(pid);

        // Terminate the calling thread — exit() must never return to EL0.
        // Only do this if the calling thread IS the process's own thread.
        let tid = akuma_exec::threading::current_thread_id();
        if proc_tid == Some(tid) {
            if let Some(io_ch) = akuma_exec::process::get_channel(tid) {
                io_ch.set_exited(code);
            }
            // Do NOT unregister_process — leave as zombie for wait4.
            akuma_exec::threading::mark_thread_terminated(tid);
            drain_retired_before_parking();
            loop { akuma_exec::threading::yield_now(); }
        }
    }
    code as u64
}

/// Pressure-driven reclaim of RETIRED processes, from the terminal park of `sys_exit`
/// and `sys_exit_group`.
///
/// This is `process::reclaim`'s teardown drain site for the path userspace actually
/// takes. `return_to_kernel` has the same call, but a process that calls `exit_group`
/// (every musl `exit()`, i.e. nearly every clean exit) never gets there: it marks its
/// own thread terminated and parks in the `yield_now` loop below, so without this the
/// most common process exit in the system collected nothing.
///
/// Safe here for the same reasons as the `return_to_kernel` site: fds are already
/// closed, the thread group is torn down, the parent has been notified, no lifecycle
/// guard is held (neither exit path takes one), and the only lock we hold is the BKL —
/// which sits above every drop-path lock in the order, exactly like the `netpoll_maint`
/// collector's context. The calling process is still an ACTIVE zombie awaiting `wait4`,
/// not RETIRED, so this can never free the address space we are standing on.
#[inline]
fn drain_retired_before_parking() {
    akuma_exec::process::reclaim::drain_retired_if_requested();
}

/// Public wrapper for sys_exit_group, callable from exception handlers.
/// Used when a fatal signal (SIGSEGV) in a clone_thread needs to kill
/// the entire thread group.
pub fn sys_exit_group_pub(code: i32) -> ! {
    sys_exit_group(code);
    // sys_exit_group should not return for the calling thread, but if it does
    // (e.g., called from a kernel helper thread), fall through to return_to_kernel.
    akuma_exec::process::return_to_kernel(code);
}

pub(super) fn sys_exit_group(code: i32) -> u64 {
    if let Some(proc) = akuma_exec::process::current_process_shared() {
        if crate::config::FUTEX_DBG_ENABLED {
            crate::tprint!(96, "[exit94] tid={} pid={} tgid={}\n",
                akuma_exec::threading::current_thread_id(), proc.pid, proc.tgid);
        }
        if crate::config::SYSCALL_DEBUG_NET_ENABLED {
            let elapsed_us = crate::timer::uptime_us().saturating_sub(proc.start_time_us);
            let secs = elapsed_us / 1_000_000;
            let frac = (elapsed_us % 1_000_000) / 10_000;
            crate::tprint!(128, "[exit_group] pid={} name={} code={} after {}.{:02}s\n",
                proc.pid, proc.name, code, secs, frac);
        }
        // Gated 2026-08-29. It was unconditional for the reason below, and that
        // reason has aged out: the J4 investigation is archived, while the line
        // fires once per process exit — 262 in a plain boot-suite run, thousands
        // in an in-VM build, at ~160 bytes and ~2.4 us/byte. Turn
        // `syscall-debug-info` on to get it back for a rerun of that hunt.
        //
        // Was: unconditional (not gated by SYSCALL_DEBUG_NET_ENABLED): correlates with
        // the `[syscall] execve(path=..., args=...)` line by pid to answer "did the
        // linker's own exit_group report success" for the truncated-linker-output
        // investigation — see
        // docs/archive/J4_WRITE_PERM_FAULT_AND_HALF_WRITTEN_LINKER_OUTPUT.md §4.
        if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
            crate::tprint!(160, "[PROC-EXIT] pid={} tgid={} name={} code={}\n",
                proc.pid, proc.tgid, proc.name, code);
        }
        let pid = proc.pid;
        let tgid = proc.tgid;
        let proc_tid = proc.thread_id;
        let l0_phys = proc.address_space.l0_phys();
        akuma_exec::process::with_current_process(|p| {
            p.exited = true;
            p.exit_code = code;
            p.state = akuma_exec::process::ProcessState::Zombie(code);
        });
        if crate::config::PROC_SYSCALL_LOG_ENABLED {
            crate::syscall::log::mark_exited(pid);
        }
        // Flush writable MAP_SHARED file mappings to disk while the address space
        // is still intact (kill_thread_group below tears it down).
        super::mem::flush_and_clear_shared_file_mappings(tgid);
        // Reap sibling threads BEFORE notifying the parent.
        //
        // ORDERING IS LOAD-BEARING: notify_child_channel_exited() wakes the parent's
        // wait4, which on a single core can immediately preempt us and reap THIS
        // process (unregister_process), terminating the calling thread before it
        // ever reaches kill_thread_group. Any sibling worker parked in FUTEX_WAIT
        // (e.g. rustc's rayon pool) would then be orphaned — never terminated, never
        // woken — hanging forever and keeping the process alive so the build driver
        // (cargo) blocks indefinitely. This was the in-VM self-host deadlock
        // (docs §7g). Killing the thread group first guarantees every sibling is
        // terminated + woken regardless of when the parent runs.
        akuma_exec::process::kill_thread_group(pid, l0_phys, code);
        // Close all fds immediately so pipe write-ends are decremented and
        // epoll pollers (e.g. Go's parent waiting for compile stdout EOF) are
        // woken now. close_all() is idempotent — cleanup_process_fds() later
        // will find an empty table and skip any double-close.
        //
        // ORDERING IS LOAD-BEARING, for the same reason as kill_thread_group above:
        // this MUST happen before notify_child_channel_exited(). That notify wakes the
        // parent's wait4, which can immediately reap this process on the peer core;
        // `unregister_process` then calls `mark_thread_terminated` on *this* thread, so
        // a reap that lands mid-syscall stops us dead before we release our own fds.
        //
        // That race was survivable only by accident until Phase 7e: the reap used to
        // free the `Process` synchronously, and `SharedFdTable`'s Drop closes the fd
        // table, so the reaper released them on our behalf. Phase 7e defers that drop
        // to `reclaim_retired_processes`, which during the boot self-test suite never
        // runs at all (it is wired into `netpoll_maint`) — so losing the race meant the
        // fds were held indefinitely. Concretely, in `yes | head -n 1`: `head`'s pipe
        // read end was never released, `yes` never saw read_count reach 0, never got
        // EPIPE/SIGPIPE, and slept forever on a full pipe
        // (`test_sigpipe_terminate_no_deadlock`). Closing first makes releasing our own
        // fds independent of who wins the reap race.
        proc.fds.close_all();
        notify_child_channel_exited(pid, code);
        // If a goroutine thread (tgid != pid) is calling exit_group, the parent's
        // wait4 waits on CHILD_CHANNELS[tgid], not CHILD_CHANNELS[pid].  Notify
        // the tgid leader's channel too so the parent doesn't hang.
        if tgid != pid {
            notify_child_channel_exited(tgid, code);
        }
        vfork_complete(pid);

        // Terminate the calling thread — exit_group() must never return to EL0.
        // Only do this if the calling thread IS the process's own thread;
        // kernel helpers must NOT be terminated.
        let tid = akuma_exec::threading::current_thread_id();
        if proc_tid == Some(tid) {
            if let Some(io_ch) = akuma_exec::process::get_channel(tid) {
                io_ch.set_exited(code);
            }
            akuma_exec::threading::mark_thread_terminated(tid);
            drain_retired_before_parking();
            // Yield to trigger scheduler. Once terminated, we should never run again.
            loop { akuma_exec::threading::yield_now(); }
        }
    }
    code as u64
}

pub(super) fn sys_clone(flags: u64, stack: u64, parent_tid: u64, tls: u64, child_tid: u64) -> u64 {
    sys_clone_pidfd(flags, stack, parent_tid, tls, child_tid, 0)
}

/// Internal clone implementation that optionally writes a pidfd to `pidfd_out_ptr`.
/// `pidfd_out_ptr = 0` means no pidfd requested (used by sys_clone).
pub(super) fn sys_clone_pidfd(flags: u64, stack: u64, parent_tid: u64, tls: u64, child_tid: u64, pidfd_out_ptr: u64) -> u64 {
    use akuma_syscalls_linux::proc::clone_flags::{CLONE_THREAD, CLONE_VFORK, CLONE_VM};
    #[cfg(feature = "sc-pidfd")]
    use akuma_syscalls_linux::proc::clone_flags::CLONE_PIDFD;
    // pidfd_out_ptr is only consumed by the CLONE_PIDFD block below, which is
    // gated out when pidfd support is compiled away.
    #[cfg(not(feature = "sc-pidfd"))]
    let _ = pidfd_out_ptr;

    if crate::config::SYSCALL_DEBUG_INFO_ENABLED || crate::config::SYSCALL_DEBUG_NET_ENABLED {
        let tid = akuma_exec::threading::current_thread_id();
        let pid = akuma_exec::process::read_current_pid().unwrap_or(0);
        crate::tprint!(128, "[clone] tid={} pid={} flags=0x{:x} stack=0x{:x}\n", tid, pid, flags, stack);
    }

    // Bits 32+ are unused by any valid clone flag.  Garbage values like
    // -ENOSYS (0xffffffffffffffda) or -EAGAIN (0xfffffffffffffff5) have them
    // set and coincidentally match CLONE_THREAD|CLONE_VM, causing an infinite
    // error→retry loop.  Reject early.
    if flags >> 32 != 0 {
        return ENOSYS;
    }

    if flags & CLONE_THREAD != 0 && flags & CLONE_VM != 0 {
        // POSIX: a new thread inherits the creating thread's (per-thread) signal mask.
        let parent_mask = akuma_exec::threading::thread_signal_mask();
        match akuma_exec::process::clone_thread(stack, tls, parent_tid, child_tid, flags) {
            Ok(tid) => {
                akuma_exec::threading::seed_thread_signal_mask(tid as usize, parent_mask);
                if crate::config::SYSCALL_DEBUG_NET_ENABLED {
                    crate::tprint!(64, "[clone] new thread TID={}\n", tid);
                }
                return u64::from(tid);
            }
            Err(e) => {
                crate::safe_print!(128, "[syscall] clone_thread failed: {}\n", e);
                return EAGAIN;
            }
        }
    }

    // Route to fork_process for known fork-like flag combinations:
    //   - CLONE_VFORK (with or without CLONE_VM / SIGCHLD)
    //   - SIGCHLD (0x11) in the low 8 bits (standard fork)
    //
    // Other flag combos (including flags=0) return ENOSYS.  Go's runtime
    // may accidentally call clone(0) due to register-state leakage in the
    // vfork child; returning ENOSYS allows Go's error handling to continue
    // to the next syscall.  Routing clone(0) to fork_process creates a
    // fork bomb: each fork child runs the Go scheduler → newosproc → clone.
    if flags & CLONE_VFORK != 0 || flags & 0xFF == 0x11 {
        let parent_proc = match akuma_exec::process::current_process_shared() {
            Some(p) => p,
            None => return ESRCH,
        };

        let child_pid = akuma_exec::process::allocate_pid();

        if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
            crate::safe_print!(128, "[syscall] clone: forking PID {} -> {} (flags=0x{:x})\n", parent_proc.pid, child_pid, flags);
        }

        // CLONE_VFORK: register the parent TID in VFORK_WAITERS *before* fork_process
        // marks the child thread READY.  If we insert after fork_process returns, there
        // is a race window where the child can exec, call vfork_complete (which removes
        // the entry), and find nothing — leaving the parent blocked forever.
        if flags & CLONE_VFORK != 0 {
            crate::irq::with_irqs_disabled(|| {
                VFORK_WAITERS.lock().insert(child_pid, akuma_exec::threading::current_wake_handle());
            });
        }

        // vfork fast-path (docs/COW_OPTIMIZATIONS.md Fix B): a CLONE_VFORK child
        // shares the parent's address space instead of replicating it.  Only
        // safe for CLONE_VFORK (the parent blocks below until the child
        // execs/_exits) — plain SIGCHLD fork (0x11) runs concurrently and must
        // use the full copy.  Gated by config so it can be toggled off.
        let use_vfork_fastpath = flags & CLONE_VFORK != 0
            && akuma_exec::runtime::config().vfork_fastpath_enabled;
        let fork_result = if use_vfork_fastpath {
            akuma_exec::process::vfork_process(child_pid, stack)
        } else {
            akuma_exec::process::fork_process(child_pid, stack)
        };
        match fork_result {
            Ok(new_pid) => {
                // POSIX/Linux: a fork() child inherits the parent's signal mask. Only the
                // CLONE_THREAD path above seeded it, so a forked child started with mask 0
                // — everything UNBLOCKED — while `claim_free_slot`/`scrub_thread_slot`
                // deliberately zero the slot's mask on reuse.
                //
                // That gap is load-bearing for `Command::spawn`: the runtime blocks every
                // signal in the parent immediately before forking precisely so the child
                // cannot take one in the pre-exec window, where its handler state and
                // sigaltstack are not yet valid (`[signal] sig N needs sigaltstack but slot
                // M has none — re-pending`). Starting that child fully unblocked reopens
                // exactly the window the caller paid a syscall to close.
                let parent_mask = akuma_exec::threading::thread_signal_mask();
                if let Some(Some(child_tid)) =
                    akuma_exec::process::with_process(new_pid, |p| p.thread_id)
                {
                    akuma_exec::threading::seed_thread_signal_mask(child_tid, parent_mask);
                }
                // CLONE_PIDFD: atomically create a pidfd for the child and write the fd number
                // back to the caller. Go 1.22+ uses this to get the pidfd in a single syscall.
                #[cfg(feature = "sc-pidfd")]
                if flags & CLONE_PIDFD != 0 && pidfd_out_ptr != 0
                    && validate_user_ptr(pidfd_out_ptr, 4) {
                        let pidfd_fd = super::pidfd::sys_pidfd_open(new_pid, 0 /* no flags */);
                        if (pidfd_fd as i64) >= 0 {
                            // Linux clone3 with CLONE_PIDFD always sets O_CLOEXEC.
                            if let Some(proc) = akuma_exec::process::current_process_shared() {
                                proc.set_cloexec(pidfd_fd as u32);
                            }
                            let fd_i32 = pidfd_fd as i32;
                            let _ = write_user_val(pidfd_out_ptr, &fd_i32);
                            crate::tprint!(96, "[clone] CLONE_PIDFD: child={} pidfd={}\n", new_pid, pidfd_fd);
                        }
                    }
                // CLONE_VFORK: block parent until child calls execve or exits.
                // Without this, both parent and child run the Go runtime concurrently
                // in the same address space, corrupting each other's state.
                // Note: VFORK_WAITERS was already populated above, before fork_process.
                if flags & CLONE_VFORK != 0 {
                    // Loop to absorb spurious wakeups caused by signal delivery.
                    // pend_signal_for_thread() calls wake() which sets the sticky
                    // WOKEN_STATES flag, making schedule_blocking() return even though
                    // the child hasn't called execve/exit yet.  Re-block until
                    // vfork_complete() removes the VFORK_WAITERS entry.
                    loop {
                        akuma_exec::threading::schedule_blocking(u64::MAX);
                        let still_pending = crate::irq::with_irqs_disabled(|| {
                            VFORK_WAITERS.lock().contains_key(&new_pid)
                        });
                        if !still_pending { break; }
                    }
                }
                return u64::from(new_pid);
            },
            Err(e) => {
                // Fork failed: clean up the VFORK_WAITERS entry we pre-inserted.
                if flags & CLONE_VFORK != 0 {
                    crate::irq::with_irqs_disabled(|| {
                        VFORK_WAITERS.lock().remove(&child_pid);
                    });
                }
                crate::safe_print!(128, "[syscall] clone: fork failed: {}\n", e);
                return ENOMEM;
            }
        }
    }

    if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
        crate::safe_print!(128, "[syscall] clone: flags=0x{:x} not supported, returning ENOSYS\n", flags);
    }
    ENOSYS
}

pub(super) fn sys_clone3(cl_args_ptr: u64, size: usize) -> u64 {
    // `struct clone_args` is `akuma_syscalls_linux::CloneArgs` since
    // 2026-08-27, reached here through `use super::*`. The short-copy contract
    // that makes its field order unchangeable has a host test there.
    let struct_size = size.min(core::mem::size_of::<CloneArgs>());
    let mut cl_args = CloneArgs::default();
    if copy_from_user(
        &mut as_user_bytes_mut(core::slice::from_mut(&mut cl_args))[..struct_size],
        cl_args_ptr,
    )
    .is_err()
    {
        return EFAULT;
    }

    let flags = cl_args.flags | cl_args.exit_signal;
    let stack = if cl_args.stack != 0 {
        cl_args.stack + cl_args.stack_size
    } else {
        0
    };

    if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
        crate::tprint!(128, "[syscall] clone3(flags=0x{:x}, stack=0x{:x})\n", flags, stack);
    }

    // Pass the pidfd pointer through so CLONE_PIDFD can write the fd number back
    // to the caller's clone_args.pidfd field.
    sys_clone_pidfd(flags, stack, cl_args.parent_tid, cl_args.tls, cl_args.child_tid, cl_args.pidfd)
}

pub(super) fn sys_execve(path_ptr: u64, argv_ptr: u64, envp_ptr: u64) -> u64 {
    let pid = akuma_exec::process::read_current_pid().unwrap_or(0);
    let path = match copy_from_user_str(path_ptr, 1024) {
        Ok(p) => p,
        Err(e) => {
            crate::safe_print!(64, "[syscall] execve: path copy failed with {} pid={}\n", e as i64, pid);
            return e;
        },
    };
    if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
        crate::tprint!(128, "[syscall] execve(path=\"{}\", argv_ptr=0x{:x}, envp_ptr=0x{:x}) PID {}\n", path, argv_ptr, envp_ptr, pid);
    }

    let resolved_path = if path.starts_with('/') {
        path
    } else if let Some(proc) = akuma_exec::process::current_process_shared() {
        crate::vfs::resolve_path(&proc.cwd, &path)
    } else {
        path
    };
    let resolved_path = crate::vfs::resolve_symlinks(&resolved_path);

    let mut args = Vec::new();
    if argv_ptr != 0 {
        let mut i = 0;
        loop {
            let mut str_ptr: u64 = 0;
            if read_user_into(&mut str_ptr, argv_ptr + i * 8).is_err() {
                break;
            }
            if str_ptr == 0 { break; }
            // Fail the whole execve (E2BIG, Linux semantics) if an argument can't
            // be copied — most often because it exceeds MAX_ARG_STRLEN. Previously
            // this `break`d, which silently dropped the over-long arg AND every arg
            // after it, then exec'd a corrupt argv (e.g. rustc saw `--check-cfg`
            // with its giant value truncated away → "Argument to option missing").
            if let Ok(s) = copy_from_user_str(str_ptr, crate::config::MAX_ARG_STRLEN) { args.push(s) } else {
                crate::safe_print!(96, "[syscall] execve: argv[{}] too long or unreadable (cap={}) — E2BIG\n",
                    i, crate::config::MAX_ARG_STRLEN);
                return E2BIG;
            }
            i += 1;
        }
    }

    let env = parse_argv_array(envp_ptr);

    // Gated 2026-08-30, same reasoning as the `[PROC-EXIT]` line above: it was
    // unconditional for the reason below, and that reason has aged out. The line
    // fires once per `execve` at up to 2048 bytes — a `cargo build` in the guest
    // prints a full rustc/ld command line per compilation unit, which is what
    // makes the serial log unreadable during an in-VM build. Turn
    // `syscall-debug-info` on to get it back (that also re-enables the
    // pre-resolution `argv_ptr`/`envp_ptr` line above, and is what
    // `scripts/bkl_smp_regimen/analyze_workload.py` now needs).
    //
    // Was: unconditional. 192 bytes truncated a linker's full argv (output path
    // included) well before the interesting part — see
    // docs/archive/J4_WRITE_PERM_FAULT_AND_HALF_WRITTEN_LINKER_OUTPUT.md §4.5,
    // "no `ld` line names the crate proves nothing". Widened so the `-o <output>`
    // argument survives for `cc`/`collect2`/`ld` invocations.
    if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
        crate::tprint!(2048, "[syscall] execve(path=\"{}\", args={:?}) PID {}\n", resolved_path, args, pid);
    }

    do_execve(resolved_path, args, env)
}

pub fn do_execve(resolved_path: String, args: Vec<String>, env: Vec<String>) -> u64 {
    // On the size profile the heap seed is 1 MB. Reading a large binary
    // (e.g. the 700+ KB system linker that tcc invokes) would exhaust it.
    // Read just the first 256 bytes — enough for shebang detection — and
    // use the path-based loader for the actual ELF.
    #[cfg(kernel_profile_extreme)]
    let mut file_data: Option<alloc::vec::Vec<u8>> = {
        let mut head = alloc::vec![0u8; 256];
        match crate::fs::read_at(&resolved_path, 0, &mut head) {
            Ok(n) => { head.truncate(n); Some(head) }
            Err(crate::vfs::FsError::Internal) => None,
            Err(e) => {
                // Gated 2026-08-30: this is the PATH search, not an error. A shell
                // or cargo resolving `cc`/`rustc` probes every PATH entry in turn,
                // so a miss per entry is the NORMAL case and the flood scales with
                // PATH length x exec count. A genuinely unreadable binary still
                // surfaces as the execve's errno to the caller.
                if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
                    crate::safe_print!(128, "[syscall] execve: failed to read {}\n", resolved_path);
                }
                return super::fs::fs_error_to_errno(e);
            }
        }
    };
    #[cfg(not(kernel_profile_extreme))]
    let mut file_data = {
        // M5c hold-shortening: DROP the BKL around the whole-file ELF read so peer cores can
        // enter the kernel while this core waits on disk (execve's dominant BKL-held window).
        // Safe: this runs before `replace_image` touches the process, and the read goes only
        // through the VFS/ext2/block locks (all BKL-independent) — the same profile as the
        // proven file-fault drop. The dropped-window ledger keeps the window BKL-free across
        // timer ticks (a bare leave/enter pair let the first tick re-hold the BKL for the
        // rest of the read); the syscall wrapper's leave_kernel still balances it.
        #[cfg(kernel_smp_shared)]
        let exec_dropped_bkl = crate::smp_shared::exec_bkl_drop_enabled();
        #[cfg(kernel_smp_shared)]
        if exec_dropped_bkl { akuma_exec::bkl::dropped_window_open(); }
        let read_result = crate::fs::read_file(&resolved_path);
        #[cfg(kernel_smp_shared)]
        if exec_dropped_bkl { akuma_exec::bkl::dropped_window_close(); }
        match read_result {
            Ok(data) => Some(data),
            Err(crate::vfs::FsError::Internal) => None,
            Err(e) => {
                // Gated 2026-08-30: this is the PATH search, not an error. A shell
                // or cargo resolving `cc`/`rustc` probes every PATH entry in turn,
                // so a miss per entry is the NORMAL case and the flood scales with
                // PATH length x exec count. A genuinely unreadable binary still
                // surfaces as the execve's errno to the caller.
                if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
                    crate::safe_print!(128, "[syscall] execve: failed to read {}\n", resolved_path);
                }
                return super::fs::fs_error_to_errno(e);
            }
        }
    };

    if file_data.as_ref().is_some_and(|d| d.len() >= 2 && d[0] == b'#' && d[1] == b'!') {
        // Owned handoff: exec_shebang frees the script bytes (and the original
        // argv) BEFORE recursing into do_execve — the inner exec's eret abandons
        // every frame below it, so nothing big may still be owned here.
        let data = file_data.take().unwrap_or_default();
        return exec_shebang(resolved_path, data, args, env);
    }

    let cur_pid = match akuma_exec::process::current_pid() {
        Some(p) => p,
        None => return ESRCH,
    };

    // On the size profile file_data only holds the 256-byte shebang probe —
    // always use replace_image_from_path for the actual ELF load.
    #[cfg(kernel_profile_extreme)]
    let mut file_data: Option<alloc::vec::Vec<u8>> = None;

    // Resolve the on-demand load's file size before entering the exclusive
    // window so the stat-failure early return stays outside it.
    let file_size = if file_data.is_none() {
        match crate::vfs::file_size(&resolved_path) {
            Ok(sz) => sz as usize,
            Err(e) => {
                crate::safe_print!(128, "[syscall] execve: failed to stat {}\n", resolved_path);
                return super::fs::fs_error_to_errno(e);
            }
        }
    } else {
        0
    };

    // SAFETY: `cur_pid` is the calling thread's own process on the BKL-held
    // execve path; `replace_image*` is the destructive window that stays
    // `&mut`-exclusive (Phase 7f owns converting it). No other Process
    // reference is live on this thread.
    let ret = unsafe {
        akuma_exec::process::with_process_exclusive(cur_pid, move |proc| {
    let replace_result = if let Some(data) = file_data.take() {
        let r = proc.replace_image(&data, &args, &env);
        // Free the whole-file image buffer HERE, success or failure. The
        // enter_user_mode below erets to EL0 and never returns, so any heap
        // still owned by this frame at that point is leaked kernel heap —
        // this exact leak (~1.1 MB per `busybox sh` execve) ratcheted the
        // heap into the [OOM] wall under exec-heavy load (the rustc hammer).
        drop(data);
        r
    } else {
        proc.replace_image_from_path(&resolved_path, file_size, &args, &env)
    };

    if let Err(e) = replace_result {
        crate::safe_print!(128, "[syscall] execve: replace_image failed for {}: {}\n", resolved_path, e);
        // `replace_image` returns a stringly-typed error from the ELF loader;
        // a "Failed to load ELF: ..." message means the binary is malformed
        // (missing PT_LOAD, bad magic, etc.) — Linux returns ENOEXEC for that.
        //
        // NOTE: close-on-exec fds are *not* closed until after this point.  On
        // Linux, `execve` only closes O_CLOEXEC descriptors once it has
        // committed to replacing the image (the point of no return).  If we
        // closed them earlier and then failed here, the process would resume at
        // the failed `execve` with a corrupted fd table.  For a libstd
        // fork+exec child that means its O_CLOEXEC error-report pipe would
        // already be gone, so it could neither report the exec failure to the
        // parent nor leave the parent's handshake pipe in a coherent state.
        return if e.contains("Failed to load ELF") { ENOEXEC } else { ENOMEM };
    }

    // Image replacement committed — *now* close the close-on-exec descriptors.
    // This is the POSIX "point of no return": a successful execve closes every
    // O_CLOEXEC fd, while a failed one (handled above) leaves them untouched.
    let closed_fds = proc.close_cloexec_fds();
    for (_fd, entry) in closed_fds {
        match entry {
            akuma_exec::process::FileDescriptor::PipeWrite(pipe_id) => super::pipe::pipe_close_write(pipe_id),
            akuma_exec::process::FileDescriptor::PipeRead(pipe_id) => super::pipe::pipe_close_read(pipe_id),
            akuma_exec::process::FileDescriptor::UnixSocket { rx, tx, sock } => {
                super::pipe::pipe_close_read(rx);
                super::pipe::pipe_close_write(tx);
                super::unixsock::unix_sock_close(sock);
            }
            akuma_exec::process::FileDescriptor::Socket(idx) => akuma_net::socket::remove_socket(idx),
            akuma_exec::process::FileDescriptor::ChildStdout(child_pid) => {
                akuma_exec::process::remove_child_channel(child_pid);
            }
            #[cfg(feature = "sc-eventfd")]
            akuma_exec::process::FileDescriptor::EventFd(efd_id) => super::eventfd::eventfd_close(efd_id),
            #[cfg(feature = "sc-epoll")]
            akuma_exec::process::FileDescriptor::EpollFd(epoll_id) => super::poll::epoll_destroy(epoll_id),
            #[cfg(feature = "sc-pidfd")]
            akuma_exec::process::FileDescriptor::PidFd(pidfd_id) => super::pidfd::pidfd_close(pidfd_id),
            _ => {}
        }
    }

    proc.name.clone_from(&resolved_path);

    if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
        crate::safe_print!(128, "[syscall] execve: replaced image for PID {} with {}\n", proc.pid, resolved_path);
    }

    // This frame is abandoned by the eret below (enter_user_mode never
    // returns), so destructors will not run: explicitly free every heap
    // local the closure still owns. args/env were last used by
    // replace_image*; resolved_path by the name/debug lines above.
    drop(args);
    drop(env);
    drop(resolved_path);

    // Wake any CLONE_VFORK parent that was blocked waiting for this exec.
    // Must happen before enter_user_mode (which never returns).
    let pid = proc.pid;
    vfork_complete(pid);

    proc.address_space.activate();

    // Never returns (lexically inside the `with_process_exclusive` unsafe block).
    akuma_exec::process::enter_user_mode(&proc.context);
        })
    };
    // `None` means the process vanished between `current_pid` and the lookup;
    // a successful exec never gets here (enter_user_mode does not return).
    ret.unwrap_or(ESRCH)
}

fn exec_shebang(script_path: String, file_data: Vec<u8>, original_args: Vec<String>, env: Vec<String>) -> u64 {
    // Parse everything we need out of `file_data` into OWNED strings first:
    // this function recurses into do_execve, whose successful exec erets and
    // abandons every frame below it — so the script bytes and the original
    // argv must be freed BEFORE the recursion, not after it "returns".
    // Shared with `spawn`'s shebang path so the two cannot drift: one parser, one
    // argv-construction rule, both host-tested in akuma-exec's `shebang_tests`.
    let head = &file_data[..file_data.len().min(akuma_exec::process::SHEBANG_MAX)];
    let Some((interpreter, shebang_arg)) = akuma_exec::process::parse_shebang(head) else {
        crate::safe_print!(128, "[syscall] execve: invalid shebang in {}\n", script_path);
        return ENOENT;
    };

    // TWO different strings, and collapsing them is a real bug. `interp_argv0` is
    // the interpreter AS WRITTEN in the `#!` line and is what Linux puts in
    // argv[0]; `interp_path` is the symlink-resolved file to actually load. This
    // used to shadow one with the other, so an interpreter reached through a
    // symlink lost its identity — and busybox is a multi-call binary that
    // dispatches ENTIRELY on argv[0], so `#!/bin/sh` ran as `/bin/busybox` with
    // argv[0]="/bin/busybox" and busybox had no idea it was meant to be a shell.
    // See docs/archive/DEVBOX_ISSUES.md Issue 14.
    let interp_argv0 = String::from(interpreter);
    let interp_arg = shebang_arg.map(String::from);
    drop(file_data); // last borrow (interpreter/shebang_arg) ended above

    let interp_path = crate::vfs::resolve_symlinks(&interp_argv0);

    if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
        if let Some(ref arg) = interp_arg {
            crate::safe_print!(128, "[syscall] execve: shebang {} {} {}\n", interp_argv0, arg, script_path);
        } else {
            crate::safe_print!(128, "[syscall] execve: shebang {} {}\n", interp_argv0, script_path);
        }
    }

    let mut new_args =
        akuma_exec::process::shebang_hop(&interp_argv0, interp_arg.as_deref(), &script_path, &[]);
    if original_args.len() > 1 {
        new_args.extend_from_slice(&original_args[1..]);
    }
    drop(original_args);
    drop(script_path);

    // If `interp_path` is ITSELF a script, do_execve recurses here and this
    // function sees the resolved path as its `script_path` — one level deeper the
    // as-written name is gone. `spawn`'s `resolve_shebang_chain` walks the whole
    // chain in one pass and does not have that hole; matching it here means
    // threading both names through do_execve's signature.
    do_execve(interp_path, new_args, env)
}

pub(super) fn sys_wait4(pid: i32, status_ptr: u64, options: i32, rusage_ptr: u64) -> u64 {
    if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
        crate::safe_print!(128, "[syscall] wait4(pid={}, options=0x{:x})\n", pid, options);
    }

    const RUSAGE_SIZE: usize = 144;
    if rusage_ptr != 0 {
        let zero = [0u8; RUSAGE_SIZE];
        let _ = copy_to_user(rusage_ptr, &zero);
    }

    let wnohang = options & 1 != 0;

    let current_pid = match akuma_exec::process::read_current_pid() {
        Some(p) => p,
        None => return ECHILD,
    };

    let waiter_tid = akuma_exec::threading::current_thread_id();

    if pid > 0 {
        let p = pid as u32;
        // Waiting on a process that is not our child (thread-group-wise) must
        // fail with ECHILD, not block — see is_child_of_group.
        let waiter_tgid = akuma_exec::process::current_process_shared().map_or(current_pid, |pr| pr.tgid);
        if !akuma_exec::process::is_child_of_group(p, waiter_tgid) {
            return ECHILD;
        }
        if let Some(ch) = akuma_exec::process::get_child_channel(p) {
            loop {
                if ch.has_exited() {
                    let code = ch.exit_code();
                    if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
                        let st = encode_wait_status(code);
                        crate::safe_print!(128, "[syscall] wait4: PID {} exit_code={} wait_status=0x{:08x}\n", p, code, st);
                    }
                    if status_ptr != 0 {
                        let status = encode_wait_status(code);
                        let _ = write_user_val(status_ptr, &status);
                    }
                    // Reap the zombie: remove from process table + child channels.
                    // On Linux, waitpid is the only way to reap a zombie.
                    akuma_exec::process::clear_lazy_regions(p);
                    let _ = akuma_exec::process::unregister_process(p);
                    akuma_exec::process::reap_child_channel(p);
                    return u64::from(p);
                }

                if wnohang {
                    return 0;
                }
                // Register as a poller so set_exited() wakes us instead of busy-spinning.
                ch.add_poller(waiter_tid);
                // Double-check after registering to avoid missed wakeup race.
                if ch.has_exited() {
                    let code = ch.exit_code();
                    if status_ptr != 0 {
                        let status = encode_wait_status(code);
                        let _ = write_user_val(status_ptr, &status);
                    }
                    akuma_exec::process::clear_lazy_regions(p);
                    let _ = akuma_exec::process::unregister_process(p);
                    akuma_exec::process::reap_child_channel(p);
                    return u64::from(p);
                }
                // Tested *before* blocking so that on wake the loop re-runs
                // has_exited() first: a reapable child outranks EINTR. SIGCHLD
                // pends exactly when a child exits, so the two race by design,
                // and Linux hands back the child.
                if akuma_exec::process::should_interrupt_blocking_syscall() {
                    return EINTR;
                }
                akuma_exec::threading::schedule_blocking(u64::MAX);
            }
        }
    } else if pid == -1 || pid == 0 {
        if !akuma_exec::process::has_children(current_pid) {
            if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
                crate::safe_print!(128, "[syscall] wait4: no children for PID {}\n", current_pid);
            }
            return ECHILD;
        }

        loop {
            if let Some((child_pid, ch)) = akuma_exec::process::find_exited_child(current_pid) {
                let code = ch.exit_code();
                if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
                    let st = encode_wait_status(code);
                    crate::safe_print!(128, "[syscall] wait4: PID {} exit_code={} wait_status=0x{:08x}\n", child_pid, code, st);
                }
                if status_ptr != 0 {
                    let status = encode_wait_status(code);
                    let _ = write_user_val(status_ptr, &status);
                }
                // Reap the zombie
                akuma_exec::process::clear_lazy_regions(child_pid);
                let _ = akuma_exec::process::unregister_process(child_pid);
                akuma_exec::process::reap_child_channel(child_pid);
                return u64::from(child_pid);
            }

            if wnohang {
                return 0;
            }
            // Register as poller on ALL children so any exit wakes us immediately.
            akuma_exec::process::add_poller_to_all_children(current_pid, waiter_tid);
            // Double-check after registering to avoid missed-wakeup race.
            if let Some((child_pid, ch)) = akuma_exec::process::find_exited_child(current_pid) {
                let code = ch.exit_code();
                if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
                    let st = encode_wait_status(code);
                    crate::safe_print!(128, "[syscall] wait4: PID {} exit_code={} wait_status=0x{:08x}\n", child_pid, code, st);
                }
                if status_ptr != 0 {
                    let status = encode_wait_status(code);
                    let _ = write_user_val(status_ptr, &status);
                }
                // Reap the zombie
                akuma_exec::process::clear_lazy_regions(child_pid);
                let _ = akuma_exec::process::unregister_process(child_pid);
                akuma_exec::process::reap_child_channel(child_pid);
                return u64::from(child_pid);
            }
            // Before blocking, so a child that became reapable while we slept is
            // returned ahead of EINTR (see the pid>0 arm).
            if akuma_exec::process::should_interrupt_blocking_syscall() {
                return EINTR;
            }
            akuma_exec::threading::schedule_blocking(u64::MAX);
        }
    }

    if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
        crate::safe_print!(128, "[syscall] wait4: no child found for PID {}\n", pid);
    }
    ECHILD
}

pub(super) fn sys_waitid(idtype: u32, id: u32, infop: u64, options: i32) -> u64 {
    use akuma_syscalls_linux::proc::wait_idtype::{P_ALL, P_PID};
    #[cfg(feature = "sc-pidfd")]
    use akuma_syscalls_linux::proc::wait_idtype::P_PIDFD;
    use akuma_syscalls_linux::proc::wait_options::{WNOHANG, WNOWAIT};
    use akuma_syscalls_linux::signal::cld::{CLD_EXITED, CLD_KILLED};
    use akuma_syscalls_linux::signal::{SIGCHLD, SIGINFO_SIZE};

    if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
        crate::safe_print!(128, "[syscall] waitid(idtype={}, id={}, options=0x{:x})\n", idtype, id, options);
    }

    // Zero siginfo buffer before any early return so Go sees clean data.
    if infop != 0 {
        let zero = [0u8; SIGINFO_SIZE];
        if copy_to_user(infop, &zero).is_err() {
            return EFAULT;
        }
    }

    let wnohang = (options & WNOHANG) != 0;

    let current_pid = match akuma_exec::process::read_current_pid() {
        Some(p) => p,
        None => return ECHILD,
    };

    let waiter_tid = akuma_exec::threading::current_thread_id();
    // Waits may come from any thread of a multithreaded parent (Go M's), so
    // parentage is checked against the caller's thread group.
    let waiter_tgid = akuma_exec::process::current_process_shared().map_or(current_pid, |p| p.tgid);
    let result: Option<(u32, i32)> = match idtype {
        P_PID => {
            if !akuma_exec::process::is_child_of_group(id, waiter_tgid) {
                return ECHILD;
            }
            if let Some(ch) = akuma_exec::process::get_child_channel(id) {
                loop {
                    if ch.has_exited() {
                        break Some((id, ch.exit_code()));
                    }
                    if wnohang { break None; }
                    ch.add_poller(waiter_tid);
                    if ch.has_exited() { break Some((id, ch.exit_code())); }
                    // Before blocking: on wake the loop re-tests has_exited()
                    // first, so a reapable child outranks EINTR.
                    if akuma_exec::process::should_interrupt_blocking_syscall() { return EINTR; }
                    akuma_exec::threading::schedule_blocking(u64::MAX);
                }
            } else {
                return ECHILD;
            }
        }
        P_ALL => {
            if !akuma_exec::process::has_children(current_pid) {
                return ECHILD;
            }
            loop {
                if let Some((cpid, ch)) = akuma_exec::process::find_exited_child(current_pid) {
                    break Some((cpid, ch.exit_code()));
                }
                if wnohang { break None; }
                akuma_exec::process::add_poller_to_all_children(current_pid, waiter_tid);
                if let Some((cpid, ch)) = akuma_exec::process::find_exited_child(current_pid) {
                    break Some((cpid, ch.exit_code()));
                }
                // Before blocking: on wake the loop re-tests find_exited_child()
                // first, so a reapable child outranks EINTR.
                if akuma_exec::process::should_interrupt_blocking_syscall() { return EINTR; }
                akuma_exec::threading::schedule_blocking(u64::MAX);
            }
        }
        #[cfg(feature = "sc-pidfd")]
        P_PIDFD => {
            // `id` is the fd number of a pidfd; resolve it to a target PID.
            let target_pid = if let Some(proc) = akuma_exec::process::current_process_shared() {
                match proc.get_fd(id) {
                    Some(akuma_exec::process::FileDescriptor::PidFd(pidfd_id)) => {
                        match super::pidfd::pidfd_get_pid(pidfd_id) {
                            Some(p) => p,
                            None => return ECHILD,
                        }
                    }
                    _ => return EBADF,
                }
            } else {
                return ECHILD;
            };
            // A pidfd can reference any live process (e.g. Go's os/exec probe opens
            // a pidfd of ITSELF), but waitid on one that is not our child must
            // fail with ECHILD, exactly like Linux — blocking would deadlock the
            // prober against its own exit.
            if !akuma_exec::process::is_child_of_group(target_pid, waiter_tgid) {
                return ECHILD;
            }
            if let Some(ch) = akuma_exec::process::get_child_channel(target_pid) {
                loop {
                    if ch.has_exited() {
                        break Some((target_pid, ch.exit_code()));
                    }
                    if wnohang { break None; }
                    ch.add_poller(waiter_tid);
                    if ch.has_exited() { break Some((target_pid, ch.exit_code())); }
                    // Before blocking: on wake the loop re-tests has_exited()
                    // first, so a reapable child outranks EINTR.
                    if akuma_exec::process::should_interrupt_blocking_syscall() { return EINTR; }
                    akuma_exec::threading::schedule_blocking(u64::MAX);
                }
            } else {
                return ECHILD;
            }
        }
        _ => return EINVAL,
    };

    if let Some((child_pid, code)) = result {
        if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
            crate::safe_print!(128, "[syscall] waitid: PID {} exited with code {}\n", child_pid, code);
        }
        if infop != 0 {
            // siginfo_t layout for SIGCHLD (AArch64 LP64 Linux):
            //   0: si_signo (u32), 4: si_errno (u32), 8: si_code (i32),
            //  12: __pad0 (u32) — aligns the _sifields union to 8 bytes,
            //  16: si_pid (u32), 20: si_uid (u32), 24: si_status (i32).
            // The union (containing `void *si_addr` / `clock_t si_utime`) is
            // 8-byte aligned, so si_pid starts at offset 16, NOT 12. musl agrees
            // (`__pad[128 - 2*sizeof(int) - sizeof(long)]`); the kernel's own
            // signal frame writes `si_addr` at offset 16 too.
            let (si_code, si_status) = if code < 0 { (CLD_KILLED, -code) } else { (CLD_EXITED, code) };
            let info = SigChld { si_signo: SIGCHLD, si_errno: 0, si_code, __pad0: 0,
                                 si_pid: child_pid, si_uid: 0, si_status };
            let _ = write_user_val(infop, &info);
        }
        if (options & WNOWAIT) == 0 {
            // Reap the zombie (unless WNOWAIT says "don't consume")
            akuma_exec::process::clear_lazy_regions(child_pid);
            let _ = akuma_exec::process::unregister_process(child_pid);
            akuma_exec::process::reap_child_channel(child_pid);
        }
        0
    } else {
        // WNOHANG with no exited child: return 0, si_pid stays 0
        0
    }
}

pub(super) fn sys_prlimit64(_pid: u32, resource: u32, _new_rlim: u64, old_rlim: u64) -> u64 {
    if old_rlim != 0 {
        use akuma_syscalls_linux::proc::rlimit::RLIM_INFINITY;
        let (cur, max) = match resource {
            3 => {
                let stack_size = akuma_exec::runtime::config().user_stack_size as u64;
                (stack_size, stack_size)
            },
            7 => (1024, 1024),
            _ => (RLIM_INFINITY, RLIM_INFINITY),
        };
        let rlim = Rlimit { rlim_cur: cur, rlim_max: max };
        if write_user_val(old_rlim, &rlim).is_err() {
            return EFAULT;
        }
    }
    0
}

pub(super) fn sys_sysinfo(info_ptr: usize) -> u64 {
    // This used to be a `[u8; 112]` filled by five `core::ptr::write(ptr.add(N))`
    // calls under a comment listing the AArch64 offsets. The comment was
    // correct and nothing checked that it stayed correct; the offsets are
    // `akuma_syscalls_linux::Sysinfo`'s now, asserted there. Every field the
    // old code left at zero still is — `Default` zeroes the struct exactly as
    // the array did.
    if !validate_user_ptr(info_ptr as u64, core::mem::size_of::<Sysinfo>()) { return EFAULT; }
    let info = Sysinfo {
        uptime: (crate::timer::uptime_us() / 1_000_000).cast_signed(),
        totalram: crate::pmm::total_count() as u64 * 4096,
        freeram: crate::pmm::free_count() as u64 * 4096,
        procs: 1,
        mem_unit: 1,
        ..Sysinfo::default()
    };
    if write_user_val(info_ptr as u64, &info).is_err() {
        return EFAULT;
    }
    0
}

pub(super) fn sys_getpid() -> u64 {
    // Linux's getpid(2) cannot fail; we return ESRCH only as a defensive
    // sentinel for kernel paths where no current PID has been registered.
    akuma_exec::process::read_current_pid().map_or(ESRCH, u64::from)
}

/// `core_init` (syscall nr::CORE_INIT): activated a parked secondary core in the
/// removed one-kernel-per-core multikernel (docs/archive/TRIM_FAT_MULTIKERNEL.md).
/// herd (`userspace/herd`) still calls this to try pinning a service to a
/// secondary core and already treats `ENOSYS` as the expected result under
/// shared-kernel SMP, so this stub stays for ABI compatibility.
pub(super) fn sys_core_init(core_idx: usize, init_program_ptr: u64) -> u64 {
    let _ = (core_idx, init_program_ptr);
    ENOSYS
}

pub(super) fn sys_getppid() -> u64 {
    if let Some(proc) = akuma_exec::process::current_process_shared() {
        u64::from(proc.parent_pid)
    } else {
        ESRCH
    }
}

pub(super) fn sys_geteuid() -> u64 {
    0
}

/// `capget`: report a full-root capability set, with real version negotiation.
///
/// The old stub zeroed 24 bytes of `data` and returned 0 for any input. Both
/// halves of that were wrong in ways that broke libcap-ng:
///
///  * **No version negotiation.** Linux's `capget` rejects an unknown
///    `hdr.version` by writing the version it *does* support back into the header
///    and returning EINVAL. That is a probe, not an error — libcap-ng calls
///    `capget` with version 0 precisely to be told which layout to use. Blindly
///    returning 0 told it "version 0 is fine", so every later call used a layout
///    the kernel never agreed to.
///  * **Zero capabilities.** Reporting an empty set makes a privilege-dropping
///    wrapper believe it has nothing to preserve across a uid change, while
///    `/proc/<pid>/status` says the opposite (`CapEff: 000001ffffffffff`). Two
///    sources of truth that disagree is worse than either answer alone.
///
/// Everything here runs as root, so the honest answer is "all capabilities", and
/// it matches what procfs reports.
pub(super) fn sys_capget(hdr_ptr: u64, data_ptr: u64) -> u64 {
    /// `_LINUX_CAPABILITY_VERSION_1/2/3`. v3 is what every current libc uses.
    const CAP_V1: u32 = 0x1998_0330;
    const CAP_V2: u32 = 0x2007_1026;
    const CAP_V3: u32 = 0x2008_0522;
    /// The low 41 capability bits (`CAP_LAST_CAP` = 40), as procfs reports them.
    const CAP_LOW: u32 = 0xffff_ffff;
    const CAP_HIGH: u32 = 0x0000_01ff;

    if hdr_ptr == 0 || !validate_user_ptr(hdr_ptr, 8) {
        return EFAULT;
    }
    let mut version: u32 = 0;
    if read_user_into(&mut version, hdr_ptr).is_err() {
        return EFAULT;
    }

    // Unknown version: tell the caller which one to use, then fail — this IS the
    // negotiation, and returning success here is what broke libcap-ng.
    if version != CAP_V1 && version != CAP_V2 && version != CAP_V3 {
        if write_user_val(hdr_ptr, &CAP_V3).is_err() {
            return EFAULT;
        }
        return EINVAL;
    }

    // A NULL `data` with a valid version is the pure "which version?" query.
    if data_ptr == 0 {
        return 0;
    }

    // v1 is one 32-bit triple; v2/v3 are two (low, high) — the struct is
    // `{effective, permitted, inheritable}` repeated per 32-bit slot.
    let slots: usize = if version == CAP_V1 { 1 } else { 2 };
    let mut caps = [0u32; 6];
    for slot in 0..slots {
        let (eff, perm, inh) = if slot == 0 {
            (CAP_LOW, CAP_LOW, 0)
        } else {
            (CAP_HIGH, CAP_HIGH, 0)
        };
        caps[slot * 3] = eff;
        caps[slot * 3 + 1] = perm;
        caps[slot * 3 + 2] = inh;
    }
    let bytes = slots * 12;
    if !validate_user_ptr(data_ptr, bytes) {
        return EFAULT;
    }
    if copy_to_user(data_ptr, &as_user_bytes(&caps)[..bytes]).is_err() {
        return EFAULT;
    }
    0
}

/// `getresuid`/`getresgid`: write the real, effective and saved id.
///
/// One function for both because this kernel has exactly one identity — root —
/// so all six values are 0. Answering matters even though the answer is
/// constant: util-linux's `setpriv` treats ENOSYS from these as fatal, and
/// `redis:alpine`'s entrypoint runs `setpriv --reuid=redis` under `set -e`, so
/// the container died before `redis-server` ever started.
///
/// A NULL pointer is EFAULT, matching Linux — a caller passing one has a bug,
/// and silently succeeding would hide it.
pub(super) fn sys_getresugid(rptr: u64, eptr: u64, sptr: u64) -> u64 {
    let zero: u32 = 0;
    for ptr in [rptr, eptr, sptr] {
        if ptr == 0 || !validate_user_ptr(ptr, 4) {
            return EFAULT;
        }
        if write_user_val(ptr, &zero).is_err() {
            return EFAULT;
        }
    }
    0
}

/// `getgroups`: this kernel has no supplementary groups, so the count is 0.
///
/// `size == 0` is the standard "how many are there?" probe and must return the
/// count without touching the buffer — that is the only form `setpriv` uses, but
/// the buffer form is handled too rather than left as a trap. A negative size is
/// EINVAL, per Linux.
pub(super) fn sys_getgroups(size: i32, _list_ptr: u64) -> u64 {
    if size < 0 {
        return EINVAL;
    }
    // Zero groups to report, so nothing is ever written to `list_ptr`.
    0
}

pub(super) fn sys_getrandom(ptr: u64, len: usize) -> u64 {
    if !validate_user_ptr(ptr, len) { return EFAULT; }
    let _drv_bkl = super::fs::DriverBklGuard::new();
    let mut remaining = len;
    let mut current_ptr = ptr;
    while remaining > 0 {
        let chunk = remaining.min(256);
        let mut kernel_buf = alloc::vec![0u8; chunk];
        if crate::rng::fill_bytes(&mut kernel_buf).is_ok() {
            if copy_to_user(current_ptr, &kernel_buf).is_err() {
                return EFAULT;
            }
        } else {
            return EIO;
        }
        remaining -= chunk;
        current_ptr += chunk as u64;
    }
    len as u64
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct SpawnOptions {
    pub cwd_ptr: u64,
    pub cwd_len: usize,
    pub root_dir_ptr: u64,
    pub root_dir_len: usize,
    pub args_ptr: u64,
    pub args_len: usize,
    pub stdin_ptr: u64,
    pub stdin_len: usize,
    pub box_id: u64,
    /// A NULL-terminated `char *envp[]`, or 0 for "use the default environment".
    ///
    /// Appended rather than inserted: `box_id` keeps offset 64, so a userspace
    /// binary built before this field existed still lands every field it knows
    /// where the kernel reads it. It passes `args_len` sized for the old struct,
    /// which `read_user_into` zero-fills past — hence 0 meaning "default".
    pub env_ptr: u64,
    pub env_len: usize,
}

// This layout is restated independently in userspace (`boxlib::sys::SpawnOptions`,
// docs/archive/HERD_PLUS_BOX.md) because a kernel crate cannot depend on a
// userspace one — see that doc's "kernel's copy stays put" section. Nothing
// checks the two agree except this: if either side's field order or width
// drifts, one of the two builds fails here rather than the kernel silently
// reading `box_id` out of what userspace meant as `stdin_len`.
const _: () = {
    assert!(core::mem::size_of::<SpawnOptions>() == 88);
    assert!(core::mem::offset_of!(SpawnOptions, cwd_ptr) == 0);
    assert!(core::mem::offset_of!(SpawnOptions, cwd_len) == 8);
    assert!(core::mem::offset_of!(SpawnOptions, root_dir_ptr) == 16);
    assert!(core::mem::offset_of!(SpawnOptions, root_dir_len) == 24);
    assert!(core::mem::offset_of!(SpawnOptions, args_ptr) == 32);
    assert!(core::mem::offset_of!(SpawnOptions, args_len) == 40);
    assert!(core::mem::offset_of!(SpawnOptions, stdin_ptr) == 48);
    assert!(core::mem::offset_of!(SpawnOptions, stdin_len) == 56);
    assert!(core::mem::offset_of!(SpawnOptions, box_id) == 64);
    assert!(core::mem::offset_of!(SpawnOptions, env_ptr) == 72);
    assert!(core::mem::offset_of!(SpawnOptions, env_len) == 80);
};

pub(super) fn parse_argv_array(ptr: u64) -> Vec<String> {
    if ptr == 0 { return Vec::new(); }
    let mut args = Vec::new();
    let mut i = 0;
    loop {
        let mut str_ptr: u64 = 0;
        if read_user_into(&mut str_ptr, ptr + i * 8).is_err() {
            break;
        }
        if str_ptr == 0 { break; }
        
        match copy_from_user_str(str_ptr, crate::config::MAX_ARG_STRLEN) {
            Ok(s) => args.push(s),
            Err(_) => break,
        }
        i += 1;
    }
    args
}

/// Spawn flag bits (Akuma's own SPAWN ABI, arg6). Keep in sync with
/// `libakuma::SPAWN_FLAG_PTY`.
const SPAWN_FLAG_PTY: u64 = 1;

pub(super) fn sys_spawn(path_ptr: u64, argv_ptr: u64, envp_ptr: u64, stdin_ptr: u64, stdin_len: usize, flags: u64) -> SysResult {
    // arg6 carries spawn flags. Bit 0 (SPAWN_FLAG_PTY) marks the child's stdin
    // as a terminal so the kernel line discipline runs (sshd sets it for an
    // interactive login shell — the client's `pty-req`).
    let pty = (flags & SPAWN_FLAG_PTY) != 0;
    let path = copy_from_user_str(path_ptr, 512)?;
    
    let args_vec = parse_argv_array(argv_ptr);
    let env_vec = parse_argv_array(envp_ptr);
    
    let args_refs: Vec<&str> = if args_vec.len() > 1 {
        args_vec.iter().skip(1).map(alloc::string::String::as_str).collect()
    } else {
        Vec::new()
    };
    
    let stdin_data = if stdin_ptr != 0 {
        if !BYPASS_VALIDATION.load(Ordering::Acquire)
            && !validate_user_ptr(stdin_ptr, stdin_len) { return Err(EFAULT); }
        let mut data = alloc::vec![0u8; stdin_len];
        if copy_from_user(&mut data, stdin_ptr).is_err() {
            return Err(EFAULT);
        }
        Some(data)
    } else {
        None
    };
    
    let stdin_slice = stdin_data.as_deref();

    match akuma_exec::process::spawn_process_with_channel_ext(&path, Some(&args_refs), Some(&env_vec), stdin_slice, None, 0, pty) {
        Ok((_tid, ch, pid)) => {
            if let Some(proc) = akuma_exec::process::current_process_shared() {
                akuma_exec::process::register_child_channel(pid, ch, proc.pid);
                return Ok(u64::from(pid) | (u64::from(proc.alloc_fd(akuma_exec::process::FileDescriptor::ChildStdout(pid))) << 32));
            }
        }
        Err(e) => {
            crate::safe_print!(128, "[sys_spawn] path={} failed: {}\n", path, e);
        }
    }
    Err(ENOMEM)
}

/// The size of `SpawnOptions` before `env_ptr`/`env_len` were appended.
///
/// A `/bin/box` or `/bin/herd` on disk can be older than the kernel booting it —
/// they are separate build artifacts on the same image — so the struct's size is
/// negotiated rather than assumed. Reading the current 88 bytes out of a caller
/// that only wrote 72 would hand the kernel whatever followed its struct on the
/// stack and use it as an `envp` pointer.
const SPAWN_OPTIONS_V1_SIZE: usize = 72;

/// `SPAWN_EXT(path, options, options_len)`.
///
/// `options_len` is the size of the caller's `SpawnOptions`. Zero means "the
/// original 72-byte layout", which is what every binary built before the field
/// was added passes (the argument was ignored and they leave it at whatever the
/// register held — the wrapper zeroes it).
pub fn sys_spawn_ext(path_ptr: u64, options_ptr: u64, options_len: u64, _a3: u64, _a4: u64, _a5: u64) -> SysResult {
    let path = copy_from_user_str(path_ptr, 512)?;

    if options_ptr == 0 { return Err(EINVAL); }
    // Zeroed, then filled only as far as the caller actually wrote: every field
    // the caller does not know about reads as 0, which is each one's "unset".
    let mut o = SpawnOptions::default();
    let known = core::mem::size_of::<SpawnOptions>();
    let supplied = if options_len == 0 {
        SPAWN_OPTIONS_V1_SIZE
    } else {
        (options_len as usize).min(known)
    };
    if supplied < SPAWN_OPTIONS_V1_SIZE {
        return Err(EINVAL);
    }
    {
        let bytes = as_user_bytes_mut(core::slice::from_mut(&mut o));
        if copy_from_user(&mut bytes[..supplied], options_ptr).is_err() {
            return Err(EFAULT);
        }
    }

    // Entering a box is a privilege boundary, not a preference: the child takes
    // that box's `box_id` AND its mount namespace (`spawn.rs`, "Set up isolation
    // context"). Unchecked, a boxed process spawns straight into box 0's
    // namespace — or into a sibling's — and reads whatever that box can see.
    // `box_id == 0` means "inherit the caller's box" and needs no check.
    if o.box_id != 0 {
        let (caller_box, caller_pid) = super::caller_box_and_pid();
        let registry = akuma_exec::process::registry_snapshot();
        if !akuma_exec::process::box_access::can_access_box(&registry, caller_box, o.box_id, caller_pid) {
            return Err(EPERM);
        }
    }

    let cwd = if o.cwd_ptr != 0 {
        let mut kernel_cwd = alloc::vec![0u8; o.cwd_len];
        if copy_from_user(&mut kernel_cwd, o.cwd_ptr).is_err() {
            return Err(EFAULT);
        }
        Some(alloc::string::String::from_utf8(kernel_cwd).unwrap_or_else(|_| String::from("/")))
    } else {
        None
    };
    
    let cwd_ref = cwd.as_deref();

    // `args_ptr` is an argv array INCLUDING argv[0] = program name (both
    // box/main.rs and herd build it that way), so always drop argv[0]. The old
    // `len > 1` special-case leaked argv[0] (the path) through as a real argument
    // for no-arg commands — e.g. a boxed `/bin/rump_server` with no args saw
    // "/bin/rump_server" as a positional URL and tried rump_init_server() on it.
    let args_vec = parse_argv_array(o.args_ptr);
    let args_refs: Vec<&str> = args_vec.iter().skip(1).map(alloc::string::String::as_str).collect();
    let args_opt = if args_refs.is_empty() { None } else { Some(args_refs.as_slice()) };

    let stdin_data = if o.stdin_ptr != 0 {
        let mut data = alloc::vec![0u8; o.stdin_len];
        if copy_from_user(&mut data, o.stdin_ptr).is_err() {
            return Err(EFAULT);
        }
        Some(data)
    } else {
        None
    };
    
    let stdin_slice = stdin_data.as_deref();

    // An empty list means "no environment was specified" and the spawn falls back
    // to `DEFAULT_ENV`. A caller that wants to *narrow* the environment therefore
    // has to pass at least one variable; `box run` composes the full list
    // (image `Env`, then `-e` overrides) rather than passing only the overrides.
    let env_vec = parse_argv_array(o.env_ptr);
    let env_opt = if env_vec.is_empty() { None } else { Some(env_vec.as_slice()) };

    if let Ok((_tid, ch, pid)) = akuma_exec::process::spawn_process_with_channel_ext(&path, args_opt, env_opt, stdin_slice, cwd_ref, o.box_id, false) {
        // For a `stack=rump` box, when herd spawns its `rump_server` the kernel
        // wires a sysproxy channel onto fd 3 (BEFORE the server runs) and brings
        // the proxy up. herd owns the process; the kernel owns the channel.
        #[cfg(feature = "rump")]
        if path.contains("rump_server") && crate::rump_proxy::box_is_rump(o.box_id) {
            crate::rump_proxy::attach_server(o.box_id, pid);
        }
        if let Some(proc) = akuma_exec::process::current_process_shared() {
            akuma_exec::process::register_child_channel(pid, ch, proc.pid);
            return Ok(u64::from(pid) | (u64::from(proc.alloc_fd(akuma_exec::process::FileDescriptor::ChildStdout(pid))) << 32));
        }
    }
    Err(ENOMEM)
}

/// `SET_BOX_STACK(box_id, stack)` — select a box's network stack. `stack == 1`
/// marks the box as using the NetBSD rump kernel (its AF_INET syscalls are
/// routed to that box's rump_server); any other value is a no-op (smoltcp
/// default). herd calls this for a `stack = rump` service. Without the `rump`
/// kernel feature the call is harmlessly ignored.
pub fn sys_set_box_stack(box_id: u64, stack: u64) -> u64 {
    // Repointing a box's network stack decides where its AF_INET syscalls are
    // proxied, so it is only the owner's call — otherwise any process could
    // route a box it does not own at a rump server it does.
    let (caller_box, caller_pid) = super::caller_box_and_pid();
    let registry = akuma_exec::process::registry_snapshot();
    if !akuma_exec::process::box_access::can_access_box(&registry, caller_box, box_id, caller_pid) {
        return EPERM;
    }

    #[cfg(feature = "rump")]
    if stack == 1 {
        crate::rump_proxy::mark_box_rump(box_id);
    }
    #[cfg(not(feature = "rump"))]
    let _ = (box_id, stack);
    0
}

/// `CLOSE_CHILD_STDIN(pid)` — deliver EOF to a spawned child's stdin so a shell
/// reading a piped script (busybox `sh`) finishes reading and runs the commands.
/// The userspace SSH-into-box bridge calls this on the client's `CHANNEL_EOF`.
/// Authorization mirrors the procfs `/proc/<pid>/fd/0` write path: only the
/// spawner may close its child's stdin, and box isolation is enforced.
pub(super) fn sys_close_child_stdin(pid: u32) -> u64 {
    let caller = match akuma_exec::process::current_process_shared() { Some(p) => p, None => return ESRCH };
    let target = match akuma_exec::process::lookup_process_shared(pid) { Some(p) => p, None => return ESRCH };

    // Box isolation: a boxed caller may only touch its own box's processes.
    if caller.box_id != 0 && target.box_id != caller.box_id {
        return ESRCH;
    }
    // Only the spawner may close its child's stdin (kernel-spawned => no spawner).
    if let Some(spawner) = target.spawner_pid
        && spawner != caller.pid
    {
        return EPERM;
    }

    match akuma_exec::process::close_process_stdin(pid) {
        Ok(()) => 0,
        Err(_) => ESRCH,
    }
}

pub(super) fn sys_kill(pid: u32, sig: u32) -> u64 {
    if pid == 0 { return 0; }
    // Linux: kill(pid <= 0, ...) targets a process group; we don't support
    // those semantics, so return EPERM for "operation not permitted".
    if pid <= 1 { return EPERM; }

    // sig=0 is a "does the process exist?" probe — don't actually send anything.
    if sig == 0 {
        return if akuma_exec::process::lookup_process_shared(pid).is_some() { 0 } else { ESRCH };
    }

    if crate::config::SYSCALL_DEBUG_INFO_ENABLED || crate::config::SYSCALL_DEBUG_NET_ENABLED {
        crate::tprint!(96, "[signal] kill(pid={}, sig={})\n", pid, sig);
    }

    // Per-pid delivery (thread-group interrupt+pend, SIGKILL hard-kill, and
    // the no-live-thread fallback) lives in `akuma_exec::process::deliver_signal`
    // now, shared with `kill_process_group`'s per-member broadcast (used by the
    // kernel's Ctrl-C/SIGINT handling — see `write_to_process_stdin`).
    if akuma_exec::process::deliver_signal(pid, sig) { 0 } else { ESRCH }
}

pub fn sys_waitpid(pid: u32, status_ptr: u64) -> u64 {
    if status_ptr != 0 && !validate_user_ptr(status_ptr, 4) { return EFAULT; }

    if let Some(ch) = akuma_exec::process::get_child_channel(pid)
        && ch.has_exited() {
            if status_ptr != 0 {
                let status = encode_wait_status(ch.exit_code());
                if write_user_val(status_ptr, &status).is_err() {
                    return EFAULT;
                }
            }
            // Reap the zombie
            akuma_exec::process::clear_lazy_regions(pid);
            let _ = akuma_exec::process::unregister_process(pid);
            akuma_exec::process::reap_child_channel(pid);
            return u64::from(pid);
        }
    0
}

/// prctl - process control
pub(super) fn sys_prctl(option: i32, arg2: u64, arg3: u64, arg4: u64, arg5: u64) -> u64 {
    const PR_SET_NAME: i32 = 15;
    const PR_GET_NAME: i32 = 16;
    const PR_SET_PDEATHSIG: i32 = 1;
    const PR_GET_PDEATHSIG: i32 = 2;
    const PR_SET_DUMPABLE: i32 = 4;
    const PR_GET_DUMPABLE: i32 = 3;
    const PR_SET_SECCOMP: i32 = 22;
    const PR_GET_SECCOMP: i32 = 21;
    const PR_SET_NO_NEW_PRIVS: i32 = 38;
    const PR_GET_NO_NEW_PRIVS: i32 = 39;
    const PR_SET_VMA: i32 = 0x53564d41; // "SVMA"
    const PR_CAPBSET_READ: i32 = 23;
    /// The highest capability number this kernel admits to, matching the
    /// `000001ffffffffff` set `sys_capget` and `/proc/<pid>/status` report.
    const CAP_LAST_CAP: u32 = 40;
    const PR_CAPBSET_DROP: i32 = 24;
    const PR_CAP_AMBIENT: i32 = 47;
    const PR_SET_PTRACER: i32 = 42;

    match option {
        PR_SET_NAME => {
            // Set process name (up to 16 chars including null)
            if arg2 != 0 && validate_user_ptr(arg2, 16) {
                let mut name_bytes = [0u8; 16];
                if copy_from_user(&mut name_bytes, arg2).is_err() {
                    return EFAULT;
                }
                let end = name_bytes.iter().position(|&b| b == 0).unwrap_or(16);
                if let Ok(name) = core::str::from_utf8(&name_bytes[..end]) {
                    // Build the String outside the IRQ-masked closure; only the
                    // move (and the old name's drop) happens inside.
                    let new_name = alloc::string::String::from(name);
                    akuma_exec::process::with_current_process(|p| p.name = new_name);
                }
            }
            0
        }
        PR_GET_NAME => {
            // Get process name
            if arg2 != 0 && validate_user_ptr(arg2, 16)
                && let Some(proc) = akuma_exec::process::current_process_shared() {
                    let name = proc.name.as_bytes();
                    let len = name.len().min(15);
                    let mut kernel_buf = [0u8; 16];
                    kernel_buf[..len].copy_from_slice(&name[..len]);
                    if copy_to_user(arg2, &kernel_buf).is_err() {
                        return EFAULT;
                    }
                }
            0
        }
        PR_SET_PDEATHSIG | PR_SET_DUMPABLE | PR_SET_NO_NEW_PRIVS | PR_SET_VMA => {
            // Accept but ignore these settings
            0
        }
        PR_GET_PDEATHSIG => {
            // Return 0 (no signal set)
            if arg2 != 0 && validate_user_ptr(arg2, 4) {
                let zero: i32 = 0;
                let _ = write_user_val(arg2, &zero);
            }
            0
        }
        PR_GET_DUMPABLE => {
            // Return 1 (dumpable)
            1
        }
        PR_GET_NO_NEW_PRIVS => {
            // Return 0 (not set)
            0
        }
        PR_SET_SECCOMP | PR_GET_SECCOMP => {
            // Return -EINVAL for seccomp (not supported)
            EINVAL
        }
        PR_CAPBSET_READ => {
            // We hold every capability, so the answer is 1 — but only for a
            // capability that *exists*. Linux rejects an out-of-range index with
            // EINVAL, and that rejection is load-bearing: util-linux's
            // `cap_last_cap()` falls back to probing this option when
            // `/proc/sys/kernel/cap_last_cap` is unreadable, so an unconditional
            // 1 makes it conclude CAP_LAST_CAP is INT_MAX. `setpriv --dump` then
            // iterates ~2.1 billion capability indices per capability set —
            // ~13 minutes of pure prctl spin each, which is indistinguishable
            // from a hang. That is what parks `redis:alpine`'s entrypoint, whose
            // `has_cap()` is `setpriv -d | grep -q 'Capability bounding set:…'`
            // (docs/archive/DEVBOX_ISSUES.md Issue 15).
            if arg2 > u64::from(CAP_LAST_CAP) {
                return EINVAL;
            }
            1
        }
        PR_CAPBSET_DROP => {
            // Accept but ignore capability operations
            0
        }
        PR_CAP_AMBIENT => {
            // The ambient set is empty and stays empty; raising into it is a
            // no-op. `PR_CAP_AMBIENT_IS_SET` is a query over a capability
            // number, though, so it inherits `PR_CAPBSET_READ`'s range rule —
            // `setpriv --dump` walks 0..=cap_last_cap() through this option, and
            // answering 0 for every integer is the second half of the spin
            // described above.
            const PR_CAP_AMBIENT_IS_SET: u64 = 1;
            if arg2 == PR_CAP_AMBIENT_IS_SET && arg3 > u64::from(CAP_LAST_CAP) {
                return EINVAL;
            }
            0
        }
        PR_SET_PTRACER => {
            // Accept but ignore - allows process to be traced by specific PID
            0
        }
        _ => {
            if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
                crate::tprint!(128, "[prctl] unsupported option={} arg2={:#x} arg3={:#x} arg4={:#x} arg5={:#x}\n",
                option, arg2, arg3, arg4, arg5);
            }
            0
        }
    }
}
