//! `clone(CLONE_VM|CLONE_THREAD)` — threads that share one address space.
//!
//! Measured 2026-09-06 (`docs/archive/AKUMA_AMD64_RUST_STD.md`): a real Rust
//! `std` binary reached stage 6 of 6 on this kernel and died on exactly one
//! syscall — `clone(0x7d0f00)` answering `ENOSYS`. Not futex, which the same
//! trace shows is *never called* by a single-threaded program: musl only
//! syscalls into a futex on contention, and there is nothing to contend with
//! until there is a second thread. Threads are the wall; futex is what the
//! wall is holding up.
//!
//! # A thread is a task, not a process
//!
//! Everything a thread needs already existed on this target, in the `fork`
//! machinery, and the whole of this module is the *subtraction*:
//!
//! | `fork` gives the child | a thread wants |
//! |---|---|
//! | a new `UserAddressSpace` (CoW-shared) | the parent's, unchanged |
//! | its own frame ledger (inside that address space) | none — it owns no frames |
//! | its own fd table (`clone_deep_for_fork`) | the parent's, shared (`CLONE_FILES`) |
//! | its own `%fs` base, copied from the parent | its own, **from `CLONE_SETTLS`** |
//! | a `waitpid`-visible exit status | a `futex` wake on `clear_child_tid` |
//!
//! The scheduler needed nothing at all: `Task::space_root` has been per-task
//! since Stage I and `UserCtx::fs_base` is already saved and restored across a
//! switch (`sched.rs`'s switch, and `arch_prctl`'s own comment). §11.2 of
//! `AKUMA_AMD64_STREAMLINING.md` listed that save/restore as work to do; the
//! `fork` work had already done it.
//!
//! # Why `CLONE_VM` must not go anywhere near the CoW share pass
//!
//! `Process::fork_from` demotes the parent's live PTEs to read-only so the next
//! write faults and copies. A thread must see the parent's writes and the
//! parent must see its — that *is* `CLONE_VM` — so a thread that went through
//! the share pass would get a private copy of every page it touched and the two
//! would silently diverge. There is no shared code path to get this wrong in:
//! [`sys_clone_thread`] never constructs a `Process` and never calls
//! `fork_from`. It reuses the parent's `space_root` as an opaque number.
//!
//! Nothing here demotes a PTE, so there is no stale-translation window of its
//! own to shoot down — and since 2026-09-09 the target has a shootdown anyway
//! (`shootdown.rs`), so CoW `fork` is no longer SMP=1 only either
//! (`AKUMA_AMD64_COW.md`).
//!
//! # The lifetime rule
//!
//! A process's address space is freed by whoever reaps it — `sys_waitpid`,
//! `cleanup_spawn_slot` — after the *main* thread's task is `Finished`. A
//! sibling thread still running in that space when it is freed is a
//! use-after-free of a page table. [`live_count`] is what closes that: the main
//! thread's exit path drains it to zero before it finishes, so by the time
//! anything can reap, no task holds that `space_root`.
//!
//! # The process table is the thread list
//!
//! Every thread is a registered `Process` (its own pid, `tgid` = the leader's),
//! so "the group's threads" is a walk of the process table by `tgid`
//! (`akuma_exec::process::for_each_live_group_thread`), and "is this task a
//! thread" is `pid != tgid`. Until 2026-11-12 this file kept a second list, a
//! 448-row `THREADS` table keyed by process slot, and every way the two
//! disagreed was a bug: a row left standing by a thread that never ran
//! [`teardown`] made the next process in that slot drain threads it never had
//! (`DRAIN INCOMPLETE` after `lifeprobe`'s `SIGKILL`). The AArch64 kernel never
//! had one (`proposals/AKUMA_ONE_EXIT.md` step 3).

use crate::fd::errno;
use crate::serial;

/// The calling thread's group: its `SharedSignalTable`, which `clone_thread` shares across the
/// thread group and `fork`/`spawn` make fresh. The group's exit state lives there (Linux's
/// `signal_struct`), so it is the group's by construction rather than a process slot's.
fn group() -> Option<&'static akuma_exec::process::SharedSignalTable> {
    akuma_exec::process::current_process_shared().map(|p| &*p.signal_actions)
}

/// Mark the calling thread's whole group for exit, so its siblings leave ring 3 at their next
/// syscall (or tick) instead of running on into a torn-down space. Called by `exit_group`.
pub fn set_group_exiting() {
    if let Some(g) = group() {
        g.begin_group_exit();
    }
}

/// How the calling thread's group is ending, when a sibling decided it: a sibling's
/// `exit_group(code)` ([`exit_group_from_thread`]) or its death by a default-action signal
/// (`crate::signal::notify_group_of_thread_fatal`).
///
/// Why this exists beside [`group_exiting`]: the interrupt-and-pend route (`deliver_signal`)
/// honors dispositions, and a main thread with a `SIGSEGV` handler — every rustc, which
/// installs one process-wide — would *handle* the pended signal and run on, hung forever
/// behind its already-dead workers. Measured 2026-09-13: rustc's worker `#GP`ed, its eight
/// siblings died, and the leader survived in `ps` because it handled the notification.
/// Linux's `do_group_exit` does not consult handlers either; the group exit code is fixed at
/// the signal that started it.
#[must_use]
pub fn group_exit_status() -> Option<akuma_exec::process::GroupExit> {
    group().and_then(akuma_exec::process::SharedSignalTable::group_exit_status)
}

/// Record how the calling thread's group ends.
pub fn set_group_exit_status(how: akuma_exec::process::GroupExit) {
    if let Some(g) = group() {
        g.set_group_exit_status(how);
    }
}

/// **`exit_group` from a thread that is not the leader.**
///
/// The leader's `run_process` epilogue is the only place a group dies on this target
/// (`drain`, `finish_process`), and the calling thread leaves through `run_thread`, which does
/// none of that. When the caller *is* the leader that is fine; when it is not, the leader has
/// to be told, or it sits where it was and the group never ends.
///
/// That is not exotic: a Rust program that runs `main` on a spawned big-stack thread leaves
/// its original thread in `pthread_join`'s untimed `FUTEX_WAIT`, and `std::process::exit` from
/// that thread is an `exit_group`. `goose --version` printed its version and then never exited
/// — the leader parked on the join futex for good, the parent's `wait4` with it (2026-10-03,
/// trashcan).
///
/// The route out is the one a fatal signal in a sibling already takes (the group's exit status
/// and an interrupt, read at the leader's next syscall return by `signal::deliver_pending`),
/// minus the signal: the status carries the exit code, and the interrupt is
/// `request_thread_kill` + `wake`, which is what every blocking arm's
/// `should_interrupt_blocking_syscall` already reads. The leader's slot is resolved through
/// `thread_for_pid`, never `Process::thread_id`, because a recorded slot may have been recycled.
///
/// Siblings other than the leader are not touched here: once the leader is out, its
/// epilogue's [`drain`] reaches them as it always has.
pub fn exit_group_from_thread(tgid: u32, code: u64) {
    set_group_exit_status(akuma_exec::process::GroupExit::Code(code as u8));
    if let Some(leader) = akuma_exec::process::thread_for_pid(tgid) {
        akuma_threading::request_thread_kill(leader);
        crate::sched::wake(leader);
    }
}

/// How many threads of `tgid`'s group other than its leader are still alive.
#[must_use]
pub fn live_count(tgid: u32) -> usize {
    akuma_exec::process::live_group_threads(tgid)
}

/// Is the running task a process's main thread (as opposed to a `clone` child)?
///
/// The one question `exit` has to answer differently from `exit_group`. A thread's
/// `Process` has its own pid and the leader's `tgid`; a task running no process is
/// nobody's thread.
#[must_use]
pub fn current_is_main() -> bool {
    akuma_exec::process::current_process_shared().is_none_or(|p| p.pid == p.tgid)
}

/// A non-main thread whose group has been told to exit should leave ring 3 at
/// its next syscall rather than run on in an address space about to be freed.
///
/// Checked at syscall entry, which is where a thread that makes syscalls
/// passes through, **and on the timer tick** since 2026-10-09
/// (`signal::deliver_pending_on_tick` → `TickOutcome::Exit`), which is the
/// only place a thread in a syscall-free compute loop can be reached. Until
/// then such a thread was unreachable — "a real gap", and the reason
/// [`drain`] is a bounded wait rather than a guarantee; the bound stays, as
/// the backstop for anything else that goes wrong on the way out.
#[must_use]
pub fn should_leave_now() -> bool {
    akuma_exec::process::current_process_shared()
        .is_some_and(|p| p.pid != p.tgid && p.signal_actions.group_exiting())
}

/// `clone(flags, child_stack, parent_tid, child_tid, tls)` — x86_64 56, the
/// thread arm. **Glue's `proc::clone_thread`**, one copy for both kernels:
/// the flag rules, the errno mapping, and (in `akuma_exec`) the signal mask
/// and the `CLONE_SETTLS` rule, and the retired-row drain that keeps thread churn
/// from exhausting the process table and the ASIDs. What stays here is this
/// kernel's own state: a caller with no process slot.
///
/// x86_64's argument order is its own — `tls` **last**, after `child_tid`,
/// where asm-generic puts it fourth — which is why the forward reorders
/// rather than passing straight through.
///
/// Until 2026-11-11 this arm also refused a thread without `CLONE_SETTLS`
/// (`EINVAL`), on the reasoning that it would run on the parent's `%fs`. Linux
/// runs it on the parent's `%fs`, and Go's runtime asks for exactly that: its
/// flag word has no `CLONE_SETTLS` and the child sets its TLS itself
/// (`userspace/forktest/c_stress/cloneflags.c`).
pub fn sys_clone_thread(
    flags: u64,
    child_stack: u64,
    parent_tid: u64,
    child_tid: u64,
    tls: u64,
) -> u64 {
    // A task that is not running a registered process has no thread group to
    // join. (`clone_thread` would refuse too, with a less exact errno.)
    if crate::usermode::current_process().is_none() {
        return errno::ENOSYS;
    }

    // asm-generic order: `(flags, stack, parent_tid, tls, child_tid)`.
    akuma_syscalls_glue::proc::clone_thread(flags, child_stack, parent_tid, tls, child_tid)
}

/// **A `clone` child's ring-3 lifetime**, from `Process::run`'s
/// `ExecRuntime::enter_user` hook down to `sched::finish`.
///
/// This is the thread half of `usermode::enter_ring3`'s two-way split, and the
/// reason that split exists: the process half (`run_process`) ends by closing
/// the fd table, draining the thread group, publishing an exit status and
/// retiring the process, all of which are wrong for one thread of several.
///
/// It was `thread_entry`, an `extern "C" fn() -> !` that `sched` spawned
/// directly and that read its own `rip`/`rsp` out of a thread-table row. Since
/// the `clone` fold the child is spawned by `spawn_child_thread_and_publish`
/// like every other child, enters at the shared `entry_point_trampoline`, and
/// is handed its first context. One authority for where a task enters ring
/// 3, which is `ProcessImage::context`.
///
/// **There is no `execve` loop here** — and since 2026-09-11 there is not one
/// in [`crate::usermode::run_process`] either: `execve` writes its own
/// `UserCtx` and the syscall return path `sysret`s into the new image, so no
/// task on this target leaves ring 3 and comes back for one. A thread enters
/// ring 3 exactly once, and so does a process.
///
/// It was never an omission here regardless: `execve` from a non-main thread
/// replaces the whole process image, which on Linux kills every sibling first,
/// and this target refuses it (`sys_execve` is reached only by a main thread).
pub fn run_thread(first: &akuma_exec::process::UserContext) -> ! {
    // `forked = true`: enter ring 3 through `enter_user_mode_forked`, which
    // restores the parent's register set from this task's own `saved_regs` and
    // sets `rax = 0`. A `clone` child sees 0 for exactly the reason a `vfork`
    // child does, so the path is the same one.
    let _status = crate::usermode::enter_user_from_thread(first.pc, first.sp);

    teardown();
    crate::sched::finish();
}

/// A thread has left ring 3: publish its death the way `pthread_join` reads it.
///
/// The order is the contract. Zero the word *before* the wake, or a joiner that
/// wakes and re-reads sees the old tid and parks again — a lost wakeup that
/// only shows up when the joiner happens to be scheduled between the two.
///
/// The word is the `Process`'s `clear_child_tid`: `clone_thread` set it from
/// `CLONE_CHILD_CLEARTID` and `set_tid_address` moves it, as on Linux. Written
/// while this thread's `THREAD_PID_MAP` row still stands, so the leader's
/// [`drain`] still counts it while it touches user memory.
fn teardown() {
    let task = crate::sched::current_task();
    let ctid = akuma_exec::process::current_process_shared()
        .map_or(0, |p| p.clear_child_tid.load(core::sync::atomic::Ordering::Relaxed));
    if ctid != 0 {
        let _ = crate::uaccess::write_val::<u32>(ctid, 0);
        // Glue's kernel-internal wake: both namespaces, since a joiner may
        // have waited with or without `FUTEX_PRIVATE_FLAG` (musl's
        // `pthread_join` uses `priv = 0`).
        akuma_syscalls_glue::futex_wake(crate::usermode::current_pid(), ctid as usize, 1);
    }
    // The slot this thread ran on becomes reusable the moment it is `Finished`.
    // Anything of its still on the futex table would then name whoever inherits
    // it, and absorb a wake meant for them.
    akuma_syscalls_glue::futex_purge_tid(task);
    // Same argument for the identity map: a stale `tid -> pid` row would hand
    // the next occupant of this slot the dead thread's process.
    //
    // **And retire the `Process` behind it**, which is the half this target had
    // no need of until the `clone` fold: a thread used to be a thread-table row
    // and a task, with no `Process` of its own, so there was nothing here to
    // release. `clone_thread` gives every thread a real registered `Process`
    // (its own pid, `tgid` = the leader's), and on this target **nothing else
    // releases it**: a process is retired by `sys_waitpid`, and a thread is
    // never waited for.
    //
    // The other kernel does this from `akuma_exec::process::on_thread_cleanup`,
    // registered as `threading::set_cleanup_callback` and run when a thread slot
    // is recycled. That callback never fires here, because x86 slots are
    // recycled by `claim_slot` taking a `TERMINATED` one directly rather
    // than through the crate's collector — so the release has to happen at the
    // one point this target *does* know a thread is finished, which is here.
    //
    // Left out, it is a `Process` leaked per `pthread_create`, and the symptom
    // is two: `ps` grows by a row that never goes away, and — because the dead
    // `Process` keeps `thread_id = Some(task)` — `resolve_thread_process`'s
    // table scan starts finding it for whoever inherits the task slot and logs
    // `[TRAMP-MISMATCH]`. Measured on the metal: two `threadprobe` rows still in
    // `ps` long after the boot self-test, and two mismatch lines naming them.
    //
    // No remaining-thread count, unlike the shared callback: the map row just
    // removed was this thread's own pid, and a thread's `Process` has exactly
    // one thread by construction. `unregister_process` retires the slot; the
    // frames come back on the next `drain_retired_if_requested`.
    if let Some(pid) = akuma_exec::process::thread_pid_map_remove(task) {
        akuma_exec::process::unregister_process(pid);
    }
}

/// Ask every thread of `tgid`'s group other than its leader (the caller) to
/// leave ring 3, and wait for them.
///
/// Called from the main thread's exit path. A thread parked in `FUTEX_WAIT`
/// notices through [`should_leave_now`], which its poll loop checks; one running
/// in ring 3 notices at its next syscall, or at the timer tick
/// (`signal::deliver_pending_on_tick`) if it makes none.
///
/// **Mark first, count after.** `clone_thread` refuses a new thread once the
/// group is exiting, under the same BKL this runs under, so a thread is either
/// refused or registered before the mark — and then counted below.
pub fn drain(tgid: u32) {
    set_group_exiting();
    wake_group(tgid);
    // Bounded, like every other drive loop in this kernel. An unbounded wait
    // here turns any bug in the leave path into a boot that hangs with no
    // output — the failure mode that costs the most to diagnose and says the
    // least. A bounded one leaves a named line and a process the reaper will
    // free out from under a live thread, which is worse in principle and far
    // better to debug, because it *tells you*.
    let mut spins = 0u32;
    while live_count(tgid) > 0 && spins < DRAIN_SPINS {
        spins += 1;
        crate::sched::yield_now();
    }
    let left = live_count(tgid);
    if left > 0 {
        serial::puts("  [thread] DRAIN INCOMPLETE: ");
        serial::put_dec(left as u64);
        serial::puts(" thread(s) still live in group ");
        serial::put_dec(u64::from(tgid));
        serial::puts(" — the reaper may free a live address space\n");
    }
}

/// Interrupt and wake every live thread of `tgid`'s group, so each sees the exit
/// flag.
///
/// **Waking is not interrupting, and the difference is the whole of `DRAIN
/// INCOMPLETE`.** `sched::wake` makes a parked thread runnable. A thread parked
/// in the *futex* loop is then fine: that loop re-checks `should_leave_now` and
/// leaves. A thread parked anywhere else — a pipe `read`, `poll`, the console —
/// wakes, re-evaluates its **own** condition (still no data), and parks again.
/// It never returns to syscall entry, which is the only other place
/// `should_leave_now` is tested, so it never learns the group is exiting.
///
/// `request_thread_kill` arms `PENDING_KILL`, which is what
/// `should_interrupt_blocking_syscall` reads — the one hook the park loop
/// consults (`akuma_threading::schedule_blocking`). So the sibling's wait
/// returns `EINTR`, unwinds to syscall entry, and sees the flag. That is also
/// what Linux does: `exit_group` is a group-wide kill, not a group-wide nudge.
/// Found by `cargo`: `rustc` exited with two threads parked on its jobserver
/// pipe and the build stopped dead (`docs/archive/RUST_TOOLCHAIN_AMD64.md`
/// § session 4).
///
/// Only slots the process table resolves to this group are touched, so a kill
/// request can never land on a slot that has since been reissued to somebody
/// else — the hazard the old per-process-slot table had
/// (`docs/archive/AKUMA_AMD64_SIGKILL_NATIVE_PATH.md`).
fn wake_group(tgid: u32) {
    let me = crate::sched::current_task();
    akuma_exec::process::for_each_live_group_thread(tgid, |_, task| {
        if task != me {
            akuma_threading::request_thread_kill(task);
            crate::sched::wake(task);
        }
    });
}

/// How many scheduler rounds [`drain`] gives the group before it gives up.
///
/// Generous: every thread leaves within one round of noticing the flag, and the
/// only reason to need more is a thread in a long ring-3 stretch with no
/// syscall in it — which this target cannot interrupt at all.
const DRAIN_SPINS: u32 = 100_000;
