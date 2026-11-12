use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;
use spinning_top::Spinlock;

use crate::process::Process;
use crate::process::types::{Pid, ProcessInfo, PROCESS_INFO_ADDR, LazyRegion, LazySource, MmapRegion, ProcessInfo2, ProcessState};
use crate::process::channel::{ProcessChannel, get_channel};
use crate::process::table::{THREAD_PID_MAP, find_process};
use crate::runtime::{with_irqs_disabled, runtime};
use akuma_tty as terminal;

/// Registry mapping child PIDs to (ProcessChannel, parent_pid)
/// Used by parent processes to read child stdout via ChildStdout FD
/// and by wait4(-1) to find children of a specific parent.
static CHILD_CHANNELS: Spinlock<BTreeMap<Pid, (Arc<ProcessChannel>, Pid)>> =
    Spinlock::new(BTreeMap::new());

/// Register a child process channel (called when spawning via syscall)
///
/// `parent_pid` is the pid of the thread that forked, and the registry records its **thread group**
/// instead when that thread is still known: a child belongs to the process, not to the one thread that
/// happened to call `fork`/`clone`. Recording the thread's own pid meant that once a pool thread exited,
/// `is_child_of_group` could no longer resolve its group and `wait4` answered `ECHILD` for a zombie
/// that nobody could ever reap — while its pidfd stayed readable, so a runtime polling it (Bun)
/// spun forever and starved its own stdin (`docs/archive/CHILD_OF_EXITED_THREAD_UNREAPABLE.md`).
pub fn register_child_channel(child_pid: Pid, channel: Arc<ProcessChannel>, parent_pid: Pid) {
    // Resolved before the registry lock is taken: `find_process` takes the process-table lock.
    let parent = find_process(|p| if p.pid == parent_pid { Some(p.tgid) } else { None }).unwrap_or(parent_pid);
    with_irqs_disabled(|| {
        CHILD_CHANNELS.lock().insert(child_pid, (channel, parent));
    })
}

/// Get a child process channel by PID
pub fn get_child_channel(child_pid: Pid) -> Option<Arc<ProcessChannel>> {
    with_irqs_disabled(|| {
        CHILD_CHANNELS.lock().get(&child_pid).map(|(ch, _)| ch.clone())
    })
}

/// True when `child_pid` is registered as a child of the thread group `waiter_tgid`.
///
/// The registered parent is the pid of whichever thread called fork/clone; a
/// multithreaded parent (e.g. the Go runtime) may wait from a *different* thread
/// of the same group, so the comparison is by thread group, not raw pid. Linux
/// `wait*` on a process that is not your child fails with ECHILD — the wait4 /
/// waitid paths use this to enforce that. Notably Go's os/exec pidfd probe
/// calls `waitid(P_PIDFD, <pidfd of itself>)` and *requires* ECHILD; blocking
/// on a non-child instead deadlocks the caller against its own exit.
pub fn is_child_of_group(child_pid: Pid, waiter_tgid: Pid) -> bool {
    let ppid = with_irqs_disabled(|| {
        CHILD_CHANNELS.lock().get(&child_pid).map(|(_, ppid)| *ppid)
    });
    let Some(ppid) = ppid else { return false };
    if ppid == waiter_tgid {
        return true;
    }
    // The recorded parent may be a non-leader thread; resolve its thread group.
    find_process(|p| if p.pid == ppid { Some(p.tgid) } else { None })
        .is_some_and(|tgid| tgid == waiter_tgid)
}

/// The pid recorded as `child_pid`'s parent at fork time, if it is still a
/// registered child. This is the *forking thread's* pid, which may be a
/// non-leader thread of a multithreaded parent (e.g. the Go runtime's M
/// threads) — resolve its thread group before using it as a signal target
/// (see [`sigchld_target_thread`]).
pub fn parent_pid_of(child_pid: Pid) -> Option<Pid> {
    with_irqs_disabled(|| CHILD_CHANNELS.lock().get(&child_pid).map(|(_, ppid)| *ppid))
}

/// Pick which thread of group `tgid` receives a process-directed signal.
///
/// Linux delivers a process-directed signal to any thread that does not block
/// it. We approximate that with an explicit preference order, because two of
/// our delivery-path guards silently drop a signal aimed at the wrong thread:
///
///   1. a thread not blocking SIGCHLD **and** with a sigaltstack configured —
///      preferred because `try_deliver_signal` (src/exceptions.rs) re-pends a
///      signal whose handler is `SA_ONSTACK` when the target thread's
///      `alt_sp == 0` (a Go M that has not reached `mstart`'s `sigaltstack`
///      call); targeting such a thread would re-pend SIGCHLD at every syscall
///      return forever and never deliver;
///   2. any thread not blocking SIGCHLD;
///   3. the thread-group leader (the blocked-signal fallback — Linux would pick
///      *some* thread and leave the bit pending; we pin it to the leader so a
///      blocked SIGCHLD stays pending on a real thread of the group until the
///      mask is cleared).
fn sigchld_target_thread(tgid: Pid) -> Option<usize> {
    const SIGCHLD_BIT: u64 = 1u64 << (17 - 1);
    let mut best_with_altstack: Option<usize> = None;
    let mut best_unblocked: Option<usize> = None;
    let mut leader_tid: Option<usize> = None;

    crate::process::table::for_each_process(|p| {
        if p.tgid != tgid { return; }
        let Some(tid) = p.thread_id else { return; };
        if p.pid == tgid { leader_tid = Some(tid); }
        let blocks_sigchld = crate::threading::thread_signal_mask_of(tid) & SIGCHLD_BIT != 0;
        if !blocks_sigchld {
            if best_unblocked.is_none() { best_unblocked = Some(tid); }
            let (sp, _size, flags) = crate::threading::get_sigaltstack(tid);
            if sp != 0 && flags != 2 /* SS_DISABLE */ && best_with_altstack.is_none() {
                best_with_altstack = Some(tid);
            }
        }
    });

    best_with_altstack.or(best_unblocked).or(leader_tid)
}

/// Raise SIGCHLD on the parent of `child_pid`, if it has a live parent process.
///
/// MUST be called *after* the child's channel is marked exited (see
/// [`publish_child_exit`]): shells respond to the handler by calling
/// `waitpid(WNOHANG)`, which has to already see the zombie or the shell
/// concludes nothing happened and re-suspends. The ordering is structural in
/// [`publish_child_exit`]; callers that invoke this directly must honour it.
///
/// Records `exit_code` in the per-thread SIGCHLD siginfo side-channel so an
/// `SA_SIGINFO` handler reads a real `si_pid`/`si_status` instead of zeros.
///
/// Never sets the interrupted flag (`interrupt_thread`) — that is Ctrl+C's
/// channel and would turn every child exit into a spurious `EINTR` storm across
/// the parent's unrelated blocking syscalls. Pending + wake is enough: the
/// pending bit is what `sys_rt_sigsuspend` polls, and delivery happens at the
/// parent's next syscall-return boundary.
pub fn raise_sigchld_for_parent(child_pid: Pid, exit_code: i32) {
    const SIGCHLD: u32 = 17;
    let Some(ppid) = parent_pid_of(child_pid) else { return };
    // The forking thread may be a non-leader of a multithreaded parent; resolve
    // its thread group so we can target any of its threads.
    let Some(tgid) = find_process(|p| if p.pid == ppid { Some(p.tgid) } else { None }) else { return };
    // Kernel-thread parents (e.g. the in-kernel sshd bridge) have no live child
    // entry and no userspace to signal — silently skip.
    if let Some(tid) = sigchld_target_thread(tgid) {
        crate::threading::set_last_sigchld(tid, child_pid, exit_code);
        crate::threading::pend_signal_for_thread(tid, SIGCHLD);
    }
}

/// Publish a child's exit atomically: mark the child channel exited (waking any
/// `wait4`/pidfd pollers) and then raise SIGCHLD on the parent, **iff this call
/// is the one that published the exit**.
///
/// Ordering is load-bearing: the channel MUST be marked exited before SIGCHLD
/// is pended, because the parent's SIGCHLD handler immediately re-polls with
/// `waitpid(WNOHANG)` and must observe the zombie. Routing every exit site
/// through this helper makes that ordering structural instead of a comment
/// repeated at each call site.
///
/// SIGCHLD is raised only on the first publish — a child that exits cleanly and
/// is then torn down (`return_to_kernel` / `kill_child_processes`) must not
/// raise two SIGCHLDs for one death. A duplicate is harmless for shells (which
/// re-poll with `WNOHANG` and get `ECHILD`/0) but would confuse a
/// signal-counting handler, so the guard is worth keeping.
pub fn publish_child_exit(child_pid: Pid, exit_code: i32) {
    // The process is done executing its image (this helper is every exit site's one route): lift `ETXTBSY` on its executable now, not
    // when the zombie is reaped, which may be much later and which a still-running parent can postpone indefinitely.
    let _ = super::table::with_process(child_pid, |p| p.release_text());
    let published = match get_child_channel(child_pid) {
        Some(ch) if !ch.has_exited() => {
            // Before `set_exited`: the parent's `wait4` reads this as soon as it
            // sees the channel exited. The thread slot the time comes from is
            // still the child's here; it may not be by the time of the reap.
            let tgid = super::table::with_process(child_pid, |p| p.tgid).unwrap_or(child_pid);
            ch.set_cpu_time_us(super::table::group_cpu_time_us(tgid));
            ch.set_exited(exit_code);
            true
        }
        _ => false,
    };
    if published {
        raise_sigchld_for_parent(child_pid, exit_code);
    }
}

/// Remove a child process channel (called when the parent CLOSES its
/// `ChildStdout` read fd, or on `execve`/teardown of the reading process).
pub fn remove_child_channel(child_pid: Pid) -> Option<Arc<ProcessChannel>> {
    with_irqs_disabled(|| {
        CHILD_CHANNELS.lock().remove(&child_pid).map(|(ch, _)| ch)
    })
}

/// Reap a child's channel on the `wait*` (waitpid/wait4/waitid) path.
///
/// This is distinct from [`remove_child_channel`], which fires when the parent
/// closes its `ChildStdout` read fd. Reaping a zombie must NOT discard stdout the
/// child wrote right before exiting: the parent's `ChildStdout` fd resolves the
/// channel by pid via [`get_child_channel`] on every read, so if `wait*` removed
/// the channel the instant it reaped, a parent that reads stdout *after*
/// observing the exit would find it gone (EBADF) and lose all buffered output.
///
/// That is exactly the sshd interactive bridge: it checks `waitpid` first, then
/// drains the child's stdout. A fully-buffered shell (busybox flushes stdio at
/// `_exit`) loses everything; an unbuffered one (toybox) loses only its final
/// pre-exit write. So here we only drop the channel if its stdout buffer is
/// already empty; otherwise we keep it and let the parent's `close()` (or process
/// teardown) remove it via [`remove_child_channel`] once drained.
///
/// Race-free: the child is confirmed exited before reaping, so no further writes
/// can arrive — an empty buffer stays empty, and a non-empty one only shrinks as
/// the reader drains it. Returns `true` if the channel was removed, `false` if it
/// was kept (data still buffered) or was absent.
///
/// # A kept entry is no longer anybody's child
///
/// Keeping the entry for its stdout must not keep the *child*: every caller has
/// just handed this pid back from a `wait*` as reaped, and a reaped pid that
/// `find_exited_child` can still find is returned again by the next
/// `wait4(-1)` — forever, since the buffer only drains if someone reads it. So
/// the kept entry's parent is rewritten to [`REAPED_PARENT`], which no process
/// is: `get_child_channel` (the `ChildStdout` reader) still finds it by pid,
/// while `find_exited_child`, `has_children`, `is_child_of_group` and
/// `parent_pid_of`'s callers all stop seeing it.
///
/// Found 2026-09-24 on the bare-metal amd64 box: herd's orphan sweep
/// (`while let Some(_) = wait_any()`) got a dead, still-buffered service back
/// ~900 k times a second and grew a `Vec` until every physical page was gone
/// (`docs/archive/AKUMA_AMD64_BARE_METAL_SELFHOST.md` §6 item 6).
pub fn reap_child_channel(child_pid: Pid) -> bool {
    with_irqs_disabled(|| {
        let mut map = CHILD_CHANNELS.lock();
        match map.get_mut(&child_pid) {
            Some((ch, ppid)) if ch.has_stdout_data() => {
                *ppid = REAPED_PARENT;
                false
            }
            Some(_) => map.remove(&child_pid).is_some(),
            None => false,
        }
    })
}

/// The parent recorded for a child that has been reaped but whose channel is
/// kept for a `ChildStdout` reader still draining it (see
/// [`reap_child_channel`]). No process has this pid, so no `wait*` can find it.
pub const REAPED_PARENT: Pid = Pid::MAX;

/// Find any exited child of the given parent. Returns (child_pid, channel).
pub fn find_exited_child(parent_pid: Pid) -> Option<(Pid, Arc<ProcessChannel>)> {
    with_irqs_disabled(|| {
        let channels = CHILD_CHANNELS.lock();
        for (&child_pid, (ch, ppid)) in channels.iter() {
            if *ppid == parent_pid && ch.has_exited() {
                return Some((child_pid, ch.clone()));
            }
        }
        None
    })
}

/// Register `poller_tid` as a poller on every child channel of `parent_pid`.
/// When any child exits, `set_exited()` wakes the poller.
pub fn add_poller_to_all_children(parent_pid: Pid, poller_tid: usize) {
    with_irqs_disabled(|| {
        let channels = CHILD_CHANNELS.lock();
        for (ch, ppid) in channels.values() {
            if *ppid == parent_pid {
                ch.add_poller(poller_tid);
            }
        }
    })
}

/// Check if the given parent has any children registered.
pub fn has_children(parent_pid: Pid) -> bool {
    with_irqs_disabled(|| {
        CHILD_CHANNELS.lock().values().any(|(_, ppid)| *ppid == parent_pid)
    })
}

/// The registry's children of `parent_pid` — the set `wait4(-1)` from that
/// parent draws on. Allocates one `Vec`; for tests and diagnostics, not for a
/// hot path.
pub fn children_of(parent_pid: Pid) -> Vec<Pid> {
    with_irqs_disabled(|| {
        CHILD_CHANNELS
            .lock()
            .iter()
            .filter(|(_, (_, ppid))| *ppid == parent_pid)
            .map(|(&pid, _)| pid)
            .collect()
    })
}

/// Hand every child of `dying` to `new_parent` — Linux's reparent-to-init at
/// exit — in **both** places that answer "whose child is this": the process
/// table's `parent_pid` (what `/proc/<pid>/status` shows) and this registry
/// (what `wait4(-1)`, [`has_children`], [`is_child_of_group`],
/// [`find_exited_child`] and [`raise_sigchld_for_parent`] read).
///
/// Until 2026-09-23 the amd64 exit path rewrote only the table row. init's
/// `wait4(-1, WNOHANG)` then asked `has_children(1)`, found nothing here,
/// and answered `ECHILD` — so every orphan on that target was a zombie for the
/// life of the boot, each holding a task slot: a service's workers after
/// `herd disable`, a `nohup … &` whose ssh session ended, a pipeline whose
/// shell left first. Eleven of them on the bare-metal box after one evening
/// (`docs/archive/AKUMA_AMD64_TRAP_ENTRY_DIRECTION_FLAG.md` § "Found while
/// checking process/socket cleanup"). Two views of parenthood is the bug; this
/// is the one function that moves both. Returns how many registry entries
/// moved. A child that has already exited moves too, and the new parent's
/// next `wait4(-1)` finds it through [`find_exited_child`].
pub fn reparent_children_to(dying: Pid, new_parent: Pid) -> usize {
    let moved = with_irqs_disabled(|| {
        let mut map = CHILD_CHANNELS.lock();
        let mut n = 0usize;
        for (_, ppid) in map.values_mut() {
            if *ppid == dying {
                *ppid = new_parent;
                n += 1;
            }
        }
        n
    });
    // Two passes rather than a mutation inside `for_each_process`, which hands
    // out `&Process`. The `Vec` is empty for a process with no children — nearly
    // all of them — and `Vec::new` does not allocate until something is pushed,
    // so the common exit path stays allocation-free.
    for orphan in crate::process::collect_pids(|p| p.parent_pid == dying) {
        crate::process::with_process(orphan, |p| p.parent_pid = new_parent);
    }
    moved
}

/// [`reparent_children_to`] for a whole thread group, when `pid` is its leader.
///
/// Every thread has a pid of its own, and a child records the *spawning
/// thread's* pid as its parent, so a dying multi-threaded process's children
/// are spread across its members. Reparenting only the leader's left the rest
/// as zombies with a dead thread for a parent — nobody could `wait` for them
/// (found 2026-09-24 on amd64 with kot's tokio workers). A non-leader `pid`
/// moves only its own children: the group, and its other threads, live on.
pub fn reparent_group_children_to(pid: Pid, new_parent: Pid) -> usize {
    let tgid = find_process(|p| if p.pid == pid { Some(p.tgid) } else { None }).unwrap_or(pid);
    if tgid != pid {
        return reparent_children_to(pid, new_parent);
    }
    let mut members = crate::process::collect_pids(|p| p.tgid == tgid);
    if !members.contains(&pid) {
        members.push(pid);
    }
    members.into_iter().map(|m| reparent_children_to(m, new_parent)).sum()
}

/// Get channel for the current thread (used by syscall handlers)
pub fn current_channel() -> Option<Arc<ProcessChannel>> {
    if let Some(proc) = current_process_shared() {
        if let Some(ref ch) = proc.channel {
            return Some(ch.clone());
        }
    }
    
    // Fallback to thread-ID based lookup for legacy system threads
    let thread_id = crate::threading::current_thread_id();
    get_channel(thread_id)
}

/// Check if the current process has been interrupted (Ctrl+C)
///
/// Called by syscall handlers to detect interrupt signal.
/// Returns true if the process should terminate.
pub fn is_current_interrupted() -> bool {
    // **One relaxed load in the common case**, and that is the whole design.
    // This runs in the syscall prologue of both kernels, so what it must *not*
    // do is take a lock — see `akuma_threading::THREAD_INTERRUPTED` for the two
    // earlier shapes and what the second one cost on real silicon.
    let tid = crate::threading::current_thread_id();
    if !crate::threading::take_thread_interrupted(tid) {
        return false;
    }
    // Interrupted. Now — and only now, off the hot path — consume the channel's
    // copy as well, so a holder that reads its own channel directly
    // (`process/exec.rs`'s exec-wait) does not see a flag this call has already
    // answered. That equivalence is not new: on AArch64 the two *were* one
    // object for a spawned process, and this keeps them behaving as one.
    if let Some(ch) = get_channel(tid) {
        let _ = ch.is_interrupted();
    }
    true
}

/// The interrupt bit's job is finished: a signal frame has been installed for
/// `tid`, so the signal it stood for has reached userspace.
///
/// # The spurious `EINTR` this removes
///
/// [`interrupt_thread`] raises a flag meaning "break whatever blocking syscall
/// this thread is in", and [`is_current_interrupted`] — which **consumes** it —
/// is read by two sorts of caller: a blocking wait loop, and the syscall
/// prologue of both kernels, which answers `EINTR` and returns.
///
/// That works when the thread is blocked. When it is *running*, nothing
/// consumes the flag: the signal is delivered at the next tick or syscall
/// return, the handler runs and returns, and then the **next syscall the
/// program makes — any syscall, including ones that cannot block — fails with
/// `EINTR`**. Measured on amd64 2026-09-12 with an `alarm(1)`/`pause()` pair,
/// where the `getpid()` after the handler returned `-4`; Linux returns the pid.
///
/// So delivery consumes it. Called from the point each kernel *installs a
/// frame* — `akuma_exceptions::try_deliver_signal` and `amd64`'s
/// `signal::enter_handler` — and deliberately not from the decision before it:
/// a signal that turns out to be ignored, or one whose handler cannot be
/// entered, has not reached userspace, and a flag raised for a blocking wait
/// that has not happened yet must survive to break it.
///
/// Idempotent, and returns nothing: the caller is reporting an event, not
/// asking a question.
pub fn signal_frame_installed(tid: usize) {
    let _ = crate::threading::take_thread_interrupted(tid);
    if let Some(ch) = get_channel(tid) {
        // The channel's copy too, for the same reason `is_current_interrupted`
        // consumes both: a holder reading its own channel directly must not
        // see a flag this delivery has already answered.
        let _ = ch.is_interrupted();
    }
}

/// Interrupt a process by thread ID
///
/// Used by the SSH shell to send Ctrl+C signal to a running process.
///
/// # Why this writes the per-thread registry and **not** `Process::channel`
///
/// It tried to, for one day (2026-09-11), and the reason it was tried is a real
/// gap: [`is_current_interrupted`] reads `Process::channel` **first** and only
/// falls back to this registry, so on a target where the two differ the flag
/// lands on the channel nobody reads. That target is amd64, where every process
/// registers an *exit* channel under its task slot and a console-attached one
/// also carries the serial line's channel in `Process::channel`.
///
/// **Writing both is wrong, and the reason is one line up the file:**
/// `Process::inherit_from` does `channel: parent.channel.clone()`, so a
/// `Process::channel` is shared by an entire **`fork` tree** — and a
/// per-process interrupt flag cannot live in an object a tree shares.
///
/// How far that reaches on amd64, checked rather than assumed — and it is
/// **two** trees, not one machine-wide object as the first draft of this
/// comment claimed:
///
/// * `init` on the serial line and its `fork` descendants share the *console's*
///   channel. `register_exec_process` derives "is this the console's process"
///   from fd 0 being a `FileDescriptor::Stdin`, and `fd::bind_stdio` gives every
///   `sys_spawn` child a `PipeRead` there before the registration reads it, so
///   nothing `sshd` spawns is in this tree.
/// * an `ssh` session's shell and its `fork` descendants share that **session's**
///   channel, since `SPAWN_FLAG_PTY` started arriving (2026-09-11). It is a
///   different object per session, which is what makes `^C` reach one session's
///   foreground job and not another's — and it is just as shared *within* a
///   session, so the flag would still interrupt a whole session at once.
///
/// Narrower than one object per machine, and still the wrong place for the
/// flag. See `AKUMA_AMD64_SIGNAL_DELIVERY.md` §5d.
///
/// **The reader was the half that was actually wrong**, and it is fixed:
/// [`is_current_interrupted`] read `Process::channel` first, which agreed with
/// this registry only while the two were the same object. They stopped being
/// one on amd64 the day a session child got an I/O channel — see that
/// function.
///
/// So the gap stands, and it is narrower than it looks: the *per-thread*
/// `EINTR` path (`current_thread_has_pending_interrupt`, reading the pending
/// set) is not affected and is what every `kill`-interrupts-a-blocking-syscall
/// case actually uses. Closing it properly means giving
/// `is_current_interrupted` a per-**thread** flag that is not a shared `Arc`,
/// and not paying a `get_channel` map lookup on every syscall to find it.
pub fn interrupt_thread(thread_id: usize) {
    // The bit is what [`is_current_interrupted`] reads; the channel flag is what
    // a holder of that channel reads (`process/exec.rs`). Both, because they
    // answer different questions for different readers — and neither may be
    // dropped in favour of the other, which is what the two paragraphs above
    // are about.
    crate::threading::set_thread_interrupted(thread_id);
    if let Some(channel) = get_channel(thread_id) {
        channel.set_interrupted();
    }
}

/// Does the *current thread* have a pending signal that must abort a blocking
/// syscall with `EINTR`?
///
/// This is the per-thread counterpart to [`is_current_interrupted`], which reads
/// `ProcessChannel::is_interrupted` — a flag set only by Ctrl-C and `sys_kill`.
/// That makes it structurally blind to `tkill`/`tgkill` (i.e. `pthread_kill`),
/// which pend a signal on one thread slot and wake it. Without this check a woken
/// thread re-tests its loop predicate, still sees "not interrupted", and blocks
/// again — so `pthread_kill` could never interrupt a blocking syscall.
///
/// Linux reports `EINTR` when a signal is actually *delivered*: pending, not
/// blocked, and carrying a userspace handler. Two deliberate exclusions:
///
/// - **Blocked signals** stay pending silently and interrupt nothing.
/// - **`SA_RESTART` handlers** get the syscall transparently restarted instead.
///   Every caller of this helper is a "retry until the predicate holds" loop, so
///   *not reporting* the interrupt makes the loop take another pass — which is
///   exactly a restart. (Go installs its SIGURG preemption handler with
///   `SA_RESTART`; reporting `EINTR` for it would break every blocking syscall a
///   Go program makes.)
///
/// A `Default`-disposition signal needs no check here: `sys_tkill` applies a
/// fatal default action inline and only pends it when blocked — and blocked is
/// already excluded above.
///
/// The motivating case is jobserver-rs's `Helper::join`, which sends SIGUSR1 with
/// `SA_SIGINFO` and *no* `SA_RESTART` specifically to break its helper thread out
/// of a blocking pipe `read`; every rustc that reaches codegen leaks that thread
/// otherwise.
pub fn current_thread_has_pending_interrupt() -> bool {
    // Hot path: one relaxed-ish atomic load for the overwhelmingly common
    // "nothing pending" case, before any lookup or lock.
    let tid = crate::threading::current_thread_id();
    // Two sources, and the second is why `pthread_kill` can interrupt a blocking
    // syscall at all under a fast signal source:
    //
    //   pending    not yet delivered — the ordinary case.
    //   delivered  ALREADY delivered since this syscall began. `take_pending_signal`
    //              cleared the pending bit at the return-to-EL0 path, so without
    //              this the blocking loop has no way to learn it was interrupted.
    //              The deliver -> handler -> rt_sigreturn -> deliver chain can stay
    //              saturated indefinitely, and the loop never gets a look in.
    //              See `threading::DELIVERED_SIGNALS` and
    //              docs/archive/PTHREAD_KILL_EINTR_DELIVERY_STARVATION.md.
    let pending = crate::threading::pending_signals_raw(tid);
    let delivered = crate::threading::delivered_signals_raw(tid);
    if pending | delivered == 0 {
        return false;
    }
    let deliverable = (pending | delivered) & !crate::threading::thread_signal_mask();
    if deliverable == 0 {
        return false;
    }

    let Some(pid) = read_current_pid() else { return false };
    let Some(proc) = crate::process::lookup_process_shared(pid) else { return false };

    /// AArch64 Linux `SA_RESTART`.
    const SA_RESTART: u64 = 0x1000_0000;

    let actions = proc.signal_actions.actions.lock();
    let mut bits = deliverable;
    while bits != 0 {
        // Bit `i` is signal `i + 1`, and `actions` is indexed by `sig - 1`, so
        // the bit index indexes `actions` directly.
        let idx = bits.trailing_zeros() as usize;
        bits &= bits - 1;
        if idx >= crate::process::types::MAX_SIGNALS {
            continue;
        }
        let action = actions[idx];
        if matches!(action.handler, crate::process::SignalHandler::UserFn(_))
            && action.flags & SA_RESTART == 0
        {
            // Consume the delivered record for THIS signal only, and only once
            // we have decided it produces an EINTR — otherwise a single delivery
            // could interrupt several later syscalls in a row. `pending` bits are
            // left alone: the delivery path still owns those.
            crate::threading::consume_delivered_signals(tid, 1u64 << idx);
            return true;
        }
    }
    false
}

/// Should the current blocking syscall give up and return `EINTR`?
///
/// Combines the deferred thread-kill path ([`crate::threading::has_pending_kill`]),
/// the process-wide Ctrl-C / `sys_kill` path ([`is_current_interrupted`]) and the
/// per-thread `pthread_kill` path ([`current_thread_has_pending_interrupt`]).
/// Blocking loops should call this rather than any one half.
///
/// **The kill arm is checked first, and it is not a signal.**
/// `kill_thread_group` does not hard-terminate a sibling — that would leak the
/// locks it holds — it posts a deferred request via
/// [`crate::threading::request_thread_kill`] and *wakes* the thread, expecting it
/// to self-terminate at its EL1→EL0 boundary. A thread blocked in an untimed wait
/// wakes, finds no signal and no readiness, and parks again: it never reaches a
/// boundary, so it can only die when the 2 s `KILL_GRACE_US` expires and the
/// group kill hard-terminates it. Every blocking family had that hole — the wake
/// was delivered and then discarded by the very loop it was meant to end.
///
/// Measured 2026-08-30: **60 `[ktg] grace expired` lines in a 90 s guest `cargo`
/// build**, each one a guaranteed full 2 s stall in `exit_group`. See
/// `docs/archive/KTG_GRACE_EXPIRY_KILL_INTERRUPT.md`.
///
/// Returning `EINTR` here is safe precisely because the thread is dying: it
/// unwinds to the EL1→EL0 boundary, `take_thread_kill_request` fires, and it
/// self-terminates without ever handing the errno to userspace. The flag is
/// cleared on slot reset and on recycle (`threading/mod.rs`), so a recycled slot's
/// next occupant cannot inherit a spurious interrupt.
pub fn should_interrupt_blocking_syscall() -> bool {
    if crate::threading::has_pending_kill(crate::threading::current_thread_id()) {
        return true;
    }
    if is_current_interrupted() {
        return true;
    }
    crate::runtime::config().pthread_kill_eintr_enabled
        && current_thread_has_pending_interrupt()
}

/// Count of `read_current_pid` tgid resolutions that fell back to the thread's own pid
/// because the process table would not resolve the mapped pid. Non-zero is the signature
/// of the identity-degradation window described at the fallback site.
pub static TGID_RESOLVE_MISSES: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);

/// Read the current process PID from the process info page.
///
/// During a syscall, TTBR0 is still set to the user's page tables, so reading
/// from `PROCESS_INFO_ADDR` gives us the calling process's PID. This prevents PID
/// spoofing since the page is read-only for userspace.
///
/// Returns `None` when there is no user process context: TTBR0 is the boot page
/// tables, or the live TTBR0 has no process info page mapped (a bare
/// `UserAddressSpace`), or the page says pid 0.
pub fn read_current_pid() -> Option<Pid> {
    // Identity cache fast path: one validated slot-state load replaces the
    // lock + map walk + tgid table-scan this function used to pay on every
    // resolution (several per syscall — see `table::THREAD_IDENTITY`).
    if let Some((tgid, _)) = crate::process::table::current_thread_tgid_process() {
        return Some(tgid);
    }
    // vfork fast-path: a shared-AS child reads the *parent's* PROCESS_INFO page,
    // so the page no longer uniquely identifies the caller.  THREAD_PID_MAP is
    // authoritative for every user thread; resolve it to the owning process's
    // tgid.  This is behavior-preserving for normal threads (page pid == tgid
    // leader, so callers including getpid see the same value) and gives a vfork
    // child its own pid (its tgid == its pid).  Gated so toggling the fast-path
    // off restores the exact prior page-only behavior.
    if crate::runtime::config().vfork_fastpath_enabled {
        let tid = crate::threading::current_thread_id();
        let mapped = with_irqs_disabled(|| THREAD_PID_MAP.lock().get(&tid).copied());
        if let Some(pid) = mapped {
            return Some(match crate::process::table::with_process(pid, |p| p.tgid) {
                Some(tgid) => tgid,
                // The map named a pid the process table will not resolve. `with_process`
                // only matches slots in state ACTIVE, so this is the window between
                // `unregister_process`'s ACTIVE→RETIRED CAS and the thread's last
                // instruction — the identity silently degrades from tgid to the thread's
                // OWN pid for whatever runs in it.
                //
                // That matters most for `futex_key_tgid`: a non-leader thread degrading
                // here parks on key `(own_pid, uaddr)` while every waker publishes to
                // `(tgid, uaddr)`, which is a lost wakeup that only affects multi-threaded
                // processes. Counted and logged (first 10) rather than silently absorbed —
                // an earlier pass wrongly blamed the sibling `unwrap_or(0)` for this class
                // without ever confirming it fired.
                None => {
                    let n = TGID_RESOLVE_MISSES.fetch_add(1, core::sync::atomic::Ordering::Relaxed) + 1;
                    if n <= 10 {
                        (runtime().print_str)(
                            "[identity] WARNING: THREAD_PID_MAP pid not ACTIVE in process table \
                             — tgid degraded to own pid (futex keys may not match wakers)\n");
                    }
                    pid
                }
            });
        }
        // No THREAD_PID_MAP entry → fall through to the page read below
        // (early boot, or a thread not yet registered).
    }
    // CRITICAL: Check TTBR0 before reading from user address space!
    //
    // PROCESS_INFO_ADDR (0x1000) is only mapped in USER page tables.
    // With boot TTBR0, address 0x1000 is in the device memory region (0x0-0x40000000)
    // and reading from it returns garbage, causing FAR=0x5 crashes.
    let ttbr0: u64 = akuma_cpu::sysreg::ttbr0_el1();
    
    // Compare against actual boot TTBR0, not a range check.
    // User page tables are allocated from the same physical memory pool,
    // so they can have addresses in the same range as boot tables.
    let boot_ttbr0 = crate::mmu::get_boot_ttbr0();
    let ttbr0_addr = ttbr0 & 0x0000_FFFF_FFFF_FFFF; // Mask off ASID bits
    if ttbr0_addr == boot_ttbr0 {
        return None; // Boot TTBR0 - no user process context
    }

    // "Not the boot address space" is NOT "there is a process here". A bare
    // `UserAddressSpace::new()` — which several boot tests construct and
    // `activate()` — is a non-boot TTBR0 with nothing mapped at 0x1000, so the
    // check above passes and the read below is a wild EL1 access that wedges the
    // VM with no output (docs/archive/USER_COPY_FOLD.md §7; the first version of
    // `test_kernel_va_rejected_as_user_pointer` died exactly this way, reaching
    // here through `address_space_owner_pid_for_fault`'s fallback).
    //
    // So ask the page tables directly, which is the question the `unsafe` below
    // actually depends on. §7 proposed `owner_pid_for_l0_phys` ("is this L0 owned
    // by a live process") instead; that answers a strictly different question —
    // identity, not mappedness — and costs an O(MAX_PROCESSES) table scan on a
    // path kernel threads take repeatedly, where this costs a four-level walk.
    // It also would not have caught anything this misses: an L0 with no process
    // info page mapped is exactly the case that wedges.
    //
    // The AP-gated predicate rather than the presence one because this is a read
    // *of user memory from EL1* — the same question `validate_user_range` asks of
    // any syscall buffer. Reaching it through `user_access` would recurse
    // (`validate_user_range` → `address_space_owner_pid_for_fault` → here), hence
    // the raw `mmu` call. The page is mapped `user_flags::RO` (`AP_RO_ALL`, EL0
    // bit set) at every site that maps it — `image.rs` on exec, `mod.rs` on fork
    // and on the post-CoW re-map — so it passes.
    // Read from the fixed address in the current address space. `read_current_user_val`
    // does the AP-gated presence check and the EL1 read behind `akuma-mmu`'s
    // page-table contract — it deliberately does not route through
    // `akuma-user-access`, whose `validate_user_range` would recurse back through
    // `address_space_owner_pid_for_fault` into this function.
    let pid = crate::mmu::read_current_user_val::<ProcessInfo>(PROCESS_INFO_ADDR)?.pid;
    if pid == 0 { None } else { Some(pid) }
}

// `lookup_process(pid) -> Option<&'static mut Process>` is GONE (Phase 7e
// "Access" half, 2026-08-01): two cores could each materialize `&'static mut`
// to the same `Process` (aliasing UB), and the `'static` lifetime structurally
// outlives the RETIRED→FREE deferred reclamation the "Free" half introduced.
// Use [`lookup_process_shared`] for reads and `&self` methods,
// `table::with_process` for short IRQ-masked field writes, and
// `table::with_process_exclusive` (unsafe, enumerated call sites only) for the
// execve/first-run lifecycle windows Phase 7f owns.

/// Look up a process by PID, returning a **shared** `&'static Process`.
///
/// The shared-kernel-SMP (M5b) BKL-free page-fault path uses this instead of
/// [`lookup_process`] so two cores faulting in different address spaces don't both
/// materialize `&'static mut` to the same object (aliasing UB). Every address-space
/// mutation the fault path needs is a `&self` method (`track_user_frame`,
/// `track_page_table_frame`, `vm_with_regions`, `with_address_space`) or a free function
/// (`mmu::map_user_page*`); the actual cross-core mutual exclusion on the raw
/// page-table writes comes from [`Process::address_space`]'s lock, not from
/// `&mut` exclusivity.
///
/// # Safety warning
/// Same lifetime caveat as [`lookup_process`]: valid only while the process stays
/// registered. The fault fast path only ever looks up its **own** live thread-group
/// leader (`as_owner`), which cannot be freed while the faulting thread runs, so the
/// reference is sound there. Foreign-PID lookups must stay on the BKL slow path.
pub fn lookup_process_shared(pid: Pid) -> Option<&'static Process> {
    crate::process::table::active_process_ref(pid)
}

/// Outcome of [`fault_slot_acquire`] — how the per-page demand-paging slot was won.
pub enum FaultSlot {
    /// No address-space-owner process is registered; caller skips serialization.
    NoProc,
    /// Slot was free and acquired cleanly.
    Acquired,
    /// **This thread already held the slot** for this page — a re-entrant acquire.
    ///
    /// Unreachable in this tree (all three call sites are mutually exclusive
    /// branches of `rust_sync_el0_handler_inner`, which cannot re-enter itself: a
    /// fault taken while it runs comes from EL1 and takes the EL1 vector, and
    /// nothing on that path touches a fault slot). It exists to make the outcome
    /// *nameable*, because the alternative was silently wrong: this used to return
    /// [`FaultSlot::Acquired`], so the inner RAII guard's release removed the
    /// **outer** guard's entry and the page ran unserialized for the rest of the
    /// outer critical section, with the outer release a no-op.
    ///
    /// The caller must **not** pair this with a [`fault_slot_release`] — the
    /// outermost holder still owns the release. See `COW_PILE_AUDIT.md` §9 F6.
    AlreadyHeld,
    /// Slot was reclaimed from a holder thread that had already died
    /// (TERMINATED/FREE) without releasing it — the root-cause poison recovery.
    /// Carries the dead holder's thread id.
    ReclaimedDead(usize),
    /// Slot was force-reclaimed after spinning past the safety bound: the holder
    /// neither released nor visibly died (wedged, or its slot was recycled to a
    /// live thread). Carries the stale holder id. Should be vanishingly rare.
    ReclaimedWedged(usize),
}

/// Why a [`FaultSlot`] acquisition had to take the slot away from its recorded
/// holder. Carried out of [`FaultSlot::reclaim_report`] so the caller can pick a
/// message without re-matching the enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReclaimCause {
    /// The holder thread had already died without releasing — the root-cause
    /// poison recovery, and the smoking gun for a build-script deadlock.
    Dead,
    /// The holder neither released nor visibly died within
    /// [`FAULT_SLOT_SPIN_BOUND`]; the bounded fallback fired.
    Wedged,
}

impl FaultSlot {
    /// The decision behind the reclaim trace: `Some((cause, holder_tid))` for the
    /// two reclaim outcomes, `None` for the hot `Acquired` / `NoProc` paths that
    /// must print nothing.
    ///
    /// Split out from the caller's `safe_print!` so the *decision* is
    /// host-testable while the *effect* stays on the fault path — the same shape
    /// as `trace_snippet` in `process/channel.rs`. Callers must keep the silent
    /// arm silent: this runs on every demand-paging fault, and a print here is a
    /// console write per page.
    #[must_use]
    pub fn reclaim_report(&self) -> Option<(ReclaimCause, usize)> {
        match *self {
            FaultSlot::ReclaimedDead(holder) => Some((ReclaimCause::Dead, holder)),
            FaultSlot::ReclaimedWedged(holder) => Some((ReclaimCause::Wedged, holder)),
            FaultSlot::NoProc | FaultSlot::Acquired | FaultSlot::AlreadyHeld => None,
        }
    }
}

/// Spin bound before [`fault_slot_acquire`] force-reclaims a slot. Generous: any
/// legitimate concurrent demand-paging of the same page completes in well under
/// this many cooperative yields; reaching it means the holder is wedged.
const FAULT_SLOT_SPIN_BOUND: u32 = 200_000;

/// Acquire the per-page demand-paging serialization slot for `page_va` on the
/// address-space-owner process `as_owner`, recording the calling thread as the
/// holder. Serializes concurrent faults on the same page across CLONE_VM threads
/// (the leader holds the shared `fault_mutex`).
///
/// Unlike the previous raw `BTreeSet` spin-loop, this can never deadlock: if the
/// recorded holder thread has died (its RAII release guard never ran because a
/// kernel thread teardown abandons the stack rather than unwinding), a sibling
/// reclaims the slot instead of spinning forever. A bounded fallback also covers
/// a wedged or slot-recycled holder.
///
/// The caller MUST pair a successful (`Acquired`/`Reclaimed*`) return with exactly
/// one [`fault_slot_release`] — normally via an RAII guard. [`FaultSlot::NoProc`]
/// may be released harmlessly (the release is holder-gated), but
/// [`FaultSlot::AlreadyHeld`] must **not** be: the outer holder owns that entry,
/// and releasing it here is exactly the bug that variant exists to name.
pub fn fault_slot_acquire(as_owner: Pid, page_va: usize) -> FaultSlot {
    let my_tid = crate::threading::current_thread_id();
    let mut spins: u32 = 0;
    loop {
        // IRQ-safe critical section. `fault_mutex` is a shared spinlock on the
        // EL0 demand-paging path; like every other such lock here it must be
        // taken with IRQs disabled, otherwise a holder could be preempted by the
        // timer/SGI mid-section while a CLONE_VM sibling (which shares the
        // leader's one `fault_mutex`) contended on it — and a contender that
        // reached the lock with IRQs already masked (a nested/EL1-side fault,
        // or any call site inside an `IrqGuard`) would spin on a preempted
        // holder that can never be rescheduled (timer masked). Masking here
        // guarantees the holder can never be preempted while holding the slot
        // on a single CPU. `with_irqs_disabled` is reentrant and nests fine
        // with the IRQ-safe heap lock that `BTreeMap::insert` may touch. The
        // `yield_now()` below stays OUTSIDE the IRQ-disabled region so the
        // scheduler + IRQs keep making progress while we wait. (Correct hygiene
        // — but note this was investigated and is *not* the `curl https` freeze;
        // see docs/OPTIONAL_SMOLTCP.md: that was `clone_thread` handing the
        // child a stale TTBR0.)
        let outcome = with_irqs_disabled(|| {
            let proc = match lookup_process_shared(as_owner) {
                Some(p) => p,
                None => return Some(FaultSlot::NoProc),
            };
            let mut faults = proc.fault_mutex.lock();
            match faults.get(&page_va).copied() {
                None => {
                    faults.insert(page_va, my_tid);
                    Some(FaultSlot::Acquired)
                }
                // Re-entrant acquire by the holder itself. NOT `Acquired`: the
                // caller must not release, or the inner guard drops the outer
                // guard's entry (see `FaultSlot::AlreadyHeld`).
                Some(holder) if holder == my_tid => Some(FaultSlot::AlreadyHeld),
                Some(holder) => {
                    if crate::threading::is_thread_terminated(holder) {
                        faults.insert(page_va, my_tid);
                        return Some(FaultSlot::ReclaimedDead(holder));
                    }
                    if spins >= FAULT_SLOT_SPIN_BOUND {
                        faults.insert(page_va, my_tid);
                        return Some(FaultSlot::ReclaimedWedged(holder));
                    }
                    None // contended — retry after yielding (IRQs on)
                }
            }
        });
        if let Some(slot) = outcome {
            return slot;
        }
        spins = spins.wrapping_add(1);
        // Wait for the slot holder to release, DROPPING the Big Kernel Lock under
        // shared-kernel SMP. The holder may be a CLONE_VM sibling doing its fault
        // block I/O on a peer core (M5b BKL-dropped file-fault path); it must be able
        // to re-take the BKL to release the slot, which it can't if we spin holding it
        // (the bounded `FAULT_SLOT_SPIN_BOUND` reclaim above only papers over that).
        crate::threading::blocking_relax();
    }
}

/// Release the per-page demand-paging slot for `page_va`, but only if the calling
/// thread still owns it. If a sibling reclaimed the slot (because we were assumed
/// dead/wedged), we must NOT remove its entry — the reclaimer releases it.
pub fn fault_slot_release(as_owner: Pid, page_va: usize) {
    let my_tid = crate::threading::current_thread_id();
    // IRQ-safe critical section — same discipline as `fault_slot_acquire`. Reached
    // from the EL0 demand-paging fault path (IRQs-enabled), and contended across
    // CLONE_VM siblings sharing the leader's one `fault_mutex`.
    with_irqs_disabled(|| {
        if let Some(proc) = lookup_process_shared(as_owner) {
            let mut faults = proc.fault_mutex.lock();
            if faults.get(&page_va).copied() == Some(my_tid) {
                faults.remove(&page_va);
            }
        }
    });
}

// `current_process() -> Option<&'static mut Process>` is GONE — see the note
// above `lookup_process_shared`. Use [`current_process_shared`] /
// [`with_current_process`].

/// Get the current process as a **shared** `&'static Process` — the
/// [`lookup_process_shared`] counterpart of [`current_process`], resolving the
/// PID the same way (thread-to-PID map first, then the ProcessInfo page).
///
/// This is the accessor for the Phase 7e "Access" migration: reads and calls
/// to `&self` methods (fd table, `vm_*`, `with_address_space`, `Arc` fields) go
/// through this; plain-field *writes* go through [`with_current_process`] /
/// `table::with_process` instead, so no call site materializes a long-lived
/// `&'static mut Process` (aliasing UB when two cores hold one to the same
/// object). Same lifetime caveat as [`lookup_process_shared`]: valid only
/// while the process stays registered.
/// The calling process's credentials, snapshotted. A kernel thread with no
/// process is root — the same "kernel thread is box 0" convention the box
/// checks use.
#[must_use]
pub fn current_creds() -> super::Creds {
    current_process_shared().map_or(super::Creds::ROOT, |p| p.creds.get())
}

pub fn current_process_shared() -> Option<&'static Process> {
    // Identity cache fast path (own-process half): the map value's `Process`
    // in two validated loads — same resolution, same fallbacks.
    if let Some((_, proc)) = crate::process::table::current_thread_own_process() {
        return Some(proc);
    }
    lookup_process_shared(current_pid()?)
}

/// Run `f` on the current process with IRQs disabled — the current-process
/// counterpart of `table::with_process`, resolving the PID exactly like
/// [`current_process`] (thread-to-PID map first, then the ProcessInfo page).
///
/// Same contract as `with_process`: the callback runs with IRQs disabled and
/// MUST NOT allocate on the heap (moving a pre-built value into a field, or
/// letting the replaced value drop, is fine — deallocation cannot re-enter the
/// OOM path). Use it for short plain-field writes and scalar copies; use
/// [`current_process_shared`] for reads and `&self` methods.
pub fn with_current_process<T>(f: impl FnOnce(&mut Process) -> T) -> Option<T> {
    crate::process::table::with_process(current_pid()?, f)
}

/// Resolve the current process PID (checking THREAD_PID_MAP first, then ProcessInfo page).
pub fn current_pid() -> Option<Pid> {
    // Identity cache fast path — the map's own-pid value, without the lock.
    if let Some((pid, _)) = crate::process::table::current_thread_own_process() {
        return Some(pid);
    }
    let tid = crate::threading::current_thread_id();
    let thread_pid = with_irqs_disabled(|| {
        THREAD_PID_MAP.lock().get(&tid).copied()
    });
    if thread_pid.is_some() { return thread_pid; }
    read_current_pid()
}

/// Get the current process's TerminalState (for syscall handlers)
///
/// Returns a mutable reference to the TerminalState if found.
pub fn current_terminal_state() -> Option<Arc<Spinlock<terminal::TerminalState>>> {
    // 1. Try thread-ID based lookup (for system threads or overridden processes)
    let tid = crate::threading::current_thread_id();
    if let Some(state) = crate::process::channel::get_terminal_state(tid) {
        return Some(state);
    }

    // 2. Fallback to process table
    current_process_shared().map(|p| p.terminal_state.clone())
}

/// Allocate mmap region for current process
/// Returns the address or 0 on failure
pub fn alloc_mmap(size: usize) -> usize {
    // Use address-space owner so CLONE_VM threads share allocation state.
    let pid = address_space_owner_pid_for_fault().unwrap_or(0);
    let proc = match lookup_process_shared(pid) {
        Some(p) => p,
        None => {
            (runtime().print_str)("[mmap] ERROR: No current process\n");
            return 0;
        }
    };

    // Use per-process memory tracking
    match proc.vm_alloc_mmap(size) {
        Some(addr) => addr,
        None => {
            log::debug!("[mmap] REJECT: pid={} size=0x{:x} next=0x{:x} limit=0x{:x}",
                proc.pid, size, proc.memory.next_mmap.load(core::sync::atomic::Ordering::Relaxed), proc.memory.mmap_limit.load(core::sync::atomic::Ordering::Relaxed));
            0
        }
    }
}

// ==========================================================================
// Lazy (demand-paged) regions.
//
// A lazy region is an `MmapRegion` with `lazy` set, in the same sorted
// `Process::mmap_regions` list as the eager ones (step 4b of
// `proposals/AKUMA_MM_CONVERGENCE.md`). It used to be a `LazyRegion` in a
// per-process `BTreeMap` of its own (`LazyRegionMap`), which is the list the
// amd64 kernel never had; one list is what lets the two kernels' fault paths
// read the same record.
//
// The pid-keyed API below (`lazy_region_lookup*`, `push_lazy_region*`,
// `munmap_lazy_regions_in_range`, ...) kept its names and shapes: a lookup
// still hands back `(flags word, LazySource, start, size)`, the `LazySource`
// rebuilt from the region's `FileBacking` (`lazy_source_of`). The pure layer
// under it (`lazy_mmap_region`, `insert_lazy_region`, `lazy_lookup`,
// `detach_lazy`) takes a `&mut Vec<MmapRegion>` and is host-tested without a
// process.
//
// What keeps a mapped file alive is the address space's `MappingPins`
// (`akuma-user-space`), taken on push and released when no region names the
// inode any more. A `LazySource` handed out by a lookup carries a pin of its
// own, held only while the caller reads the file.
//
// The rule-2 hang that moved the old global table onto `Process`
// (docs/archive/LAZY_REGION_TABLE_ALLOC_UNDER_LOCK_HANG.md) still does not
// apply: the list drops with the `Process`, and no teardown path takes
// `mmap_regions`' lock on a dying process.
// ==========================================================================
/// The protection an AArch64 `user_flags` word records: `0` is "none recorded",
/// anything else its [`Prot`](akuma_mmap::Prot). Exact for every word this tree
/// stores (the six [`to_pte`](crate::mmu::user_flags::to_pte) encodings), which
/// [`region_word`] inverts.
fn region_prot(word: u64) -> Option<akuma_mmap::Prot> {
    (word != 0).then(|| crate::mmu::user_flags::from_pte(word))
}

/// The AArch64 word for a region's recorded protection: [`region_prot`] read
/// backwards. Where the map's callers still speak the PTE word, until the fault
/// path takes a `Prot` (step 2 of `proposals/AKUMA_MM_CONVERGENCE.md`).
fn region_word(prot: Option<akuma_mmap::Prot>) -> u64 {
    prot.map_or(0, crate::mmu::user_flags::to_pte)
}

/// The `MmapRegion` that records a lazy region of `size` bytes at `start_va`
/// reading from `source`.
///
/// A file source becomes a [`FileBacking`](akuma_mmap::FileBacking) whose
/// `lead` is the distance from the region's first page to the segment's first
/// byte; [`lazy_source_of`] reads it back. A source whose `segment_va` lies
/// below `start_va` (a piece of a clipped region) is advanced to `start_va`
/// first, the same arithmetic `FileBacking::advance` applies to a clip.
#[must_use]
pub fn lazy_mmap_region(start_va: usize, size: usize, flags: u64, source: &LazySource) -> MmapRegion {
    debug_assert!(start_va % crate::mmu::PAGE_SIZE == 0 && size % crate::mmu::PAGE_SIZE == 0,
        "a lazy region is whole pages");
    let region = MmapRegion::demand_paged(start_va, size.div_ceil(crate::mmu::PAGE_SIZE), region_prot(flags));
    match *source {
        LazySource::File { inode, mount_id, file_offset, filesz, segment_va, .. } => {
            let file = if segment_va >= start_va {
                akuma_mmap::FileBacking { mount_id, inode, offset: file_offset, filesz, lead: segment_va - start_va }
            } else {
                let skip = start_va - segment_va;
                akuma_mmap::FileBacking {
                    mount_id,
                    inode,
                    offset: file_offset.saturating_add(skip),
                    filesz: filesz.saturating_sub(skip),
                    lead: 0,
                }
            };
            region.file_backed(file)
        }
        LazySource::Zero => region,
    }
}

/// The [`LazySource`] a lazy region reads from: [`lazy_mmap_region`] read
/// backwards. Takes a pin on a file's inode, which the caller holds while it
/// reads the file and drops after.
#[must_use]
pub fn lazy_source_of(region: &MmapRegion) -> LazySource {
    match region.file {
        Some(f) => LazySource::file(f.mount_id, f.inode, f.offset, f.filesz, region.start_va + f.lead),
        None => LazySource::Zero,
    }
}

/// `(flags word, source, start_va, size)` for a lazy region: the shape every
/// lookup has always returned. The word is `0` when the region records no
/// protection ([`region_word`]).
fn lazy_view(region: &MmapRegion) -> (u64, LazySource, usize, usize) {
    (region_word(region.recorded_prot()), lazy_source_of(region), region.start_va, region.len_bytes())
}

/// Set once a lazy region was pushed over an eager one. The list is then no
/// longer free of overlaps, which [`region_index_containing`]'s binary search
/// relies on, so [`lazy_lookup`] falls back to a scan.
static LAZY_OVER_EAGER: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// The lazy region containing `va`, as [`lazy_view`] describes it.
#[must_use]
pub fn lazy_lookup(regions: &[MmapRegion], va: usize) -> Option<(u64, LazySource, usize, usize)> {
    if let Some(i) = region_index_containing(regions, va) {
        if regions[i].lazy {
            return Some(lazy_view(&regions[i]));
        }
    }
    if LAZY_OVER_EAGER.load(core::sync::atomic::Ordering::Relaxed) == 0 {
        return None;
    }
    regions.iter().rev().find(|r| r.lazy && r.contains(va)).map(lazy_view)
}

/// Whether any region of the list reads from `inode`.
#[must_use]
pub fn regions_name_inode(regions: &[MmapRegion], inode: u32) -> bool {
    regions.iter().any(|r| r.file.is_some_and(|f| f.inode == inode))
}

/// The inodes a removal may have stopped mapping, collected under the region
/// lock without allocating. Holds up to four; an inode past that keeps its
/// address-space pin, which costs disk space until exit and never correctness.
#[derive(Default)]
pub struct OrphanedInodes {
    inodes: [u32; 4],
    n: usize,
}

impl OrphanedInodes {
    fn note(&mut self, regions: &[MmapRegion], inode: u32) {
        if inode == 0 || self.inodes[..self.n].contains(&inode) || regions_name_inode(regions, inode) {
            return;
        }
        if self.n < self.inodes.len() {
            self.inodes[self.n] = inode;
            self.n += 1;
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = u32> + '_ {
        self.inodes[..self.n].iter().copied()
    }

    /// Release the address space's pin on each.
    fn unpin(&self, proc: &Process) {
        if self.n > 0 {
            proc.with_address_space(|a| self.iter().for_each(|i| a.unpin_mapped_inode(i)));
        }
    }
}

/// What [`insert_lazy_region`] displaced.
#[derive(Default)]
pub struct LazyInsert {
    /// Files no region maps after the insert.
    pub orphaned: OrphanedInodes,
    /// The new region overlaps an eager one (see [`LAZY_OVER_EAGER`]).
    pub over_eager: bool,
}

/// Insert a lazy region, first removing whatever part of an existing lazy
/// region it covers: the later push wins, as it did when the map was keyed by
/// `start_va`. That case is real, not defensive: the ELF loader registers
/// adjacent segments that share a boundary page, and the later segment is the
/// one that serves it.
pub fn insert_lazy_region(regions: &mut Vec<MmapRegion>, region: MmapRegion) -> LazyInsert {
    debug_assert!(region.lazy);
    let (start, end) = (region.start_va, region.start_va + region.len_bytes());
    let new_inode = region.file.map_or(0, |f| f.inode);
    let removed = akuma_mmap::detach_lazy_regions_in_range(regions, start, end);
    let over_eager = akuma_mmap::regions_overlapping(regions, start, end)
        .iter()
        .any(|r| !r.lazy && r.start_va < end && start < r.start_va + r.len_bytes());
    insert_region_sorted(regions, region);
    let mut out = LazyInsert { over_eager, ..LazyInsert::default() };
    for piece in &removed {
        if let Some(f) = piece.file && f.inode != new_inode {
            out.orphaned.note(regions, f.inode);
        }
    }
    out
}

/// Remove the lazy regions' share of `[start, end)`, clipping at both ends.
/// Returns each removed piece as `(start_va, pages)`, and the files no region
/// maps afterwards.
pub fn detach_lazy(regions: &mut Vec<MmapRegion>, start: usize, end: usize) -> (Vec<(usize, usize)>, OrphanedInodes) {
    let removed = akuma_mmap::detach_lazy_regions_in_range(regions, start, end);
    let mut orphaned = OrphanedInodes::default();
    for piece in &removed {
        if let Some(f) = piece.file {
            orphaned.note(regions, f.inode);
        }
    }
    (removed.iter().map(|p| (p.start_va, p.pages)).collect(), orphaned)
}

/// Check if a virtual address falls within any lazy region of the current process.
/// Returns `(flags, source, region_start, region_size)` if found.
/// The source is built under the region lock and owned, so the caller can
/// release the lock before performing I/O.
pub fn lazy_region_lookup(va: usize) -> Option<(u64, LazySource, usize, usize)> {
    let pid = address_space_owner_pid_for_fault()?;
    lazy_region_lookup_for_pid(pid, va)
}

/// Number of lazy regions registered for `pid` (0 if the pid isn't registered).
pub fn lazy_region_count_for_pid(pid: Pid) -> usize {
    lookup_process_shared(pid).map_or(0, |p| p.vm_with_regions(|r| r.iter().filter(|x| x.lazy).count()))
}

pub fn lazy_region_lookup_for_pid(pid: Pid, va: usize) -> Option<(u64, LazySource, usize, usize)> {
    lookup_process_shared(pid).and_then(|p| p.vm_with_regions(|r| lazy_lookup(r, va)))
}
/// Rotating sweep cursor (VA) for [`reclaim_clean_file_pages`], so successive
/// reclaims page out across the whole file region (clock-like) instead of always
/// hitting the same low addresses.
static RECLAIM_CURSOR: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// Evict up to `want` clean, read-only, **file-backed** pages of the current
/// address space and return them to the PMM — the page-reclaim half of demand
/// paging that lets a file mmap larger than physical RAM make progress under
/// memory pressure (model weights are paged out and re-faulted from the file).
///
/// Only pages inside `LazySource::File` lazy regions are candidates, and only
/// those still mapped read-only (`try_evict_ro_page` re-checks the PTE), so anon
/// memory (stack/heap/compute buffers) and any CoW-dirtied page are never
/// touched. Allocates nothing (it runs on the OOM path): regions are snapshotted
/// onto the stack and frames are freed via the runtime hook. Returns the number
/// of pages freed. Called from `pmm::alloc_page_zeroed_user` before it declares
/// OOM.
pub fn reclaim_clean_file_pages(want: usize) -> usize {
    if want == 0 { return 0; }
    use core::sync::atomic::Ordering;

    let pid = match address_space_owner_pid_for_fault() {
        Some(p) => p,
        None => return 0,
    };

    let proc = match lookup_process_shared(pid) {
        Some(p) => p,
        None => return 0,
    };

    // Snapshot the file-backed regions onto the stack — no heap allocation, since
    // we are already under memory pressure and a Vec growth could recurse into
    // the allocator's OOM handler. 64 regions is ample (llama uses ~37 total).
    let mut regions: [(usize, usize); 64] = [(0, 0); 64];
    let n = proc.vm_with_regions(|r| {
        let mut n = 0usize;
        for reg in r.iter().filter(|reg| reg.lazy && reg.file.is_some()) {
            if n == regions.len() {
                break;
            }
            regions[n] = (reg.start_va, reg.len_bytes());
            n += 1;
        }
        n
    });
    if n == 0 { return 0; }

    // Cap pages scanned per call so a sparse (mostly-unmapped) region set can't
    // spin; eviction is the slow path, but it must still bound its own work.
    const MAX_SCAN: usize = 262_144; // up to ~1 GB of VA scanned per reclaim
    let cursor = RECLAIM_CURSOR.load(Ordering::Relaxed);
    let mut freed = 0usize;
    let mut scanned = 0usize;
    let mut next_cursor = 0usize; // 0 ⇒ wrap to the start next time

    'sweep: for i in 0..n {
        let (start, size) = regions[i];
        let end = start + size;
        // Resume from the cursor; regions are stored sorted by start_va.
        let mut va = if start < cursor { cursor & !0xFFF } else { start };
        if va >= end { continue; }
        while va < end {
            if freed >= want || scanned >= MAX_SCAN {
                next_cursor = va;
                break 'sweep;
            }
            scanned += 1;
            // `try_evict_ro_page` walks and clears a live PTE, same class of edit as
            // every other page-table mutation in this address space — it needs
            // `as_lock` to exclude a concurrent BKL-free fault (or a future BKL-free
            // mmap-family syscall) on the same page, exactly like the fault handler's
            // own per-page `as_lock` holds. One hold per page (not one hold spanning
            // the whole up-to-262144-page scan): this loop is a bounded but
            // potentially long sweep, and the AS-lock hold masks IRQs for its duration —
            // holding it across the entire sweep would starve this core's timer for
            // however long the scan runs, exactly the "mask per attempt, never across
            // an unbounded wait" rule (docs/reference/subsystems/locking.md).
            let evicted = proc.with_address_space(|aspace| aspace.try_evict_ro_page(va));
            if let Some(frame) = evicted {
                akuma_pmm::free_page(frame.addr, akuma_primitives::preempt::current_tid() as u32);
                freed += 1;
            }
            va += 0x1000;
        }
    }
    RECLAIM_CURSOR.store(next_cursor, Ordering::Relaxed);
    freed
}

/// Find the PID of the non-shared process whose address space's L0 page-table frame
/// matches `l0_phys`. CLONE_THREAD goroutines share an address space (is_shared==true),
/// so this returns the thread-group leader (the owner of the real page tables).
fn owner_pid_for_l0_phys(l0_phys: usize) -> Option<Pid> {
    find_process(|p| {
        if !p.address_space.is_shared() && p.address_space.l0_phys() == l0_phys {
            Some(p.pid)
        } else {
            None
        }
    })
}

/// The process whose address space `pid` runs in, with the L0 that was resolved.
///
/// `pid` itself for an address-space owner; for a `CLONE_VM` member (a thread, a
/// `vfork` child) the owner that holds the same L0. A member's own
/// [`Process::address_space`] has the owner's root but a **fresh lock** (see the
/// comment in `Process::new`), so taking it excludes nothing: the owner's `execve`
/// or exit frees the tables under a different lock. A walk of the tables must be
/// done on the owner, and must confirm under the owner's lock that the root is
/// still the returned one — the second element is what to compare it with.
///
/// `None` if `pid` is gone, or is a member whose owner has already exited.
pub fn address_space_owner_of(pid: Pid) -> Option<(Pid, usize)> {
    let (l0, shared) = crate::process::table::with_process(pid, |p| {
        (p.address_space.l0_phys(), p.address_space.is_shared())
    })?;
    if !shared {
        return Some((pid, l0));
    }
    owner_pid_for_l0_phys(l0).map(|owner| (owner, l0))
}

/// Thread group leader PID for page-fault / CoW paths: all `CLONE_VM` threads in a group must
/// share one [`Process::fault_mutex`] and resolve to the leader's [`Process::mmap_regions`]
/// (forktest / GO_FORKTEST_DEBUG).
///
/// Uses TTBR0-derived lookup as the primary mechanism: the current TTBR0_EL1 unambiguously
/// identifies the running address space regardless of THREAD_PID_MAP state.  Stale
/// THREAD_PID_MAP entries (e.g. when a kernel thread slot is reused for a different process)
/// would otherwise cause the demand-pager to look up lazy regions under the wrong PID,
/// triggering an EL1 copy-path fault and delivering a spurious SIGSEGV to the wrong process.
pub fn address_space_owner_pid_for_fault() -> Option<Pid> {
    // TTBR0 identifies the running address space with certainty.  Find the non-shared
    // process (i.e. the thread-group leader) that owns this L0 frame.
    let ttbr0 = crate::mmu::get_current_ttbr0() as usize;
    let boot_ttbr0 = crate::mmu::get_boot_ttbr0() as usize;
    let l0_phys = ttbr0 & 0x0000_FFFF_FFFF_F000;
    if l0_phys != 0 && l0_phys != boot_ttbr0 {
        if let Some(pid) = owner_pid_for_l0_phys(l0_phys) {
            return Some(pid);
        }
    }
    // Fallback: THREAD_PID_MAP tgid, then ProcessInfo page.
    current_process_shared().map(|p| p.tgid).or_else(read_current_pid)
}

/// Like [`lazy_region_lookup_for_pid`], but resolves demand-paging metadata keyed by the
/// thread-group id ([`Process::tgid`]) first — the same key as `sys_mmap` uses via `proc.tgid`
/// — then falls back to `pid` (e.g. [`read_current_pid`] from EL0).
///
/// Ordering matters when only the leader's `Process` holds the regions but the caller
/// passes another thread id (clone snapshot keys, or stale ProcessInfo).
///
/// The fallback resolves `pid`'s own address-space owner, since a `CLONE_VM`
/// member's list is empty: its group's regions are on the leader's.
pub fn lazy_region_lookup_for_page_fault(pid: Pid, va: usize) -> Option<(u64, LazySource, usize, usize)> {
    if let Some(owner) = address_space_owner_pid_for_fault() {
        if let Some(r) = lazy_region_lookup_for_pid(owner, va) {
            return Some(r);
        }
    }
    let owner = address_space_owner_of(pid).map_or(pid, |(owner, _)| owner);
    lazy_region_lookup_for_pid(owner, va)
}

// `LazyDebugWriter` was a third copy of the stack writer; `lazy_region_debug`
// below uses the shared `akuma_primitives::console::StackWriter` instead.

pub fn lazy_region_debug(va: usize) {
    use core::fmt::Write;
    let pid = address_space_owner_pid_for_fault().unwrap_or(0);
    let Some(proc) = lookup_process_shared(pid) else {
        crate::safe_print!(128, "[DP] lazy miss: pid={} va={:#x} no process entry\n", pid, va);
        return;
    };
    proc.vm_with_regions(|r| {
        let count = r.iter().filter(|x| x.lazy).count();
        let mut w = akuma_primitives::console::StackWriter::<256>::new();
        let _ = write!(w, "[DP] lazy miss: pid={} va={:#x} regions={} [", pid, va, count);
        for (i, reg) in r.iter().filter(|x| x.lazy).take(8).enumerate() {
            if i > 0 { let _ = w.write_str(","); }
            let _ = write!(w, "{:#x}+{:#x}", reg.start_va, reg.len_bytes());
        }
        let _ = w.write_str("]\n");
        w.flush();
    });
}

pub fn push_lazy_region(pid: Pid, start_va: usize, size: usize, page_flags: u64) -> usize {
    push_lazy_region_with_source(pid, start_va, size, page_flags, LazySource::Zero)
}

/// Register a lazy region on `pid`'s list. Returns the list's length after.
pub fn push_lazy_region_with_source(pid: Pid, start_va: usize, size: usize, page_flags: u64, source: LazySource) -> usize {
    push_lazy_mmap_region(pid, lazy_mmap_region(start_va, size, page_flags, &source))
}

/// Register a prebuilt lazy region (a file-backed one may carry a
/// `shared_write` record) on `pid`'s list, pinning its file. Returns the
/// list's length after.
pub fn push_lazy_mmap_region(pid: Pid, region: MmapRegion) -> usize {
    let Some(proc) = lookup_process_shared(pid) else { return 0 };
    let (start_va, size) = (region.start_va, region.len_bytes());
    let inode = region.file.map_or(0, |f| f.inode);
    let (inserted, count) = proc.vm_with_regions(|r| {
        let ins = insert_lazy_region(r, region);
        (ins, r.len())
    });
    note_lazy_over_eager(pid, start_va, size, inserted.over_eager);
    // The address space's pin. `pid` is the thread-group id, so this is the
    // owner's address space, not a thread's view. Taken after the region lock
    // is released: the two never nest.
    if inode != 0 {
        proc.with_address_space(|a| a.pin_mapped_inode(inode));
    }
    inserted.orphaned.unpin(proc);
    count
}

/// Report a lazy region pushed over an eager one, and switch [`lazy_lookup`]
/// to the scan that still finds it.
pub fn note_lazy_over_eager(pid: Pid, start_va: usize, size: usize, over: bool) {
    if !over {
        return;
    }
    let seen = LAZY_OVER_EAGER.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    if seen < 8 {
        crate::safe_print!(128, "[LR-OVERLAP] pid={} lazy {:#x}+{:#x} overlaps an eager region\n",
            pid, start_va, size);
    }
}

/// How many lazy regions were pushed over an eager one since boot.
pub fn lazy_over_eager_count() -> usize {
    LAZY_OVER_EAGER.load(core::sync::atomic::Ordering::Relaxed)
}

/// Derive a CoW-forked child's `mmap_regions` from its parent's, and `munmap`'s
/// clip-and-split over a region list — both re-exported from `akuma-mmap`.
///
/// Neither ever took a process: one maps a slice, the other rewrites a `&mut Vec`.
/// They moved with `MmapRegion` and took their ~230 lines of host tests with them,
/// which is the whole point — region splitting is where this code has gone wrong
/// before (`docs/archive/CARGO_HEAP_NULL_RC.md` D8/D9,
/// `docs/archive/FORK_EXEC_HEAP_LAZY_REGION_SIGSEGV.md`) and every shape that
/// matters is now pinned without a live process. The pid-keyed accessors below
/// (`eager_region_flags_for_page_fault`, `update_eager_region_flags`,
/// `munmap_lazy_regions_in_range`, …) stay here: each resolves a process and takes
/// `vm_lock` before it can touch a region list.
pub use akuma_mmap::{
    detach_eager_regions_in_range, inherit_mmap_regions_for_cow_child, insert_region_sorted,
    region_index_containing, regions_overlapping,
};

/// Protection recorded for the **eager** mmap region covering `va`, if any.
///
/// The eager counterpart of [`lazy_region_lookup_for_page_fault`], and resolved the
/// same way: eager regions live on the address-space owner's `mmap_regions`, so a
/// CLONE_VM worker must look them up under the owner, not its own pid.
///
/// Used by the EL0 write-permission-fault handler to decide whether a read-only PTE
/// inside an eager mapping is a repairable inconsistency or a genuine access
/// violation.
pub fn eager_region_flags_for_page_fault(pid: Pid, va: usize) -> Option<u64> {
    let lookup = |p: Pid| -> Option<u64> {
        let proc = lookup_process_shared(p)?;
        proc.vm_with_regions(|r| {
            r.iter().find(|reg| !reg.lazy && reg.contains(va)).map(|reg| crate::mmu::user_flags::to_pte(reg.prot))
        })
    };
    if let Some(owner) = address_space_owner_pid_for_fault()
        && let Some(f) = lookup(owner)
    {
        return Some(f);
    }
    lookup(pid)
}

/// The protection an eager region **states** for `va`, if it states one.
///
/// The *deny*-side counterpart of [`eager_region_flags_for_page_fault`]. That one
/// answers "what flags does the record hold", which is right for granting a write
/// — the unrecorded default is `NONE` and grants nothing. This one answers "did
/// the region actually say anything", which is what a refusal needs: treating an
/// unrecorded region as read-only refuses legitimate CoW breaks and kills `rustc`.
///
/// See `MmapRegion::prot_recorded` for the `NONE`-is-two-facts problem this exists
/// to resolve.
pub fn eager_region_recorded_prot_for_page_fault(pid: Pid, va: usize) -> Option<u64> {
    let lookup = |p: Pid| -> Option<u64> {
        let proc = lookup_process_shared(p)?;
        proc.vm_with_regions(|r| {
            r.iter()
                .find(|reg| !reg.lazy && reg.contains(va))
                .and_then(MmapRegion::recorded_prot)
                .map(crate::mmu::user_flags::to_pte)
        })
    };
    if let Some(owner) = address_space_owner_pid_for_fault()
        && let Some(f) = lookup(owner)
    {
        return Some(f);
    }
    lookup(pid)
}

/// Every eager region covering `va`, as `(start_va, pages, flags)`.
///
/// [`eager_region_flags_for_page_fault`] answers from the **first** `Vec` match,
/// so if two regions cover one VA the winner is decided by insertion order and an
/// obsolete record can shadow the live one — which is indistinguishable, from the
/// fault handler's side, from a page whose protection was legitimately recorded.
/// This exists so an anomaly report can say how many regions actually claim the
/// address: more than one is a bookkeeping bug, not a close call. See
/// docs/archive/CARGO_HEAP_NULL_RC.md.
pub fn eager_regions_containing(pid: Pid, va: usize) -> alloc::vec::Vec<(usize, usize, u64)> {
    let owner = address_space_owner_pid_for_fault().unwrap_or(pid);
    let collect = |p: Pid| -> alloc::vec::Vec<(usize, usize, u64)> {
        lookup_process_shared(p).map_or_else(alloc::vec::Vec::new, |proc| {
            proc.vm_with_regions(|r| {
                r.iter()
                    .filter(|reg| !reg.lazy && reg.contains(va))
                    .map(|reg| (reg.start_va, reg.pages, crate::mmu::user_flags::to_pte(reg.prot)))
                    .collect()
            })
        })
    };
    let owned = collect(owner);
    if owned.is_empty() && owner != pid { collect(pid) } else { owned }
}

/// Record `new_prot` on exactly `[range_start, range_start+range_size)`:
/// `mprotect`'s region half. Covers every region in the list, lazy ones
/// included (the name predates their move into it), splitting any region the
/// range only partly covers.
///
/// Takes the neutral [`akuma_mmap::Prot`] rather than a raw PTE: a region
/// *records* a protection and the encoding lives with the walker.
pub fn update_eager_region_flags(
    pid: Pid,
    range_start: usize,
    range_size: usize,
    new_prot: akuma_mmap::Prot,
) {
    let Some(proc) = lookup_process_shared(pid) else { return };
    let range_end = range_start.saturating_add(range_size);
    // SPLITS now, rather than recording the new protection against the whole
    // overlapping region. The old widening was justified as safe "because the
    // fault handler only ever uses these flags to grant a write" — true for
    // granting, false for refusing, and the day the write-fault handler started
    // refusing on this record a guard page `mprotect(PROT_NONE)`-ed inside a
    // larger mapping marked every page of it and killed `rustc` mid-build.
    //
    // The algebra is `akuma_mmap::mprotect_eager_regions_in_range`, host-tested
    // against every shape (head/middle/tail, fully covered, multi-region,
    // CoW-inherited, and that neighbours keep whatever the original region said).
    // Splitting frames in step is the same walk `detach_eager_regions_in_range`
    // has always done — the old "we cannot split `frames`" objection was never
    // true. See docs/reference/subsystems/syscalls/mem.md.
    let touched = proc.vm_with_regions(|r| {
        akuma_mmap::mprotect_eager_regions_in_range(r, range_start, range_end, new_prot)
    });
    if touched > 0 {
        EAGER_FLAG_WIDENED.fetch_add(0, core::sync::atomic::Ordering::Relaxed);
    }
}

/// Count of `mprotect` upgrades that recorded "writable" for pages outside the
/// requested range (see [`update_eager_region_flags`]). Every one of them is a page
/// the fault handler will silently grant a write on.
pub static EAGER_FLAG_WIDENED: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);

/// `munmap`'s record half, for both kernels: detach every region in
/// `[start, end)`, eager and lazy, clipping at both ends, and release the
/// address space's pin on each file no region maps afterwards. The pieces come
/// back with their recorded frames, which a write-back may need; the pages
/// themselves are the caller's to unmap.
///
/// Orphans are collected in a `Vec`, not [`OrphanedInodes`]: a `munmap` may
/// cross any number of files, and a pin left behind is the 1024-slot pin table
/// filling up (`retain_mapped_inode_pins`' history on amd64).
pub fn detach_regions_in_range(proc: &Process, start: usize, end: usize) -> Vec<akuma_mmap::DetachedPiece> {
    let (pieces, orphans) = proc.vm_with_regions(|r| {
        let pieces = akuma_mmap::detach_regions_in_range_where(r, start, end, |_| true);
        let mut orphans: Vec<u32> = Vec::new();
        for f in pieces.iter().filter_map(|p| p.file) {
            if f.inode != 0 && !orphans.contains(&f.inode) && !regions_name_inode(r, f.inode) {
                orphans.push(f.inode);
            }
        }
        (pieces, orphans)
    });
    if !orphans.is_empty() {
        proc.with_address_space(|a| orphans.iter().for_each(|&i| a.unpin_mapped_inode(i)));
    }
    pieces
}

/// Handle munmap across all lazy regions overlapping [unmap_addr, unmap_addr+unmap_len).
/// Returns each removed piece as `(start_va, pages)`.
pub fn munmap_lazy_regions_in_range(pid: Pid, unmap_addr: usize, unmap_len: usize) -> Vec<(usize, usize)> {
    let Some(proc) = lookup_process_shared(pid) else { return Vec::new() };
    let unmap_end = unmap_addr.saturating_add(unmap_len);
    // A piece removed may have been the last to name its file. The check and
    // the removal share one hold, so the answer is about the list the removal
    // left.
    let (pieces, orphaned) = proc.vm_with_regions(|r| detach_lazy(r, unmap_addr, unmap_end));
    orphaned.unpin(proc);
    for &(start, pages) in &pieces {
        log::debug!("[LR] pid={} munmap {:#x}+{:#x} ({} pages)", pid, start, pages * 4096, pages);
    }
    pieces
}

/// Drop every lazy region registered for `pid`. No-op if `pid` isn't a
/// registered process. The per-process teardown paths (`return_to_kernel*`,
/// `teardown_forked_process_thread_group`) deliberately do NOT call this: the
/// list drops inside `Process::drop` on the reclaim path, and the exit/OOM-kill
/// path must not take a lock an OOM'd mutator frame may still hold
/// (docs/archive/LAZY_REGION_TABLE_ALLOC_UNDER_LOCK_HANG.md). It is kept for the
/// zombie-reap paths (`sys_wait4`/`sys_waitid`), which release a *child's*
/// regions early from the reaping parent's syscall context. Purely an
/// optimization there: the list would drop with the `Process` anyway.
pub fn clear_lazy_regions(pid: Pid) {
    let Some(proc) = lookup_process_shared(pid) else { return };
    let count = proc.vm_with_regions(|r| {
        let before = r.len();
        r.retain(|x| !x.lazy);
        before - r.len()
    });
    if count > 0 {
        log::debug!("[LR!] clear pid={} ({} regions)", pid, count);
    }
}

/// Owned snapshot of a pid's lazy regions as [`LazyRegion`] views (`None` if
/// the pid isn't registered). Diagnostics and tests only: it allocates, so
/// call it from non-pressure contexts.
pub fn lazy_regions_snapshot(pid: Pid) -> Option<BTreeMap<usize, LazyRegion>> {
    let proc = lookup_process_shared(pid)?;
    let regions: Vec<MmapRegion> = proc.vm_with_regions(|r| r.iter().filter(|x| x.lazy).cloned().collect());
    Some(
        regions
            .iter()
            .map(|r| {
                (r.start_va, LazyRegion {
                    start_va: r.start_va,
                    size: r.len_bytes(),
                    prot: r.recorded_prot(),
                    source: lazy_source_of(r),
                })
            })
            .collect(),
    )
}


/// List all running processes.
///
/// Collects scalar fields with IRQs disabled (safe from use-after-free),
/// then does a second pass to clone Strings per PID.
/// The String clone uses lookup_process which re-validates the pointer.
pub fn list_processes() -> Vec<ProcessInfo2> {
    // Phase 1: collect scalar fields atomically (IRQs disabled, no allocation)
    #[derive(Copy, Clone, Default)]
    struct Info {
        pid: u32,
        ppid: u32,
        box_id: u64,
        state: u8, // 0=ready 1=running 2=blocked 3=zombie
        current_syscall: u64,
        last_syscall: u64,
    }
    let infos = crate::process::table::collect_process_info(|p| {
        let st = match p.state.load() {
            ProcessState::Ready => 0u8,
            ProcessState::Running => 1,
            ProcessState::Blocked => 2,
            ProcessState::Zombie(_) => 3,
        };
        Some(Info {
            pid: p.pid,
            ppid: p.parent_pid,
            box_id: p.box_id,
            state: st,
            current_syscall: p.current_syscall.load(core::sync::atomic::Ordering::Relaxed),
            last_syscall: p.last_syscall.load(core::sync::atomic::Ordering::Relaxed),
        })
    });

    // Phase 2: clone Strings per PID (IRQs enabled, safe to allocate).
    // lookup_process re-validates the pointer; if the process was freed
    // between phase 1 and 2, lookup returns None and we use fallback values.
    let mut result = Vec::with_capacity(infos.len());
    for info in &infos {
        let state_str = match info.state {
            0 => "ready", 1 => "running", 2 => "blocked", _ => "zombie",
        };
        let (name, args) = if let Some(proc) = lookup_process_shared(info.pid) {
            let img = proc.image.lock();
            if img.name.len() <= 4096 && img.args.len() <= 256 {
                (img.name.clone(), img.args.clone())
            } else {
                (alloc::string::String::from("?"), Vec::new())
            }
        } else {
            (alloc::string::String::from("?"), Vec::new())
        };
        result.push(ProcessInfo2 {
            pid: info.pid,
            ppid: info.ppid,
            box_id: info.box_id,
            name,
            state: state_str,
            current_syscall: info.current_syscall,
            last_syscall: info.last_syscall,
            args,
        });
    }
    result
}

/// Find a process PID by thread ID (lock-free scan).
pub fn find_pid_by_thread(thread_id: usize) -> Option<Pid> {
    crate::process::table::find_process(|p| {
        if p.thread_id == Some(thread_id) { Some(p.pid) } else { None }
    })
}

#[cfg(test)]
mod child_channel_drain_tests {
    //! Regression tests for the sshd interactive-shell "lost output" bug: a child
    //! that wrote stdout and exited (busybox/toybox login shell over sshd) had its
    //! buffered output discarded because `wait*` called `remove_child_channel` the
    //! instant it reaped the zombie, before the parent's bridge could drain it.
    //! `reap_child_channel` keeps the channel until its stdout is drained.
    use super::*;
    use crate::process::channel::ProcessChannel;
    use crate::test_support::ensure_test_runtime;


    #[test]
    fn reap_keeps_channel_until_buffered_stdout_is_drained() {
        ensure_test_runtime();
        // High, test-local pids so the shared CHILD_CHANNELS registry can't collide
        // with other parallel host tests.
        let pid: Pid = 0x7000_0001;
        let parent: Pid = 0x7000_0002;

        let ch = Arc::new(ProcessChannel::new());
        // Child writes output, then exits (mirrors busybox flushing stdio at _exit).
        ch.write(b"HELLO_FROM_CHILD");
        ch.set_exited(0);
        register_child_channel(pid, ch.clone(), parent);

        // The wait* path reaps the zombie. Output is still buffered, so the channel
        // MUST be kept (returns false = not removed) — otherwise the parent's
        // ChildStdout fd would resolve to nothing and lose the output.
        assert!(
            !reap_child_channel(pid),
            "reap must KEEP the channel while stdout is still buffered"
        );
        let surviving = get_child_channel(pid)
            .expect("channel must survive the reap while data is pending");

        // Parent drains the buffered output (exactly what sshd's bridge does after
        // observing the child's exit).
        let mut buf = [0u8; 64];
        let n = surviving.read(&mut buf);
        assert_eq!(&buf[..n], b"HELLO_FROM_CHILD", "buffered child output preserved");

        // Now that it is drained, a subsequent reap removes the channel.
        assert!(reap_child_channel(pid), "reap removes the channel once drained");
        assert!(get_child_channel(pid).is_none(), "channel gone after drained reap");
    }

    #[test]
    fn reap_removes_immediately_when_no_buffered_stdout() {
        let pid: Pid = 0x7000_0011;
        let parent: Pid = 0x7000_0012;

        let ch = Arc::new(ProcessChannel::new());
        ch.set_exited(0); // exited with no pending output (or already drained)
        register_child_channel(pid, ch, parent);

        // Nothing buffered → reap removes it right away, so callers that waitpid
        // without ever reading the ChildStdout fd don't leak channels.
        assert!(reap_child_channel(pid), "empty channel is removed on reap");
        assert!(get_child_channel(pid).is_none());
    }

    #[test]
    fn is_child_of_group_matches_registered_parent_only() {
        let pid: Pid = 0x7000_0031;
        let parent: Pid = 0x7000_0032;
        let stranger: Pid = 0x7000_0033;

        register_child_channel(pid, Arc::new(ProcessChannel::new()), parent);

        assert!(is_child_of_group(pid, parent), "registered parent may wait");
        assert!(!is_child_of_group(pid, stranger), "non-parent gets ECHILD");
        // The Go os/exec pidfd probe: a process waitid()s a pidfd of ITSELF and
        // must get ECHILD (it is not its own child), never block.
        assert!(!is_child_of_group(pid, pid), "self-wait is not a child wait");

        remove_child_channel(pid);
    }

    #[test]
    fn is_child_of_group_unregistered_pid_is_not_a_child() {
        assert!(!is_child_of_group(0x7000_0041, 0x7000_0042));
    }

    #[test]
    fn reparent_moves_registry_entries_so_the_new_parent_can_wait() {
        // The amd64 orphan-zombie bug: reparenting rewrote only the process
        // table, so init's `wait4(-1)` — which reads THIS registry — answered
        // ECHILD and orphans were never reapable.
        let dying: Pid = 0x7000_0051;
        let init: Pid = 0x7000_0052;
        let live: Pid = 0x7000_0053;
        let dead: Pid = 0x7000_0054;
        let unrelated: Pid = 0x7000_0055;
        let other_parent: Pid = 0x7000_0056;

        register_child_channel(live, Arc::new(ProcessChannel::new()), dying);
        let dead_ch = Arc::new(ProcessChannel::new());
        dead_ch.set_exited(3); // already a zombie when its parent dies
        register_child_channel(dead, dead_ch, dying);
        register_child_channel(unrelated, Arc::new(ProcessChannel::new()), other_parent);

        assert!(!has_children(init), "precondition: init has no children yet");
        assert_eq!(reparent_children_to(dying, init), 2, "both of the dying parent's children move");

        assert!(has_children(init), "init now has children to wait for");
        assert!(!has_children(dying), "the dead parent has none left");
        assert!(is_child_of_group(live, init), "wait4(<live>) from init is permitted");
        assert!(!is_child_of_group(live, dying), "and no longer from the dead parent");
        assert_eq!(parent_pid_of(dead), Some(init));
        assert_eq!(parent_pid_of(unrelated), Some(other_parent), "other parents' children untouched");
        // The already-exited child is what init's next wait4(-1) reaps.
        let (found, ch) = find_exited_child(init).expect("the zombie is findable by init");
        assert_eq!(found, dead);
        assert_eq!(ch.exit_code(), 3);
        // Reparenting an idempotent second time moves nothing.
        assert_eq!(reparent_children_to(dying, init), 0);

        remove_child_channel(live);
        remove_child_channel(dead);
        remove_child_channel(unrelated);
    }

    #[test]
    fn reaping_a_buffered_child_keeps_its_output_but_not_the_child() {
        // The herd memory-eater: `wait4(-1)` reported this child reaped, but
        // the entry stayed (its stdout was not drained) and `find_exited_child`
        // returned it again on every following call — a `while wait_any()`
        // loop never ended.
        let parent: Pid = 0x7000_0061;
        let buffered: Pid = 0x7000_0062;
        let drained: Pid = 0x7000_0063;

        let ch = Arc::new(ProcessChannel::new());
        ch.write(b"last words\n");
        ch.set_exited(1);
        register_child_channel(buffered, ch, parent);
        let empty = Arc::new(ProcessChannel::new());
        empty.set_exited(0);
        register_child_channel(drained, empty, parent);

        assert!(!reap_child_channel(buffered), "kept: it has unread output");
        assert!(reap_child_channel(drained), "an empty one is removed");
        assert!(find_exited_child(parent).is_none(), "a reaped pid is never returned twice");
        assert!(!has_children(parent), "so the next wait4(-1) answers ECHILD");
        assert!(!is_child_of_group(buffered, parent), "wait4(<pid>) on it is ECHILD too");
        let kept = get_child_channel(buffered).expect("the ChildStdout reader still finds it");
        assert_eq!(kept.read_all(), b"last words\n");
        assert!(get_child_channel(drained).is_none());

        remove_child_channel(buffered);
    }

    #[test]
    fn reap_absent_channel_is_a_noop() {
        // Reaping a pid with no registered channel (a process spawned without a
        // stdout pipe) must not panic and reports "not removed".
        assert!(!reap_child_channel(0x7000_0021));
    }
}

#[cfg(test)]
mod lazy_region_list_tests {
    //! The lazy half of `Process::mmap_regions`: a lazy region is an
    //! `MmapRegion` with `lazy` set, and these pin down the conversions and the
    //! list edits the pid-keyed API is built from.
    use super::*;
    use crate::mmu::user_flags;
    use akuma_primitives::inode_pin::pin_count;

    /// Mount id 1 throughout.
    fn file_source(inode: u32, file_offset: usize, filesz: usize, segment_va: usize) -> LazySource {
        LazySource::file(1, inode, file_offset, filesz, segment_va)
    }

    fn push(list: &mut Vec<MmapRegion>, start: usize, size: usize, flags: u64, source: &LazySource) -> LazyInsert {
        insert_lazy_region(list, lazy_mmap_region(start, size, flags, source))
    }

    #[test]
    fn a_lookup_reads_back_what_was_pushed() {
        let mut list = Vec::new();
        push(&mut list, 0x1000_0000, 0x1000, user_flags::RX, &LazySource::Zero);
        // An ELF segment starting 0x234 into its first page.
        push(&mut list, 0x2000_0000, 0x3000, user_flags::RW_NO_EXEC, &file_source(60_001, 0x5234, 0x1800, 0x2000_0234));

        let (flags, source, start, size) = lazy_lookup(&list, 0x1000_0800).unwrap();
        assert_eq!((flags, start, size), (user_flags::RX, 0x1000_0000, 0x1000));
        assert!(matches!(source, LazySource::Zero));

        let (flags, source, start, size) = lazy_lookup(&list, 0x2000_2fff).unwrap();
        assert_eq!((flags, start, size), (user_flags::RW_NO_EXEC, 0x2000_0000, 0x3000));
        match source {
            LazySource::File { inode, mount_id, file_offset, filesz, segment_va, .. } => {
                assert_eq!((mount_id, inode, file_offset, filesz, segment_va), (1, 60_001, 0x5234, 0x1800, 0x2000_0234));
            }
            LazySource::Zero => panic!("file source lost"),
        }
        assert!(lazy_lookup(&list, 0x2000_3000).is_none());
        assert!(lazy_lookup(&list, 0x0fff_f000).is_none());
    }

    #[test]
    fn an_unrecorded_protection_reads_back_as_zero() {
        let mut list = Vec::new();
        push(&mut list, 0x1000_0000, 0x1000, 0, &LazySource::Zero);
        assert_eq!(lazy_lookup(&list, 0x1000_0000).unwrap().0, 0);
        push(&mut list, 0x2000_0000, 0x1000, user_flags::NONE, &LazySource::Zero);
        assert_eq!(lazy_lookup(&list, 0x2000_0000).unwrap().0, user_flags::NONE);
    }

    #[test]
    fn eager_regions_are_not_lazy_hits() {
        let mut list = alloc::vec![MmapRegion::inherited_with_prot(0x3000_0000, 2, akuma_mmap::Prot::RW)];
        push(&mut list, 0x1000_0000, 0x1000, user_flags::RW, &LazySource::Zero);
        assert!(lazy_lookup(&list, 0x3000_0000).is_none());
        assert!(lazy_lookup(&list, 0x1000_0000).is_some());
        assert_eq!(list.iter().map(|r| r.start_va).collect::<Vec<_>>(), [0x1000_0000, 0x3000_0000]);
    }

    #[test]
    fn a_later_segment_takes_the_boundary_page() {
        // Two ELF segments share the page at 0x2000_1000. The map keyed by
        // `start_va` served it from the later one; the list must too.
        let mut list = Vec::new();
        push(&mut list, 0x2000_0000, 0x2000, user_flags::RX, &file_source(60_002, 0, 0x1800, 0x2000_0000));
        push(&mut list, 0x2000_1000, 0x2000, user_flags::RW, &file_source(60_002, 0x1900, 0x400, 0x2000_1900));
        assert_eq!(list.len(), 2);
        let (flags, _, start, size) = lazy_lookup(&list, 0x2000_1abc).unwrap();
        assert_eq!((flags, start, size), (user_flags::RW, 0x2000_1000, 0x2000));
        let (flags, _, start, size) = lazy_lookup(&list, 0x2000_0abc).unwrap();
        assert_eq!((flags, start, size), (user_flags::RX, 0x2000_0000, 0x1000));
    }

    #[test]
    fn a_clipped_file_region_reads_the_same_bytes() {
        // Clip the head off a segment with a lead; the remaining piece must map
        // each page to the same file offset as before.
        let mut list = Vec::new();
        let src = file_source(60_003, 0x8100, 0x3000, 0x4000_0100);
        push(&mut list, 0x4000_0000, 0x4000, user_flags::RO, &src);
        let off_before = |src: &LazySource, va: usize| match *src {
            LazySource::File { file_offset, segment_va, .. } => file_offset as isize + (va as isize - segment_va as isize),
            LazySource::Zero => unreachable!(),
        };
        let want = off_before(&src, 0x4000_2000);
        let (pieces, _) = detach_lazy(&mut list, 0x4000_0000, 0x4000_1000);
        assert_eq!(pieces, [(0x4000_0000, 1)]);
        let (_, after, start, _) = lazy_lookup(&list, 0x4000_2000).unwrap();
        assert_eq!(start, 0x4000_1000);
        assert_eq!(off_before(&after, 0x4000_2000), want);
        match after {
            LazySource::File { filesz, segment_va, .. } => {
                // The 0xf00 bytes of data in the first page are gone with it.
                assert_eq!((segment_va, filesz), (0x4000_1000, 0x3000 - 0xf00));
            }
            LazySource::Zero => unreachable!(),
        }
    }

    #[test]
    fn a_source_below_its_region_is_advanced() {
        let r = lazy_mmap_region(0x5000_1000, 0x1000, 0, &file_source(60_004, 0x100, 0x2000, 0x5000_0800));
        let f = r.file.unwrap();
        assert_eq!((f.lead, f.offset, f.filesz), (0, 0x900, 0x1800));
    }

    #[test]
    fn detach_reports_a_file_once_nothing_maps_it() {
        let ino = 60_005;
        let mut list = Vec::new();
        push(&mut list, 0x1000_0000, 0x4000, user_flags::RO, &file_source(ino, 0, 0x4000, 0x1000_0000));
        push(&mut list, 0x2000_0000, 0x1000, user_flags::RO, &file_source(ino, 0, 0x1000, 0x2000_0000));
        push(&mut list, 0x3000_0000, 0x1000, user_flags::RW, &LazySource::Zero);

        let (pieces, orphaned) = detach_lazy(&mut list, 0x1000_1000, 0x1000_2000); // middle split
        assert_eq!(pieces, [(0x1000_1000, 1)]);
        assert_eq!(orphaned.iter().count(), 0);
        assert_eq!(list.len(), 4);

        let (_, orphaned) = detach_lazy(&mut list, 0x1000_0000, 0x1000_4000);
        assert_eq!(orphaned.iter().count(), 0, "the mapping at 0x2000_0000 still names it");

        let (pieces, orphaned) = detach_lazy(&mut list, 0x2000_0000, 0x3000_1000);
        assert_eq!(pieces, [(0x2000_0000, 1), (0x3000_0000, 1)]);
        assert_eq!(orphaned.iter().collect::<Vec<_>>(), [ino]);
        assert!(list.is_empty());
    }

    #[test]
    fn detach_leaves_eager_regions_alone() {
        let mut list = alloc::vec![MmapRegion::inherited_with_prot(0x1000_1000, 1, akuma_mmap::Prot::RW)];
        push(&mut list, 0x1000_0000, 0x1000, user_flags::RW, &LazySource::Zero);
        let (pieces, _) = detach_lazy(&mut list, 0x1000_0000, 0x1000_2000);
        assert_eq!(pieces, [(0x1000_0000, 1)]);
        assert_eq!(list.len(), 1);
        assert!(!list[0].lazy);
    }

    #[test]
    fn replacing_a_mapping_orphans_the_old_file() {
        let (old, new) = (60_006, 60_007);
        let mut list = Vec::new();
        push(&mut list, 0x1000_0000, 0x1000, 0, &file_source(old, 0, 0x1000, 0x1000_0000));
        let ins = push(&mut list, 0x1000_0000, 0x1000, 0, &file_source(new, 0, 0x1000, 0x1000_0000));
        assert_eq!(ins.orphaned.iter().collect::<Vec<_>>(), [old]);
        assert!(!ins.over_eager);
        assert_eq!(list.len(), 1);
        // Re-pushing the same file orphans nothing.
        let ins = push(&mut list, 0x1000_0000, 0x1000, 0, &file_source(new, 0, 0x1000, 0x1000_0000));
        assert_eq!(ins.orphaned.iter().count(), 0);
    }

    #[test]
    fn a_push_over_an_eager_region_is_reported() {
        let mut list = alloc::vec![MmapRegion::inherited_with_prot(0x1000_1000, 1, akuma_mmap::Prot::RW)];
        let ins = push(&mut list, 0x1000_0000, 0x2000, 0, &LazySource::Zero);
        assert!(ins.over_eager);
        let ins = push(&mut list, 0x2000_0000, 0x1000, 0, &LazySource::Zero);
        assert!(!ins.over_eager);
    }

    #[test]
    fn regions_hold_no_pins_and_a_lookup_holds_one_while_alive() {
        // The address space pins mapped files now (`MappingPins`); a region
        // record holds none, and a looked-up source holds one for its reader.
        let ino = 60_008;
        let mut list = Vec::new();
        push(&mut list, 0x1000_0000, 0x1000, 0, &file_source(ino, 0, 0x1000, 0x1000_0000));
        assert_eq!(pin_count(ino), 0);
        let hit = lazy_lookup(&list, 0x1000_0000).unwrap();
        assert_eq!(pin_count(ino), 1);
        drop(hit);
        assert_eq!(pin_count(ino), 0);
    }

    #[test]
    fn cow_inheritance_carries_lazy_regions_and_their_files() {
        let mut parent = Vec::new();
        push(&mut parent, 0x1000_0000, 0x2000, user_flags::RX, &file_source(60_009, 0x40, 0x1000, 0x1000_0040));
        push(&mut parent, 0x2000_0000, 0x1000, user_flags::RW, &LazySource::Zero);
        let child = inherit_mmap_regions_for_cow_child(&parent);
        let (flags, src, start, size) = lazy_lookup(&child, 0x1000_1000).unwrap();
        assert_eq!((flags, start, size), (user_flags::RX, 0x1000_0000, 0x2000));
        assert!(matches!(src, LazySource::File { inode: 60_009, segment_va: 0x1000_0040, .. }));
        assert!(lazy_lookup(&child, 0x2000_0000).is_some());
    }
}

/// The map stores a `Prot` and speaks the AArch64 word at its edges; the
/// conversion must give back exactly the word it was handed for every word
/// this tree stores, or `[WPF]`'s `lazy_self=` and every mprotect would see a
/// different value than before.
#[cfg(test)]
mod region_word_tests {
    use super::{region_prot, region_word};
    use crate::mmu::user_flags;

    #[test]
    fn every_stored_word_round_trips() {
        for w in [
            0,
            user_flags::NONE,
            user_flags::RO,
            user_flags::RW,
            user_flags::RW_NO_EXEC,
            user_flags::RX,
            user_flags::RO_NO_EXEC,
            user_flags::EXEC,
        ] {
            assert_eq!(region_word(region_prot(w)), w, "word {w:#x}");
        }
    }

    #[test]
    fn zero_is_unrecorded_not_none() {
        assert_eq!(region_prot(0), None);
        assert_eq!(region_prot(user_flags::NONE), Some(akuma_mmap::Prot::NONE));
    }
}

#[cfg(test)]
mod fault_slot_tests {
    //! Host unit tests for [`FaultSlot::reclaim_report`] — the pure half of the
    //! per-page demand-paging slot's observability. `fault_slot_acquire` itself
    //! needs a live thread slot and a registered process, so it stays in the boot
    //! suite (`crates/akuma-kernel-tests-aarch64/src/process_tests.rs`); the classification does not.
    use super::*;

    /// The hot outcomes must stay silent: this runs on every demand-paging fault,
    /// so a `Some` here is a console write per page.
    #[test]
    fn acquired_and_noproc_report_nothing() {
        assert_eq!(FaultSlot::Acquired.reclaim_report(), None);
        assert_eq!(FaultSlot::NoProc.reclaim_report(), None);
    }

    /// Both reclaim outcomes report, and each carries the *stale holder's* tid —
    /// not the reclaiming thread's. That tid is the whole diagnostic value: it
    /// names the thread that died or wedged mid-fault.
    #[test]
    fn both_reclaims_report_their_stale_holder() {
        assert_eq!(
            FaultSlot::ReclaimedDead(41).reclaim_report(),
            Some((ReclaimCause::Dead, 41))
        );
        assert_eq!(
            FaultSlot::ReclaimedWedged(42).reclaim_report(),
            Some((ReclaimCause::Wedged, 42))
        );
    }

    /// The two reclaim causes must stay distinguishable: `Dead` is the expected
    /// recovery from a thread that died mid-fault, `Wedged` means the bounded
    /// fallback fired and "should be vanishingly rare" — collapsing them would
    /// hide the second behind the first in the logs.
    #[test]
    fn dead_and_wedged_are_distinct_causes() {
        let dead = FaultSlot::ReclaimedDead(7).reclaim_report();
        let wedged = FaultSlot::ReclaimedWedged(7).reclaim_report();
        assert_ne!(dead, wedged);
    }

    /// Holder tid 0 is a legal thread slot, so it must not be confused with "no
    /// reclaim" — an `Option<(_, usize)>` keyed on the tid being non-zero would
    /// silently drop the reclaim of slot 0.
    #[test]
    fn holder_tid_zero_still_reports() {
        assert_eq!(
            FaultSlot::ReclaimedDead(0).reclaim_report(),
            Some((ReclaimCause::Dead, 0))
        );
    }

    /// A nested acquire is not a reclaim: nothing was taken from anyone, so the
    /// `[FAULT-RECLAIM]` trace must stay silent for it. The caller distinguishes
    /// it from `Acquired` by matching the variant, not by this report — the two
    /// differ in who owns the *release*, which `reclaim_report` says nothing about
    /// (F6, `COW_PILE_AUDIT.md` §9).
    #[test]
    fn already_held_reports_nothing() {
        assert_eq!(FaultSlot::AlreadyHeld.reclaim_report(), None);
    }

    /// ...but it must not be *spelled* `Acquired`, which is the shape of the F6
    /// defect: one variant for "you now own the release" and "someone else still
    /// does" is what let the inner RAII guard drop the outer guard's entry. This
    /// pins the discriminants apart so a future `reclaim_report`-driven refactor
    /// cannot quietly merge them back.
    #[test]
    fn already_held_is_not_acquired() {
        assert!(!matches!(FaultSlot::AlreadyHeld, FaultSlot::Acquired));
        assert!(!matches!(FaultSlot::Acquired, FaultSlot::AlreadyHeld));
    }
}

#[cfg(test)]
mod sigchld_delivery_tests {
    //! Host unit tests for the SIGCHLD delivery edge added to fix the hanging
    //! `wait` builtin (docs/archive/SIGCHLD_DELIVERY_PLAN.md). The kernel boot
    //! self-test exercises the in-VM reproducer; these cover the pure-logic
    //! helpers (`parent_pid_of`, `raise_sigchld_for_parent`, `publish_child_exit`,
    //! and the `LAST_SIGCHLD` siginfo side-channel) that have no dependency on a
    //! live thread slot.
    //!
    //! Every assertion here relies only on the shared `CHILD_CHANNELS` registry
    //! and the static per-slot arrays; high test-local pids (0x7000_005x) avoid
    //! collisions with the parallel sibling test modules.
    use super::*;
    use crate::process::channel::ProcessChannel;
    use crate::threading;

    /// `parent_pid_of` mirrors the lookup `is_child_of_group` already does, but
    /// returns the raw forking-thread pid (which may be a non-leader of a
    /// multithreaded parent — resolution happens later, in `raise_sigchld_for_parent`).
    #[test]
    fn parent_pid_of_returns_registered_parent_then_none_after_remove() {
        let child: Pid = 0x7000_0050;
        let parent: Pid = 0x7000_0051;
        register_child_channel(child, Arc::new(ProcessChannel::new()), parent);
        assert_eq!(parent_pid_of(child), Some(parent));
        remove_child_channel(child);
        assert_eq!(parent_pid_of(child), None);
    }

    /// Raising SIGCHLD for a child with no registered channel (reaped, or a
    /// double-published exit) must be a silent no-op — never a panic.
    #[test]
    fn raise_sigchld_for_unregistered_child_is_noop() {
        raise_sigchld_for_parent(0x7000_0052, 0);
        raise_sigchld_for_parent(0x7000_0052, -9);
    }

    /// The in-kernel sshd bridge spawns children whose recorded parent is a
    /// kernel system thread with no `Process` entry. Resolving the parent's
    /// thread group must fail cleanly (no signal, no panic) rather than index a
    /// missing process.
    #[test]
    fn raise_sigchld_for_parent_with_no_process_is_noop() {
        let child: Pid = 0x7000_0053;
        let kernel_thread_parent: Pid = 0x7000_0054; // not in the process table
        register_child_channel(child, Arc::new(ProcessChannel::new()), kernel_thread_parent);
        // No registered Process → find_process returns None → no signal pended.
        raise_sigchld_for_parent(child, 0);
        remove_child_channel(child);
    }

    /// With an empty process table, every thread-group preference falls through
    /// and `sigchld_target_thread` returns `None` — the signal simply isn't
    /// raised. (The preference-order test against registered Processes is a
    /// kernel boot self-test: it needs live thread slots, which host tests lack.)
    #[test]
    fn sigchld_target_thread_is_none_with_no_processes() {
        assert!(sigchld_target_thread(0x7000_0055).is_none());
    }

    /// The load-bearing invariant: `publish_child_exit` marks the channel exited
    /// (so a racing `waitpid(WNOHANG)` in the parent's handler sees the zombie)
    /// and is safe even though the SIGCHLD itself is a no-op here (no parent
    /// Process registered). A second publish must NOT overwrite the real exit
    /// code with a teardown code like 137 — that is the "one death, one signal"
    /// guard, and it is what keeps `go build`'s buildID honest when a goroutine
    /// crashes after the leader already published a clean exit.
    #[test]
    fn publish_child_exit_marks_channel_and_dedups() {
        let child: Pid = 0x7000_0056;
        let parent: Pid = 0x7000_0057;
        let ch = Arc::new(ProcessChannel::new());
        register_child_channel(child, ch.clone(), parent);

        // First publish: real exit code 0, channel transitions to exited.
        publish_child_exit(child, 0);
        assert!(ch.has_exited(), "channel must be marked exited by publish");
        assert_eq!(ch.exit_code(), 0);

        // Second publish (e.g. return_to_kernel / subtree teardown with code 137):
        // the `has_exited` guard suppresses the overwrite AND the duplicate
        // SIGCHLD. The real exit code survives.
        publish_child_exit(child, 137);
        assert_eq!(ch.exit_code(), 0, "second publish must not clobber the real code");

        remove_child_channel(child);
    }

    /// `publish_child_exit` on a pid with no registered child channel (a process
    /// spawned without a stdout pipe, or one already reaped) is a silent no-op.
    #[test]
    fn publish_child_exit_for_absent_channel_is_noop() {
        publish_child_exit(0x7000_0058, 0);
        publish_child_exit(0x7000_0058, -9);
    }

    /// The SIGCHLD `siginfo` side-channel packs `(child_pid, exit_code)` into one
    /// u64 and must round-trip both clean exits (code >= 0) and signal kills
    /// (code < 0, where `-code` is the signal number). Slot 63 is unused by any
    /// other host test.
    #[test]
    fn last_sigchld_roundtrips_clean_and_signaled_exits() {
        let slot = 63usize;

        threading::set_last_sigchld(slot, 1234, 0);
        assert_eq!(threading::peek_last_sigchld(slot), Some((1234, 0)));

        threading::set_last_sigchld(slot, 4321, -9);
        assert_eq!(threading::peek_last_sigchld(slot), Some((4321, -9)),
            "negative exit codes (signal kills) must round-trip");

        threading::set_last_sigchld(slot, 7, 42);
        assert_eq!(threading::peek_last_sigchld(slot), Some((7, 42)));
    }

    /// `peek_last_sigchld` on a slot that was never written returns `None`, so
    /// the SIGCHLD siginfo path falls back to zeros gracefully.
    #[test]
    fn peek_last_sigchld_unset_is_none() {
        // Slot 62 is untouched by the round-trip test (which uses 63), and no
        // other host test writes LAST_SIGCHLD, so it is reliably the zero init.
        assert_eq!(threading::peek_last_sigchld(62), None);
    }
}
