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
//! | a `PROCS` slot and a `Spawn` record | the parent's, shared |
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
//! That also means threads inherit the target's `invlpg`-has-no-shootdown
//! limit for free rather than adding to it: nothing here demotes a PTE, so
//! there is no stale-translation window to shoot down. **CoW `fork` is still
//! SMP=1 only** (`AKUMA_AMD64_COW.md`); threads do not change that either way.
//!
//! # The lifetime rule
//!
//! A process's address space is freed by whoever reaps it — `sys_waitpid`,
//! `cleanup_spawn_slot` — after the *main* thread's task is `Finished`. A
//! sibling thread still running in that space when it is freed is a
//! use-after-free of a page table. [`live_count`] is what closes that: the main
//! thread's exit path drains it to zero before it finishes, so by the time
//! anything can reap, no task holds that `space_root`.

use core::sync::atomic::{AtomicU32, Ordering};

use crate::fd::errno;
use crate::serial;

/// Threads in flight, across all processes.
///
/// A fixed array, not a `Vec`: this is per-thread kernel state on a path that
/// runs under memory pressure, and the ceiling wants to be a refusal
/// (`EAGAIN`, which is what `pthread_create` already reports) rather than an
/// allocation that can fail. 64 is well past what the scheduler's own
/// `MAX_TASKS` budget makes useful — every thread also costs a task slot and
/// two 32 KiB kernel stacks.
pub const MAX_THREADS: usize = 64;

/// "This task is not a thread." `UserCtx::thread_slot`'s resting value.
pub const NO_THREAD: usize = usize::MAX;

/// One live non-main thread.
#[derive(Clone, Copy)]
struct Thread {
    /// Scheduler task slot. The identity a futex wake reaches.
    task: usize,
    /// The `PROCS` slot whose address space this thread runs in — shared with
    /// its process, which is what makes fd 0/1/2 route the same way.
    proc_slot: usize,
    /// Linux tid. Drawn from the same counter as pids, as on Linux, so a tid
    /// and a pid can never name two different things.
    tid: u32,
    /// Where ring 3 resumes: the parent's post-`clone` instruction, on the
    /// stack the caller supplied.
    rip: u64,
    rsp: u64,
    /// `CLONE_CHILD_CLEARTID`'s address, or 0. On exit the kernel zeroes this
    /// word and wakes one futex waiter on it — which is precisely how
    /// `pthread_join` learns the thread is gone, and the reason a `join` that
    /// never returns is the classic symptom of getting this wrong.
    clear_child_tid: u64,
}

/// `static mut` under the BKL — `usermode::PROCS`'s discipline exactly.
static mut THREADS: [Option<Thread>; MAX_THREADS] = [const { None }; MAX_THREADS];

fn threads() -> *mut [Option<Thread>; MAX_THREADS] {
    &raw mut THREADS
}

/// Set when any thread of a process calls `exit_group`, so its siblings leave
/// ring 3 at their next syscall instead of running on into a torn-down space.
///
/// Indexed by `PROCS` slot. An `AtomicU32` bitmap would be tighter; an array of
/// flags is what the rest of this file's tables look like.
static GROUP_EXIT: [AtomicU32; crate::usermode::PROC_SLOTS] =
    [const { AtomicU32::new(0) }; crate::usermode::PROC_SLOTS];

/// Has `proc_slot` been asked to exit as a whole?
#[must_use]
pub fn group_exiting(proc_slot: usize) -> bool {
    GROUP_EXIT.get(proc_slot).is_some_and(|f| f.load(Ordering::Relaxed) != 0)
}

/// Mark `proc_slot`'s whole thread group for exit. Called by `exit_group`.
pub fn set_group_exiting(proc_slot: usize) {
    if let Some(f) = GROUP_EXIT.get(proc_slot) {
        f.store(1, Ordering::Relaxed);
    }
}

/// Clear the flag when a slot is reused — `execve` and `sys_spawn` both put a
/// new program in an existing slot, and a stale flag would kill it on its first
/// syscall.
pub fn clear_group_exiting(proc_slot: usize) {
    if let Some(f) = GROUP_EXIT.get(proc_slot) {
        f.store(0, Ordering::Relaxed);
    }
}

/// How many non-main threads of `proc_slot` are still alive.
#[must_use]
pub fn live_count(proc_slot: usize) -> usize {
    // SAFETY: raw-pointer read under the BKL.
    unsafe {
        (*threads())
            .iter()
            .filter(|t| t.as_ref().is_some_and(|t| t.proc_slot == proc_slot))
            .count()
    }
}

/// The tid of the running task: its thread tid, or its process pid if it is a
/// main thread. `gettid` (186).
#[must_use]
pub fn current_tid() -> u32 {
    match current_thread_slot() {
        NO_THREAD => crate::usermode::current_pid(),
        // SAFETY: raw-pointer read under the BKL.
        slot => unsafe {
            (*threads())[slot].map_or_else(crate::usermode::current_pid, |t| t.tid)
        },
    }
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
/// Checked at syscall entry, which is the only place a thread reliably passes
/// through: this target has no signals, so there is no way to interrupt one in
/// ring 3. A thread in an unbounded compute loop with no syscall in it is
/// therefore not reachable — a real gap, and the reason [`drain`] is a bounded
/// wait rather than a guarantee.
#[must_use]
pub fn should_leave_now() -> bool {
    if current_is_main() {
        return false;
    }
    group_exiting(crate::usermode::current_proc_slot())
}

fn current_thread_slot() -> usize {
    // SAFETY: under the BKL; the per-CPU `UserCtx` is the running task's.
    unsafe {
        let uctx = crate::smp::current_uctx();
        if uctx.is_null() { NO_THREAD } else { (*uctx).thread_slot }
    }
}

/// `clone(flags, child_stack, parent_tid, child_tid, tls)` — x86_64 56, the
/// `CLONE_VM` half. Without `CLONE_VM` the caller wants a `fork` and
/// `syscall_dispatch` routes there instead.
///
/// Note the argument order: x86_64's `clone` puts `tls` **last**, after
/// `child_tid` — the reverse of most other architectures. Getting that pair
/// backwards produces a thread whose `%fs` points at the join word, which
/// fails in a way that looks like memory corruption rather than like a
/// wrong argument.
pub fn sys_clone_thread(
    flags: u64,
    child_stack: u64,
    parent_tid: u64,
    child_tid: u64,
    tls: u64,
) -> u64 {
    use crate::usermode::clone_flags::*;

    // `CLONE_VM` without `CLONE_THREAD` is a distinct thing — a process that
    // shares memory, which `vfork` and some sandboxes ask for. It needs its own
    // `Spawn` record and its own `waitpid` semantics, and nothing on this target
    // asks for it. Refuse rather than approximate: a "thread" that a parent's
    // `wait4` never reaps hangs the parent, which is much harder to read than
    // an `ENOSYS` at the call.
    if flags & CLONE_THREAD == 0 {
        return errno::ENOSYS;
    }
    // A thread with no stack of its own would run on the parent's. musl always
    // supplies one; refusing is cheaper than discovering the overlap later.
    if child_stack == 0 {
        return errno::EINVAL;
    }

    let proc_slot = crate::usermode::current_proc_slot();
    if proc_slot == usize::MAX {
        return errno::ENOSYS;
    }

    // The point the child resumes from, and the register set it resumes with:
    // the parent's own, captured by `syscall_entry` on the way in. Identical to
    // `sys_fork`'s snapshot, and for the identical reason — a C compiler
    // assumes r12-r15/rbx survive a `syscall`.
    // SAFETY: raw-pointer read under the BKL; the per-CPU `UserCtx` is this task's.
    let (rip, gs_base, saved_regs) = unsafe {
        let uctx = crate::smp::current_uctx();
        if uctx.is_null() {
            return errno::ENOSYS;
        }
        ((*uctx).user_rip, (*uctx).gs_base, (*uctx).saved_regs)
    };
    if rip == 0 {
        return errno::ENOSYS;
    }

    // SAFETY: raw-pointer read under the BKL.
    let Some(slot) = (unsafe { (*threads()).iter().position(Option::is_none) }) else {
        // `EAGAIN`, which is what Linux returns when the thread limit is hit
        // and what `pthread_create` already knows how to report.
        serial::puts("  [clone] thread table full\n");
        return errno::EAGAIN;
    };

    let tid = crate::usermode::alloc_pid();

    // `CLONE_SETTLS` is not optional for a musl thread: without it the child
    // runs on the *parent's* `%fs`, so both write one `struct pthread` and the
    // first thing to break is `errno`. Absent the flag there is no sane base to
    // invent, so refuse.
    if flags & CLONE_SETTLS == 0 {
        return errno::EINVAL;
    }

    let space_root = crate::sched::current_space_root();
    let Some(task) = crate::sched::spawn_in_space_unpublished(thread_entry, space_root) else {
        return errno::EAGAIN;
    };

    // SAFETY: raw-pointer write under the BKL; `slot` was just found free.
    unsafe {
        (*threads())[slot] = Some(Thread {
            task,
            proc_slot,
            tid,
            rip,
            rsp: child_stack,
            clear_child_tid: if flags & CLONE_CHILD_CLEARTID == 0 { 0 } else { child_tid },
        });
    }

    // Everything the child reads must be in place before it can be scheduled —
    // the same ordering rule `sys_fork` states, and the reason
    // `spawn_in_space_unpublished` exists.
    crate::sched::seed_thread_task(task, tls, gs_base, &saved_regs, proc_slot, slot);

    // The tid, into the parent's and/or the child's memory. Both write into the
    // shared address space, which is live right now, so no `CR3` gymnastics.
    if flags & CLONE_PARENT_SETTID != 0 && !crate::uaccess::write_val::<u32>(parent_tid, tid) {
        cancel(slot, task);
        return errno::EFAULT;
    }
    if flags & CLONE_CHILD_SETTID != 0 && !crate::uaccess::write_val::<u32>(child_tid, tid) {
        cancel(slot, task);
        return errno::EFAULT;
    }

    crate::sched::publish_task(task);
    u64::from(tid)
}

/// Undo a half-built thread. The task was never published, so nothing else can
/// have seen either half.
fn cancel(slot: usize, task: usize) {
    // SAFETY: raw-pointer write under the BKL; the task is `Reserved`.
    unsafe { (*threads())[slot] = None };
    crate::sched::abandon_unpublished(task);
}

/// Every thread task starts here.
///
/// One entry function for all of them, unlike `usermode::proc_entry_for`'s
/// sixteen hand-written trampolines: those bake a `PROCS` index into a `fn`
/// pointer because there was nowhere else to put it, and by the time a thread
/// runs there *is* somewhere — `UserCtx::thread_slot`, seeded before
/// publication.
extern "C" fn thread_entry() -> ! {
    let slot = current_thread_slot();
    // SAFETY: raw-pointer read under the BKL.
    let Some(t) = (unsafe { (*threads()).get(slot).copied().flatten() }) else {
        serial::puts("  [thread] entry with no record\n");
        crate::sched::finish();
    };

    // `forked = true`: enter ring 3 through `enter_user_mode_forked`, which
    // restores the parent's register set from this task's own `saved_regs` and
    // sets `rax = 0`. A `clone` child sees 0 for exactly the reason a `vfork`
    // child does, so the path is the same one.
    let _status = crate::usermode::enter_user_from_thread(t.rip, t.rsp);

    teardown(slot);
    crate::sched::finish();
}

/// A thread has left ring 3: publish its death the way `pthread_join` reads it.
///
/// The order is the contract. Zero the word *before* the wake, or a joiner that
/// wakes and re-reads sees the old tid and parks again — a lost wakeup that
/// only shows up when the joiner happens to be scheduled between the two.
fn teardown(slot: usize) {
    // SAFETY: raw-pointer access under the BKL.
    let Some(t) = (unsafe { (*threads())[slot].take() }) else { return };
    if t.clear_child_tid != 0 {
        let _ = crate::uaccess::write_val::<u32>(t.clear_child_tid, 0);
        crate::futex::sys_futex(t.clear_child_tid, u64::from(FUTEX_WAKE_PRIVATE), 1, 0, 0, 0);
    }
    // The slot this thread ran on becomes reusable the moment it is `Finished`.
    // Anything of its still on the futex table would then name whoever inherits
    // it, and absorb a wake meant for them.
    crate::futex::purge_task(t.task);
}

/// `FUTEX_WAKE | FUTEX_PRIVATE_FLAG`, the op `teardown`'s wake uses. Spelled
/// here rather than reached through `akuma_syscalls_linux` so the one place
/// the kernel *originates* a futex op is visible in this file.
const FUTEX_WAKE_PRIVATE: u32 = 1 | 128;

/// Ask every thread of `proc_slot` other than the caller to leave ring 3, and
/// wait for them.
///
/// Called from the main thread's exit path. A thread parked in `FUTEX_WAIT`
/// notices through [`group_exiting`], which its poll loop checks; one running
/// in ring 3 notices at its next syscall. Threads have no signals here, so a
/// thread in a genuinely unbounded ring-3 loop is not reachable — bounded by
/// the same preemption that bounds any other runaway user program, and called
/// out in `AKUMA_AMD64_RUST_STD.md` as a known gap rather than papered over.
pub fn drain(proc_slot: usize) {
    set_group_exiting(proc_slot);
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
    // SAFETY: raw-pointer read under the BKL.
    unsafe {
        for t in (*threads()).iter().flatten() {
            if t.proc_slot == proc_slot {
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
