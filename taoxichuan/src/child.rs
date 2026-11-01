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

/// One slot's kernel+trap stacks (the twin's `prepare_task_slot` answer,
/// amd64's `Stacks` discipline): leaked once on first use, reused on every
/// recycle — a slot given a fresh pair per spawn would turn the slot ring
/// into a memory leak. Dormant until `fork_process` (slice 3) points a
/// crossing at them; nothing indexes them today.
struct SlotStacks {
    kernel: &'static mut [u64],
    trap: &'static mut [u64],
}

/// The per-slot machine state, indexed by ring slot: the slot's leaked
/// stacks and the ring-3 register capture `fork_process` consumes. Reached
/// only through [`with_slot_state`] (a `BorrowCell`: two overlapping borrows
/// are a panic naming the bug).
pub struct SlotState {
    /// This slot's stacks, once `prepare_task_slot` made them.
    stacks: Option<SlotStacks>,
    /// The ring-3 register capture the `write/read_user_context` hooks serve
    /// — `None` until a capture is written, cleared on recycle.
    uctx: Option<akuma_exec::process::UserContext>,
    /// The stack tops `prepare_task_slot` answered with.
    kstack_top: u64,
    trap_stack_top: u64,
}

impl SlotState {
    const fn new() -> Self {
        Self { stacks: None, uctx: None, kstack_top: 0, trap_stack_top: 0 }
    }
}

static SLOT_STATE: akuma_primitives::cell::BorrowCell<[SlotState; NSLOTS]> =
    akuma_primitives::cell::BorrowCell::new([const { SlotState::new() }; NSLOTS]);

/// Run `f` on the machine state of `slot`.
pub fn with_slot_state<R>(slot: usize, f: impl FnOnce(&mut SlotState) -> R) -> R {
    SLOT_STATE.with_mut(|st| f(&mut st[slot]))
}

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
/// Linux's `E2BIG`: the argument block does not fit the initial stack.
const E2BIG: u64 = 7;
/// The loader lays argv/envp/auxv out in a block of `ARG_BLOCK_PAGES` pages;
/// leave room for the pointer vectors, auxv and alignment.
const ARG_BYTES_MAX: usize = crate::ARG_BLOCK_PAGES * 4096 - 2048;

/// Read a NUL-terminated pointer array (argv/envp shape) from the task.
///
/// `Err` is the errno: `EFAULT` for a pointer that is not user memory, `E2BIG` for more than `max`
/// entries before the terminating NULL (Linux answers `E2BIG` for an oversize block, never `EFAULT`).
fn read_ptr_array(arr_va: u64, max: usize) -> Result<Vec<u64>, u64> {
    use akuma_user_walk::Fault;
    akuma_user_walk::read_ptr_array(&crate::task_walk(), arr_va, max, crate::user_ok).map_err(|f| match f {
        Fault::Bad => crate::err::EFAULT,
        Fault::TooLong => E2BIG, // unterminated within the cap
    })
}

/// Read one NUL-terminated string from the task. `Err` is `EFAULT` (unreadable, or not UTF-8, which this
/// kernel's `&str` argument path cannot carry) or `E2BIG` (no NUL within `STR_MAX` bytes).
fn read_user_string(va: u64) -> Result<String, u64> {
    use akuma_user_walk::Fault;
    let bytes = akuma_user_walk::read_cstr(&crate::task_walk(), va, STR_MAX).map_err(|f| match f {
        Fault::Bad => crate::err::EFAULT,
        Fault::TooLong => E2BIG,
    })?;
    String::from_utf8(bytes).map_err(|_| crate::err::EFAULT)
}

/// `execve`'s `(path, argv, envp)`, marshalled out of the calling task.
struct ExecArgs {
    path: String,
    argv: Vec<String>,
    env: Vec<String>,
}

/// Marshal `(path, argv, envp)` from user memory. `Err` is the errno the arm
/// returns: EFAULT for a bad pointer, ENOENT for an empty or unresolvable path.
/// A relative path resolves against `Process.cwd`; ash resolves `PATH` itself.
fn read_exec_args(path_va: u64, argv_va: u64, envp_va: u64) -> Result<ExecArgs, u64> {
    let mut pbuf = [0u8; 512];
    let path = match crate::user_str(path_va, &mut pbuf) {
        Some(p) => String::from(p),
        None => return Err(crate::err::EFAULT),
    };
    if path.is_empty() {
        return Err(crate::err::ENOENT);
    }
    // execve resolves a relative path against the cwd (`./hello`): the
    // process's (`Process.cwd`, which glue's `chdir` writes).
    let Ok(path) = akuma_syscalls_glue::fs::resolve_path_at(-100, &path) else {
        return Err(crate::err::ENOENT);
    };
    let argv_ptrs = read_ptr_array(argv_va, ARGV_MAX)?;
    let mut argv: Vec<String> = Vec::new();
    for p in &argv_ptrs {
        argv.push(read_user_string(*p)?);
    }
    if argv.is_empty() {
        argv.push(path.clone()); // Linux would EFAULT; a path-only exec is kinder
    }
    let mut env: Vec<String> = Vec::new();
    if envp_va != 0 {
        let env_ptrs = read_ptr_array(envp_va, ENV_MAX)?;
        for p in &env_ptrs {
            env.push(read_user_string(*p)?);
        }
    }
    if argv.len() + env.len() > MAX_STRS {
        return Err(E2BIG);
    }
    // Strings plus one pointer each (+ argc, the NULs of both vectors, auxv slack).
    let total: usize = argv.iter().chain(env.iter()).map(|s| s.len() + 1 + 8).sum();
    if total > ARG_BYTES_MAX {
        return Err(E2BIG);
    }
    Ok(ExecArgs { path, argv, env })
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
    let stack: &'static mut [u64; INIT_STACK_WORDS] =
        alloc::boxed::Box::leak(alloc::boxed::Box::new([0u64; INIT_STACK_WORDS]));
    let top = stack.as_mut_ptr() as usize + INIT_STACK_WORDS * 8;
    akuma_threading::x86_seed_entry(tid, top, init_thread_entry);
    // The boot thread becomes pool slot 0 (the pool hands out 1.., amd64's `x86_adopt_running_thread(0)`): it is about to
    // block, and a thread the pool has never heard of has no slot to park.
    let _ = prepare_task_slot(0);
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
    // give it its kernel+trap stacks (slice 2's leaked pairs, reused on
    // recycle — `switch_to` points the syscall stub here), and copy the
    // shell's task record into its own (`tid + 1`; 0 is sshd, 1 the shell):
    // root from `fork_process`'s cloned address space, regions/brk/fds from
    // the shell — the same snapshot the M2 arm makes, at bind time instead
    // of fork time.
    let slot_idx = tid;
    with_slots(|s| {
        s[slot_idx] = SpawnSlot::new();
        s[slot_idx].used = true;
        s[slot_idx].running = true;
        s[slot_idx].pid = pid;
    });
    let _ = prepare_task_slot(slot_idx);
    if is_thread {
        // A thread runs against its creator's record: nothing to copy, and
        // nothing to clone-ref (the fd table is the creator's own).
        TASK_OF.with_mut(|t| t[slot_idx] = crate::task_slot_index());
    } else {
        TASK_OF.with_mut(|t| t[slot_idx] = slot_idx + 1);
    }
    if !is_thread {
        let (regions, n_regions, brk_base, brk_cur, brk_end, mmap_cur, holes) =
            crate::with_task(|t| {
                (
                    t.regions,
                    t.n_regions,
                    t.brk_base,
                    t.brk_cur,
                    t.brk_end,
                    t.mmap_cur,
                    t.holes,
                )
            });
        crate::with_task_slot(slot_idx + 1, |ct| {
        ct.holes = holes;
        ct.root_pa = root_pa;
        ct.regions = regions;
        ct.n_regions = n_regions;
        ct.brk_base = brk_base;
        ct.brk_cur = brk_cur;
        ct.brk_end = brk_end;
        ct.mmap_cur = mmap_cur;
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
        SLOT_STATE.with(|st| st[tid].kstack_top)
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
    crate::futex::purge_task(tid);
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
    if lead != cur {
        crate::futex::purge_task(lead);
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
fn kill_siblings(cur: usize, lead: usize) {
    let tgid = slot_tgid(cur);
    for s in 0..NSLOTS {
        let spid = slot_pid(s);
        if s == cur || s == lead || !slot_in_use(s) || spid == tgid || slot_tgid(s) != tgid {
            continue;
        }
        crate::futex::purge_task(s);
        with_slots(|t| {
            t[s].running = false;
            t[s].used = false;
        });
        akuma_threading::mark_thread_terminated(s);
        retire_thread_process(spid);
    }
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
            let _ = crate::futex::wake_key(p.tgid, addr as usize, 1);
        }
    }
    crate::futex::purge_task(cur);
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

/// A real fork child's `execve`: replace the image in the child's OWN task
/// record (`tid + 1`) and cross — the same load/snapshot/sweep the shell's
/// `exec_replace` does, minus the crossing bookkeeping (a real child has no
/// crossing to resume; its exit goes through [`task_exit_real`]). Never
/// returns: `child_enter` loads the new root and sysrets.
/// Per-slot IA32_FS_BASE shadow (session pi): the MSR is per-CORE, this
/// kernel multiplexes every task onto that one core, and `switch_to` never
/// restored it — a parked task resumed with the LAST-RAN task's TLS pointer.
/// Invisible for three stages because every busybox process shares the same
/// TLS address (deterministic layout); grandfork is the first image with its
/// own TLS, and its parent's wait4 resume ran with the shell's 0x713198 in
/// the MSR (boot pi-3: seven child entries printed the correct 0x100051b8,
/// the parent's next guard read still took fs+0x28 to the shell's bss).
/// `switch_to` installs the incoming task's shadow; `arch_prctl` and the
/// crossings keep it in step. Plain `static mut`: single core, every access
/// on the IF-clear syscall/scheduler path (the TASK_SLOT discipline).
static SLOT_FS: akuma_primitives::cell::BorrowCell<[u64; NSLOTS]> =
    akuma_primitives::cell::BorrowCell::new([0; NSLOTS]);

/// The slot's fs_base (what `switch_to` installs). A slot >= NSLOTS is the
/// idle thread: kernel-only, no TLS, and the answer 0 is never installed.
pub fn slot_fs_of(slot: usize) -> u64 {
    if slot < NSLOTS {
        SLOT_FS.with(|t| t[slot])
    } else {
        0
    }
}

/// Stamp the slot's fs_base (crossings and the `arch_prctl` SET_FS arm).
pub fn set_slot_fs(slot: usize, v: u64) {
    if slot < NSLOTS {
        SLOT_FS.with_mut(|t| t[slot] = v);
    }
}

pub fn exec_real_task(fr: &ring3::SyscallFrame) -> u64 {
    let cur = crate::exec_twin::current_slot();
    let args = match read_exec_args(fr.arg1(), fr.arg2(), fr.arg3()) {
        Ok(a) => a,
        Err(no) => return crate::e(no),
    };
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
    if let Some(p) = akuma_exec::process::current_process_shared() {
        // `/proc/<pid>/exe` (procfs renders `image.exe`): musl's loader expands
        // `$ORIGIN` in a RUNPATH through it — rustc finds `librustc_driver`
        // in `$ORIGIN/../lib` — and fork inherits it.
        {
            let mut img = p.image.lock();
            img.exe.clone_from(&effective);
            img.name = String::from(effective.rsplit('/').next().unwrap_or(&effective));
            img.args = args.argv.clone();
        }
        p.close_cloexec_fds_releasing();
        // execve: caught handlers go back to the default, ignored ones stay
        // ignored (the handler addresses named the old image).
        for a in p.signal_actions.actions.lock().iter_mut() {
            if matches!(a.handler, akuma_exec::process::SignalHandler::UserFn(_)) {
                *a = akuma_exec::process::SignalAction::default();
            }
        }
    }
    // The registered Process's shared root view goes stale with the old
    // image — re-point it, as `exec_replace` does for the shell.
    {
        let pid = slot_pid(cur);
        let first_exec = OWNED_ROOT.with(|t| t[cur]) == 0;
        if disown_space(pid, ti.root_pa) {
            // A fork child's first exec: the root it ran on is the eager
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
    set_slot_fs(cur, 0);
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
    write_user_context(slot, &ctx);
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
fn fork_arm_real(fr: &ring3::SyscallFrame) -> u64 {
    // The boot task has no registered Process to fork. Any other task forks
    // for real, however deep: each child has its own thread and stacks (the
    // M2 crossing's depth ceiling went with the crossing).
    let cur = crate::exec_twin::current_slot();
    if cur == crate::exec_twin::NO_SLOT {
        return crate::e(crate::err::EPERM);
    }
    collect();
    let child_pid = akuma_exec::process::allocate_pid();
    // stack_ptr = 0: the child resumes at the parent's saved user RSP — the
    // syscall stub stashed it (`user_rsp_stash`) and step 6's captured
    // context carries it into the child's UserContext.
    // A vfork-style clone (CLONE_VM, musl posix_spawn) hands the child its own
    // stack in rsi and its __clone stub pops fn/arg from it, so the child must
    // resume there; a plain fork keeps the parent's rsp (0).
    let child_stack = if fr.rax == akuma_syscalls_abi::x86_64::CLONE && fr.arg1() & 0x100 != 0 {
        fr.arg2()
    } else {
        0
    };
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
    const CLONE_THREAD: u64 = 0x0001_0000;
    if fr.rax == akuma_syscalls_abi::x86_64::CLONE && fr.arg1() & CLONE_THREAD != 0 {
        return clone_thread_arm(fr);
    }
    // fork (57), vfork (58) and every non-thread `clone` - libakuma's `fork()` under `linux-abi` issues
    // `clone(SIGCHLD)` - through `fork_process` over the registered Process. `fork_arm_real` takes a vfork-style
    // child stack from `clone`'s `CLONE_VM` form itself.
    fork_arm_real(fr)
}

/// `rt_sigsuspend(mask, sigsetsize)` - x86_64 130: replace the signal mask and sleep until a
/// signal arrives, then restore it and answer `EINTR`, always.
///
/// This kernel delivers no signal, so a sleeper can only be woken by the two things a signal
/// would have meant here: a `tkill` (the thread's interrupt bit, [`crate::tkill_arm`]) and a
/// child's death (`SIGCHLD`) - busybox ash's `wait` builtin parks in exactly this call and
/// re-polls `wait4(WNOHANG)` after it, which is why the `ENOSYS` this answered before made
/// `cmd & ...; wait` spin forever. The child side is wait4's: a live child of this process gets
/// this task's slot stamped as its `waiter`, so its exit wakes the sleeper.
pub fn sigsuspend_arm(fr: &mut ring3::SyscallFrame, mask_va: u64, sigsetsize: u64) -> u64 {
    if sigsetsize != 8 {
        return crate::e(ring3::err::EINVAL);
    }
    let mut b = [0u8; 8];
    if !crate::user_ok(mask_va, 8) || !crate::copy_from_user(&mut b, mask_va) {
        return crate::e(ring3::err::EFAULT);
    }
    let me = crate::exec_twin::current_slot();
    if me == crate::exec_twin::NO_SLOT || me >= NSLOTS {
        // A task that may not park (the boot task) cannot sleep: report the interruption.
        return crate::e(crate::EINTR);
    }
    // SIGKILL (9) and SIGSTOP (19) can never be blocked.
    let unblockable = (1u64 << 8) | (1u64 << 18);
    let new = u64::from_le_bytes(b) & !unblockable;
    let old = akuma_exec::threading::thread_signal_mask();
    akuma_exec::threading::set_thread_signal_mask(new);
    let mut child_died = false;
    loop {
        if akuma_threading::take_thread_interrupted(me) {
            #[cfg(feature = "trace-err")]
            sigsuspend_note(b'I', me);
            break;
        }
        // A child's exit pends SIGCHLD on a thread of its parent's group and
        // wakes it (`publish_child_exit`); this kernel delivers no other signal.
        if akuma_threading::pending_signals_raw(me) & (1 << (SIGCHLD - 1)) != 0 {
            akuma_threading::clear_pending_signal(me, SIGCHLD as u32);
            #[cfg(feature = "trace-err")]
            sigsuspend_note(b'D', me);
            child_died = true;
            break;
        }
        let stash = ring3::user_rsp_stash();
        akuma_threading::schedule_blocking(u64::MAX);
        ring3::set_user_rsp_stash(stash);
    }
    // SIGCHLD with a real handler: run it before this call returns (ash's `wait` sets a flag
    // there and sleeps again until it is set). The mask during the call is the temporary one.
    if child_died && deliver_sigchld(fr, new, old) {
        return crate::e(crate::EINTR);
    }
    akuma_exec::threading::set_thread_signal_mask(old);
    crate::e(crate::EINTR)
}

const SIGCHLD: u64 = 17;
const SA_RESTORER: u64 = 0x0400_0000;
const SA_NODEFER: u64 = 0x4000_0000;
const SA_SIGINFO: u64 = 4;
/// The registers `rt_sigreturn` puts back, in `pack_frame` order.
const FRAME_WORDS: usize = 15;
/// What the signal frame holds above the restorer slot: the saved registers, the user `rsp`
/// at the interruption, the mask to restore.
const SIGFRAME_BYTES: usize = FRAME_WORDS * 8 + 16;

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

/// Push a signal frame for `SIGCHLD` and redirect the syscall's return into its handler. `false`
/// when there is nothing to run (default/ignored disposition, blocked, or the user stack is not
/// writable), in which case nothing was changed. The frame, from the handler's `rsp` up:
/// `[restorer][saved registers][user rsp][mask to restore]`; the restorer's `rt_sigreturn`
/// finds the saved block at its `rsp`. The kernel reads the block back itself, so its layout is
/// private to [`deliver_sigchld`] and [`sigreturn_arm`].
fn deliver_sigchld(fr: &mut ring3::SyscallFrame, cur_mask: u64, restore_mask: u64) -> bool {
    // The disposition is the process's, in `akuma-exec`'s table (glue's
    // `rt_sigaction` writes it).
    let Some(act) = akuma_exec::process::current_process_shared()
        .map(|p| p.signal_actions.actions.lock()[(SIGCHLD - 1) as usize])
    else {
        return false;
    };
    let akuma_exec::process::SignalHandler::UserFn(handler) = act.handler else {
        return false;
    };
    let (handler, flags, restorer, sa_mask) = (handler as u64, act.flags, act.restorer as u64, act.mask);
    if flags & SA_RESTORER == 0 || flags & SA_SIGINFO != 0 {
        return false;
    }
    if cur_mask & (1 << (SIGCHLD - 1)) != 0 {
        return false;
    }
    let user_rsp = ring3::user_rsp_stash();
    // Below the red zone, 16-aligned, then one word lower so the handler sees rsp = 8 (mod 16),
    // as after a `call`.
    let Some(top) = user_rsp.checked_sub(128 + SIGFRAME_BYTES as u64 + 8) else {
        return false;
    };
    let at = (top & !0xF) - 8;
    let mut buf = [0u8; 8 + SIGFRAME_BYTES];
    buf[..8].copy_from_slice(&restorer.to_le_bytes());
    let words = pack_frame(fr);
    for (i, w) in words.iter().enumerate() {
        buf[8 + i * 8..16 + i * 8].copy_from_slice(&w.to_le_bytes());
    }
    // Resume value of the interrupted call is EINTR.
    let rax_at = 8 + 12 * 8;
    buf[rax_at..rax_at + 8].copy_from_slice(&crate::e(crate::EINTR).to_le_bytes());
    let tail = 8 + FRAME_WORDS * 8;
    buf[tail..tail + 8].copy_from_slice(&user_rsp.to_le_bytes());
    buf[tail + 8..tail + 16].copy_from_slice(&restore_mask.to_le_bytes());
    if !crate::user_ok(at, buf.len() as u64) || !crate::copy_to_user(at, &buf) {
        return false;
    }
    let during = cur_mask | sa_mask | if flags & SA_NODEFER == 0 { 1 << (SIGCHLD - 1) } else { 0 };
    akuma_exec::threading::set_thread_signal_mask(during);
    fr.user_rip = handler;
    fr.rdi = SIGCHLD;
    fr.rsi = 0;
    fr.rdx = 0;
    ring3::set_user_rsp_stash(at);
    true
}

/// `rt_sigreturn` - x86_64 15: the restorer's call after a handler returned. The user `rsp` is
/// just past the restorer slot, where [`deliver_sigchld`] put the saved block.
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
    // Never trust a user-built frame with a kernel address or odd flags.
    const USER_TOP: u64 = 0x0000_8000_0000_0000;
    if words[13] >= USER_TOP || user_rsp >= USER_TOP {
        return crate::e(ring3::err::EFAULT);
    }
    words[14] = (words[14] & 0x0000_0CD5) | 0x202;
    unpack_frame(fr, &words);
    ring3::set_user_rsp_stash(user_rsp);
    akuma_exec::threading::set_thread_signal_mask(mask);
    // The dispatcher stores this in RAX: the interrupted call's own result.
    words[12]
}

#[cfg(feature = "trace-err")]
fn sigsuspend_note(why: u8, me: usize) {
    static N: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
    if N.fetch_add(1, core::sync::atomic::Ordering::Relaxed) < 24 {
        akuma_primitives::safe_print!(96, "[sigsuspend] {} me={} pid={}\n", why as char, me, slot_pid(me));
    }
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
/// the address space it owned WITHOUT dropping it. The eager fork clone
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
/// A fork child's address space is the twin's eager clone, every frame of which
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

// ---------------------------------------------------------------------------
// The X86ArchHooks machine-state arms (session pc, slice 2).
//
// `prepare_task_slot` / `write_user_context` / `read_user_context`, over the
// per-slot table above. Nothing calls them until `fork_process` (slice 3)
// claims slots through the shared spawn path — they are landed now, per the
// plan's slice 2, so the seam exists before its first customer.
// ---------------------------------------------------------------------------

/// The twin's stack sizes. The kernel stack matches the shared
/// `SYSCALL_STACK` band's 64 KiB (the T-Y bug-3 number: depth-2 exec band on
/// top of two crossing halves is what fits); the trap stack is amd64's 32
/// KiB. Four slots × 96 KiB = 384 KiB, bounded by the ring, against 767 MiB
/// of guest RAM.
const KSTACK_WORDS: usize = akuma_ring3_amd64::SYSCALL_STACK_SIZE / 8;
const TSTACK_WORDS: usize = 32 * 1024 / 8;

/// Two fresh zeroed stacks, leaked: a slot's stacks must outlive the frame
/// that made them, and a recycled slot reuses the pair it already owns
/// (`SlotCross::stacks` is set once) — per-spawn leaks would turn the ring's
/// recycling into a memory leak.
fn leak_stacks() -> SlotStacks {
    SlotStacks {
        kernel: alloc::vec![0u64; KSTACK_WORDS].leak(),
        trap: alloc::vec![0u64; TSTACK_WORDS].leak(),
    }
}

/// `X86ArchHooks::prepare_task_slot`: give a freshly claimed slot its
/// kernel+trap stacks and its default machine state, answer the kernel stack
/// top. `None` = the slot is out of range or the stacks could not be made;
/// the caller abandons the slot. The crossing record is dead until the next
/// crossing writes it, and a recycled slot's saved user context is cleared —
/// a recycled slot cannot inherit the previous occupant's capture.
pub fn prepare_task_slot(slot: usize) -> Option<usize> {
    if slot >= NSLOTS {
        return None;
    }
    // The claim path (syscall context in slice 3).
    let ktop = with_slot_state(slot, |c| {
        if c.stacks.is_none() {
            c.stacks = Some(leak_stacks());
        }
        let s = c.stacks.as_ref()?;
        let ktop = (s.kernel.as_ptr() as usize + KSTACK_WORDS * 8) as u64;
        let ttop = ((s.trap.as_ptr() as usize + TSTACK_WORDS * 8) & !0xf) as u64;
        c.kstack_top = ktop;
        c.trap_stack_top = ttop;
        c.uctx = None;
        Some(ktop)
    })?;
    crate::exec_twin::fx_reset(slot);
    crate::smp::born_locked(slot);
    Some(ktop as usize)
}

/// `X86ArchHooks::write_user_context`: store a ring-3 register capture into
/// `slot`'s machine state. Always called on a slot that is not running (the
/// shared spawn path's INITIALIZING discipline), which is what makes writing
/// another slot's state safe at all.
pub fn write_user_context(slot: usize, ctx: &akuma_exec::process::UserContext) {
    if slot >= NSLOTS {
        return;
    }
    // `slot` is not running.
    with_slot_state(slot, |c| c.uctx = Some(*ctx));
}

/// `X86ArchHooks::read_user_context`: the exact mirror — `None` means the
/// slot has never been given a capture, the same refusal amd64's table makes
/// for a slot that never trapped (a child built from such a slot would
/// launch at whatever the registers happened to hold).
pub fn read_user_context(
    slot: usize,
) -> Option<akuma_exec::process::UserContext> {
    if slot >= NSLOTS {
        return None;
    }
    with_slot_state(slot, |c| c.uctx)
}
