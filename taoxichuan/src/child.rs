//! Stage 4: the child model — a pipe that is just a buffer.
//!
//! The one design decision of the stage. `sshd`'s exec path spawns
//! `/bin/sh -c <cmd>` (Akuma's SPAWN syscall), then bridges the child's
//! stdout to the ssh channel while polling `waitpid` for the exit. The full
//! kernel answers that with fork, a real pipe, and a scheduler. This kernel
//! has none of those, pinned exclusions all — so the honest minimal model is:
//!
//! **A spawned child runs to completion inside the SPAWN syscall.** Its
//! stdout lands in a kernel buffer (the "pipe" — there is no reader to
//! block, so a `Vec<u8>` is the whole semantics); SPAWN returns the pid and
//! a `ChildOut` fd; the child's exit status is recorded and `waitpid`-able
//! from the moment SPAWN returns. The parent (sshd) sees exactly the
//! protocol it expects — an fd to drain, a pid to reap — and the kernel
//! never needs a second runnable task: at every instant either the parent
//! is in the kernel running the child, or the parent is in ring 3 and the
//! child does not exist.
//!
//! The mechanics (one core, `IF` clear, one kernel stack): SPAWN saves the
//! parent's parked user `RSP`, the syscall stack top, and the callee-saved
//! registers; points the syscall stack at a lower region; loads the child's
//! address space and `sysretq`s into it. The child's syscalls dispatch on
//! the nested stack region. Its `exit_group` does **not** chain the cmdline
//! reaper (`task_done`) — it records the status, switches `CR3` back to the
//! parent's root, restores the saved registers, and `ret`s straight back
//! into the SPAWN arm, coroutine-style. The parent's syscall then completes
//! normally: the stub finds its own stash, and the parent is back in ring 3
//! holding a pid that has already exited.

use alloc::string::String;
use alloc::vec::Vec;
use akuma_ring3_amd64 as ring3;

use crate::net;

/// One reaped-or-running child. Slots recycle round-robin: sshd serves
/// sessions one bridge at a time in practice, and four cover even the
/// `max_sessions` stall case without unbounded kernel memory.
#[allow(clippy::struct_excessive_bools)] // one flag per independent slot fact; a bitset would hide them
pub struct SpawnSlot {
    pub used: bool,
    pub pid: u32,
    /// Always false by the time SPAWN returns — a slot's child runs to
    /// completion inside the syscall. Kept so the state machine is honest.
    pub running: bool,
    /// Linux wait status (`code << 8`), what `waitpid_status` relays.
    pub status: u32,
    /// M2 (fork): WAIT4 reaped this child — a second wait on the pid is
    /// ECHILD, which is how a shell's job table closes the entry.
    pub reaped: bool,
    /// Slice 3: the tid parked in WAIT4 for this child (`u32::MAX` = none).
    /// The child's exit arm wakes it through the threading waker.
    pub waiter: u32,
    /// Slice 3 (session pe): this slot's child is a REAL fork child —
    /// published by `fork_process` with its own threading slot — as opposed
    /// to an M2 spawned child, which also occupies a slot (`used`+`running`+
    /// `current_slot`, spawn_arm stamps the same triple) but must reach
    /// `child_exit`. Boot A's wedge: the unqualified slot test read true for
    /// the session's second SPAWN child (slot 1; the first always lands on
    /// slot 0, which the test's `cur == 0` guard excludes) and routed its
    /// exit_group into `task_exit_real`, which abandoned sshd mid-syscall.
    /// Only `bind_child_task` sets this, so the reset in `new()` clears any
    /// previous occupant's bit.
    pub real: bool,
    /// A `CLONE_THREAD` sibling: shares its creator's task record
    /// (`TASK_OF`), owns no fds or address space of its own, and exits without
    /// a status for a parent to reap.
    pub thread: bool,
}

impl SpawnSlot {
    const fn new() -> Self {
        Self {
            used: false,
            pid: 0,
            running: false,
            status: 0,
            reaped: false,
            waiter: u32::MAX,
            real: false,
            thread: false,
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
/// reaches the table again — `reap_process`, `schedule_blocking` and every print run with the borrow closed.
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
// The SPAWN arm.
// ---------------------------------------------------------------------------

/// argv/envp read-back budget: pointer slots scanned per array, and the byte
/// budget per string. The SPAWN callers pass `argv <= 4`, `env <= 8`; the
/// budgets bound a malicious or confused caller, not the honest case.
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

/// The SPAWN/EXECVE argument block: the two syscalls marshal `(path, argv,
/// envp)` out of the calling task identically.
struct SpawnArgs {
    path: String,
    argv: Vec<String>,
    env: Vec<String>,
}

/// Marshal `(path, argv, envp)` from user memory. `Err` is the errno the arm
/// returns (EFAULT for a bad pointer, ENOENT for a relative path — the
/// spawner passes absolute paths, and ash resolves PATH itself before execve).
fn read_spawn_args(path_va: u64, argv_va: u64, envp_va: u64) -> Result<SpawnArgs, u64> {
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
        argv.push(path.clone()); // Linux would EFAULT; a path-only spawn is kinder
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
    Ok(SpawnArgs { path, argv, env })
}

// Akuma's SPAWN (0x1000+301): `spawn(path, argv, envp, stdin_ptr, stdin_len, flags)`.
//
// Runs the child to completion inside the call (the model this module
// exists to state), then returns `pid | stdout_fd << 32` — the same packed
// word the full kernel's SPAWN returns, read by the same `spawn_full`.
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
    let cur = crate::exec_twin::current_slot();
    cur != crate::exec_twin::NO_SLOT && cur < NSLOTS && SLOTS.with(|s| s[cur].used && s[cur].thread)
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
        let (running, status) = SLOTS.with(|s| (s[tid].running, s[tid].status));
            if !running {
            crate::task_done(u64::from(status >> 8));
        }
        akuma_threading::schedule_blocking(crate::exec_twin::uptime_us() + 10_000);
    }
}

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
        s[slot_idx].real = true;
        s[slot_idx].thread = is_thread;
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
        let (regions, n_regions, brk_base, brk_cur, brk_end, mmap_cur, holes, exe) =
            crate::with_task(|t| {
                (
                    t.regions,
                    t.n_regions,
                    t.brk_base,
                    t.brk_cur,
                    t.brk_end,
                    t.mmap_cur,
                    t.holes,
                    t.exe.clone(),
                )
            });
        crate::with_task_slot(slot_idx + 1, |ct| {
        ct.holes = holes;
        ct.exe = exe;
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

/// The kernel-stack top `switch_to` installs for `tid`: a real child's
/// leaked pair (slice 2's `prepare_task_slot`), the shell's SPAWN crossing
/// band (recorded at the crossing below), and anything else (the idle
/// thread, out-of-range) — the current global, which for the idle task is
/// simply left alone by the caller.
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
    if tid == crate::exec_twin::shell_tid() {
        1
    } else if tid < NSLOTS {
        task_of(tid)
    } else {
        tid + 1
    }
}

pub fn sched_root_of(tid: usize) -> u64 {
    if tid == crate::exec_twin::shell_tid() {
        crate::task_root_of(1) // the shell
    } else if tid < NSLOTS {
        crate::task_root_of(task_of(tid))
    } else {
        0
    }
}

/// A REAL fork child is on the CPU: a threading tid 1..NSLOTS whose
/// SpawnSlot is stamped and running. The shell (tid 0, inside its SPAWN
/// crossing) and the boot task are NOT — their exits keep the M2 paths.
pub fn is_real_task() -> bool {
    let cur = crate::exec_twin::current_slot();
    if cur == crate::exec_twin::NO_SLOT || cur == 0 || cur >= NSLOTS {
        return false;
    }
    // `real` is the whole discriminator, not an optimization: the M2 spawn
    // path stamps the same used/running/current_slot triple for its
    // run-to-completion children (spawn_arm), so without the bind-time bit
    // this test misreads every SPAWN child from the ring's second slot on as
    // a real fork child — boot A wedged the machine exactly there (session
    // pe; the field's doc has the full chain).
    SLOTS.with(|s| s[cur].used && s[cur].running && s[cur].real)
}

/// A real fork child's `exit_group`: status into the slot, the task's pipe
/// references dropped (the sweep that turns the last writer close into the
/// reader's EOF), the WAIT4-parked parent woken through the threading
/// waker, then the threading slot terminated — `x86_finish_current` never
/// returns (the dispatcher frame on this task's own band is abandoned, the
/// same model `task_done` uses for the boot's tasks).
pub fn task_exit_real(code: u64) -> ! {
    task_exit_status(((code & 0xFF) as u32) << 8)
}

/// A real task killed by a fault: the group dies with wait status SIGSEGV
/// (11), the parent's `wait4` sees `WIFSIGNALED` as on Linux, and the machine
/// keeps running. Only for a real fork child or thread; the shell and the
/// boot task keep the halting report.
/// `akuma-threading`'s slot-purge hook: thread `tid` has just been marked
/// TERMINATED. When `akuma-exec` ended its process rather than this kernel's
/// exit path — a fatal signal's default action inside `akuma-syscalls-glue`
/// (SIGPIPE on a pipe with no reader runs glue's `sys_exit_group`, which kills
/// the group, the caller included) — the ring row still says `running` and
/// the parent's `wait4` would sleep forever. Record the status, clear
/// `running` and wake the waiter, as `task_exit_status` does. This kernel's own
/// exits clear `running` first, so for them this is only the futex purge.
/// Bridges the two wait models until `wait4` is `akuma-exec`'s (pm S2).
pub fn on_thread_terminated(tid: usize) {
    crate::futex::purge_task(tid);
    if tid >= NSLOTS {
        return;
    }
    let pid = slot_pid(tid);
    let Some(code) = akuma_exec::process::lookup_process_shared(pid)
        .filter(|p| p.exited.load(core::sync::atomic::Ordering::Relaxed))
        .map(|p| p.exit_code.load(core::sync::atomic::Ordering::Relaxed))
    else {
        return;
    };
    // A negative code is "killed by that signal" (status = the signal number).
    let status = if code < 0 { code.unsigned_abs() & 0x7F } else { ((code as u32) & 0xFF) << 8 };
    let waiter = with_slots(|s| {
        if !s[tid].used || !s[tid].running {
            return u32::MAX;
        }
        s[tid].running = false;
        s[tid].status = status;
        s[tid].waiter
    });
    if waiter != u32::MAX {
        akuma_threading::get_waker_for_thread(waiter as usize).wake();
    }
}

pub fn task_die_segv() -> ! {
    task_exit_status(11)
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

fn task_exit_status(status: u32) -> ! {
    // A shared file mapping persists at exit (rust-lld never munmaps its output).
    crate::shared_flush_all();
    let cur = crate::exec_twin::current_slot();
    // The group's leader slot: `cur` itself unless a thread is the one
    // calling `exit_group`, in which case the leader's status and waiter are
    // the process's. Every other thread of the group dies here (Linux's
    // `exit_group`): purged from the futex table and terminated.
    let lead = group_leader(cur);
    kill_siblings(cur, lead);
    let (waiter, lead_pid) = with_slots(|s| {
        s[lead].status = status;
        s[lead].running = false;
        (s[lead].waiter, s[lead].pid)
    });
    // The zombie keeps its ring row (status!) until the parent's wait4: without
    // this hold `x86_claim_slot` hands the TERMINATED slot to the next fork and
    // `SpawnSlot::new()` erases the status, so the waiter gets ECHILD (cargo
    // -j4). `reap_process`'s `unregister_process` releases it.
    akuma_exec::process::hold_exited_thread_slot(lead_pid, lead);
    // Init's exit is what the parked boot thread waits for (`start_init_thread`): wake it by event, slot 0, rather than leave it
    // to a timed sleep that nothing is guaranteed to service once init's thread has switched away for good.
    if lead == INIT_TID.load(core::sync::atomic::Ordering::Relaxed) {
        akuma_threading::get_waker_for_thread(0).wake();
    }
    close_process_fds();
    if waiter != u32::MAX {
        akuma_threading::get_waker_for_thread(waiter as usize).wake();
    }
    if lead != cur {
        crate::futex::purge_task(lead);
        akuma_threading::mark_thread_terminated(lead);
    }
    akuma_threading::x86_finish_current()
}

/// The pid of the thread GROUP slot `slot` belongs to: the registered
/// `Process`'s `tgid` (a `clone_thread` Process carries its leader's), else the
/// slot's own pid. Children are stamped with and matched against this, so a
/// child forked by one thread can be waited on by another.
fn group_pid(slot: usize) -> u32 {
    let pid = slot_pid(slot);
    akuma_exec::process::table::with_process(pid, |p| p.tgid).unwrap_or(pid)
}

/// The slot that owns the process `cur` belongs to: the one non-thread slot
/// running against the same task record. `cur` when it is not a thread.
fn group_leader(cur: usize) -> usize {
    if cur >= NSLOTS || !SLOTS.with(|s| s[cur].thread) {
        return cur;
    }
    let rec = task_of(cur);
    (0..NSLOTS)
        .find(|&s| s != cur && SLOTS.with(|t| t[s].used && !t[s].thread) && task_of(s) == rec)
        .unwrap_or(cur)
}

/// `exit_group`: end every thread of `cur`'s group except `cur` and `lead`
/// (the caller finishes those itself).
fn kill_siblings(cur: usize, lead: usize) {
    let rec = task_of(cur);
    for s in 0..NSLOTS {
        if s != cur && s != lead && SLOTS.with(|t| t[s].used && t[s].thread) && task_of(s) == rec {
            crate::futex::purge_task(s);
            let pid = with_slots(|t| {
                t[s].running = false;
                t[s].used = false;
                t[s].thread = false;
                t[s].pid
            });
            akuma_threading::mark_thread_terminated(s);
            retire_thread_process(pid);
        }
    }
}

/// Take a dead thread's registered `Process` out of the process table.
///
/// `clone_thread` registers a real `Process` per thread (so `current_process` answers
/// per thread), and nothing but `reap_process` - which runs for a group LEADER, on its
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
        s[cur].thread = false;
        s[cur].real = false;
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
    let _ = akuma_exec::process::reclaim::drain_retired();
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

/// Session pi (grandfork step 5): child pid -> parent pid, stamped at
/// `fork_arm_real` and consulted by the wait4 reapers. The slot table is
/// GLOBAL — before this map, any task's `wait4(-1)` could reap (or park on)
/// an UNRELATED task's zombie: grandfork step 2 deliberately leaves an
/// un-reaped grandchild, and step 5's own childless `wait4(-1)` returned
/// that orphan instead of ECHILD (exit 5). Unstamped pids (0 = the M2 spawn
/// path's children) stay visible to everyone, so the M-gate semantics do
/// not move. Bounded ring; pid reuse just overwrites an entry.
static PARENT_OF: akuma_primitives::cell::BorrowCell<[(u32, u32); 256]> = akuma_primitives::cell::BorrowCell::new([(0, 0); 256]);

pub fn stamp_child_parent(child: u32, parent: u32) {
    PARENT_OF.with_mut(|p| p[child as usize % 256] = (child, parent));
}

/// 0 = unknown (an M2-spawned child, or the map's ring moved past it).
pub fn parent_of(child: u32) -> u32 {
    if child == 0 {
        return 0;
    }
    PARENT_OF.with(|p| p.iter().find(|&&(c, _)| c == child).map_or(0, |&(_, par)| par))
}

pub fn exec_real_task(fr: &ring3::SyscallFrame) -> u64 {
    let cur = crate::exec_twin::current_slot();
    let args = match read_spawn_args(fr.arg1(), fr.arg2(), fr.arg3()) {
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
    crate::with_task(|t| t.exe.clone_from(&effective));
    // POSIX exec keeps open fds across the image swap; close-on-exec ones go,
    // each released through the runtime hooks (the table is the process's).
    if let Some(p) = akuma_exec::process::current_process_shared() {
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
    let child_pid = akuma_exec::process::allocate_pid();
    // stack_ptr = 0: the child resumes at the parent's saved user RSP — the
    // syscall stub stashed it (`user_rsp_stash`) and step 6's captured
    // context carries it into the child's UserContext.
    // A vfork-style clone (CLONE_VM, musl posix_spawn) hands the child its own
    // stack in rsi and its __clone stub pops fn/arg from it, so the child must
    // resume there; a plain fork keeps the parent's rsp (0).
    let child_stack = if fr.rax == ring3::nr::CLONE && fr.arg1() & 0x100 != 0 {
        fr.arg2()
    } else {
        0
    };
    match akuma_exec::process::fork_process(child_pid, child_stack) {
        Ok(pid) => {
            // Session pj wiring of pi's PARENT_OF: the child's parent is THIS
            // task's pid — the slot ROW's pid, never the slot index (slots are
            // reused across successive children — pi4's console shows tid=10/11
            // alternating for pids 29..37; pids are monotonic). The wait4
            // reapers consult the map for the crate's is_child_of_group
            // decision: wait* on a non-child is ECHILD and never blocks.
            // A child belongs to the thread GROUP: cargo forks from a job thread
            // and another thread waits on it (`wait4` compares group pids).
            stamp_child_parent(pid, group_pid(cur));
            u64::from(pid)
        }
        // The refusal line is the evidence this slice hunts for; name the
        // message verbatim (the same discipline as spawn's read-failed
        // print).
        Err(msg) => {
            akuma_primitives::safe_print!(192, "[forkp] refused: {}\n", msg);
            for i in 0..NSLOTS {
                let (used, running, reaped, thread, real, rpid) =
                    SLOTS.with(|t| (t[i].used, t[i].running, t[i].reaped, t[i].thread, t[i].real, t[i].pid));
                akuma_primitives::safe_print!(
                    128,
                    "  ring {}: pool={} oncpu={} held={} used={} running={} reaped={} thread={} real={} pid={}\n",
                    i,
                    akuma_threading::get_thread_state(i),
                    akuma_threading::on_cpu_flag(i),
                    akuma_threading::slot_held_by(i),
                    used,
                    running,
                    reaped,
                    thread,
                    real,
                    rpid
                );
            }
            crate::e(ring3::err::EAGAIN)
        }
    }
}

// ---------------------------------------------------------------------------
// Exec-replace: the nested child's execve.
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// The wait family, as the bridge drives it.
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// M2 scaffold: the blocking fork (the trim plan's H3-f) and a real WAIT4.
//
// The plan's owner-approved exception to the composition rule: a fork that
// runs the child to completion INSIDE the fork call — the SPAWN shape, but
// the child is the CALLER's own code, resumed at its current RIP with RAX 0
// on an eager, flag-faithful copy of its memory (no CoW: there is no page
// fault handler, and eager copy is what the loader already does), with the
// fd table the extracted crate's own fork decision (`fdtable::FdTable::
// fork_copy`), never a hand copy. FORK RETURNS THE CHILD'S PID in the
// parent; the child's exit status is in the slot, and WAIT4 — now a real
// arm — reports it. **THROWAWAY: M4 (akuma-exec adoption — real scheduler,
// `fork_process`, parked parents) replaces this whole section.** Depth 2 is
// the ceiling here: a fork of the fork child has no stack region left, and
// a parked parent cannot pump a pipe, which is why M4 exists.
// ---------------------------------------------------------------------------

/// CLONE/FORK/VFORK (56/57/58): the M2 blocking fork. On x86_64 musl issues
/// `fork()` as `clone(SIGCHLD)` and `posix_spawn` as
/// `clone(SIGCHLD|CLONE_VM|CLONE_VFORK)`, but **vfork as the raw SYS_vfork
/// in assembly — no flags word at all** (`vfork.s`: `mov $58, %eax`), and
/// busybox ash forks its non-final commands through exactly that, so the
/// flags check keys on the number and FORK/VFORK are accepted bare. For
/// CLONE: `SIGCHLD` alone (a real fork — the eager copy makes the child
/// independent) and `SIGCHLD|CLONE_VM|CLONE_VFORK` (posix_spawn's shape;
/// vfork's contract — the child touches nothing before exec — holds by
/// construction, since the parent does not resume until the child has
/// exited) are accepted; anything else (CLONE_THREAD and friends) is M4's
/// scheduler, not this scaffold.
pub fn fork_arm(fr: &ring3::SyscallFrame) -> u64 {
    const CLONE_THREAD: u64 = 0x0001_0000;
    if fr.rax == ring3::nr::CLONE && fr.arg1() & CLONE_THREAD != 0 {
        return clone_thread_arm(fr);
    }
    // fork (57), vfork (58) and every non-thread `clone` - libakuma's `fork()` under `linux-abi` issues
    // `clone(SIGCHLD)` - through `fork_process` over the registered Process. `fork_arm_real` takes a vfork-style
    // child stack from `clone`'s `CLONE_VM` form itself.
    fork_arm_real(fr)
}

/// Linux WAIT4 (61): `wait4(pid, &status, options, rusage)` — the M2 answer,
/// reading the same slot table `waitpid_arm` does. Returns the reaped
/// child's pid with the status word written out; `ECHILD` for a pid that is
/// not an unreaped child (including one already reaped — that is how a
/// shell's job table closes its entry); `0` only where the ABI allows
/// "nothing reaped" (WNOHANG on a running child — unreachable in this
/// model, since a child completes inside its own fork call, but answered
/// honestly). `rusage` is accepted and ignored: the gate's callers pass
/// NULL, and zero-filled accounting is what this box has always given.
pub fn wait4_arm(pid: u64, status_va: u64, options: u64, _rusage_va: u64) -> u64 {
    wait4_real(pid, status_va, options)
}

/// The REAL wait4 (slice 3): reap a dead child from the slot scan, or PARK
/// the caller until one dies. The park is `schedule_blocking(u64::MAX)` —
/// WAITING, then `x86_yield_now` hands the core to the next READY task (or
/// the idle thread); the child's exit arm wakes the registered waiter
/// through the threading waker, which makes this caller READY again, and
/// the loop re-scans. WNOHANG answers 0 while the matched children still
/// run. The boot task never parks (the bridge drives it through
/// `waitpid_arm`); a WAIT4 that reaches it keeps the M2 scan.
fn wait4_real(pid: u64, status_va: u64, options: u64) -> u64 {
    let want_pid = pid as u32;
    let any = pid == u64::MAX || want_pid == 0; // -1, or "my process group"
    let wnohang = options & 1 != 0;
    let my_tid = crate::exec_twin::current_slot();
    if my_tid == crate::exec_twin::NO_SLOT || my_tid >= NSLOTS {
        // Not a task that may park — answer without blocking.
        return wait4_arm_blocking_scan(pid, status_va);
    }
    // Session pj: the caller's OWN pid (the slot row's — never the slot index,
    // slots are reused). This is the crate's is_child_of_group waiter side:
    // wait* reaps only children registered to THIS pid, answers ECHILD for a
    // stranger's, and never parks on one (akuma-exec children.rs:42).
    let my_pid = group_pid(my_tid);
    loop {
        // The scan runs with the ring borrowed, so it only DECIDES: marking the row and
        // noting (index, pid, status). `reap_process` reads the ring itself and runs after.
        let reaped = with_slots(|s| {
            for (sidx, sl) in s.iter_mut().enumerate() {
                if !sl.used || sl.reaped || sl.running || (!any && sl.pid != want_pid) {
                    continue;
                }
            // Not my child: the crate's is_child_of_group answer, reached by
            // skipping the candidate here and in the live scan below. The
            // parked arm's callers are REAL tasks — every child they could
            // own went through fork_arm_real and is STAMPED (spawn rows are
            // the sshd's, and the spawn model never waits: their rows are
            // zombies nobody reaps — pj-4's instrument caught step 5's
            // wait(0) reaping PK1's left-behind shell, po=0). A caller's own
            // pid is never 0, so po==0 skips too.
            if parent_of(sl.pid) != my_pid {
                continue;
            }
            sl.waiter = u32::MAX;
            sl.reaped = true;
            return Some((sidx, sl.pid, sl.status));
            }
            None
        });
        if let Some((sidx, child, status)) = reaped {
            reap_process(sidx, child);
            if status_va != 0
                && (!crate::user_ok(status_va, 4)
                    || !crate::copy_to_user(status_va, &status.to_le_bytes()))
            {
                return crate::e(crate::err::EFAULT);
            }
            return u64::from(child);
        }
        let live = with_slots(|s| {
            let mut live = false;
            for sl in s.iter_mut() {
                if sl.used && !sl.reaped && sl.running && (any || sl.pid == want_pid) {
                    // Live = MY OWN child (stamped mine): a real task parks only
                    // for real children; a stranger's running child — spawn rows
                    // included — neither parks me nor takes my waiter stamp.
                    if parent_of(sl.pid) == my_pid {
                        live = true;
                        sl.waiter = my_tid as u32;
                    }
                }
            }
            live
        });
        if !live {
            return crate::e(ring3::err::ECHILD);
        }
        if wnohang {
            return 0;
        }
        // The stub's stash is a GLOBAL — every task's syscall entry overwrites
        // it, so the parked task's own user RSP must be saved across the
        // park and restored before its syscall returns (the stub tail reads
        // the global, not a per-task copy).
        let stash = ring3::user_rsp_stash();
        akuma_threading::schedule_blocking(u64::MAX);
        ring3::set_user_rsp_stash(stash);
        // Woken (or the deadline passthrough never fires at u64::MAX): the
        // loop re-scans. Spurious wakes are fine — the scan decides.
    }
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
    let my_pid = group_pid(me);
    let mut child_died = false;
    loop {
        if akuma_threading::take_thread_interrupted(me) {
            #[cfg(feature = "trace-err")]
            sigsuspend_note(b'I', me, 0, 0);
            break;
        }
        // A child of mine that has exited and is not yet reaped is the SIGCHLD that already
        // arrived; a live one gets this slot as its waiter so its exit wakes the sleep.
        let dead = with_slots(|s| {
            let mut dead = None;
            for (i, sl) in s.iter_mut().enumerate() {
                if !sl.used || sl.reaped || parent_of(sl.pid) != my_pid {
                    continue;
                }
                if sl.running {
                    sl.waiter = me as u32;
                } else {
                    dead = Some((i, sl.pid));
                }
            }
            dead
        });
        if let Some((_i, _pid)) = dead {
            #[cfg(feature = "trace-err")]
            sigsuspend_note(b'D', me, _i, _pid);
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
fn sigsuspend_note(why: u8, me: usize, slot: usize, pid: u32) {
    static N: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
    if N.fetch_add(1, core::sync::atomic::Ordering::Relaxed) < 24 {
        akuma_primitives::safe_print!(96, "[sigsuspend] {} me={} slot={} pid={} my_pid={}\n", why as char, me, slot, pid, group_pid(me));
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

/// A reaped real child's `Process` leaves the table and its memory comes
/// back. `unregister_process` retires the row and the drain (cooldown 0 on
/// this kernel, `exec_twin`) drops it — `UserAddressSpace::drop` returns the
/// frames of a fork-cloned space the child never replaced, and the retire
/// closure marks the thread slot TERMINATED so `x86_claim_slot` can reuse it.
/// An exec'd image's root is the twin's own (see [`OWNED_ROOT`]): freed here.
/// Without this every exited child kept its whole address space for the rest
/// of the boot (the second `cargo` of a boot died of it).
fn reap_process(slot: usize, pid: u32) {
    let before = akuma_exec::pmm::free_count();
    let own = OWNED_ROOT.with_mut(|t| core::mem::replace(&mut t[slot], 0));
    // The root the child last ran on: its exec'd image, or - never exec'd -
    // the fork clone in its task record. A thread shares its creator's.
    let root = if own != 0 {
        own
    } else if SLOTS.with(|s| s[slot].thread) {
        0
    } else {
        crate::task_root_of(task_of(slot))
    };
    if root != 0 {
        let _ = disown_space(pid, root);
        park_root(root);
    }
    let retired = akuma_exec::process::unregister_process(pid);
    let drained = akuma_exec::process::reclaim::drain_retired();
    let mut pages = sweep_parked_roots();
    pages += akuma_mmu::drain_pending_ttbr_frees();
    if cfg!(feature = "trace-err") {
        akuma_primitives::safe_print!(
        224,
        "[reap] pid={} retired={} drained={} root={} free {} -> {} heap={}K cowrefs={} cow={:?} procs={}/{}\n",
        pid,
        retired,
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

/// The M2 body of [`wait4_arm`], kept for the boot task's non-blocking
/// answer (the bridge's reaper never parks).
fn wait4_arm_blocking_scan(pid: u64, status_va: u64) -> u64 {
    let want_pid = pid as u32;
    let any = pid == u64::MAX || want_pid == 0;
    let found = with_slots(|s| {
        for sl in s.iter_mut() {
            if !sl.used || sl.reaped || sl.running || (!any && sl.pid != want_pid) {
                continue;
            }
        // Session pj: a STAMPED row names a parent — and this arm's callers
        // (the boot task behind NO_SLOT, kernel-side drivers) own no stamped
        // children (the fork depth gate refuses forks outside the spawned
        // shell). Skip: an orphan whose parent died stays unreapable here —
        // the crate reparents orphans to init (children.rs:310), but this
        // kernel has no init reaper, so unreapable is the honest no-init
        // answer (one slot row per abandoned grandchild, harmless at
        // NSLOTS=12). Unstamped rows (spawn-path children, the shell) stay
        // visible: the M-gates and the bridge's reaper do not move.
        if parent_of(sl.pid) != 0 {
            continue;
        }
        sl.reaped = true;
        return Some((sl.pid, sl.status));
        }
        None
    });
    let Some((child, status)) = found else {
        return crate::e(ring3::err::ECHILD);
    };
    if status_va != 0
        && (!crate::user_ok(status_va, 4) || !crate::copy_to_user(status_va, &status.to_le_bytes()))
    {
        return crate::e(crate::err::EFAULT);
    }
    u64::from(child)
}

/// The `net` module's clock is the only time source; re-exported read for
/// the dispatcher's nanosleep arms (kept here so `net.rs` stays device-only).
pub fn sleep_us(us: u64) {
    net::sleep_until(net::uptime_us() + us);
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
