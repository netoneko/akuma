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
//! | a process slot and a `Spawn` record | the parent's, shared |
//! | its own fd routing (`UserCtx::proc_slot`) | the parent's, shared |
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

use spinning_top::Spinlock;

use crate::fd::errno;
use crate::serial;

/// Threads in flight, across all processes.
///
/// A fixed array, not a `Vec`: this is per-thread kernel state on a path that
/// runs under memory pressure, and the ceiling wants to be a refusal
/// (`EAGAIN`, which is what `pthread_create` already reports) rather than an
/// allocation that can fail.
///
/// **Was 64 until 2026-10-08**, with the note that it was "well past what the
/// scheduler's own `MAX_TASKS` budget makes useful". It was not: `MAX_TASKS`
/// is 512, and one headless Chromium session is a browser with ~20 threads
/// plus a dozen GPU/utility/renderer processes of 5–7 each. The 65th thread
/// system-wide got `EAGAIN` from `clone`, the GPU process turned that into a
/// `CHECK` (`[Fault] #BP`) five times over and the browser gave up ("GPU
/// process isn't usable"). The `[sc!]` trace never showed it, because it
/// leaves `EAGAIN` out on purpose; the `[clone] thread table full` serial
/// line is the tell. The table is now sized to the task budget it draws
/// from, less headroom for the main threads: an entry is three words, so
/// this costs about 14 KiB of `.bss`. Every thread still costs a task slot
/// and two 32 KiB kernel stacks, which is the ceiling that remains. Gate:
/// `thrprobe.c` (8 processes × 12 threads alive together).
pub const MAX_THREADS: usize = 448;

/// "This task is not a thread." `UserCtx::thread_slot`'s resting value.
pub const NO_THREAD: usize = usize::MAX;

/// One live non-main thread.
#[derive(Clone, Copy)]
struct Thread {
    /// Scheduler task slot. The identity a futex wake reaches.
    task: usize,
    /// The process slot whose address space this thread runs in — shared with
    /// its process, which is what makes fd 0/1/2 route the same way.
    proc_slot: usize,
    /// `CLONE_CHILD_CLEARTID`'s address, or 0. On exit the kernel zeroes this
    /// word and wakes one futex waiter on it — which is precisely how
    /// `pthread_join` learns the thread is gone, and the reason a `join` that
    /// never returns is the classic symptom of getting this wrong.
    clear_child_tid: u64,
}

/// The thread table. It was `static mut` "under the BKL — the discipline
/// `usermode::PROCS` had", an unchecked claim; a `Spinlock` makes it a fact. The
/// guard is held only for a lookup or an edit, **never across a call into
/// another subsystem**: the scans below copy a row out (`Thread` is `Copy`),
/// drop the guard, and only then ask the process table or the scheduler about it.
static THREADS: Spinlock<[Option<Thread>; MAX_THREADS]> = Spinlock::new([None; MAX_THREADS]);

/// Row `i`, copied out, or `None` for a free row.
fn thread_row(i: usize) -> Option<Thread> {
    THREADS.lock()[i]
}

/// The calling thread's group: its `SharedSignalTable`, which `clone_thread` shares across the
/// thread group and `fork`/`spawn` make fresh. The group's exit state lives there (Linux's
/// `signal_struct`), so it is the group's by construction rather than a process slot's.
fn group() -> Option<&'static akuma_exec::process::SharedSignalTable> {
    akuma_exec::process::current_process_shared().map(|p| &*p.signal_actions)
}

/// Has the calling thread's group been asked to exit as a whole?
#[must_use]
pub fn group_exiting() -> bool {
    group().is_some_and(akuma_exec::process::SharedSignalTable::group_exiting)
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

/// How many non-main threads of `proc_slot` are still alive.
#[must_use]
pub fn live_count(proc_slot: usize) -> usize {
    THREADS
        .lock()
        .iter()
        .filter(|t| t.as_ref().is_some_and(|t| t.proc_slot == proc_slot))
        .count()
}

/// Is the running task a process's main thread (as opposed to a `clone` child)?
///
/// The one question `exit` has to answer differently from `exit_group`.
#[must_use]
pub fn current_is_main() -> bool {
    current_thread_slot() == NO_THREAD
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
    if current_is_main() {
        return false;
    }
    group_exiting()
}

pub fn current_thread_slot() -> usize {
    // Under the BKL; the running task's `UserCtx`.
    crate::sched::read_current_uctx(|u| u.thread_slot).unwrap_or(NO_THREAD)
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
    // join. `current_proc_slot` answering `usize::MAX` is how that shows up,
    // and `bind_clone_child` would have nothing to key its row on.
    if crate::usermode::current_proc_slot() >= crate::usermode::PROC_SLOTS {
        return errno::ENOSYS;
    }

    // asm-generic order: `(flags, stack, parent_tid, tls, child_tid)`.
    akuma_syscalls_glue::proc::clone_thread(flags, child_stack, parent_tid, tls, child_tid)
}

/// **The `ChildKind::Thread` half of `ExecRuntime::bind_child_task`** — give a
/// child the shared `clone_thread` just spawned its `THREADS` row and its
/// `UserCtx::thread_slot`, so this file's four thread questions can be asked of
/// it.
///
/// Those four are the whole reason the row exists, and each is silent if the
/// row is missing:
///
/// * [`current_tid`] — `gettid`, which musl caches in `pthread_self()->tid` and
///   `tkill`s. Without a row a thread answers its *process's* pid, so a
///   `tkill(self->tid, …)` addresses the main thread.
/// * [`current_is_main`] — the one question `exit` answers differently from
///   `exit_group`. Without a row every thread claims to be the main one and its
///   `exit()` tears the whole process down.
/// * [`live_count`] / [`drain`] — a process's exit waits for its threads. A
///   thread with no row is invisible to that wait, and the reaper frees an
///   address space a live thread is standing in.
/// * [`teardown`] — `clear_child_tid` and its futex wake, i.e. `pthread_join`.
///
/// # `tid` is the kernel thread slot, which is a change on this target
///
/// See [`Thread::tid`]'s own note. Storing anything else here would make
/// `gettid` disagree with what `clone` returned.
///
/// # `clear_child_tid` comes off the `Process`, not off the flags
///
/// `clone_thread` has already applied the `CLONE_CHILD_CLEARTID` rule —
/// `InheritOverrides::clear_child_tid` is `child_tid_ptr` only when the flag is
/// set, and 0 otherwise — so reading the field is reading that decision rather
/// than making it a second time. The two used to be made independently, in two
/// files, from the same flag word.
pub fn bind_clone_child(
    task: usize,
    proc_slot: usize,
    clear_child_tid: u64,
) -> Result<(), &'static str> {
    // Find and claim a free row under one guard, so two cores cannot take the same.
    let claimed = {
        let mut table = THREADS.lock();
        table.iter().position(Option::is_none).inspect(|&slot| {
            table[slot] = Some(Thread { task, proc_slot, clear_child_tid });
        })
    };
    let Some(slot) = claimed else {
        // The caller turns this string into `EAGAIN`, which is what Linux
        // returns at the thread limit and what `pthread_create` reports.
        serial::puts("  [clone] thread table full\n");
        return Err("clone: thread table full");
    };
    crate::sched::seed_thread_slots(task, proc_slot, slot);
    Ok(())
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
/// directly and that read its own `rip`/`rsp` out of the `THREADS` row. Since
/// the `clone` fold the child is spawned by `spawn_child_thread_and_publish`
/// like every other child, enters at the shared `entry_point_trampoline`, and
/// is handed its first context — so `first` replaces the row's two fields and
/// the row no longer carries them. One authority for where a task enters ring
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
pub fn run_thread(slot: usize, first: &akuma_exec::process::UserContext) -> ! {
    // `forked = true`: enter ring 3 through `enter_user_mode_forked`, which
    // restores the parent's register set from this task's own `saved_regs` and
    // sets `rax = 0`. A `clone` child sees 0 for exactly the reason a `vfork`
    // child does, so the path is the same one.
    let _status = crate::usermode::enter_user_from_thread(first.pc, first.sp);

    teardown(slot);
    crate::sched::finish();
}

/// A thread has left ring 3: publish its death the way `pthread_join` reads it.
///
/// The order is the contract. Zero the word *before* the wake, or a joiner that
/// wakes and re-reads sees the old tid and parks again — a lost wakeup that
/// only shows up when the joiner happens to be scheduled between the two.
fn teardown(slot: usize) {
    let Some(t) = THREADS.lock()[slot].take() else { return };
    if t.clear_child_tid != 0 {
        let _ = crate::uaccess::write_val::<u32>(t.clear_child_tid, 0);
        // Glue's kernel-internal wake: both namespaces, since a joiner may
        // have waited with or without `FUTEX_PRIVATE_FLAG` (musl's
        // `pthread_join` uses `priv = 0`).
        akuma_syscalls_glue::futex_wake(crate::usermode::current_pid(), t.clear_child_tid as usize, 1);
    }
    // The slot this thread ran on becomes reusable the moment it is `Finished`.
    // Anything of its still on the futex table would then name whoever inherits
    // it, and absorb a wake meant for them.
    akuma_syscalls_glue::futex_purge_tid(t.task);
    // Same argument for the identity map: a stale `tid -> pid` row would hand
    // the next occupant of this slot the dead thread's process.
    //
    // **And retire the `Process` behind it**, which is the half this target had
    // no need of until the `clone` fold: a thread used to be a `THREADS` row and
    // a task, with no `Process` of its own, so there was nothing here to
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
    if let Some(pid) = akuma_exec::process::thread_pid_map_remove(t.task) {
        akuma_exec::process::unregister_process(pid);
    }
}

/// Ask every thread of `proc_slot` other than the caller to leave ring 3, and
/// wait for them.
///
/// Called from the main thread's exit path. A thread parked in `FUTEX_WAIT`
/// notices through [`group_exiting`], which its poll loop checks; one running
/// in ring 3 notices at its next syscall. Signal delivery is at a syscall return
/// too (`crate::signal`), so a thread in a genuinely unbounded ring-3 loop is
/// still not reachable by either route — bounded by
/// the same preemption that bounds any other runaway user program, and called
/// out in `AKUMA_AMD64_RUST_STD.md` as a known gap rather than papered over.
pub fn drain(proc_slot: usize) {
    set_group_exiting();
    wake_group(proc_slot);
    // Bounded, like every other drive loop in this kernel. An unbounded wait
    // here turns any bug in the leave path into a boot that hangs with no
    // output — the failure mode that costs the most to diagnose and says the
    // least. A bounded one leaves a named line and a process the reaper will
    // free out from under a live thread, which is worse in principle and far
    // better to debug, because it *tells you*.
    let mut spins = 0u32;
    while live_count(proc_slot) > 0 && spins < DRAIN_SPINS {
        spins += 1;
        crate::sched::yield_now();
    }
    let left = live_count(proc_slot);
    if left > 0 {
        serial::puts("  [thread] DRAIN INCOMPLETE: ");
        serial::put_dec(left as u64);
        serial::puts(" thread(s) still live in proc slot ");
        serial::put_dec(proc_slot as u64);
        serial::puts(" — the reaper may free a live address space\n");
    }
}

/// Make every thread of `proc_slot` runnable, so each can see the exit flag.
///
/// The flag is only tested at syscall entry and inside the `futex` wait loop,
/// and since 2026-09-07 that wait loop **parks** rather than spins. Without this
/// an `exit_group` while a sibling holds an untimed `FUTEX_WAIT` would wait out
/// the scheduler's one-second backstop before the sibling looked at anything —
/// correct, because the backstop exists so a missing wake is slow rather than
/// fatal, and far too slow to be the design. This is the wake path that stops it
/// being the design.
///
/// Waking a thread that is not parked is a no-op with a recorded flag, so this
/// cannot race the sibling into a park it will not come out of.
fn wake_group(proc_slot: usize) {
    // **Only rows whose task slot is still this group's.** A row is cleared by
    // [`teardown`], which runs on the thread's own way out — so a thread that
    // was hard-terminated from another core (the shared `kill_thread_group`
    // path, which `kill(2)` on this target took until 2026-10-09) left its row
    // standing, and the next process in this `proc_slot` inherited it: its
    // exit saw 17, then 41, then 65 "live" threads, spun the whole
    // [`DRAIN_SPINS`] budget, and posted a kill request plus a wake to a task
    // slot that by then belonged to somebody else — a sticky `EINTR` on an
    // innocent thread, since nothing here consumes `PENDING_KILL` but the
    // slot scrub. Measured with `killtree` under Firecracker; recorded in
    // `docs/archive/AKUMA_AMD64_SIGKILL_NATIVE_PATH.md`.
    //
    // A row is ours if its task slot's `THREAD_PID_MAP` owner belongs to this
    // thread group. A missing owner on a *live* slot is kept, not dropped:
    // `clone_thread` publishes the map before the child can run, but the row
    // and the map entry are two writes, and dropping a row for a thread about
    // to start would let the reaper free the address space under it — the
    // failure this module's lifetime rule exists to prevent. A dead or free
    // slot with no owner is a leftover and goes.
    let my_tgid = crate::usermode::current_process().map(|p| p.tgid);
    let mut dropped = 0u32;
    for i in 0..MAX_THREADS {
        // Copied out: the process-table lookups below run with no guard held.
        let Some(t) = thread_row(i) else { continue };
        if t.proc_slot != proc_slot {
            continue;
        }
        let owner_tgid = akuma_exec::process::pid_for_thread(t.task)
            .and_then(akuma_exec::process::lookup_process_shared)
            .map(|p| p.tgid);
        let stale = match owner_tgid {
            Some(tg) => my_tgid.is_some_and(|mine| mine != tg),
            None => !akuma_threading::slot_is_live(t.task),
        };
        if stale {
            THREADS.lock()[i] = None;
            dropped += 1;
        }
    }
    if dropped > 0 {
        akuma_primitives::safe_print!(96,
            "[thread] dropped {} stale thread row(s) from proc slot {}\n", dropped, proc_slot);
    }
    {
        for t in (0..MAX_THREADS).filter_map(thread_row) {
            if t.proc_slot == proc_slot {
                // **Waking is not interrupting, and the difference is the
                // whole of `DRAIN INCOMPLETE`.**
                //
                // `sched::wake` makes a parked thread runnable. A thread parked
                // in the *futex* loop is then fine: that loop re-checks
                // `should_leave_now` and leaves. A thread parked anywhere else
                // — a pipe `read`, `poll`, the console — wakes, re-evaluates
                // its **own** condition (still no data), and parks again. It
                // never returns to syscall entry, which is the only other place
                // `should_leave_now` is tested, so it never learns the group is
                // exiting and `drain` spins out its 100 000 rounds and gives up.
                //
                // `request_thread_kill` arms `PENDING_KILL`, which is what
                // `should_interrupt_blocking_syscall` reads — the one hook the
                // park loop consults (`akuma_threading::schedule_blocking`). So
                // the sibling's wait returns `EINTR`, unwinds to syscall entry,
                // and sees the flag. That is also what Linux does: `exit_group`
                // is a group-wide kill, not a group-wide nudge.
                //
                // Found by `cargo`: `rustc` exits with two threads parked on
                // its jobserver pipe, drain gives up, the process is never
                // reaped, and the parent waits forever — a build that stops
                // dead at a random crate with nothing in cargo's own log
                // (`docs/archive/RUST_TOOLCHAIN_AMD64.md` § session 4).
                akuma_threading::request_thread_kill(t.task);
                crate::sched::wake(t.task);
            }
        }
    }
}

/// How many scheduler rounds [`drain`] gives the group before it gives up.
///
/// Generous: every thread leaves within one round of noticing the flag, and the
/// only reason to need more is a thread in a long ring-3 stretch with no
/// syscall in it — which this target cannot interrupt at all.
const DRAIN_SPINS: u32 = 100_000;
