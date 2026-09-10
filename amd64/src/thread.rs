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
    /// The process slot whose address space this thread runs in — shared with
    /// its process, which is what makes fd 0/1/2 route the same way.
    proc_slot: usize,
    /// Linux tid — **the kernel thread slot**, which is also what `clone(2)`
    /// returns and what every per-thread array in the tree is indexed by.
    ///
    /// It was `usermode::alloc_pid()`, the shared pid/tid counter, on the
    /// argument that a tid and a pid must never name two different things.
    /// True, and outranked: musl caches `clone`'s return value in
    /// `pthread_self()->tid` and `tkill`s it, so the tid `gettid` reports has
    /// to be the number the kernel indexes by, or a thread signals a stranger.
    /// The shared `clone_thread` returns the slot and says so at length; this
    /// stores the same value rather than a second one.
    tid: u32,
    /// `CLONE_CHILD_CLEARTID`'s address, or 0. On exit the kernel zeroes this
    /// word and wakes one futex waiter on it — which is precisely how
    /// `pthread_join` learns the thread is gone, and the reason a `join` that
    /// never returns is the classic symptom of getting this wrong.
    clear_child_tid: u64,
}

/// `static mut` under the BKL — the discipline `usermode::PROCS` had before 5b
/// slice 4 deleted it: every writer is kernel code and kernel code holds the lock.
static mut THREADS: [Option<Thread>; MAX_THREADS] = [const { None }; MAX_THREADS];

fn threads() -> *mut [Option<Thread>; MAX_THREADS] {
    &raw mut THREADS
}

/// Set when any thread of a process calls `exit_group`, so its siblings leave
/// ring 3 at their next syscall instead of running on into a torn-down space.
///
/// Indexed by process slot. An `AtomicU32` bitmap would be tighter; an array of
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

pub fn current_thread_slot() -> usize {
    // SAFETY: under the BKL; the per-CPU `UserCtx` is the running task's.
    unsafe {
        let uctx = crate::smp::current_uctx();
        if uctx.is_null() { NO_THREAD } else { (*uctx).thread_slot }
    }
}

/// `clone(flags, child_stack, parent_tid, child_tid, tls)` — x86_64 56, the
/// `CLONE_VM` arm. **Served by `akuma_exec::process::clone_thread` since the
/// ring-3 seam's `clone` fold**; what is left here is this target's argument
/// checks and its errno vocabulary.
///
/// x86_64's argument order is its own — `tls` **last**, after `child_tid`,
/// where most architectures put it fourth — which is why the forward reorders
/// rather than passing straight through.
///
/// # The three refusals, kept
///
/// All three predate the fold and none is expressible in the shared path,
/// which takes flags it has already been told are a thread clone:
///
/// * **`CLONE_THREAD` is required.** `CLONE_VM` without it is a distinct
///   thing — a process that shares memory, which some sandboxes ask for — and
///   it needs its own `wait4` semantics. Refuse rather than approximate: a
///   "thread" a parent's `wait4` never reaps hangs the parent, which is much
///   harder to read than an `ENOSYS` at the call.
/// * **`CLONE_SETTLS` is required.** Without it a musl thread runs on the
///   *parent's* `%fs`, so both write one `struct pthread` and the first thing
///   to break is `errno`. There is no sane base to invent.
/// * **a stack is required.** The shared path checks this too (and says so);
///   checked here as well so the errno is `EINVAL` rather than a generic
///   failure string turned into `EAGAIN`.
///
/// # Errno mapping
///
/// `clone_thread` returns `&'static str`, and the two failures a caller must
/// tell apart are the resource limits: `EAGAIN` is what `pthread_create`
/// already knows how to report and what Linux returns at the thread or task
/// ceiling. Everything else is `ENOMEM`.
pub fn sys_clone_thread(
    flags: u64,
    child_stack: u64,
    parent_tid: u64,
    child_tid: u64,
    tls: u64,
) -> u64 {
    use crate::usermode::clone_flags::*;

    if flags & CLONE_THREAD == 0 {
        return errno::ENOSYS;
    }
    if child_stack == 0 {
        return errno::EINVAL;
    }
    if flags & CLONE_SETTLS == 0 {
        return errno::EINVAL;
    }
    // A task that is not running a registered process has no thread group to
    // join. `current_proc_slot` answering `usize::MAX` is how that shows up,
    // and `bind_clone_child` would have nothing to key its row on.
    if crate::usermode::current_proc_slot() >= crate::usermode::PROC_SLOTS {
        return errno::ENOSYS;
    }

    // **The argument order is `(stack, tls, parent_tid, child_tid, flags)`** —
    // `clone_thread`'s own, which is neither x86_64's syscall order (`flags`
    // first, `tls` **last**) nor asm-generic's. Getting it wrong here does not
    // fail: it hands `flags` to `stack`, and the child enters ring 3 on a
    // plausible-looking address that is the parent's. The first `clone` after
    // the fold did exactly that and faulted on its first `popq` with
    // `cr2 == rsp` — the probe pushes the child's entry point onto the child
    // stack before the `syscall` and the child pops it back off, so a wrong
    // `sp` is a fault on the very first instruction. Spelled out per-argument
    // rather than positionally, so a future reader is not asked to trust the
    // order.
    match akuma_exec::process::clone_thread(
        /* stack */ child_stack,
        /* tls */ tls,
        /* parent_tid_ptr */ parent_tid,
        /* child_tid_ptr */ child_tid,
        /* flags */ flags,
    ) {
        Ok(tid) => u64::from(tid),
        Err(e) => {
            serial::puts("  [clone] ");
            serial::puts(e);
            serial::puts("\n");
            if e.contains("table full") || e.contains("thread slot") {
                errno::EAGAIN
            } else {
                errno::ENOMEM
            }
        }
    }
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
    // SAFETY: raw-pointer read under the BKL.
    let Some(slot) = (unsafe { (*threads()).iter().position(Option::is_none) }) else {
        // The caller turns this string into `EAGAIN`, which is what Linux
        // returns at the thread limit and what `pthread_create` reports.
        serial::puts("  [clone] thread table full\n");
        return Err("clone: thread table full");
    };
    // SAFETY: raw-pointer write under the BKL; `slot` was just found free.
    unsafe {
        (*threads())[slot] = Some(Thread {
            task,
            proc_slot,
            tid: task as u32,
            clear_child_tid,
        });
    }
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
/// **There is no `execve` loop here** and that is not an omission: `execve`
/// from a non-main thread replaces the whole process image, which on Linux
/// kills every sibling first. This target refuses it (`sys_execve` is reached
/// only by a main thread), so a thread enters ring 3 exactly once.
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
    // recycled by `x86_claim_slot` taking a `TERMINATED` one directly rather
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
