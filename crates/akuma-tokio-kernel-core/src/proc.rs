//! Processes on a tokio kernel.
//!
//! The slot ring every forked task and thread occupies, `clone`/`fork` through `akuma-exec`'s `fork_process` and
//! `clone_thread`, `execve`'s load, exit (`wait4` is glue's, over the child
//! channels), init's bookkeeping, and who owns which page-table root until the
//! reap.
//!
//! Moved from the amd64 half's `child.rs` (B21 A5.6c), generic over a
//! [`Proc`] machine. What stays in each half is what names its registers: the
//! syscall frame a `clone` decodes, the user-context capture `fork_process`
//! reads, the crossing into the new image, init's first instruction, and the
//! signal frame.
//!
//! The slot index is the task's threading tid, by construction: `bind_child_task`
//! stamps by tid, and the switch hooks index the same arrays.

use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering::{AcqRel, Acquire, Relaxed, Release}};

use akuma_primitives::cell::BorrowCell;
use akuma_primitives::safe_print;

use crate::loader::{self, Loader, TaskImage};
use crate::task::Task;
use crate::{mm, tree};

/// Ring slots: threading tids `0..NSLOTS` (0 is the boot thread).
pub const NSLOTS: usize = 48;

const EPERM: u64 = 1;
const ENOENT: u64 = 2;
const ENOEXEC: u64 = 8;
const EAGAIN: u64 = 11;
const ENOMEM: u64 = 12;
const EFAULT: u64 = 14;

/// A negative errno as a syscall return.
const fn e(code: u64) -> u64 {
    0u64.wrapping_sub(code)
}

/// What the process layer asks of the machine beyond its memory and loader.
pub trait Proc: Loader {
    /// The slot this core runs (`>= NSLOTS`: none of the ring's).
    fn current_slot() -> usize;
    /// Give ring slot `slot` its kernel stacks; `false` when they cannot be had.
    fn prepare_slot(slot: usize) -> bool;
    /// Run `f` with the kernel lock released.
    fn unlocked(f: impl FnOnce());
    /// Microseconds since boot.
    fn uptime_us() -> u64;
    /// The task record the syscall path acts on now.
    fn task_slot_index() -> usize;
    /// Run `f` on task record `i`.
    fn with_task_slot<R>(i: usize, f: impl FnOnce(&mut Task) -> R) -> R;
    /// The page-table root of task record `i`.
    fn record_root(i: usize) -> u64;
}

// ---------------------------------------------------------------------------
// The slot ring.
// ---------------------------------------------------------------------------

/// One ring slot: a forked task or a `CLONE_THREAD` sibling, from
/// `bind_child_task` until it is reaped.
#[derive(Clone, Copy)]
struct SpawnSlot {
    used: bool,
    pid: u32,
    /// The task has not exited yet (set at bind, cleared at exit).
    running: bool,
}

impl SpawnSlot {
    const fn new() -> Self {
        Self { used: false, pid: 0, running: false }
    }
}

/// The ring's rows, reached only through [`with_slots`] and the one-field
/// readers: two overlapping borrows are a panic naming the bug.
static SLOTS: BorrowCell<[SpawnSlot; NSLOTS]> = BorrowCell::new([const { SpawnSlot::new() }; NSLOTS]);

fn with_slots<R>(f: impl FnOnce(&mut [SpawnSlot; NSLOTS]) -> R) -> R {
    SLOTS.with_mut(f)
}

fn slot_pid(slot: usize) -> u32 {
    SLOTS.with(|s| s[slot].pid)
}

/// The pid ring slot `idx` runs, `None` for an unused slot
/// (`ProcessHooks::pid_for_thread`).
#[must_use]
pub fn pid_for_slot(idx: usize) -> Option<u32> {
    if idx < NSLOTS { SLOTS.with(|s| s[idx].used.then_some(s[idx].pid)) } else { None }
}

/// The task record each ring slot runs against: `slot + 1` for a process, the
/// creator's record for a `CLONE_THREAD` sibling, which is how threads share
/// regions, brk/mmap cursors, the fd table and the cwd with no second copy.
static TASK_OF: BorrowCell<[usize; NSLOTS]> = BorrowCell::new({
    let mut a = [0usize; NSLOTS];
    let mut i = 0;
    while i < NSLOTS {
        a[i] = i + 1;
        i += 1;
    }
    a
});

/// The task record ring slot `slot` acts on.
#[must_use]
pub fn task_of(slot: usize) -> usize {
    TASK_OF.with(|t| t[slot])
}

/// Is ring slot `slot` an occupied task or thread (the target of a `tkill`)?
#[must_use]
pub fn slot_in_use(slot: usize) -> bool {
    slot < NSLOTS && SLOTS.with(|s| s[slot].used)
}

/// Is the running task a `CLONE_THREAD` sibling?
#[must_use]
pub fn is_thread_task() -> bool {
    akuma_exec::process::current_process_shared().is_some_and(|p| p.pid != p.tgid)
}

/// The thread group of the task in slot `slot`.
fn slot_tgid(slot: usize) -> u32 {
    let pid = slot_pid(slot);
    akuma_exec::process::table::with_process(pid, |p| p.tgid).unwrap_or(pid)
}

/// `ExecRuntime::bind_child_task`: stamp the child's ring row.
///
/// Give it its kernel stacks, and give a fork child a task record of its own (`tid + 1`):
/// the root from `fork_process`'s cloned address space, the layout from the
/// creator's (shared mappings included: fork shares those spans' frames). A
/// thread shares its creator's record. A full ring is an error the fork
/// reports, not a panic: the crate publishes the child right after this.
///
/// # Errors
///
/// `tid` outside the ring.
pub fn bind_child_task<M: Proc>(tid: usize, pid: u32, root_pa: u64, is_thread: bool) -> Result<(), &'static str> {
    if tid >= NSLOTS {
        return Err("tokio kernel: child tid outside the spawn ring");
    }
    with_slots(|s| s[tid] = SpawnSlot { used: true, pid, running: true });
    let _ = M::prepare_slot(tid);
    if is_thread {
        let creator = M::task_slot_index();
        TASK_OF.with_mut(|t| t[tid] = creator);
    } else {
        TASK_OF.with_mut(|t| t[tid] = tid + 1);
        let layout = M::with_task(|t| t.layout.fork());
        M::with_task_slot(tid + 1, |ct| {
            ct.root_pa = root_pa;
            ct.layout = layout;
            ct.exit = None;
        });
    }
    Ok(())
}

/// A forked task is on the CPU: a ring slot past the boot thread's, stamped,
/// running and bound.
#[must_use]
pub fn is_real_task<M: Proc>() -> bool {
    let cur = M::current_slot();
    cur != 0 && cur < NSLOTS && SLOTS.with(|s| s[cur].used && s[cur].running)
}

/// The root the switch loads for slot `tid`: its task record's (exec updates
/// it). `0` for a slot outside the ring (the idle threads run on the kernel's).
#[must_use]
pub fn sched_root_of<M: Proc>(tid: usize) -> u64 {
    if tid < NSLOTS { M::record_root(task_of(tid)) } else { 0 }
}

/// The task record the syscall path acts on after a switch to `tid`.
#[must_use]
pub fn sched_task_slot_of(tid: usize) -> usize {
    if tid < NSLOTS { task_of(tid) } else { tid + 1 }
}

// ---------------------------------------------------------------------------
// Init.
// ---------------------------------------------------------------------------

/// The pool slot init runs on.
static INIT_TID: AtomicUsize = AtomicUsize::new(usize::MAX);
/// Init's exit as the boot report shows it (`128 + signal` for a death by
/// signal), or [`INIT_RUNNING`]. Init has no parent to `wait4` for it.
static INIT_EXIT: AtomicU64 = AtomicU64::new(INIT_RUNNING);
const INIT_RUNNING: u64 = u64::MAX;

/// Claim a slot for init (pid 1), bind the image `load_task` put in the
/// running record to it, and register its `Process`. Returns the slot; the
/// machine seeds it to cross into the image and publishes it.
#[must_use]
pub fn bind_init<M: Proc>(path: &str, ti: &TaskImage) -> Option<usize> {
    let tid = akuma_threading::x86_claim_slot()?;
    if let Err(msg) = bind_child_task::<M>(tid, 1, ti.root_pa, false) {
        safe_print!(160, "  [init] bind refused: {}\n", msg);
        return None;
    }
    // Parent 1: init is the boot's root of the tree (the pid floor is 2).
    if !akuma_exec::process::Process::register_adopted(1, 1, path, ti.root_pa as usize, ti.entry as usize, ti.sp as usize, tid) {
        safe_print!(128, "  [init] root view for the process table failed\n");
        return None;
    }
    INIT_TID.store(tid, Relaxed);
    Some(tid)
}

/// The boot thread, parked until init ends: a timed sleep (an unbounded park
/// starves init's startup) that init's exit also wakes. Returns init's code.
#[must_use]
pub fn wait_init<M: Proc>() -> u64 {
    loop {
        let code = INIT_EXIT.load(Acquire);
        if code != INIT_RUNNING {
            return code;
        }
        akuma_threading::schedule_blocking(M::uptime_us() + 10_000);
    }
}

// ---------------------------------------------------------------------------
// Exit.
// ---------------------------------------------------------------------------

/// `exit`/`exit_group` from a ring task: [`akuma_syscalls_proc::exit::scope`]
/// decides whether the thread or the group ends.
pub fn exit<M: Proc>(group: bool, code: u64) -> ! {
    let scope = akuma_syscalls_proc::exit::scope(group, is_thread_task());
    if scope == akuma_syscalls_proc::exit::Scope::Thread {
        task_exit_thread::<M>()
    }
    task_exit_code::<M>((code & 0xFF) as i32)
}

/// A task killed by a fault: the group dies with wait status SIGSEGV.
pub fn die_segv<M: Proc>() -> ! {
    task_exit_code::<M>(-11)
}

/// `akuma-threading`'s slot-purge hook: thread `tid` was just marked TERMINATED.
///
/// By this kernel's exit path or by `akuma-exec` (a fatal signal's
/// default action inside glue). Purge its futex waits; for the last thread of
/// a process, clear the row's `running` and hand its root to [`zombie_root`].
pub fn on_thread_terminated<M: Proc>(tid: usize) {
    akuma_syscalls_glue::futex_purge_tid(tid);
    if tid >= NSLOTS {
        return;
    }
    let pid = slot_pid(tid);
    let Some(p) = akuma_exec::process::lookup_process_shared(pid) else {
        return;
    };
    if p.pid != p.tgid || !p.exited.load(Relaxed) {
        return; // a thread (its space is the leader's), or a process still running
    }
    let was_running = with_slots(|s| core::mem::replace(&mut s[tid].running, false));
    if was_running || OWNED_ROOT.with(|t| t[tid]) != 0 {
        zombie_root::<M>(tid, pid);
    }
}

/// End the caller's whole process with `code`: an exit code (0..=255), or
/// `-signal` for a death by signal, the form glue's `encode_wait_status` reads.
///
/// Published the way `akuma-exec` publishes every exit: the `Process` becomes a
/// zombie, `publish_child_exit` marks the child channel and raises SIGCHLD on
/// the parent, glue's `wait4` reaps it.
pub fn task_exit_code<M: Proc>(code: i32) -> ! {
    // A shared file mapping persists at exit (rust-lld never munmaps its output).
    mm::shared_flush_all::<M>();
    let cur = M::current_slot();
    let lead = group_leader(cur);
    // The zombie keeps its thread slot until the parent's wait4: without the
    // hold `x86_claim_slot` hands the TERMINATED slot to the next fork while
    // the pid is still waitable. Glue's reap releases it. Taken before the
    // siblings stop: their waits release the kernel lock, and a leader stopped
    // by then is a TERMINATED slot a fork on another core could claim.
    let lead_pid = with_slots(|s| s[lead].pid);
    akuma_exec::process::hold_exited_thread_slot(lead_pid, lead);
    kill_siblings::<M>(cur, lead);
    with_slots(|s| s[lead].running = false);
    if let Some(p) = akuma_exec::process::lookup_process_shared(lead_pid) {
        p.exit_code.store(code, Relaxed);
        p.state.store(akuma_exec::process::ProcessState::Zombie(code));
        p.exited.store(true, Release);
    }
    // The descriptors close now, not at the reap: a pipe's EOF is its last
    // writer's close. Every thread sharing the table is stopped by now.
    if let Some(p) = akuma_exec::process::current_process_shared() {
        p.fds.close_all();
    }
    zombie_root::<M>(lead, lead_pid);
    if lead == INIT_TID.load(Relaxed) {
        let shown = if code < 0 { 128 + u64::from(code.unsigned_abs()) } else { code as u64 };
        INIT_EXIT.store(shown, Release);
        akuma_threading::get_waker_for_thread(0).wake();
    }
    akuma_exec::process::publish_child_exit(lead_pid, code);
    // The group's children go to init, which reaps them, as glue's exit does:
    // an orphan under a dead parent nobody can wait for leaks its slot.
    let _ = akuma_exec::process::reparent_group_children_to(lead_pid, 1);
    if lead != cur {
        akuma_syscalls_glue::futex_purge_tid(lead);
        akuma_threading::mark_thread_terminated(lead);
    }
    akuma_threading::x86_finish_current()
}

/// The slot that owns the process `cur` belongs to: the one whose pid is the
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

/// `exit_group`: stop every thread of `cur`'s group except `cur`, the leader
/// `lead` included.
///
/// A thread is ended in place only while this core holds its `ON_CPU` gate
/// (`x86_hold_off_cpu`): then it is not executing and no core can pick it. The
/// state word cannot answer "is it running": a READY thread another core has
/// just claimed is being switched in, and marking it TERMINATED did not stop it.
/// It ran on in user mode under a root the reap then freed, and its stores
/// landed in frames handed to other processes (killtree at smp=4,
/// `docs/archive/TAOXICHUAN_KILL_SIBLINGS_ON_CPU_RACE.md`). A thread on a core
/// gets a kill request and is waited for (lock released) until it ends itself
/// at its next kernel entry or exit ([`finish_killed`]) or comes off its core.
///
/// The leader is stopped but keeps its slot and its `Process`: the exiting
/// thread publishes the group's status there.
pub fn kill_siblings<M: Proc>(cur: usize, lead: usize) {
    let tgid = slot_tgid(cur);
    let mut asked = [0u32; NSLOTS]; // the pid asked to end, per slot; 0 = none
    for (s, ask) in asked.iter_mut().enumerate() {
        let spid = slot_pid(s);
        if s == cur || !slot_in_use(s) || slot_tgid(s) != tgid || (spid == tgid && s != lead) {
            continue;
        }
        if akuma_threading::is_thread_terminated(s) {
            continue;
        }
        if !stop_in_place(s, spid, s == lead) {
            akuma_threading::request_thread_kill(s);
            *ask = spid;
        }
    }
    let start = M::uptime_us();
    let mut warned = false;
    while asked.iter().any(|&p| p != 0) {
        for (s, p) in asked.iter_mut().enumerate() {
            if *p == 0 {
                continue;
            }
            if akuma_threading::is_thread_terminated(s) || slot_pid(s) != *p || stop_in_place(s, *p, s == lead) {
                *p = 0;
            }
        }
        // User mode reaches the kernel at least once a tick. A thread that does
        // not is reported, never ended where it runs.
        if !warned && M::uptime_us().saturating_sub(start) > 2_000_000 {
            warned = true;
            for (s, _) in asked.iter().enumerate().filter(|(_, p)| **p != 0) {
                safe_print!(96, "[exit_group] slot {} still on a core after 2 s; waiting\n", s);
            }
        }
        M::unlocked(|| {
            for _ in 0..2000 {
                core::hint::spin_loop();
            }
        });
    }
}

/// End thread `s` (pid `spid`) where it stands if no core is executing it:
/// `false` when it is on a core. The leader keeps its slot and `Process`.
fn stop_in_place(s: usize, spid: u32, leader: bool) -> bool {
    if !akuma_threading::x86_hold_off_cpu(s) {
        return false;
    }
    let _ = akuma_threading::take_kill_request_via_tid(s);
    akuma_syscalls_glue::futex_purge_tid(s);
    if !leader {
        with_slots(|t| {
            t[s].running = false;
            t[s].used = false;
        });
    }
    akuma_threading::mark_thread_terminated(s);
    // TERMINATED now: `x86_pick_next` skips it with the gate open.
    akuma_threading::x86_release_on_cpu(s);
    if !leader {
        retire_thread_process(spid);
    }
    true
}

/// Did `exit_group` ask the running thread to end?
#[must_use]
pub fn kill_requested<M: Proc>() -> bool {
    let cur = M::current_slot();
    cur < NSLOTS && akuma_threading::has_pending_kill(cur)
}

/// End the running thread at a kill request: no exit status (its group's
/// exiting thread publishes that), no clear-child-tid wake.
pub fn finish_killed<M: Proc>() -> ! {
    let cur = M::current_slot();
    let _ = akuma_threading::take_kill_request_via_tid(cur);
    akuma_syscalls_glue::futex_purge_tid(cur);
    // The leader, stopped by a sibling's `exit_group`: its slot and `Process`
    // stay for the status that sibling publishes.
    if slot_pid(cur) == slot_tgid(cur) {
        akuma_threading::x86_finish_current()
    }
    let pid = with_slots(|s| {
        s[cur].running = false;
        s[cur].used = false;
        s[cur].pid
    });
    retire_thread_process(pid);
    akuma_threading::x86_finish_current()
}

/// Take a dead thread's registered `Process` out of the process table. Retire
/// only: the next `drain_retired` frees it, never here, because the exiting
/// thread still runs on it (`docs/archive/TAOXICHUAN_BUGS_FOUND_2026_10_30.md` §7).
fn retire_thread_process(pid: u32) {
    let _ = akuma_exec::process::unregister_process(pid);
}

/// A thread's `exit`: only this thread ends. `CLONE_CHILD_CLEARTID` (what
/// `pthread_join` waits on): the tid word is zeroed and its futex woken first.
pub fn task_exit_thread<M: Proc>() -> ! {
    let cur = M::current_slot();
    if let Some(p) = akuma_exec::process::current_process_shared() {
        let addr = p.clear_child_tid.swap(0, AcqRel);
        if addr != 0 {
            let _ = M::copy_to_user(addr, &0u32.to_ne_bytes());
            akuma_syscalls_glue::futex_wake(p.tgid, addr as usize, 1);
        }
    }
    akuma_syscalls_glue::futex_purge_tid(cur);
    let pid = with_slots(|s| {
        s[cur].running = false;
        s[cur].used = false;
        s[cur].pid
    });
    retire_thread_process(pid);
    akuma_threading::x86_finish_current()
}

// ---------------------------------------------------------------------------
// clone, fork, execve.
// ---------------------------------------------------------------------------

/// `clone(flags, ...)`: a thread or a fork.
///
/// The asm-generic argument order (`flags`,
/// `newsp`, `parent_tid`, `tls`, `child_tid` on AArch64; the caller reorders
/// x86_64's): a thread through [`thread`], anything else a fork through
/// [`fork`]. `EINVAL` for a flag set `akuma_syscalls_proc::clone` refuses.
#[must_use]
pub fn clone<M: Proc>(flags: u64, stack: u64, ptid: u64, tls: u64, ctid: u64) -> u64 {
    use akuma_syscalls_proc::clone::{self as cl, Kind};
    match cl::decode(flags, stack) {
        Err(cl::Invalid) => e(22),
        Ok(Kind::Thread) => thread::<M>(flags, stack, ptid, ctid, tls),
        Ok(Kind::SpawnFork { child_stack }) => fork::<M>(child_stack),
        Ok(Kind::Fork) => fork::<M>(0),
    }
}

/// `clone(CLONE_THREAD|CLONE_VM|...)`: another thread of the caller's group,
/// sharing the address space; the bind hook gives it the creator's task record.
#[must_use]
pub fn thread<M: Proc>(flags: u64, stack: u64, ptid: u64, ctid: u64, tls: u64) -> u64 {
    if M::current_slot() >= NSLOTS {
        return e(EPERM);
    }
    // The drain is paid at entry, by the one path that consumes `Process` rows.
    collect::<M>();
    match akuma_exec::process::clone_thread(stack, tls, ptid, ctid, flags) {
        Ok(tid) => u64::from(tid),
        Err(msg) => {
            safe_print!(160, "[clone] thread refused: {}\n", msg);
            e(EAGAIN)
        }
    }
}

/// Fork the caller through `fork_process`.
///
/// `child_stack` 0: the child resumes
/// on the parent's stack; otherwise on that stack (a `posix_spawn` child). The
/// child does not run here: the crate publishes it and this returns its pid.
#[must_use]
pub fn fork<M: Proc>(child_stack: u64) -> u64 {
    if M::current_slot() >= NSLOTS {
        return e(EPERM);
    }
    collect::<M>();
    let child_pid = akuma_exec::process::allocate_pid();
    match akuma_exec::process::fork_process(child_pid, child_stack) {
        Ok(pid) => u64::from(pid),
        Err(msg) => {
            safe_print!(192, "[forkp] refused: {}\n", msg);
            for i in 0..NSLOTS {
                let SpawnSlot { used, running, pid } = SLOTS.with(|t| t[i]);
                safe_print!(
                    128,
                    "  ring {}: pool={} oncpu={} held={} used={} running={} pid={}\n",
                    i,
                    akuma_threading::get_thread_state(i),
                    akuma_threading::on_cpu_flag(i),
                    akuma_threading::slot_held_by(i),
                    used,
                    running,
                    pid
                );
            }
            e(EAGAIN)
        }
    }
}

/// Entry caps for `execve`: about Linux's own (`MAX_ARG_STRLEN` per string, a
/// megabyte in all). The loader sizes the block from what it holds
/// (`docs/archive/TAOXICHUAN_EXEC_ARGV_LIMIT.md`).
const ARGV_MAX: usize = 32 * 1024;
const ENV_MAX: usize = 8 * 1024;
const MAX_STRS: usize = ARGV_MAX + ENV_MAX;
const STR_MAX: usize = 128 * 1024;
const ARG_BYTES_MAX: usize = 1024 * 1024;
/// `#!` hops followed before ENOEXEC (Linux's `BINPRM_MAX_RECURSION`).
const SHEBANG_MAX_DEPTH: usize = 4;

/// `execve(path, argv, envp)` up to the crossing.
///
/// The request read, `#!`
/// followed, the new image loaded into the running record, the process side
/// committed (close-on-exec, `/proc/self/exe`, handlers reset) and the roots
/// handed over. Returns where to cross; the machine resets its per-thread
/// state (a TLS base) and enters. `Err` is the syscall's answer, the old
/// image intact.
///
/// # Errors
///
/// The negative errno `execve` returns.
pub fn exec<M: Proc>(path: u64, argv: u64, envp: u64) -> Result<TaskImage, u64> {
    let cur = M::current_slot();
    let mut budget = akuma_syscalls_proc::exec::Budget::new(MAX_STRS, STR_MAX, ARG_BYTES_MAX);
    let req = akuma_syscalls_glue::execve::read_request(path, argv, envp, ARGV_MAX, ENV_MAX, &mut budget)?;
    // The loader takes `&str`: an argument that is not UTF-8 cannot reach it.
    let utf8 = |v: Vec<Vec<u8>>| v.into_iter().map(String::from_utf8).collect::<Result<Vec<String>, _>>();
    let (Ok(mut argv_s), Ok(env_s)) = (utf8(req.argv), utf8(req.envp)) else {
        return Err(e(EFAULT));
    };
    let mut effective = loader::resolve_effective::<M>(&req.path);
    let read = |p: &str| {
        loader::read_image::<M>(p).map_err(|err| {
            safe_print!(192, "[exec] '{}': read failed ({:?})\n", p, err);
            e(ENOENT)
        })
    };
    let mut image = read(&effective)?;
    // `#!` scripts: the interpreter is loaded instead, with argv
    // `[interpreter, arg?, path as written, argv[1..]]` - one parser and one
    // argv rule with the AArch64 kernel (`akuma_exec::process::shebang_hop`).
    let mut prefix: Vec<String> = Vec::new();
    let mut script_name = req.path;
    for _ in 0..SHEBANG_MAX_DEPTH {
        let head = &image[..image.len().min(akuma_exec::process::SHEBANG_MAX)];
        let Some((interp, arg)) = akuma_exec::process::parse_shebang(head) else {
            break;
        };
        prefix = akuma_exec::process::shebang_hop(interp, arg, &script_name, &prefix);
        script_name = String::from(interp);
        effective = loader::resolve_effective::<M>(&script_name);
        image = read(&effective)?;
    }
    if !prefix.is_empty() {
        prefix.extend(core::mem::take(&mut argv_s).into_iter().skip(1));
        argv_s = prefix;
    }
    if image.len() < 4 || image[..4] != [0x7f, b'E', b'L', b'F'] {
        return Err(e(ENOEXEC));
    }
    let argv: Vec<&str> = argv_s.iter().map(String::as_str).collect();
    let env: Vec<&str> = env_s.iter().map(String::as_str).collect();
    // The image about to be replaced may hold shared file mappings.
    mm::shared_flush_all::<M>();
    let old_root = M::task_root();
    let Some(ti) = loader::load_task::<M>(&argv, &env, &image, &effective) else {
        return Err(e(ENOMEM));
    };
    if let Some(p) = akuma_exec::process::current_process_shared() {
        let argv_bytes: Vec<Vec<u8>> = argv_s.iter().map(|a| a.clone().into_bytes()).collect();
        akuma_syscalls_glue::execve::commit(p, &effective, &argv_bytes);
    }
    // The registered Process's shared root view goes stale with the old image.
    let pid = slot_pid(cur);
    let first_exec = OWNED_ROOT.with(|t| t[cur]) == 0;
    if disown_space(pid, ti.root_pa) {
        // A fork child's first exec: the root it ran on is the fork clone, every
        // frame of which is raw (see `disown_space`). Later execs park the
        // previous exec'd root in `adopt_exec_root`.
        if first_exec {
            park_root(old_root);
        }
        adopt_exec_root(cur, ti.root_pa);
    }
    Ok(ti)
}

// ---------------------------------------------------------------------------
// Who owns which root.
// ---------------------------------------------------------------------------

/// Page-table roots the kernel built for an exec'd image, per slot. The
/// registered `Process` holds only a `new_shared` view, so its drop frees none
/// of it: the kernel frees it at the reap.
static OWNED_ROOT: BorrowCell<[u64; NSLOTS]> = BorrowCell::new([0; NSLOTS]);
/// Roots an earlier image of a still-running process left (a second `execve`):
/// not freeable at the exec (the arm runs on the old root), so they wait for
/// the next sweep.
const PENDING_ROOTS: usize = 16;
static PENDING_ROOT: BorrowCell<[u64; PENDING_ROOTS]> = BorrowCell::new([0; PENDING_ROOTS]);

/// Point `pid`'s `Process` at a shared view of `root` and let go of the space
/// it owned WITHOUT dropping it: the fork clone fills the crate's fresh space
/// with frames its ledger never saw, so its drop would free only the root under
/// the kernel. Forgotten instead (its ledger's small bookkeeping leaks).
/// `false`: no such process.
fn disown_space(pid: u32, root: u64) -> bool {
    let Some(view) = akuma_mmu::UserAddressSpace::new_shared(root as usize) else {
        return false;
    };
    akuma_exec::process::table::with_process(pid, |p| {
        let old = core::mem::replace(&mut p.address_space, akuma_exec::process::ProcAddressSpace::new(view));
        if !old.is_shared() {
            core::mem::forget(old);
        }
    })
    .is_some()
}

/// `root` is a dead image's root: free it at the next sweep.
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

/// `slot` now runs the image rooted at `root`: remember it for the reap, park
/// the exec'd root it replaced.
fn adopt_exec_root(slot: usize, root: u64) {
    let old = OWNED_ROOT.with_mut(|t| core::mem::replace(&mut t[slot], root));
    if old != root {
        park_root(old);
    }
}

/// Free every parked root that is not live (`free_user_root` refuses that one,
/// and it stays parked). Returns the pages released.
fn sweep_parked_roots<M: Proc>() -> usize {
    let mut pages = 0usize;
    for i in 0..PENDING_ROOTS {
        let r = PENDING_ROOT.with(|p| p[i]);
        if r != 0 {
            let n = tree::free_user_root::<M>(r);
            if n != 0 {
                PENDING_ROOT.with_mut(|p| p[i] = 0);
                pages += n;
            }
        }
    }
    pages
}

/// Page-table roots of exited processes, by pid, until their reap: the fork
/// clone's frames are the kernel's to free (see [`disown_space`]); after glue's
/// `wait4` unregistered the pid, [`collect`] frees it.
const ZOMBIE_ROOTS: usize = 64;
static ZOMBIE_ROOT: BorrowCell<[(u32, u64); ZOMBIE_ROOTS]> = BorrowCell::new([(0, 0); ZOMBIE_ROOTS]);

/// Record process `pid`'s root (slot `slot` its leader) for [`collect`]: its
/// exec'd image, or, never exec'd, the fork clone in its task record.
/// Idempotent: the exit path and the purge hook both call it.
fn zombie_root<M: Proc>(slot: usize, pid: u32) {
    let own = OWNED_ROOT.with_mut(|t| core::mem::replace(&mut t[slot], 0));
    let root = if own != 0 { own } else { M::record_root(task_of(slot)) };
    if root == 0 || ZOMBIE_ROOT.with(|z| z.iter().any(|&(p, _)| p == pid)) {
        return;
    }
    let _ = disown_space(pid, root);
    let stored = ZOMBIE_ROOT.with_mut(|z| z.iter_mut().find(|e| e.0 == 0).map(|e| *e = (pid, root)).is_some());
    if !stored {
        safe_print!(96, "[reap] zombie-root table full: pid {} leaks its image\n", pid);
    }
}

/// What a [`collect`] pass released.
#[derive(Clone, Copy, Debug, Default)]
pub struct Collected {
    /// Zombie roots whose pid was reaped.
    pub reaped: usize,
    /// Retired `Process` rows freed.
    pub drained: usize,
    /// Pages freed from parked roots and deferred tables.
    pub pages: usize,
}

/// Free what reaped processes left.
///
/// The roots of pids glue's `wait4` unregistered, retired `Process` rows, parked roots. Run after every
/// `wait4`/`waitid` and at fork/clone entry; cheap when there is nothing to do.
pub fn collect<M: Proc>() -> Collected {
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
    let pages = sweep_parked_roots::<M>() + akuma_mmu::drain_pending_ttbr_frees();
    Collected { reaped, drained, pages }
}
