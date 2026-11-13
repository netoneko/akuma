//! Processes on this kernel: the slot table every forked task and thread
//! occupies, `clone`/`fork`/`vfork` through `akuma-exec`'s `fork_process`,
//! `execve` and exit (`wait4` is glue's, over the child channels), init's thread, the two signal arms this kernel
//! keeps (`rt_sigsuspend`, `rt_sigreturn`), and the machine-state arms
//! (`X86ArchHooks`) the shared scheduler calls.
//!
//! This file began as the stage-4 child model, where Akuma's private `SPAWN` ran
//! a child to completion inside the syscall. That path, its syscall and its
//! crossing are gone (`docs/archive/TAOXICHUAN_CLEANUP_PLAN.md`, step 2e): every
//! process here is a fork child of init.

use alloc::string::String;
use alloc::vec::Vec;
use akuma_ring3_amd64 as ring3;

/// One ring slot: a forked task or a `CLONE_THREAD` sibling, from
/// `bind_child_task` until it is reaped. The slot index is the task's
/// threading tid.
#[allow(clippy::struct_excessive_bools)] // one flag per independent slot fact; a bitset would hide them
pub struct SpawnSlot {
    pub used: bool,
    pub pid: u32,
    /// The task has not exited yet (set at bind, cleared at exit).
    pub running: bool,
}

impl SpawnSlot {
    const fn new() -> Self {
        Self {
            used: false,
            pid: 0,
            running: false,
        }
    }
}

/// 12: the shell's own slot (0) plus eleven threading tids for real fork
/// children — the crate claims tids from 1, and on this kernel the tid and
/// the ring index are the SAME number by construction (`bind_child_task`
/// stamps by tid; `switch_to`/`task()` index the same arrays).
pub const NSLOTS: usize = 48;

/// The spawn ring's rows. A [`BorrowCell`](akuma_primitives::cell::BorrowCell): the only way in is [`with_slots`] (or the one-field
/// readers below), so two overlapping borrows are a panic naming the bug, and no closure ever calls out to anything that
/// reaches the table again — `schedule_blocking` and every print run with the borrow closed.
static SLOTS: akuma_primitives::cell::BorrowCell<[SpawnSlot; NSLOTS]> =
    akuma_primitives::cell::BorrowCell::new([const { SpawnSlot::new() }; NSLOTS]);
// Pids come from `akuma_exec::process::allocate_pid` (floor 2: 1 is sshd,
// the boot's init task) - ONE allocator, because `clone_thread` draws its
// thread Processes' pids from the crate's counter and a second counter here
// issued the same pid twice (a thread and a fork child sharing a table row).

/// Run `f` on the spawn ring. Panics on a nested call, which is the point.
fn with_slots<R>(f: impl FnOnce(&mut [SpawnSlot; NSLOTS]) -> R) -> R {
    SLOTS.with_mut(f)
}

/// The pid in ring row `slot`.
fn slot_pid(slot: usize) -> u32 {
    SLOTS.with(|s| s[slot].pid)
}

/// The `ProcessHooks::pid_for_thread` / `find_pid_by_thread` arm: the pid a
/// spawn-ring slot runs under today, `None` for an unused slot. The thread
/// namespace on this kernel IS the spawn ring until `fork_process` (slice 3)
/// replaces it — one table, one answer.
pub fn pid_for_slot(idx: usize) -> Option<u32> {
    if idx < NSLOTS {
        SLOTS.with(|s| s[idx].used.then_some(s[idx].pid))
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// `execve`'s argument block.
// ---------------------------------------------------------------------------

/// Entry caps. `ring3::build_stack` holds 512 strings in all (argv + envp), so
/// the exec path refuses a larger sum with `E2BIG`. Cargo's heaviest `rustc`
/// call for this kernel has 344 arguments, one of 5192 bytes, ~19.6 KB of
/// arguments and 84 variables; the earlier caps (128 entries, 4096 bytes per
/// string) answered such an `execve` `EFAULT` ("Bad address") and failed the
/// build (`docs/archive/TAOXICHUAN_BUGS_FOUND_2026_10_30.md`).
const ARGV_MAX: usize = 448;
const ENV_MAX: usize = 256;
const MAX_STRS: usize = 512;
const STR_MAX: usize = 32 * 1024;
/// The loader lays argv/envp/auxv out in a block of `ARG_BLOCK_PAGES` pages;
/// leave room for the pointer vectors, auxv and alignment.
const ARG_BYTES_MAX: usize = crate::ARG_BLOCK_PAGES * 4096 - 2048;

/// `execve`'s `(path, argv, envp)` as this kernel's loader takes them.
struct ExecArgs {
    path: String,
    argv: Vec<String>,
    env: Vec<String>,
}

// ---------------------------------------------------------------------------
// M4 slice 3: the real fork — premise 2's build-out
// ---------------------------------------------------------------------------

/// The task record each ring slot runs against: `slot + 1` for a process, the
/// creator's record for a `CLONE_THREAD` sibling - which is how threads share
/// regions, brk/mmap cursors, the fd table and the cwd with no second copy to
/// keep coherent.
static TASK_OF: akuma_primitives::cell::BorrowCell<[usize; NSLOTS]> = akuma_primitives::cell::BorrowCell::new({
    let mut a = [0usize; NSLOTS];
    let mut i = 0;
    while i < NSLOTS {
        a[i] = i + 1;
        i += 1;
    }
    a
});

fn task_of(slot: usize) -> usize {
    TASK_OF.with(|t| t[slot])
}

/// Is ring slot `slot` an occupied task or thread (the target of a `tkill`)?
pub fn slot_in_use(slot: usize) -> bool {
    slot < NSLOTS && SLOTS.with(|s| s[slot].used)
}

/// Is the running task a `CLONE_THREAD` sibling?
pub fn is_thread_task() -> bool {
    akuma_exec::process::current_process_shared().is_some_and(|p| p.pid != p.tgid)
}

/// The thread group of the task in slot `slot`: its `Process`'s `tgid` (a
/// `clone_thread` Process carries its leader's pid), else its own pid.
fn slot_tgid(slot: usize) -> u32 {
    let pid = slot_pid(slot);
    akuma_exec::process::table::with_process(pid, |p| p.tgid).unwrap_or(pid)
}

/// `fork_process`'s `bind_child_task` records: (tid, pid, root) — the twin's
/// spawn-row. Indexed by tid; `root == 0` = free.
static CHILD_BIND: akuma_primitives::cell::BorrowCell<[(usize, u32, u64); NSLOTS]> = akuma_primitives::cell::BorrowCell::new([(0, 0, 0); NSLOTS]);

/// `ExecRuntime::bind_child_task`'s twin: stamp the child's spawn-row. The
/// crate publishes the child's thread right after this returns, so a full
/// ring must come back as an error the fork reports, not a panic.
/// The pool slot init runs on.
static INIT_TID: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(usize::MAX);
/// Init's entry point and initial stack, for [`init_thread_entry`].
static INIT_ENTRY: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static INIT_RSP: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// A kernel stack for init's thread context (the context switch's own; the syscall and trap bands come from
/// `prepare_task_slot`). Leaked once: init lives until the machine stops.
const INIT_STACK_WORDS: usize = 4096;

/// First instruction of init's pool thread: cross into ring 3 at the image's entry. The crate's switch hook has already named
/// this slot current, loaded its band and CR3 from the bind record, so this is the same entry a fork child takes.
extern "C" fn init_thread_entry() -> ! {
    // The crate does not name a freshly seeded thread current: until this line the boot thread's slot (0) is still the answer,
    // and init's first syscalls would run against task record 0 while a tick later switches them onto its own record.
    crate::exec_twin::set_current_slot(INIT_TID.load(core::sync::atomic::Ordering::Relaxed));
    let ctx = akuma_exec::process::UserContext::new(
        INIT_ENTRY.load(core::sync::atomic::Ordering::Relaxed) as usize,
        INIT_RSP.load(core::sync::atomic::Ordering::Relaxed) as usize,
    );
    crate::exec_twin::enter_user_ctx(&ctx)
}

/// Run the loaded boot program as pid 1 on a real pool thread and park the boot thread, the shape of amd64's
/// `run_init`. The thread is claimed, stamped in the ring (`bind_child_task`: the same row a fork child gets, with the task
/// record copied from the one `load_task` filled), registered as a `Process` (so `fork_process` finds a caller), seeded to enter
/// ring 3 and published. The boot thread then waits for init to finish and reports its status like any boot program.
pub fn start_init_thread(path: &str, ti: &crate::TaskImage) -> ! {
    let Some(tid) = akuma_threading::x86_claim_slot() else {
        akuma_primitives::safe_print!(96, "  [init] no pool slot for init\n");
        crate::halt();
    };
    if let Err(msg) = bind_child_task(tid, 1, ti.root_pa, false) {
        akuma_primitives::safe_print!(160, "  [init] bind refused: {}\n", msg);
        crate::halt();
    }
    register_shell_process(1, path, ti.root_pa, ti.entry, ti.rsp, tid);
    INIT_TID.store(tid, core::sync::atomic::Ordering::Relaxed);
    INIT_ENTRY.store(ti.entry, core::sync::atomic::Ordering::Relaxed);
    INIT_RSP.store(ti.rsp, core::sync::atomic::Ordering::Relaxed);
    // Straight onto the heap: `Box::new([0; N])` builds the 32 KiB array on this stack first.
    let stack: &'static mut [u64] = alloc::boxed::Box::leak(alloc::vec![0u64; INIT_STACK_WORDS].into_boxed_slice());
    let top = stack.as_mut_ptr() as usize + INIT_STACK_WORDS * 8;
    akuma_threading::x86_seed_entry(tid, top, init_thread_entry);
    // The boot thread becomes pool slot 0 (the pool hands out 1.., amd64's `x86_adopt_running_thread(0)`): it is about to
    // block, and a thread the pool has never heard of has no slot to park.
    let _ = akuma_sched_amd64::prepare_task_slot::<crate::sched::Tx>(0);
    akuma_threading::x86_adopt_running_thread(0);
    crate::exec_twin::set_current_slot(0);
    akuma_threading::x86_publish(tid);
    crate::PREEMPT_ARMED.store(true, core::sync::atomic::Ordering::Release);
    akuma_primitives::safe_print!(96, "[init] pid 1 on pool slot {}\n", tid);
    // The boot thread: a timed sleep (an unbounded park starves init's startup) that init's exit also wakes by event
    // (`task_exit_status`); on wake, finish as the boot program's exit.
    loop {
        let code = INIT_EXIT.load(core::sync::atomic::Ordering::Acquire);
        if code != INIT_RUNNING {
            crate::task_done(code);
        }
        akuma_threading::schedule_blocking(crate::net::uptime_us() + 10_000);
    }
}

/// Init's exit code as the boot report shows it (`128 + signal` for a signal death), or
/// [`INIT_RUNNING`]. Init has no parent to `wait4` for it, so its exit is recorded here.
static INIT_EXIT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(INIT_RUNNING);
const INIT_RUNNING: u64 = u64::MAX;

pub fn bind_child_task(tid: usize, pid: u32, root_pa: u64, is_thread: bool) -> Result<(), &'static str> {
    if tid >= NSLOTS {
        return Err("taoxichuan: child tid outside the spawn ring");
    }
    // The tid IS this child's slot index (see NSLOTS). Stamp its SpawnSlot,
    // give it its kernel+trap stacks (leaked pairs, reused on recycle —
    // `switch_to` points the syscall stub here), and give a fork child a task
    // record of its own (`tid + 1`): the root from `fork_process`'s cloned
    // address space, the layout from the parent's. A thread shares its
    // creator's record.
    let slot_idx = tid;
    with_slots(|s| {
        s[slot_idx] = SpawnSlot::new();
        s[slot_idx].used = true;
        s[slot_idx].running = true;
        s[slot_idx].pid = pid;
    });
    let _ = akuma_sched_amd64::prepare_task_slot::<crate::sched::Tx>(slot_idx);
    if is_thread {
        // A thread runs against its creator's record: nothing to copy, and
        // nothing to clone-ref (the fd table is the creator's own).
        TASK_OF.with_mut(|t| t[slot_idx] = crate::task_slot_index());
    } else {
        TASK_OF.with_mut(|t| t[slot_idx] = slot_idx + 1);
    }
    if !is_thread {
        // The child's layout is the parent's, minus the shared file mappings: its copies of
        // those pages are private, and writing them back would put its bytes in the file.
        let layout = crate::with_task(|t| t.layout.fork());
        crate::with_task_slot(slot_idx + 1, |ct| {
        ct.root_pa = root_pa;
        ct.layout = layout;
        ct.exit = None;
        // The descriptors are the registered Process's: `fork_process` deep-cloned
        // its table (pipe and socket references taken, epoll fds stripped).
        });
    }
    CHILD_BIND.with_mut(|b| b[tid] = (tid, pid, root_pa));
    Ok(())
}

/// The kernel-stack top `switch_to` installs for `tid`: a slot's leaked pair
/// (`prepare_task_slot`); anything else (the idle threads) gets the current
/// global, which the caller leaves alone.
pub fn sched_kstack_top(tid: usize) -> u64 {
    if tid < NSLOTS {
        crate::sched::kstack_top(tid).unwrap_or_else(ring3::syscall_stack_top)
    } else {
        ring3::syscall_stack_top()
    }
}

/// The CR3 `switch_to` loads for `tid`: the task record's live root (exec
/// updates it — a real child that exec'd must come back on the NEW image's
/// root). 0 = leave CR3 alone (the idle thread runs on the kernel root).
pub fn sched_task_slot_of(tid: usize) -> usize {
    if tid < NSLOTS {
        task_of(tid)
    } else {
        tid + 1
    }
}

pub fn sched_root_of(tid: usize) -> u64 {
    if tid < NSLOTS {
        crate::task_root_of(task_of(tid))
    } else {
        0
    }
}

/// A forked task is on the CPU: a threading tid 1..NSLOTS whose slot is
/// stamped, running and bound. The boot thread (tid 0) and the idle threads
/// are not.
pub fn is_real_task() -> bool {
    let cur = crate::exec_twin::current_slot();
    if cur == crate::exec_twin::NO_SLOT || cur == 0 || cur >= NSLOTS {
        return false;
    }
    SLOTS.with(|s| s[cur].used && s[cur].running)
}

/// A forked task's `exit_group` (or a lone process's `exit`): the whole group
/// ends with exit code `code & 0xFF`. Never returns.
pub fn task_exit_real(code: u64) -> ! {
    task_exit_code((code & 0xFF) as i32)
}

/// `akuma-threading`'s slot-purge hook: thread `tid` has just been marked
/// TERMINATED, by this kernel's exit path or by `akuma-exec` (a fatal signal's
/// default action inside `akuma-syscalls-glue`: SIGPIPE on a pipe with no
/// reader runs glue's `sys_exit_group`). Purge its futex waits; for the last
/// thread of a process, clear the row's `running` and hand its page-table root
/// to [`zombie_root`], so glue's reap can never drop it.
pub fn on_thread_terminated(tid: usize) {
    akuma_syscalls_glue::futex_purge_tid(tid);
    if tid >= NSLOTS {
        return;
    }
    let pid = slot_pid(tid);
    let Some(p) = akuma_exec::process::lookup_process_shared(pid) else {
        return;
    };
    if p.pid != p.tgid || !p.exited.load(core::sync::atomic::Ordering::Relaxed) {
        return; // a thread (its space is the leader's), or a process still running
    }
    let was_running = with_slots(|s| core::mem::replace(&mut s[tid].running, false));
    if was_running || OWNED_ROOT.with(|t| t[tid]) != 0 {
        zombie_root(tid, pid);
    }
}

/// A forked task killed by a fault: the group dies with wait status SIGSEGV
/// (11), the parent's `wait4` sees `WIFSIGNALED` as on Linux, and the machine
/// keeps running.
pub fn task_die_segv() -> ! {
    task_exit_code(-11)
}

/// The exiting process's descriptors, closed now rather than when its
/// zombie is reaped: a pipe's EOF is its last writer's close, so a pipeline's
/// reader would otherwise wait on a parent that waits on it. Each release goes
/// through the runtime hooks (the tables are glue's). Callers have already
/// stopped every thread that shares the table (`kill_siblings`).
fn close_process_fds() {
    if let Some(p) = akuma_exec::process::current_process_shared() {
        p.fds.close_all();
    }
}

/// End the caller's whole process with `code`: an exit code (0..=255), or
/// `-signal` for a death by signal, the form glue's `encode_wait_status` reads.
///
/// The exit is published the way `akuma-exec` publishes every exit: the
/// `Process` is marked a zombie, and `publish_child_exit` marks the child
/// channel `fork_process` registered (waking a parent parked in glue's
/// `wait4`) and raises SIGCHLD on the parent. Glue's `wait4` reaps it.
fn task_exit_code(code: i32) -> ! {
    // A shared file mapping persists at exit (rust-lld never munmaps its output).
    crate::shared_flush_all();
    let cur = crate::exec_twin::current_slot();
    // The group's leader slot: `cur` itself unless a thread is the one
    // calling `exit_group`. Every other thread of the group dies here
    // (Linux's `exit_group`): purged from the futex table and terminated.
    let lead = group_leader(cur);
    kill_siblings(cur, lead);
    let lead_pid = with_slots(|s| {
        s[lead].running = false;
        s[lead].pid
    });
    // The zombie keeps its thread slot until the parent's wait4: without this
    // hold `x86_claim_slot` hands the TERMINATED slot to the next fork while the
    // pid is still waitable. Glue's reap (`unregister_process`) releases it.
    akuma_exec::process::hold_exited_thread_slot(lead_pid, lead);
    if let Some(p) = akuma_exec::process::lookup_process_shared(lead_pid) {
        p.exit_code.store(code, core::sync::atomic::Ordering::Relaxed);
        p.state.store(akuma_exec::process::ProcessState::Zombie(code));
        p.exited.store(true, core::sync::atomic::Ordering::Release);
    }
    close_process_fds();
    zombie_root(lead, lead_pid);
    // Init has no parent to wait for it: the parked boot thread reports it.
    if lead == INIT_TID.load(core::sync::atomic::Ordering::Relaxed) {
        let shown = if code < 0 { 128 + u64::from(code.unsigned_abs()) } else { code as u64 };
        INIT_EXIT.store(shown, core::sync::atomic::Ordering::Release);
        akuma_threading::get_waker_for_thread(0).wake();
    }
    akuma_exec::process::publish_child_exit(lead_pid, code);
    // The group's children - its threads' too - go to init, which reaps them (this kernel's
    // init, sshd, runs `wait4(-1, WNOHANG)`), as amd64 and glue's exit do. Without it an orphan
    // stayed registered under a dead parent nobody could wait for, and its thread slot leaked
    // for good: `killtree` ran the 48-slot ring out by its third round.
    let _ = akuma_exec::process::reparent_group_children_to(lead_pid, 1);
    if lead != cur {
        akuma_syscalls_glue::futex_purge_tid(lead);
        akuma_threading::mark_thread_terminated(lead);
    }
    akuma_threading::x86_finish_current()
}

/// The slot that owns the process `cur` belongs to: the slot whose pid is the
/// group's `tgid`. `cur` when it is the leader (or the leader is gone).
fn group_leader(cur: usize) -> usize {
    if cur >= NSLOTS {
        return cur;
    }
    let tgid = slot_tgid(cur);
    if slot_pid(cur) == tgid {
        return cur;
    }
    (0..NSLOTS).find(|&s| s != cur && slot_in_use(s) && slot_pid(s) == tgid).unwrap_or(cur)
}

/// `exit_group`: end every thread of `cur`'s group except `cur` and `lead`
/// (the caller finishes those itself). A thread is a slot whose `Process` has
/// the group's `tgid` and a pid of its own.
///
/// A sibling that is not executing (parked, or ready to run) is ended here. One
/// RUNNING on another core cannot be: marking it TERMINATED does not stop it, and
/// it ran on in a dying process - parked in a `futex` it overwrote its own
/// TERMINATED with WAITING (`[REVIVE]`), with its `Process` already retired. So it
/// gets a kill request instead, ends itself at its next kernel entry or exit
/// ([`finish_killed`]: the syscall dispatcher and the ring-3 tick), and this
/// caller waits for that with the lock released, because the group's teardown
/// after this must not run under a thread still in ring 3.
/// `docs/archive/TAOXICHUAN_EXIT_GROUP_RUNNING_SIBLING.md`.
fn kill_siblings(cur: usize, lead: usize) {
    let tgid = slot_tgid(cur);
    let mut asked = [0u32; NSLOTS]; // the pid asked to end, per slot; 0 = none
    for (s, ask) in asked.iter_mut().enumerate() {
        let spid = slot_pid(s);
        if s == cur || s == lead || !slot_in_use(s) || spid == tgid || slot_tgid(s) != tgid {
            continue;
        }
        if akuma_threading::get_thread_state(s) == akuma_threading::thread_state::RUNNING {
            akuma_threading::request_thread_kill(s);
            *ask = spid;
            continue;
        }
        end_sibling(s, spid);
    }
    // Ring 3 reaches the kernel at least once a tick, so this is ~10 ms per sibling;
    // the bound is for a thread that never does (stuck with the lock released).
    let start = crate::net::uptime_us();
    while asked.iter().any(|&p| p != 0) {
        for (s, p) in asked.iter_mut().enumerate() {
            // Gone: terminated, or the slot already belongs to someone else.
            if *p != 0 && (akuma_threading::is_thread_terminated(s) || slot_pid(s) != *p) {
                *p = 0;
            }
        }
        if crate::net::uptime_us().saturating_sub(start) > 2_000_000 {
            for (s, p) in asked.iter_mut().enumerate().filter(|(_, p)| **p != 0) {
                akuma_primitives::safe_print!(96, "[exit_group] slot {} never reached the kernel; ended in place\n", s);
                end_sibling(s, *p);
                *p = 0;
            }
            break;
        }
        crate::smp::bkl_unlocked(|| {
            for _ in 0..2000 {
                core::hint::spin_loop();
            }
        });
    }
}

/// End sibling slot `s` (pid `spid`) of an exiting group where it stands: it is
/// not executing, so it never runs again.
fn end_sibling(s: usize, spid: u32) {
    let _ = akuma_threading::take_kill_request_via_tid(s);
    akuma_syscalls_glue::futex_purge_tid(s);
    with_slots(|t| {
        t[s].running = false;
        t[s].used = false;
    });
    akuma_threading::mark_thread_terminated(s);
    retire_thread_process(spid);
}

/// Did `exit_group` ask the running thread to end ([`kill_siblings`])?
pub fn kill_requested() -> bool {
    let cur = crate::exec_twin::current_slot();
    cur < NSLOTS && akuma_threading::has_pending_kill(cur)
}

/// End the running thread at a kill request: no exit status (its group's
/// exiting thread publishes that), no clear-child-tid wake (the group is going).
pub fn finish_killed() -> ! {
    let cur = crate::exec_twin::current_slot();
    let _ = akuma_threading::take_kill_request_via_tid(cur);
    akuma_syscalls_glue::futex_purge_tid(cur);
    let pid = with_slots(|s| {
        s[cur].running = false;
        s[cur].used = false;
        s[cur].pid
    });
    retire_thread_process(pid);
    akuma_threading::x86_finish_current()
}

/// Take a dead thread's registered `Process` out of the process table.
///
/// `clone_thread` registers a real `Process` per thread (so `current_process` answers
/// per thread), and nothing but the reap - which runs for a group LEADER, on its
/// parent's `wait4` - ever unregistered one. A thread's own `exit` and `exit_group`'s
/// `kill_siblings` freed the ring slot and left the `Process` ACTIVE for the rest of the
/// boot, so every thread leaked a table entry: a `cargo build` of this kernel (cargo plus
/// four `rustc`, each with a pool of threads) reached 256 live entries and panicked
/// "Process table full" (`docs/archive/TAOXICHUAN_BUGS_FOUND_2026_10_30.md` §7).
///
/// Retire only: the `Process` is freed by the next `drain_retired` (reap, idle loop, memory
/// pressure), never here, because the exiting thread is still running on it. A thread's
/// address space is the leader's shared view, so dropping it frees no frames.
fn retire_thread_process(pid: u32) {
    let _ = akuma_exec::process::unregister_process(pid);
}

/// A thread's `exit` (60): only this thread ends. `CLONE_CHILD_CLEARTID`
/// (what `pthread_join` waits on): the tid word is zeroed and its futex
/// woken before the slot is terminated.
pub fn task_exit_thread() -> ! {
    let cur = crate::exec_twin::current_slot();
    if let Some(p) = akuma_exec::process::current_process_shared() {
        let addr = p.clear_child_tid.swap(0, core::sync::atomic::Ordering::AcqRel);
        if addr != 0 {
            let _ = crate::copy_to_user(addr, &0u32.to_ne_bytes());
            akuma_syscalls_glue::futex_wake(p.tgid, addr as usize, 1);
        }
    }
    akuma_syscalls_glue::futex_purge_tid(cur);
    // The slot leaves the table: a thread has no status for a parent to reap
    // and must never look like a child to a `wait4` scan.
    let pid = with_slots(|s| {
        s[cur].running = false;
        s[cur].used = false;
        s[cur].pid
    });
    retire_thread_process(pid);
    akuma_threading::x86_finish_current()
}

/// `clone(CLONE_THREAD|CLONE_VM|...)`: another thread of the caller's group
/// through `akuma_exec::process::clone_thread`, which shares the address
/// space and publishes a runnable slot; the bind hook gives it the creator's
/// task record. Returns the thread's tid (its threading slot).
fn clone_thread_arm(fr: &ring3::SyscallFrame) -> u64 {
    let cur = crate::exec_twin::current_slot();
    if cur == crate::exec_twin::NO_SLOT {
        return crate::e(ring3::err::EPERM); // the boot task has no Process to clone
    }
    let (flags, stack, ptid, ctid, tls) = (fr.arg1(), fr.arg2(), fr.arg3(), fr.arg4(), fr.arg5());
    // Demand-driven drain at syscall ENTRY, before anything is claimed: every thread owns
    // a `Process` row, `retire_thread_process` only retires it, and this is the one path
    // that consumes rows, so it pays for collecting them. Not at thread teardown: reclaim
    // runs `Process::drop`, and the exiting thread is still running on its own row
    // (`docs/archive/AKUMA_AMD64_THREAD_LIFECYCLE.md` §3 - the same fix on the amd64 kernel).
    collect();
    match akuma_exec::process::clone_thread(stack, tls, ptid, ctid, flags) {
        Ok(tid) => u64::from(tid),
        Err(msg) => {
            akuma_primitives::safe_print!(160, "[clone] thread refused: {}\n", msg);
            crate::e(ring3::err::EAGAIN)
        }
    }
}

pub fn exec_real_task(fr: &ring3::SyscallFrame) -> u64 {
    let cur = crate::exec_twin::current_slot();
    let mut budget = akuma_syscalls_proc::exec::Budget::new(MAX_STRS, STR_MAX, ARG_BYTES_MAX);
    let req = match akuma_syscalls_glue::execve::read_request(
        fr.arg1(),
        fr.arg2(),
        fr.arg3(),
        ARGV_MAX,
        ENV_MAX,
        &mut budget,
    ) {
        Ok(r) => r,
        Err(r) => return r,
    };
    // This kernel's loader takes `&str`: an argument that is not UTF-8 cannot
    // reach it (the answer this path has always given).
    let utf8 = |v: Vec<Vec<u8>>| v.into_iter().map(String::from_utf8).collect::<Result<Vec<String>, _>>();
    let (Ok(argv_s), Ok(env_s)) = (utf8(req.argv), utf8(req.envp)) else {
        return crate::e(crate::err::EFAULT);
    };
    let args = ExecArgs { path: req.path, argv: argv_s, env: env_s };
    let effective = crate::resolve_effective(&args.path);
    let image = match crate::read_image(&effective) {
        Ok(i) => i,
        Err(err) => {
            akuma_primitives::safe_print!(192, "[exec] '{}': read failed ({:?})\n", effective, err);
            return crate::e(crate::err::ENOENT);
        }
    };
    if image.len() < 4 || image[..4] != [0x7f, b'E', b'L', b'F'] {
        return crate::e(crate::err::ENOEXEC);
    }
    let argv: Vec<&str> = args.argv.iter().map(String::as_str).collect();
    let env: Vec<&str> = args.env.iter().map(String::as_str).collect();
    // The image about to be replaced may hold shared file mappings.
    crate::shared_flush_all();
    let old_root = crate::task_root();
    let Some(ti) = crate::load_task(&argv, &env, &image) else {
        return crate::e(crate::err::ENOMEM);
    };
    // POSIX exec keeps open fds across the image swap; close-on-exec ones go,
    // each released through the runtime hooks (the table is the process's).
    // The process side (`/proc/self/exe`, which musl's `$ORIGIN` expansion
    // reads; close-on-exec; caught handlers reset) is glue's, shared with amd64.
    if let Some(p) = akuma_exec::process::current_process_shared() {
        let argv_bytes: Vec<Vec<u8>> = args.argv.iter().map(|a| a.clone().into_bytes()).collect();
        akuma_syscalls_glue::execve::commit(p, &effective, &argv_bytes);
    }
    // The registered Process's shared root view goes stale with the old
    // image — re-point it.
    {
        let pid = slot_pid(cur);
        let first_exec = OWNED_ROOT.with(|t| t[cur]) == 0;
        if disown_space(pid, ti.root_pa) {
            // A fork child's first exec: the root it ran on is the fork
            // clone (`clone_user_half`), every frame of which is raw — see
            // `disown_space`. Later execs park the previous exec'd root in
            // `adopt_exec_root`.
            if first_exec {
                park_root(old_root);
            }
            adopt_exec_root(cur, ti.root_pa);
        }
    }
    let ef = ring3::EntryFrame {
        r15: 0, r14: 0, r13: 0, r12: 0, rbp: 0, rbx: 0, r10: 0, r8: 0, r9: 0,
        rdx: 0, rsi: 0, rdi: 0, rax: 0,
        rip: ti.entry,
        rflags: 0x202,
        rsp: ti.rsp,
    };
    // The incoming image's first instructions must not see the OUTGOING
    // task's fs: the MSR rides the crossing (sysret does not touch it), and
    // a depth-2 exec (ash execs the pipeline's last command in place)
    // otherwise enters the new image with the shell's TLS base still set —
    // boot ph-7's PK2: grandfork's first stack-guard read (fs:0x28) hit the
    // shell's busybox bss at 0x7131c0, unmapped in the fresh CR3, and the
    // task died before musl could run its own arch_prctl. The fork path's
    // inheritance is deliberate (same image, the capture's comment below);
    // the exec path's is not (a new image) — 0, the same clean start a
    // depth-1 exec gets from the boot crossing, and what the amd64 kernel's
    // exec hands its images (grandfork passes there).
    let _ = akuma_cpu::msr::set_fs_base(0);
    crate::sched::set_slot_fs(cur, 0);
    // This function never returns, so nothing local is ever dropped: the
    // executable image (47 MB for cargo) and the argument strings would leak
    // from the kernel heap on every exec. Free them before the crossing.
    drop(argv);
    drop(env);
    drop(args);
    drop(image);
    drop(effective);
    // The new image's root and first frame; the dispatcher frame on the
    // task's own band is abandoned (bounded by exec counts).
    crate::exec_twin::enter_user_on(ti.root_pa, &ef)
}

/// The syscall-path capture `fork_process` step 6 consumes:
/// `get_saved_user_context(parent_tid)` resolves through the threading crate
/// to `X86ArchHooks::read_user_context` — this twin's per-slot record
/// (slice 2), written here. One plain copy per syscall on a slotted task;
/// the boot task (NO_SLOT) skips.
pub fn capture_user_context(fr: &ring3::SyscallFrame) {
    let slot = crate::exec_twin::current_slot();
    if slot == crate::exec_twin::NO_SLOT || slot >= NSLOTS {
        return;
    }
    // `UserContext::regs` is the syscall_entry push order: [rdi, rsi, rdx,
    // r10, r8, r9, rbx, rbp, r12, r13, r14, r15] (akuma-exec-core).
    let ctx = akuma_exec::process::UserContext {
        regs: [
            fr.rdi, fr.rsi, fr.rdx, fr.r10, fr.r8, fr.r9, fr.rbx, fr.rbp, fr.r12, fr.r13,
            fr.r14, fr.r15,
        ],
        sp: ring3::user_rsp_stash(),
        pc: fr.user_rip,
        // The FS base rides the MSR across the fork (sysret does not touch
        // it): the child inherits the parent's TLS pointer in hardware, the
        // same answer a real fork gives. 0 here means "not tracked".
        // Session pi: 0 was written UNCONDITIONALLY — "not tracked" was every
        // sample — so twin_enter_user had no parent value to install and the
        // child inherited WHATEVER the one core's MSR held when it first
        // entered (the last-ran task's TLS; boot pi-2's PK2: grandfork's own
        // arch_prctl won the race on ph-9 and lost it on ph-7/pi-2 with the
        // shell's 0x713198 still in the MSR). Capture the LIVE MSR: this runs
        // in the forking parent's context, before any switch, so IA32_FS_BASE
        // IS the parent's TLS pointer.
        fs_base: akuma_cpu::msr::read(akuma_cpu::msr::Msr::FsBase),
        gs_base: 0,
        rax: fr.rax,
    };
    <crate::sched::Tx as akuma_sched_amd64::Platform>::write_user_context(slot, &ctx);
}

/// The exec'd shell, registered as an `akuma-exec` `Process` — premise 2's
/// build-out: `fork_process` opens with `current_process_shared()`, which
/// resolves thread→pid→`PROCESS_TABLE`, so the shell must be in both before
/// it can fork anything. The literal is amd64's own bootstrap registration
/// (`usermode.rs:4651`) adapted field for field: this kernel's answers where
/// amd64 names its own subsystems — no channel (the session pipe is the
/// twin's own mechanism), a fresh default TerminalState (nothing reads it),
/// the stdio triple for the shared exit path's allocator-safety rule, and a
/// `new_shared` view of the shell's live root — the address space is OWNED
/// by the twin's task record, and a second owner would double-free it (the
/// exact bug amd64's 5b slice 4 note records for its own slice-1 shape).
fn register_shell_process(pid: u32, name: &str, root_pa: u64, entry: u64, rsp: u64, tid: usize) {
    let Some(space) = akuma_mmu::UserAddressSpace::new_shared(root_pa as usize) else {
        akuma_primitives::safe_print!(128, "[spawn] root view for the process table failed\n");
        return;
    };
    let proc = akuma_exec::process::Process {
        pid,
        pgid: pid,
        tgid: pid, // group leader = self, amd64's own answer
        state: akuma_exec::process::AtomicProcessState::new(
            akuma_exec::process::ProcessState::Ready,
        ),
        address_space: akuma_exec::process::ProcAddressSpace::new(space),
        parent_pid: 1, // sshd, the boot's init task (the pid floor is 2)
        brk: core::sync::atomic::AtomicUsize::new(0),
        initial_brk: core::sync::atomic::AtomicUsize::new(0),
        entry_point: core::sync::atomic::AtomicUsize::new(entry as usize),
        // The twin's own Task record owns brk/mmap/stack truth; these scalars
        // feed shared readers only.
        memory: akuma_exec::process::ProcessMemory::new(0, 0, 0, 0),
        process_info_phys: core::sync::atomic::AtomicUsize::new(0), // no info page
        image: spinning_top::Spinlock::new(akuma_exec::process::ProcessImage {
            name: String::from(name),
            exe: String::from(name),
            args: Vec::new(),
            context: akuma_exec::process::UserContext::new(entry as usize, rsp as usize),
        }),
        cwd: String::from("/"),
        root: String::from("/"),
        stdin: alloc::sync::Arc::new(spinning_top::Spinlock::new(
            akuma_exec::process::StdioBuffer::new(),
        )),
        stdout: alloc::sync::Arc::new(spinning_top::Spinlock::new(
            akuma_exec::process::StdioBuffer::new(),
        )),
        exited: core::sync::atomic::AtomicBool::new(false),
        exit_code: core::sync::atomic::AtomicI32::new(0),
        dynamic_page_tables: Vec::new(),
        mmap_regions: akuma_exec::process::AssistedSpinlock::new(Vec::new()),
        lazy_regions: spinning_top::Spinlock::new(akuma_exec::process::LazyRegionMap::new()),
        // stdio triple: `alloc_fd`'s from-0 safety + the shared exit path.
        fds: alloc::sync::Arc::new(akuma_exec::process::SharedFdTable::with_stdio()),
        thread_id: Some(tid),
        spawner_pid: None,
        ctty: core::sync::atomic::AtomicU32::new(0),
        terminal_state: alloc::sync::Arc::new(spinning_top::Spinlock::new(
            akuma_tty::TerminalState::default(),
        )),
        box_id: 0,
        creds: alloc::sync::Arc::new(akuma_exec::process::SharedCreds::new(
            akuma_exec::process::Creds::ROOT,
        )),
        namespace: akuma_isolation::global_namespace(),
        channel: None,
        delegate_pid: None,
        grabbed_by: None,
        clear_child_tid: core::sync::atomic::AtomicU64::new(0),
        robust_list_head: 0,
        robust_list_len: 0,
        signal_actions: alloc::sync::Arc::new(akuma_exec::process::SharedSignalTable::new()),
        signal_mask: 0,
        fault_mutex: spinning_top::Spinlock::new(alloc::collections::BTreeMap::new()),
        sigaltstack_sp: core::sync::atomic::AtomicU64::new(0),
        sigaltstack_flags: core::sync::atomic::AtomicI32::new(2), // SS_DISABLE
        sigaltstack_size: core::sync::atomic::AtomicU64::new(0),
        start_time_us: (akuma_exec::runtime::runtime().uptime_us)(),
        current_syscall: core::sync::atomic::AtomicU64::new(!0),
        last_syscall: core::sync::atomic::AtomicU64::new(0),
        syscall_stats: akuma_exec::process::ProcessSyscallStats::new(),
    };
    akuma_exec::process::register_process(pid, alloc::boxed::Box::new(proc));
    // tid ↔ pid: `current_process_shared()`'s first lookup. tid is this ring
    // slot — the same number `current_slot()` answers from the crossing on.
    akuma_exec::process::thread_pid_map_insert(tid, pid);
}

/// The NEW fork arm: `fork_process` over the registered shell. The child
/// does NOT run here — the crate publishes it as a runnable thread and this
/// arm returns the pid to the caller, exactly like Linux.
/// Fork the caller through `fork_process`. `child_stack` 0: the child resumes
/// on the parent's stack (a fork); otherwise on that stack (a `posix_spawn`
/// child, whose `__clone` stub pops its function and argument from it).
fn fork_arm_real(child_stack: u64) -> u64 {
    // The boot task has no registered Process to fork. Any other task forks
    // for real, however deep: each child has its own thread and stacks.
    let cur = crate::exec_twin::current_slot();
    if cur == crate::exec_twin::NO_SLOT {
        return crate::e(crate::err::EPERM);
    }
    collect();
    let child_pid = akuma_exec::process::allocate_pid();
    match akuma_exec::process::fork_process(child_pid, child_stack) {
        Ok(pid) => {
            // `fork_process` registered the child's exit channel under its
            // parent; glue's `wait4` finds it there (`is_child_of_group`).
            u64::from(pid)
        }
        // The refusal line is the evidence this slice hunts for; name the
        // message verbatim (the same discipline as spawn's read-failed
        // print).
        Err(msg) => {
            akuma_primitives::safe_print!(192, "[forkp] refused: {}\n", msg);
            for i in 0..NSLOTS {
                let (used, running, rpid) = SLOTS.with(|t| (t[i].used, t[i].running, t[i].pid));
                akuma_primitives::safe_print!(
                    128,
                    "  ring {}: pool={} oncpu={} held={} used={} running={} pid={}\n",
                    i,
                    akuma_threading::get_thread_state(i),
                    akuma_threading::on_cpu_flag(i),
                    akuma_threading::slot_held_by(i),
                    used,
                    running,
                    rpid
                );
            }
            crate::e(ring3::err::EAGAIN)
        }
    }
}

// ---------------------------------------------------------------------------
// fork, vfork and clone.
// ---------------------------------------------------------------------------

/// CLONE/FORK/VFORK (56/57/58). On x86_64 musl issues `fork()` as
/// `clone(SIGCHLD)` and `posix_spawn` as `clone(SIGCHLD|CLONE_VM|CLONE_VFORK)`,
/// but **vfork as the raw SYS_vfork in assembly, no flags word at all**
/// (`vfork.s`: `mov $58, %eax`), and busybox ash forks its non-final commands
/// through exactly that, so FORK/VFORK are accepted bare. A `CLONE_THREAD`
/// clone is a thread (`clone_thread_arm`); every other one is a fork through
/// `fork_process` (`fork_arm_real`).
pub fn fork_arm(fr: &ring3::SyscallFrame) -> u64 {
    use akuma_syscalls_proc::clone::{self, Kind};
    let kind = if fr.rax == akuma_syscalls_abi::x86_64::CLONE {
        match clone::decode(fr.arg1(), fr.arg2()) {
            Ok(k) => k,
            Err(clone::Invalid) => return crate::e(crate::err::EINVAL),
        }
    } else {
        Kind::Fork // fork (57) and vfork (58)
    };
    match kind {
        Kind::Thread => clone_thread_arm(fr),
        Kind::SpawnFork { child_stack } => fork_arm_real(child_stack),
        Kind::Fork => fork_arm_real(0),
    }
}

/// The user `rsp` a thread returns to ring 3 with when a call moved it on purpose (a signal
/// frame was pushed, or `rt_sigreturn` put the interrupted one back); 0 = none. Per thread
/// slot, because the dispatcher saves the global stash at entry and puts it back at exit (a
/// park inside the call lets other threads overwrite it), and that put-back would otherwise
/// undo the move: the handler ran on the interrupted stack and its `ret` never reached the
/// restorer.
static RETURN_RSP: [core::sync::atomic::AtomicU64; NSLOTS] = [const { core::sync::atomic::AtomicU64::new(0) }; NSLOTS];

fn set_return_rsp(rsp: u64) {
    let me = crate::exec_twin::current_slot();
    if me < NSLOTS {
        RETURN_RSP[me].store(rsp, core::sync::atomic::Ordering::Relaxed);
    }
    ring3::set_user_rsp_stash(rsp);
}

/// The dispatcher's exit: the `rsp` a signal frame or `rt_sigreturn` chose for this thread,
/// once, else `None` (the stash saved at entry is right).
pub fn take_return_rsp() -> Option<u64> {
    let me = crate::exec_twin::current_slot();
    if me >= NSLOTS {
        return None;
    }
    match RETURN_RSP[me].swap(0, core::sync::atomic::Ordering::Relaxed) {
        0 => None,
        v => Some(v),
    }
}

/// Signals no mask can block: `SIGKILL` (9) and `SIGSTOP` (19).
const UNBLOCKABLE: u64 = (1 << 8) | (1 << 18);

/// Deliver the running thread's pending signals at its return to ring 3 from syscall `nr`,
/// whose result is `r`: the dispatcher's exit. One handler at most (the next pending
/// signal waits for this one's `rt_sigreturn`, itself a syscall return); ignored signals are
/// dropped; a default action that kills ends the group ([`task_exit_code`], status
/// `-signal`). Returns what the call answers.
pub fn deliver_at_return(fr: &mut ring3::SyscallFrame, nr: u64, r: u64) -> u64 {
    // The interrupt flag exists to break a call that is *blocking*; on the way back to ring 3
    // nothing is. Left set, it makes the thread's next glue call answer EINTR without running:
    // a `kill`/`tkill` aimed at the caller itself sets it, and musl's `raise` and `abort` then
    // lost the mask restore after their `tkill`, so the signal stayed blocked.
    let me = crate::exec_twin::current_slot();
    if me < NSLOTS {
        let _ = akuma_threading::take_thread_interrupted(me);
    }
    let mask = akuma_exec::threading::thread_signal_mask();
    // `rt_sigsuspend` and `pause` (glue's) armed the restore-sigmask slot: a handler entered here
    // records the mask from before the call for its `rt_sigreturn`; with none entered, that mask
    // is the thread's again now.
    let restore = akuma_exec::threading::take_restore_sigmask();
    if !deliver_one(fr, nr, r, mask, restore.unwrap_or(mask))
        && let Some(m) = restore
    {
        akuma_exec::threading::set_thread_signal_mask(m);
    }
    r
}

/// Take pending signals the mask `cur` lets through until one runs a handler (`true`) or none
/// is left (`false`). `restore` is the mask the handler's `rt_sigreturn` puts back.
fn deliver_one(fr: &mut ring3::SyscallFrame, nr: u64, r: u64, cur: u64, restore: u64) -> bool {
    let me = crate::exec_twin::current_slot();
    if me >= NSLOTS {
        return false;
    }
    let Some(p) = akuma_exec::process::current_process_shared() else {
        return false;
    };
    while let Some(sig) = akuma_threading::take_pending_signal(cur & !UNBLOCKABLE) {
        let idx = (sig as usize).wrapping_sub(1);
        if idx >= akuma_exec::process::MAX_SIGNALS {
            continue;
        }
        let act = p.signal_actions.actions.lock()[idx];
        match act.handler {
            akuma_exec::process::SignalHandler::Ignore => {}
            akuma_exec::process::SignalHandler::UserFn(entry) => {
                // `SA_RESTART`: an interrupted call runs again after the handler - the saved
                // `rip` backs over the 2-byte `syscall` and `rax` is its number again - except
                // the calls Linux never restarts.
                let restart = r == crate::e(crate::EINTR) && act.flags & SA_RESTART != 0 && !NO_RESTART.contains(&nr);
                let (rax, rip) = if restart { (nr, fr.user_rip - 2) } else { (r, fr.user_rip) };
                if push_handler(fr, sig, entry as u64, &act, rax, rip, cur, restore) {
                    return true;
                }
                // A handler that cannot be entered (no restorer, an unwritable stack) must not
                // swallow a fatal signal.
                if akuma_syscalls_glue::signal::signal_is_fatal_default(sig) {
                    task_exit_code(-(sig as i32));
                }
            }
            akuma_exec::process::SignalHandler::Default => {
                if akuma_syscalls_glue::signal::signal_is_fatal_default(sig) {
                    akuma_primitives::safe_print!(96, "[signal] pid={} killed by signal {}\n", p.pid, sig);
                    task_exit_code(-(sig as i32));
                }
            }
        }
    }
    false
}

/// At a ring-3 tick: a pending signal whose action kills ends the group now, so a thread in a
/// loop that makes no syscall still dies of `SIGKILL` (or `SIGTERM` with no handler). Handlers
/// wait for the thread's next syscall return.
pub fn deliver_fatal_on_tick() {
    let me = crate::exec_twin::current_slot();
    if me >= NSLOTS {
        return;
    }
    let pend = akuma_threading::pending_signals_raw(me) & !(akuma_exec::threading::thread_signal_mask() & !UNBLOCKABLE);
    if pend == 0 {
        return;
    }
    let Some(p) = akuma_exec::process::current_process_shared() else {
        return;
    };
    for sig in 1..=64u32 {
        if pend & (1 << (sig - 1)) == 0 || !akuma_syscalls_glue::signal::signal_is_fatal_default(sig) {
            continue;
        }
        let idx = (sig - 1) as usize;
        let default = idx >= akuma_exec::process::MAX_SIGNALS
            || matches!(p.signal_actions.actions.lock()[idx].handler, akuma_exec::process::SignalHandler::Default);
        if sig == 9 || default {
            akuma_threading::clear_pending_signal(me, sig);
            task_exit_code(-(sig as i32));
        }
    }
}

const SA_RESTORER: u64 = 0x0400_0000;
const SA_NODEFER: u64 = 0x4000_0000;
const SA_SIGINFO: u64 = 4;
const SA_RESTART: u64 = 0x1000_0000;
/// Calls Linux never restarts after a handler, `SA_RESTART` or not: the sleeps and the waits
/// with a timeout (x86_64 numbers).
const NO_RESTART: [u64; 11] = [7, 23, 34, 35, 130, 230, 232, 270, 271, 281, 441];
/// The registers `rt_sigreturn` puts back, in `pack_frame` order.
const FRAME_WORDS: usize = 15;
/// What the signal frame holds above the restorer slot: the saved registers, the user `rsp`
/// at the interruption, the mask to restore, and where the FP image is.
const SIGFRAME_BYTES: usize = FRAME_WORDS * 8 + 24;
/// The `siginfo_t` and `ucontext_t` an `SA_SIGINFO` handler is handed (`rsi`, `rdx`): the
/// signal number and `SI_USER` in the first, zeros in the second (this kernel's frame is its
/// own, so a handler that edits `uc_mcontext` changes nothing).
const SIGINFO_BYTES: u64 = 128;
const UCONTEXT_BYTES: u64 = 944;

fn pack_frame(f: &ring3::SyscallFrame) -> [u64; FRAME_WORDS] {
    [
        f.r15, f.r14, f.r13, f.r12, f.rbp, f.rbx, f.r9, f.r8, f.r10, f.rdx, f.rsi, f.rdi, f.rax, f.user_rip,
        f.user_rflags,
    ]
}

fn unpack_frame(f: &mut ring3::SyscallFrame, w: &[u64; FRAME_WORDS]) {
    f.r15 = w[0];
    f.r14 = w[1];
    f.r13 = w[2];
    f.r12 = w[3];
    f.rbp = w[4];
    f.rbx = w[5];
    f.r9 = w[6];
    f.r8 = w[7];
    f.r10 = w[8];
    f.rdx = w[9];
    f.rsi = w[10];
    f.rdi = w[11];
    f.rax = w[12];
    f.user_rip = w[13];
    f.user_rflags = w[14];
}

/// Push a signal frame for `sig` and redirect the return to ring 3 into its handler `entry`.
/// `false` when it cannot be entered (no restorer, or the user stack is not writable), in
/// which case nothing was changed. From the handler's `rsp` up: `[restorer][saved registers,
/// user rsp, mask to restore, FP image address]`, then a zeroed `ucontext`, the `siginfo`, and
/// the 64-byte-aligned FP image. The restorer's `rt_sigreturn` finds the saved block at its
/// `rsp`; the layout is private to this function and [`sigreturn_arm`]. `rax`/`rip` are what
/// the block saves for the interrupted code (its result, or a restart).
#[allow(clippy::too_many_arguments)]
fn push_handler(
    fr: &mut ring3::SyscallFrame,
    sig: u32,
    entry: u64,
    act: &akuma_exec::process::SignalAction,
    rax: u64,
    rip: u64,
    cur_mask: u64,
    restore_mask: u64,
) -> bool {
    let (flags, restorer, sa_mask) = (act.flags, act.restorer as u64, act.mask);
    if flags & SA_RESTORER == 0 {
        return false;
    }
    let user_rsp = ring3::user_rsp_stash();
    // Below the red zone: the FP image (64-aligned, `xsave`'s rule), the siginfo and the
    // ucontext under it, then the block; the handler sees rsp = 8 (mod 16), as after a `call`.
    let fx_size = akuma_sched_amd64::FxArea::SIZE as u64;
    let Some(fp_at) = user_rsp.checked_sub(128 + fx_size).map(|v| v & !0x3F) else {
        return false;
    };
    let info_at = fp_at - SIGINFO_BYTES;
    let uc_at = info_at - UCONTEXT_BYTES;
    let Some(top) = uc_at.checked_sub(SIGFRAME_BYTES as u64 + 8) else {
        return false;
    };
    let at = (top & !0xF) - 8;
    if !crate::user_ok(at, user_rsp - at) {
        return false;
    }
    let mut buf = [0u8; 8 + SIGFRAME_BYTES];
    buf[..8].copy_from_slice(&restorer.to_le_bytes());
    let mut words = pack_frame(fr);
    words[12] = rax;
    words[13] = rip;
    for (i, w) in words.iter().enumerate() {
        buf[8 + i * 8..16 + i * 8].copy_from_slice(&w.to_le_bytes());
    }
    let tail = 8 + FRAME_WORDS * 8;
    buf[tail..tail + 8].copy_from_slice(&user_rsp.to_le_bytes());
    buf[tail + 8..tail + 16].copy_from_slice(&restore_mask.to_le_bytes());
    buf[tail + 16..tail + 24].copy_from_slice(&fp_at.to_le_bytes());
    // The interrupted FP/SSE file goes to the frame; the handler starts from the initial state.
    let mut fx = akuma_sched_amd64::FxArea::zeroed();
    akuma_sched_amd64::fpu_save_live(&mut fx);
    let mut info = [0u8; SIGINFO_BYTES as usize];
    info[..4].copy_from_slice(&(sig as i32).to_le_bytes()); // si_signo; si_errno, si_code (SI_USER) 0
    let zeros = [0u8; UCONTEXT_BYTES as usize];
    if !crate::copy_to_user(fp_at, fx.bytes())
        || !crate::copy_to_user(info_at, &info)
        || !crate::copy_to_user(uc_at, &zeros)
        || !crate::copy_to_user(at, &buf)
    {
        return false;
    }
    akuma_sched_amd64::fpu_reset_live();
    let bit = 1u64 << (sig - 1);
    let during = cur_mask | sa_mask | if flags & SA_NODEFER == 0 { bit } else { 0 };
    akuma_exec::threading::set_thread_signal_mask(during);
    fr.user_rip = entry;
    fr.rdi = u64::from(sig);
    (fr.rsi, fr.rdx) = if flags & SA_SIGINFO != 0 { (info_at, uc_at) } else { (0, 0) };
    set_return_rsp(at);
    true
}

/// `rt_sigreturn` - x86_64 15: the restorer's call after a handler returned. The user `rsp` is
/// just past the restorer slot, where [`push_handler`] put the saved block.
pub fn sigreturn_arm(fr: &mut ring3::SyscallFrame) -> u64 {
    let at = ring3::user_rsp_stash();
    let mut buf = [0u8; SIGFRAME_BYTES];
    if !crate::user_ok(at, buf.len() as u64) || !crate::copy_from_user(&mut buf, at) {
        return crate::e(ring3::err::EFAULT);
    }
    let mut words = [0u64; FRAME_WORDS];
    for (i, w) in words.iter_mut().enumerate() {
        *w = u64::from_le_bytes(buf[i * 8..i * 8 + 8].try_into().unwrap_or([0; 8]));
    }
    let tail = FRAME_WORDS * 8;
    let user_rsp = u64::from_le_bytes(buf[tail..tail + 8].try_into().unwrap_or([0; 8]));
    let mask = u64::from_le_bytes(buf[tail + 8..tail + 16].try_into().unwrap_or([0; 8]));
    let fp_at = u64::from_le_bytes(buf[tail + 16..tail + 24].try_into().unwrap_or([0; 8]));
    // Never trust a user-built frame with a kernel address or odd flags.
    if words[13] >= crate::USER_TOP || user_rsp >= crate::USER_TOP {
        return crate::e(ring3::err::EFAULT);
    }
    words[14] = (words[14] & 0x0000_0CD5) | 0x202;
    // The FP/SSE file the handler interrupted: a program chooses these bytes, and four kinds of
    // malformed image make `xrstor` a ring-0 #GP, so it is sanitized first. 0 = initial state.
    if fp_at == 0 {
        akuma_sched_amd64::fpu_reset_live();
    } else {
        let mut img = [0u8; akuma_sched_amd64::FxArea::SIZE];
        if fp_at % 64 != 0 || !crate::user_ok(fp_at, img.len() as u64) || !crate::copy_from_user(&mut img, fp_at) {
            return crate::e(ring3::err::EFAULT);
        }
        let Some(mut fx) = akuma_sched_amd64::FxArea::from_bytes(&img) else {
            return crate::e(ring3::err::EFAULT);
        };
        fx.sanitize();
        akuma_sched_amd64::fpu_restore_live(&fx);
    }
    unpack_frame(fr, &words);
    set_return_rsp(user_rsp);
    akuma_exec::threading::set_thread_signal_mask(mask);
    // The dispatcher stores this in RAX: the interrupted call's own result.
    words[12]
}

/// Page-table roots the twin built by hand for an exec'd image
/// (`exec_real_task`'s loader maps raw frames), per slot. The registered
/// `Process` only holds a `new_shared` view of such a root, so
/// `UserAddressSpace::drop` frees none of it: the twin owns the root and must
/// free it itself, at reap.
static OWNED_ROOT: akuma_primitives::cell::BorrowCell<[u64; NSLOTS]> = akuma_primitives::cell::BorrowCell::new([0; NSLOTS]);
/// Roots an earlier image of a still-running process left behind (a second
/// `execve` in the same process). They cannot be freed at the exec — the
/// arm is still running on the old CR3 — so they wait here for the next reap.
const PENDING_ROOTS: usize = 16;
static PENDING_ROOT: akuma_primitives::cell::BorrowCell<[u64; PENDING_ROOTS]> = akuma_primitives::cell::BorrowCell::new([0; PENDING_ROOTS]);

/// Point `pid`'s registered `Process` at a shared view of `root` and let go of
/// the address space it owned WITHOUT dropping it. The fork clone
/// (`clone_user_half`) fills the crate's fresh space with raw frames the
/// space's ledger never saw, so `UserAddressSpace::drop` returns only the L0
/// and the process-info page - every leaf and table frame leaked (a forked
/// 100k-page `rustc` left 104k pages per fork, the second `cargo` of a boot
/// died of it). The twin frees the whole root itself ([`free_user_root`]),
/// so the old owner must not free the L0 under it: it is forgotten, which
/// leaks its ledger's small bookkeeping instead. `false`: no such process.
fn disown_space(pid: u32, root: u64) -> bool {
    let Some(view) = akuma_mmu::UserAddressSpace::new_shared(root as usize) else {
        return false;
    };
    akuma_exec::process::table::with_process(pid, |p| {
        let old = core::mem::replace(
            &mut p.address_space,
            akuma_exec::process::ProcAddressSpace::new(view),
        );
        if !old.is_shared() {
            core::mem::forget(old);
        }
    })
    .is_some()
}

/// `root` is a dead image's page-table root: free it at the next sweep.
fn park_root(root: u64) {
    if root == 0 {
        return;
    }
    PENDING_ROOT.with_mut(|pend| {
        // Full: a leaked image beats freeing a live root.
        if let Some(r) = pend.iter_mut().find(|r| **r == 0) {
            *r = root;
        }
    });
}

/// `slot` now runs the image rooted at `root`: remember it for the reap, and
/// park the root it replaced (if it was an exec'd one) for the next sweep.
fn adopt_exec_root(slot: usize, root: u64) {
    let old = OWNED_ROOT.with_mut(|t| core::mem::replace(&mut t[slot], root));
    if old != root {
        park_root(old);
    }
}

/// Free every parked root that is not the live CR3 (`free_user_root` refuses
/// that one, and it stays parked), returning the pages released.
fn sweep_parked_roots() -> usize {
    let mut pages = 0usize;
    for i in 0..PENDING_ROOTS {
        // Read the entry out first: `free_user_root` runs with no borrow open.
        let r = PENDING_ROOT.with(|p| p[i]);
        if r != 0 {
            let n = crate::free_user_root(r);
            if n != 0 {
                PENDING_ROOT.with_mut(|p| p[i] = 0);
                pages += n;
            }
        }
    }
    pages
}

/// Page-table roots of exited processes, by pid, until their reap.
///
/// A fork child's address space is the twin's fork clone, every frame of which
/// the `Process`'s `UserAddressSpace` ledger never saw (see [`disown_space`]), so
/// the twin frees it and the `Process` must not. At the exit the `Process` is
/// handed a non-owning view of the same root and the root is recorded here;
/// after glue's `wait4` has unregistered the pid, [`collect`] frees it.
const ZOMBIE_ROOTS: usize = 64;
static ZOMBIE_ROOT: akuma_primitives::cell::BorrowCell<[(u32, u64); ZOMBIE_ROOTS]> =
    akuma_primitives::cell::BorrowCell::new([(0, 0); ZOMBIE_ROOTS]);

/// Record process `pid`'s root (slot `slot` is its leader) for [`collect`].
/// The root the leader last ran on: its exec'd image, or, never exec'd, the
/// fork clone in its task record. Idempotent: the exit path and the purge hook
/// both call it.
fn zombie_root(slot: usize, pid: u32) {
    let own = OWNED_ROOT.with_mut(|t| core::mem::replace(&mut t[slot], 0));
    let root = if own != 0 { own } else { crate::task_root_of(task_of(slot)) };
    if root == 0 || ZOMBIE_ROOT.with(|z| z.iter().any(|&(p, _)| p == pid)) {
        return;
    }
    let _ = disown_space(pid, root);
    let stored = ZOMBIE_ROOT.with_mut(|z| {
        z.iter_mut().find(|e| e.0 == 0).map(|e| *e = (pid, root)).is_some()
    });
    if !stored {
        // Full: a leaked image beats freeing a root a zombie's view still names.
        akuma_primitives::safe_print!(96, "[reap] zombie-root table full: pid {} leaks its image\n", pid);
    }
}

/// Free what reaped processes left: the roots of pids no longer in the process
/// table (glue's `wait4` unregistered them), retired `Process` rows, and parked
/// roots. Run after every `wait4`/`waitid` and at fork/clone entry; cheap when
/// there is nothing to do.
pub fn collect() {
    let before = akuma_exec::pmm::free_count();
    let mut reaped = 0;
    for i in 0..ZOMBIE_ROOTS {
        let (pid, root) = ZOMBIE_ROOT.with(|z| z[i]);
        if pid != 0 && akuma_exec::process::lookup_process_shared(pid).is_none() {
            ZOMBIE_ROOT.with_mut(|z| z[i] = (0, 0));
            park_root(root);
            reaped += 1;
        }
    }
    let drained = akuma_exec::process::reclaim::drain_retired();
    let mut pages = sweep_parked_roots();
    pages += akuma_mmu::drain_pending_ttbr_frees();
    if cfg!(feature = "trace-err") && reaped != 0 {
        akuma_primitives::safe_print!(
            224,
            "[reap] reaped={} drained={} root={} free {} -> {} heap={}K cowrefs={} cow={:?} procs={}/{}\n",
            reaped,
            drained,
            pages,
            before,
            akuma_exec::pmm::free_count(),
            akuma_alloc::allocated_bytes() / 1024,
            akuma_pmm::cow_ref_count(),
            crate::cow_stats(),
            akuma_exec::process::table::process_count(),
            akuma_exec::process::table::retired_process_count()
        );
    }
}
