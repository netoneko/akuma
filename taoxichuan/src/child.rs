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
use akuma_vfs::Filesystem;

use crate::net;

/// One reaped-or-running child. Slots recycle round-robin: sshd serves
/// sessions one bridge at a time in practice, and four cover even the
/// `max_sessions` stall case without unbounded kernel memory.
pub struct SpawnSlot {
    pub used: bool,
    pub pid: u32,
    /// Always false by the time SPAWN returns — a slot's child runs to
    /// completion inside the syscall. Kept so the state machine is honest.
    pub running: bool,
    /// Linux wait status (`code << 8`), what `waitpid_status` relays.
    pub status: u32,
    /// The pipe: the child's stdout, read out through the `ChildOut` fd.
    pub out: Vec<u8>,
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
            out: Vec::new(),
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
pub const NSLOTS: usize = 12;
/// Stdout ceiling per child: after this, writes are dropped (the paws
/// fallback truncates at 1 MiB too). The gate's output is two orders below.
const OUT_CAP: usize = 1024 * 1024;

static mut SLOTS: [SpawnSlot; NSLOTS] = [
    SpawnSlot::new(),
    SpawnSlot::new(),
    SpawnSlot::new(),
    SpawnSlot::new(),
    SpawnSlot::new(),
    SpawnSlot::new(),
    SpawnSlot::new(),
    SpawnSlot::new(),
    SpawnSlot::new(),
    SpawnSlot::new(),
    SpawnSlot::new(),
    SpawnSlot::new(),
];
static mut NEXT_SLOT: usize = 0;
// Pids come from `akuma_exec::process::allocate_pid` (floor 2: 1 is sshd,
// the boot's init task) - ONE allocator, because `clone_thread` draws its
// thread Processes' pids from the crate's counter and a second counter here
// issued the same pid twice (a thread and a fork child sharing a table row).

/// The parent-phase registers one crossing saves: what `child_enter`'s naked
/// prologue stores and its exit-side consumers restore. `repr(C)` with the
/// `ret_rsp` first — the asm addresses the record by offset, exactly the
/// layout `exec_return`'s restore already walked (0x00 rsp, 0x08 rbx, 0x10
/// rbp, 0x18 r12 … 0x30 r15).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct CrossSave {
    pub ret_rsp: u64,
    pub rbx: u64,
    pub rbp: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
}

impl CrossSave {
    const fn new() -> Self {
        Self { ret_rsp: 0, rbx: 0, rbp: 0, r12: 0, r13: 0, r14: 0, r15: 0 }
    }
}

/// One slot's kernel+trap stacks (the twin's `prepare_task_slot` answer,
/// amd64's `Stacks` discipline): leaked once on first use, reused on every
/// recycle — a slot given a fresh pair per spawn would turn the slot ring
/// into a memory leak. Dormant until `fork_process` (slice 3) points a
/// crossing at them; nothing indexes them today.
struct SlotStacks {
    kernel: &'static mut [u64],
    trap: &'static mut [u64],
}

/// The per-slot machine state (slice 2): everything the crossing statics
/// `NESTED_SLOT`/`NESTED_DEPTH`/`SPAWN_OUT_SLOT`/`PARENT_*`/`RET_RSP`/`SAVE_*`
/// held, indexed by spawn-ring slot and read **through
/// `exec_twin::current_slot()`** while a child runs. The plan's shape (§ 3):
/// the moment two tasks can block, "the one task in the kernel" identity
/// breaks, so the crossing state cannot live in shared statics — and the
/// owner's rule is no new `static` "current task" anywhere: "who am I" is
/// `current_slot()`, whose one-core stub is the sanctioned static.
///
/// Written only on the syscall path (single core, `IF` clear) or by
/// `child_enter`'s asm prologue into `save`; read the same ways.
struct SlotCross {
    /// The parent's saved registers — `child_enter`'s asm writes here (the
    /// record's address rides the crossing in `%rdx`).
    save: CrossSave,
    /// The parent's CR3 root, loaded by the exit tail (`child_exit`).
    parent_root: u64,
    /// The parent's syscall-stack top, restored by the resuming arm
    /// (`spawn_arm` / `fork_arm` tails).
    parent_stack_top: u64,
    /// The parent's parked user RSP stash, likewise.
    parent_stash: u64,
    /// The crossing depth this slot's occupant runs at: 1 = spawned child,
    /// 2 = its fork child. Was the shared `NESTED_DEPTH`; per-slot it is
    /// inherited (+1) at the fork crossing.
    depth: u32,
    /// The `TASKS` slot the child runs in (1 = spawned, 2 = fork child):
    /// which record `exit` lands in. Was implicit in the old depth read.
    task_slot: usize,
    /// The `TASKS` slot of whoever the crossing returns to (0 = sshd, 1 =
    /// the shell): `child_exit`'s `set_task_slot`. Was `depth - 1`.
    parent_task_slot: usize,
    /// The spawn-ring slot that becomes current again on exit
    /// (`exec_twin::NO_SLOT` for sshd): `child_exit`'s
    /// `set_current_slot`. Was the `NESTED_SLOT` save/restore dance.
    parent_slot: usize,
    /// The slot whose `out` buffer carries this child's fd 1. Set to the
    /// slot itself by `spawn_arm`; a fork child inherits its parent's — a
    /// fork child's writes must land in the buffer sshd drains, never in its
    /// own slot (boot-3 lesson, gate P3).
    out_slot: usize,
    /// This slot's stacks, once `prepare_task_slot` made them.
    stacks: Option<SlotStacks>,
    /// The ring-3 register capture the `write/read_user_context` hooks serve
    /// — `None` until a capture is written, cleared on recycle.
    uctx: Option<akuma_exec::process::UserContext>,
    /// The stack tops `prepare_task_slot` answered with.
    kstack_top: u64,
    trap_stack_top: u64,
}

impl SlotCross {
    const fn new() -> Self {
        Self {
            save: CrossSave::new(),
            parent_root: 0,
            parent_stack_top: 0,
            parent_stash: 0,
            depth: 0,
            task_slot: 0,
            parent_task_slot: 0,
            parent_slot: crate::exec_twin::NO_SLOT,
            out_slot: 0,
            stacks: None,
            uctx: None,
            kstack_top: 0,
            trap_stack_top: 0,
        }
    }
}

/// The per-slot table itself. SAFETY: single core, `IF` clear (syscall path
/// or boot) — the discipline that owns every static in this module; the
/// table is indexed by `exec_twin::current_slot()` while a child runs and by
/// the resuming arm's own `slot_idx` after the crossing returns.
static mut SLOT_STATE: [SlotCross; NSLOTS] = [
    SlotCross::new(),
    SlotCross::new(),
    SlotCross::new(),
    SlotCross::new(),
    SlotCross::new(),
    SlotCross::new(),
    SlotCross::new(),
    SlotCross::new(),
    SlotCross::new(),
    SlotCross::new(),
    SlotCross::new(),
    SlotCross::new(),
];
/// The crossing record of `slot`, for the syscall-path writers.
///
/// SAFETY: as `slots`.
pub(crate) unsafe fn cross_mut(slot: usize) -> &'static mut SlotCross {
    unsafe {
        let st = &mut *core::ptr::addr_of_mut!(SLOT_STATE);
        &mut st[slot]
    }
}

/// `child_enter`'s asm destination for `slot` — a raw pointer, because the
/// naked prologue takes it in `%rdx` and Rust must not see a reference to
/// memory the asm writes behind its back.
///
/// SAFETY: as `slots`.
unsafe fn save_ptr(slot: usize) -> *mut CrossSave {
    unsafe { core::ptr::addr_of_mut!((*core::ptr::addr_of_mut!(SLOT_STATE))[slot].save) }
}

/// SAFETY: single core, `IF` clear (syscall path or boot) — the discipline
/// that owns every static in this module.
fn slots() -> &'static mut [SpawnSlot; NSLOTS] {
    unsafe { &mut *core::ptr::addr_of_mut!(SLOTS) }
}

/// Is a nested child running right now? The `exit_group` arm's fork in the
/// dispatcher.
pub fn nested_active() -> bool {
    crate::exec_twin::current_slot() != crate::exec_twin::NO_SLOT
}

/// The pid the current task should report (`getpid`): 1 for the boot task,
/// the spawn slot's pid while the child runs.
pub fn current_pid() -> u32 {
    let cur = crate::exec_twin::current_slot();
    if cur != crate::exec_twin::NO_SLOT {
        // SAFETY: syscall-path discipline.
        slots()[cur].pid
    } else {
        1
    }
}

/// The `ProcessHooks::pid_for_thread` / `find_pid_by_thread` arm: the pid a
/// spawn-ring slot runs under today, `None` for an unused slot. The thread
/// namespace on this kernel IS the spawn ring until `fork_process` (slice 3)
/// replaces it — one table, one answer.
pub fn pid_for_slot(idx: usize) -> Option<u32> {
    let s = slots();
    if idx < NSLOTS && s[idx].used {
        Some(s[idx].pid)
    } else {
        None
    }
}

/// The nested child's write to fd 1: into the pipe (the buffer).
pub fn child_write(buf_va: u64, len: u64) -> bool {
    let cur = crate::exec_twin::current_slot();
    if cur == crate::exec_twin::NO_SLOT {
        // Unreachable today: only a child's fd 1 lands here, and a child
        // running means `current_slot` names one. A parent write to fd 1
        // would be a dispatcher bug; refuse rather than write into slot 0's
        // buffer (what the old default static would have done).
        return false;
    }
    // SAFETY: syscall-path discipline.
    let out_slot = unsafe { cross_mut(cur).out_slot };
    let mut chunk = [0u8; 512];
    let mut done = 0u64;
    while done < len {
        let n = (len - done).min(chunk.len() as u64) as usize;
        if !crate::copy_from_user(&mut chunk[..n], buf_va + done) {
            return false;
        }
        // SAFETY: syscall-path discipline.
        let slot = unsafe {
            let s = &mut *core::ptr::addr_of_mut!(SLOTS);
            &mut s[out_slot]
        };
        if slot.out.len() < OUT_CAP {
            let take = n.min(OUT_CAP - slot.out.len());
            slot.out.extend_from_slice(&chunk[..take]);
        }
        done += n as u64;
    }
    true
}

// ---------------------------------------------------------------------------
// The SPAWN arm.
// ---------------------------------------------------------------------------

/// argv/envp read-back budget: pointer slots scanned per array, and the byte
/// budget per string. The SPAWN callers pass `argv <= 4`, `env <= 8`; the
/// budgets bound a malicious or confused caller, not the honest case.
const ARGV_MAX: usize = 128;
const ENV_MAX: usize = 128;
const STR_MAX: usize = 4096;

/// Read a NUL-terminated pointer array (argv/envp shape) from the task.
fn read_ptr_array(arr_va: u64, max: usize) -> Option<Vec<u64>> {
    let mut out = Vec::new();
    for i in 0..max {
        let mut slot = [0u8; 8];
        if !crate::copy_from_user(&mut slot, arr_va + (i as u64) * 8) {
            return None;
        }
        let p = u64::from_le_bytes(slot);
        if p == 0 {
            return Some(out);
        }
        out.push(p);
        if !crate::user_ok(p, 1) {
            return None;
        }
    }
    None // unterminated: refuse rather than guess
}

/// Read one NUL-terminated string from the task.
fn read_user_string(va: u64) -> Option<String> {
    let mut buf = [0u8; STR_MAX];
    let mut len = 0usize;
    while len < STR_MAX {
        let mut one = [0u8; 1];
        if !crate::copy_from_user(&mut one, va + len as u64) {
            return None;
        }
        if one[0] == 0 {
            return core::str::from_utf8(&buf[..len]).ok().map(String::from);
        }
        buf[len] = one[0];
        len += 1;
    }
    None
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
    let mut pbuf = [0u8; 128];
    let path = match crate::user_str(path_va, &mut pbuf) {
        Some(p) => String::from(p),
        None => return Err(crate::err::EFAULT),
    };
    if !path.starts_with('/') {
        return Err(crate::err::ENOENT);
    }
    let argv_ptrs = match read_ptr_array(argv_va, ARGV_MAX) {
        Some(p) => p,
        None => return Err(crate::err::EFAULT),
    };
    let mut argv: Vec<String> = Vec::new();
    for p in &argv_ptrs {
        match read_user_string(*p) {
            Some(s) => argv.push(s),
            None => return Err(crate::err::EFAULT),
        }
    }
    if argv.is_empty() {
        argv.push(path.clone()); // Linux would EFAULT; a path-only spawn is kinder
    }
    let mut env: Vec<String> = Vec::new();
    if envp_va != 0 {
        let env_ptrs = match read_ptr_array(envp_va, ENV_MAX) {
            Some(p) => p,
            None => return Err(crate::err::EFAULT),
        };
        for p in &env_ptrs {
            match read_user_string(*p) {
                Some(s) => env.push(s),
                None => return Err(crate::err::EFAULT),
            }
        }
    }
    Ok(SpawnArgs { path, argv, env })
}

/// Akuma's SPAWN (0x1000+301): `spawn(path, argv, envp, stdin_ptr, stdin_len, flags)`.
///
/// Runs the child to completion inside the call (the model this module
/// exists to state), then returns `pid | stdout_fd << 32` — the same packed
/// word the full kernel's SPAWN returns, read by the same `spawn_full`.
// ---------------------------------------------------------------------------
// M4 slice 3: the real fork — premise 2's build-out
// ---------------------------------------------------------------------------

/// The M2 blocking fork stays selectable until the new path passes every
/// gate: `false` keeps the run-to-completion arm for every fork (this
/// commit's default), `true` routes plain `fork` (57/58) through
/// `fork_process` over the registered shell Process. `clone`/`vfork` keep
/// the blocking arm on both settings — out of scope this slice.
pub const REAL_FORK: bool = true;
/// The task record each ring slot runs against: `slot + 1` for a process, the
/// creator's record for a `CLONE_THREAD` sibling - which is how threads share
/// regions, brk/mmap cursors, the fd table and the cwd with no second copy to
/// keep coherent. SAFETY: syscall-path discipline.
static mut TASK_OF: [usize; NSLOTS] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12];

fn task_of(slot: usize) -> usize {
    // SAFETY: syscall-path discipline.
    unsafe { core::ptr::read_volatile(&raw const TASK_OF[slot]) }
}

/// Is the running task a `CLONE_THREAD` sibling?
pub(crate) fn is_thread_task() -> bool {
    let cur = crate::exec_twin::current_slot();
    cur != crate::exec_twin::NO_SLOT && cur < NSLOTS && slots()[cur].used && slots()[cur].thread
}

/// `fork_process`'s `bind_child_task` records: (tid, pid, root) — the twin's
/// spawn-row. Indexed by tid; `root == 0` = free. SAFETY: syscall-path
/// discipline (single core, IF clear), the rule that owns every static here.
static mut CHILD_BIND: [(usize, u32, u64); NSLOTS] = [(0, 0, 0); NSLOTS];

/// `ExecRuntime::bind_child_task`'s twin: stamp the child's spawn-row. The
/// crate publishes the child's thread right after this returns, so a full
/// ring must come back as an error the fork reports, not a panic.
pub(crate) fn bind_child_task(tid: usize, pid: u32, root_pa: u64, is_thread: bool) -> Result<(), &'static str> {
    // SAFETY: syscall-path discipline.
    let bind = unsafe { &mut *core::ptr::addr_of_mut!(CHILD_BIND) };
    if tid >= bind.len() {
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
    {
        let s = slots();
        s[slot_idx] = SpawnSlot::new();
        s[slot_idx].used = true;
        s[slot_idx].running = true;
        s[slot_idx].pid = pid;
        s[slot_idx].real = true;
        s[slot_idx].thread = is_thread;
    }
    let _ = prepare_task_slot(slot_idx);
    if is_thread {
        // A thread runs against its creator's record: nothing to copy, and
        // nothing to clone-ref (the fd table is the creator's own).
        // SAFETY: syscall-path discipline.
        unsafe { TASK_OF[slot_idx] = crate::task_slot_index() };
    } else {
        // SAFETY: syscall-path discipline.
        unsafe { TASK_OF[slot_idx] = slot_idx + 1 };
    }
    if !is_thread {
        let t = crate::task();
        let regions = t.regions;
        let n_regions = t.n_regions;
        let (brk_base, brk_cur, brk_end, mmap_cur) = (t.brk_base, t.brk_cur, t.brk_end, t.mmap_cur);
        let fds = t.fd_names.clone();
        let cloexec = t.fd_cloexec.clone();
        let nonblock = t.fd_nonblock.clone();
        let cwd = t.cwd.clone();
        let ct = &mut crate::task_slots()[slot_idx + 1];
        ct.cwd = cwd;
        ct.root_pa = root_pa;
        ct.regions = regions;
        ct.n_regions = n_regions;
        ct.brk_base = brk_base;
        ct.brk_cur = brk_cur;
        ct.brk_end = brk_end;
        ct.mmap_cur = mmap_cur;
        ct.exit = None;
        ct.fd_names = fds;
        ct.fd_cloexec = cloexec;
        ct.fd_nonblock = nonblock;
        // Session ph (boot ph-3): the clone above must take the COUNTED pipe
        // references with it — the M2 arm's own rule ("a pipe end is a
        // *counted* kernel resource: every payload the child's table
        // inherited needs one kernel-side reference taken on top of the
        // clone"). Without the walk, ash's first post-fork close of its own
        // copies drove the entry's refcount to zero and DESTROYED the pipe
        // while both children still held cloned fds: echo wrote into
        // `NoSuchPipe` and died by the SIGPIPE stand-in; the pipeline
        // delivered nothing (boot ph-3 trace: pp id=1, then no cln until the
        // children's own dup2s, then wp id=1 out=NoSuchPipe).
        for d in ct.fd_names.values() {
            let (ends, n) = d.pipe_ends();
            for (id, w) in &ends[..n] {
                crate::pipes::clone_ref(*id, *w);
            }
        }
    }
    bind[tid] = (tid, pid, root_pa);
    // The out_slot/depth stamp (session pg): the fork child writes into the
    // SAME session buffer its parent had — the bridge drains the shell's.
    let shell = crate::exec_twin::current_slot();
    if shell != crate::exec_twin::NO_SLOT && shell < NSLOTS && tid < NSLOTS {
        // SAFETY: syscall-path discipline (the fork syscall, IF clear).
        let (out, depth) = {
            let sc = unsafe { cross_mut(shell) };
            (sc.out_slot, sc.depth)
        };
        // SAFETY: as above; `tid` was just bound inside the spawn ring.
        let c = unsafe { cross_mut(tid) };
        c.out_slot = out;
        c.depth = depth + 1;
    }
    Ok(())
}

/// The kernel-stack top `switch_to` installs for `tid`: a real child's
/// leaked pair (slice 2's `prepare_task_slot`), the shell's SPAWN crossing
/// band (recorded at the crossing below), and anything else (the idle
/// thread, out-of-range) — the current global, which for the idle task is
/// simply left alone by the caller.
pub(crate) fn sched_kstack_top(tid: usize) -> u64 {
    if tid < NSLOTS {
        // SAFETY: syscall-path discipline.
        unsafe { core::ptr::read_volatile(&raw const SLOT_STATE[tid].kstack_top) }
    } else {
        ring3::syscall_stack_top()
    }
}

/// The CR3 `switch_to` loads for `tid`: the task record's live root (exec
/// updates it — a real child that exec'd must come back on the NEW image's
/// root). 0 = leave CR3 alone (the idle thread runs on the kernel root).
pub(crate) fn sched_task_slot_of(tid: usize) -> usize {
    if tid == crate::exec_twin::shell_tid() {
        1
    } else if tid < NSLOTS {
        task_of(tid)
    } else {
        tid + 1
    }
}

pub(crate) fn sched_root_of(tid: usize) -> u64 {
    if tid == crate::exec_twin::shell_tid() {
        crate::task_slots()[1].root_pa // the shell
    } else if tid < NSLOTS {
        crate::task_slots()[task_of(tid)].root_pa
    } else {
        0
    }
}

/// A REAL fork child is on the CPU: a threading tid 1..NSLOTS whose
/// SpawnSlot is stamped and running. The shell (tid 0, inside its SPAWN
/// crossing) and the boot task are NOT — their exits keep the M2 paths.
pub(crate) fn is_real_task() -> bool {
    let cur = crate::exec_twin::current_slot();
    if cur == crate::exec_twin::NO_SLOT || cur == 0 || cur >= NSLOTS {
        return false;
    }
    // SAFETY: syscall-path discipline.
    let s = slots();
    // `real` is the whole discriminator, not an optimization: the M2 spawn
    // path stamps the same used/running/current_slot triple for its
    // run-to-completion children (spawn_arm), so without the bind-time bit
    // this test misreads every SPAWN child from the ring's second slot on as
    // a real fork child — boot A wedged the machine exactly there (session
    // pe; the field's doc has the full chain).
    s[cur].used && s[cur].running && s[cur].real
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
pub fn task_die_segv() -> ! {
    task_exit_status(11)
}

fn task_exit_status(status: u32) -> ! {
    let cur = crate::exec_twin::current_slot();
    // The group's leader slot: `cur` itself unless a thread is the one
    // calling `exit_group`, in which case the leader's status and waiter are
    // the process's. Every other thread of the group dies here (Linux's
    // `exit_group`): purged from the futex table and terminated.
    let lead = group_leader(cur);
    kill_siblings(cur, lead);
    let waiter = {
        // SAFETY: syscall-path discipline.
        let s = slots();
        s[lead].status = status;
        s[lead].running = false;
        s[lead].waiter
    };
    crate::pipes::release_open_fds();
    if waiter != u32::MAX {
        akuma_threading::get_waker_for_thread(waiter as usize).wake();
    }
    if lead != cur {
        crate::futex::purge_task(lead);
        akuma_threading::mark_thread_terminated(lead);
    }
    akuma_threading::x86_finish_current()
}

/// The slot that owns the process `cur` belongs to: the one non-thread slot
/// running against the same task record. `cur` when it is not a thread.
fn group_leader(cur: usize) -> usize {
    if cur >= NSLOTS || !slots()[cur].thread {
        return cur;
    }
    let rec = task_of(cur);
    (0..NSLOTS)
        .find(|&s| s != cur && slots()[s].used && !slots()[s].thread && task_of(s) == rec)
        .unwrap_or(cur)
}

/// `exit_group`: end every thread of `cur`'s group except `cur` and `lead`
/// (the caller finishes those itself).
fn kill_siblings(cur: usize, lead: usize) {
    let rec = task_of(cur);
    for s in 0..NSLOTS {
        if s != cur && s != lead && slots()[s].used && slots()[s].thread && task_of(s) == rec {
            crate::futex::purge_task(s);
            slots()[s].running = false;
            slots()[s].used = false;
            slots()[s].thread = false;
            akuma_threading::mark_thread_terminated(s);
        }
    }
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
    let s = slots();
    s[cur].running = false;
    s[cur].used = false;
    s[cur].thread = false;
    s[cur].real = false;
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
    match akuma_exec::process::clone_thread(stack, tls, ptid, ctid, flags) {
        Ok(tid) => tid as u64,
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
static mut SLOT_FS: [u64; NSLOTS] = [0; NSLOTS];

/// The slot's fs_base (what `switch_to` installs). A slot >= NSLOTS is the
/// idle thread: kernel-only, no TLS, and the answer 0 is never installed.
pub(crate) fn slot_fs_of(slot: usize) -> u64 {
    if slot < NSLOTS {
        // SAFETY: single core, IF-clear callers (see SLOT_FS).
        unsafe { SLOT_FS[slot] }
    } else {
        0
    }
}

/// Stamp the slot's fs_base (crossings and the `arch_prctl` SET_FS arm).
pub(crate) fn set_slot_fs(slot: usize, v: u64) {
    if slot < NSLOTS {
        // SAFETY: single core, IF-clear callers (see SLOT_FS).
        unsafe { SLOT_FS[slot] = v; }
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
static mut PARENT_OF: [(u32, u32); 32] = [(0, 0); 32];

pub(crate) fn stamp_child_parent(child: u32, parent: u32) {
    // SAFETY: single core, IF-clear syscall-path caller (fork arm).
    let p = unsafe { &mut *core::ptr::addr_of_mut!(PARENT_OF) };
    p[child as usize % p.len()] = (child, parent);
}

/// 0 = unknown (an M2-spawned child, or the map's ring moved past it).
pub(crate) fn parent_of(child: u32) -> u32 {
    // SAFETY: single core, IF-clear syscall-path caller (wait4 arms).
    let p = unsafe { &*core::ptr::addr_of_mut!(PARENT_OF) };
    let mut i = 0;
    while i < p.len() {
        let (c, par) = p[i];
        if c == child && child != 0 {
            return par;
        }
        i += 1;
    }
    0
}

pub fn exec_real_task(fr: &ring3::SyscallFrame) -> u64 {
    let cur = crate::exec_twin::current_slot();
    let args = match read_spawn_args(fr.arg1(), fr.arg2(), fr.arg3()) {
        Ok(a) => a,
        Err(no) => return crate::e(no),
    };
    let effective = crate::resolve_effective(&args.path);
    let Ok(image) = crate::root_fs().read_file(&effective) else {
        akuma_primitives::safe_print!(192, "[exec] '{}': read failed\n", effective);
        return crate::e(crate::err::ENOENT);
    };
    if image.len() < 4 || image[..4] != [0x7f, b'E', b'L', b'F'] {
        return crate::e(crate::err::ENOEXEC);
    }
    let argv: Vec<&str> = args.argv.iter().map(String::as_str).collect();
    let env: Vec<&str> = args.env.iter().map(String::as_str).collect();
    // POSIX exec keeps open fds across the image swap — snapshot, load,
    // restore, sweep cloexec (the exec_replace discipline).
    let saved = {
        let t = crate::task();
        (t.fd_names.clone(), t.fd_cloexec.clone(), t.fd_nonblock.clone())
    };
    let Some(ti) = crate::load_task(&argv, &env, &image) else {
        return crate::e(crate::err::ENOMEM);
    };
    {
        let t = crate::task();
        t.fd_names = saved.0;
        t.fd_cloexec = saved.1;
        t.fd_nonblock = saved.2;
        let mut table = crate::fd_table();
        for (_fd, payload) in table.sweep_cloexec() {
            if let crate::OpenFile::Socket(idx) = payload {
                akuma_net::socket::remove_socket(idx);
            } else {
                let (ends, n) = payload.pipe_ends();
                for (id, w) in &ends[..n] {
                    crate::pipes::release(*id, *w);
                }
            }
        }
    }
    // The registered Process's shared root view goes stale with the old
    // image — re-point it, as `exec_replace` does for the shell.
    {
        let pid = slots()[cur].pid;
        if let Some(view) = akuma_mmu::UserAddressSpace::new_shared(ti.root_pa as usize) {
            let _ = akuma_exec::process::table::with_process(pid, |p| {
                p.address_space = akuma_exec::process::ProcAddressSpace::new(view);
            });
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
    // SAFETY: the crossing contract — `root` maps the new image plus the
    // shared kernel half, `ef` is ring-3-enterable. A real child never runs
    // `exit_cross`, so this crossing has no return side; the dead dispatcher
    // frame on the task's own band is abandoned, bounded by exec counts.
    let _ = unsafe { child_enter(ti.root_pa, &ef, core::ptr::addr_of_mut!(EXEC_SAVE)) };
    unreachable!("exec_real_task: a real child's crossing returned")
}

/// The syscall-path capture `fork_process` step 6 consumes:
/// `get_saved_user_context(parent_tid)` resolves through the threading crate
/// to `X86ArchHooks::read_user_context` — this twin's per-slot record
/// (slice 2), written here. One plain copy per syscall on a slotted task;
/// the boot task (NO_SLOT) skips.
pub(crate) fn capture_user_context(fr: &ring3::SyscallFrame) {
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
fn fork_arm_real(_fr: &ring3::SyscallFrame) -> u64 {
    // Fork lives only inside the spawned shell, the blocking arm's own rule:
    // the boot task has no registered Process, and a fork inside a fork
    // child has no stack region left.
    let cur = crate::exec_twin::current_slot();
    let shell_depth = match cur {
        crate::exec_twin::NO_SLOT => 0,
        // SAFETY: syscall-path discipline.
        _ => unsafe { cross_mut(cur).depth },
    };
    // Session pg: depth 2 (a REAL fork child forking again — grandfork's
    // rungs 2-5, the `( ls; ls )` class this slice exists to fix) forks for
    // real now that the child is its own thread with its own band and the
    // bind hook stamps its depth; deeper generations stay refused. Session
    // ph flattens the ceiling to 3: grandfork's rungs 3-5 fork at depth 3
    // (the grandchild generation), and under the REAL path a fork child owns
    // a threading stack and a syscall band of its own, so the M2 "no stack
    // region left" ceiling no longer binds.
    if shell_depth != 1 && shell_depth != 2 && shell_depth != 3 {
        return crate::e(crate::err::EPERM);
    }
    let child_pid = akuma_exec::process::allocate_pid();
    // stack_ptr = 0: the child resumes at the parent's saved user RSP — the
    // syscall stub stashed it (`user_rsp_stash`) and step 6's captured
    // context carries it into the child's UserContext.
    match akuma_exec::process::fork_process(child_pid, 0) {
        Ok(pid) => {
            // Session pj wiring of pi's PARENT_OF: the child's parent is THIS
            // task's pid — the slot ROW's pid, never the slot index (slots are
            // reused across successive children — pi4's console shows tid=10/11
            // alternating for pids 29..37; pids are monotonic). The wait4
            // reapers consult the map for the crate's is_child_of_group
            // decision: wait* on a non-child is ECHILD and never blocks.
            stamp_child_parent(pid, slots()[cur].pid);
            u64::from(pid)
        }
        // The refusal line is the evidence this slice hunts for; name the
        // message verbatim (the same discipline as spawn's read-failed
        // print).
        Err(msg) => {
            akuma_primitives::safe_print!(192, "[forkp] refused: {}\n", msg);
            crate::e(ring3::err::EAGAIN)
        }
    }
}

pub fn spawn_arm(fr: &ring3::SyscallFrame) -> u64 {
    if nested_active() {
        return crate::e(crate::err::EPERM); // one level only; nothing nests a nest
    }
    let args = match read_spawn_args(fr.arg1(), fr.arg2(), fr.arg3()) {
        Ok(a) => a,
        Err(no) => return crate::e(no),
    };
    // `stdin` (sshd passes None for exec) and the pty/console flag bits are
    // accepted and ignored: there is no input path and no pty in this kernel.
    let path: &str = &args.path;
    let argv: Vec<&str> = args.argv.iter().map(String::as_str).collect();
    let env: Vec<&str> = args.env.iter().map(String::as_str).collect();

    // A free slot and a pid, before anything expensive. Session ph: the ring
    // number is RESERVED in the threading pool FIRST (the fork children's
    // tids come from the same pool — an unreserved number is handed to a
    // fork child, which then binds this shell's cross record/SpawnSlot/fd
    // table; boots ph-2/ph-4). A refused number (neither FREE nor TERMINATED
    // — i.e. already reserved by an earlier crossing) is skipped: the next
    // index is tried, NSLOTS tries in total, never the blind-next wrap onto
    // a live ring (the M2 lesson, boot 3 gate P3).
    let slot_idx = {
        // SAFETY: syscall-path discipline.
        let s = slots();
        let mut idx = unsafe { core::ptr::read_volatile(&raw const NEXT_SLOT) };
        let mut idx = idx % NSLOTS;
        let mut tries = 0;
        loop {
            if akuma_threading::reserve_slot(idx) {
                break;
            }
            idx = (idx + 1) % NSLOTS;
            tries += 1;
            if tries >= NSLOTS {
                akuma_primitives::safe_print!(160, "  [spawn] every ring slot already reserved\n");
                for i in 0..NSLOTS {
                    akuma_primitives::safe_print!(
                        96,
                        "    ring slot {}: pool state {} used={} thread={}\n",
                        i,
                        akuma_threading::get_thread_state(i),
                        slots()[i].used,
                        slots()[i].thread
                    );
                }
                return crate::e(crate::err::EAGAIN);
            }
        }
        s[idx] = SpawnSlot::new();
        s[idx].used = true;
        let pid = akuma_exec::process::allocate_pid();
        s[idx].pid = pid;
        s[idx].running = true;
        unsafe {
            *core::ptr::addr_of_mut!(NEXT_SLOT) = (idx + 1) % NSLOTS;
        }
        idx
    };

    // Load the image while the PARENT's CR3 is still active: everything the
    // load touches (ext2 root, PMM, page tables) lives in the kernel half,
    // and the copy loop writes the child's frames through the physmap.
    // `/bin/sh` (what the exec path spawns) is a symlink on the serve
    // image; the chain walker resolves it the same way `open` does.
    let effective = crate::resolve_effective(path);
    let Ok(image) = crate::root_fs().read_file(&effective) else {
        // Say what failed. A silent ENOENT here reads on the client exactly
        // like a missing binary, and cost three stages of diagnosis.
        akuma_primitives::safe_print!(
            192,
            "[spawn] '{}' -> '{}': read failed\n",
            path,
            effective
        );
        slots()[slot_idx].used = false;
        return crate::e(crate::err::ENOENT);
    };
    if image.len() < 4 || image[..4] != [0x7f, b'E', b'L', b'F'] {
        akuma_primitives::safe_print!(160, "[spawn] '{}': not an ELF image\n", effective);
        slots()[slot_idx].used = false;
        return crate::e(crate::err::ENOENT);
    }

    // The child owns task slot 1 while it runs.
    crate::set_task_slot(1);
    let ti = crate::load_task(&argv, &env, &image);
    let Some(ti) = ti else {
        akuma_primitives::safe_print!(160, "[spawn] '{}': load failed\n", effective);
        crate::set_task_slot(0);
        slots()[slot_idx].used = false;
        return crate::e(crate::err::ENOMEM);
    };
    // A fresh session starts at `/`; the slot's Task outlives the last shell.
    crate::task().cwd.clear();
    // Premise 2's build-out: register the shell as an akuma-exec Process so
    // `fork_process` finds a caller. tid = this ring slot — the same number
    // `current_slot()` answers once the crossing below names the child.
    register_shell_process(slots()[slot_idx].pid, path, ti.root_pa, ti.entry, ti.rsp, slot_idx);
    // The sched_* maps key on this: the shell's threading tid is its spawn
    // slot, and which slot that is varies across a boot's login spawns.
    crate::exec_twin::set_shell_tid(slot_idx);
    // Timer preemption arms here (session pf): the first SPAWN crossing is
    // the first moment a ring-3 tick has a legitimate switch target.
    crate::PREEMPT_ARMED.store(true, core::sync::atomic::Ordering::Release);

    // Snapshot the parent and point the crossing machinery at the child —
    // into the child's own per-slot record now (slice 2): the parent's CR3,
    // stack top and stash, the depth/task/out answers, and `current_slot`
    // naming the child from here until its `child_exit`.
    // SAFETY: syscall-path discipline for the table; `child_enter`'s
    // contract for the crossing.
    let (_, save) = unsafe {
        let c = cross_mut(slot_idx);
        c.parent_root = crate::task_slots()[0].root_pa;
        c.parent_stack_top = ring3::syscall_stack_top();
        c.parent_stash = ring3::user_rsp_stash();
        c.depth = 1;
        c.task_slot = 1;
        c.parent_task_slot = 0;
        c.parent_slot = crate::exec_twin::NO_SLOT;
        c.out_slot = slot_idx;
        crate::exec_twin::set_current_slot(slot_idx);
        // The nested stack: the lower half of the syscall stack. The live
        // dispatch frames sit in the upper half — the SPAWN arm's depth from
        // the top is a few hundred bytes, and the stub's per-syscall frame is
        // 128 — so the two halves cannot meet.
        let half = (ring3::SYSCALL_STACK_SIZE / 2) as u64;
        c.kstack_top = c.parent_stack_top - half;
        ring3::set_syscall_stack_top(c.kstack_top);
        (c.parent_stack_top, save_ptr(slot_idx))
    };

    let ef = ring3::EntryFrame {
        r15: 0, r14: 0, r13: 0, r12: 0, rbp: 0, rbx: 0, r10: 0, r8: 0, r9: 0,
        rdx: 0, rsi: 0, rdi: 0, rax: 0,
        rip: ti.entry,
        rflags: 0x202,
        rsp: ti.rsp,
    };
    // Session pi: the spawn crossing hands the fresh image a deterministic
    // fs (0 — what the exec crossings give, what amd64 hands its images) and
    // stamps the shadow so the child's first park-resume re-installs 0 until
    // musl's own arch_prctl names its TLS. The MSR here held WHATEVER ran
    // last on the core — for three stages that leftover happened to be the
    // previous shell's TLS, which busybox's own arch_prctl always replaced
    // before its first fs-relative read.
    let _ = akuma_cpu::msr::set_fs_base(0);
    set_slot_fs(slot_idx, 0);
    // The crossing runs the child ON this slot's number, so the pool must say
    // so: `reserve_slot` leaves it INITIALIZING, which the picker never
    // takes, and a tick that preempts the child then switches away from a
    // slot it can never pick again - the spawn parks after `[SSH] Exec:`
    // forever (supervisor, 2026-10-30; QEMU TCG, deterministic with the
    // switch on, absent with it off).
    akuma_threading::set_thread_state(slot_idx, 2);
    let code = unsafe { child_enter(ti.root_pa, &ef, save) };
    akuma_threading::set_thread_state(slot_idx, 3);
    // `code` (the child's exit status) was recorded into the slot by the
    // exit arm before it returned here; SPAWN's own result is the packed
    // pid/fd word below.
    let _ = code;

    // Back in the parent, in every sense: registers and stack top restored
    // by the exit arm, CR3 back, `current_slot` back to the boot task (the
    // exit tail's `set_current_slot(parent_slot)`). The stack top and stash
    // come from the child's own record — the same values the snapshot put
    // there, read back through the slot index this frame owns.
    // SAFETY: syscall-path discipline.
    unsafe {
        let c = cross_mut(slot_idx);
        ring3::set_syscall_stack_top(c.parent_stack_top);
        ring3::set_user_rsp_stash(c.parent_stash);
    }
    let pid = slots()[slot_idx].pid;

    // The ChildOut fd the parent drains. Spawned children do not inherit fd
    // tables (nothing here forks) — the fd is made fresh in the parent's own
    // table, which is the current slot again.
    let mut table = crate::fd_table();
    let Some(_probe) = table.free_below(3, crate::SLOT_CEILING) else {
        // No fd slot: the output is dropped, but the child DID run and the
        // pid is real — Linux would have handed out the fd or failed the
        // spawn. Failing the spawn now would lie about the child; report it
        // with fd 0 (never a valid fd) and let the bridge log the miss.
        return u64::from(pid);
    };
    let fd = table.alloc_fd_from(3, crate::OpenFile::ChildOut(slot_idx));
    (u64::from(pid)) | ((u64::from(fd)) << 32)
}

// ---------------------------------------------------------------------------
// Exec-replace: the nested child's execve.
// ---------------------------------------------------------------------------

/// The first-level (SPAWN) saved state, snapshotted across an exec-replace:
/// the second [`child_enter`] overwrites the slot's save record with this
/// (nested) call's registers, and [`spawn_arm`]'s resume needs the originals.
/// SPAWN-level crossing state saved by `exec_replace` — one PER FRAME now,
/// not the old single statics: test A nests two exec_replace levels (outer
/// sh -> inner sh -> cat) plus a fork, and each level's snapshot overwrote
/// the one shared set, so the outer `exec_return` restored a dead image's
/// user registers into `spawn_arm`'s live Rust frame (the 16 KiB #GP shape,
/// then the 32 KiB silent spin). A frame local lives above the deeper
/// dispatch bands (deeper runs write strictly below it), so it is stable
/// until its own `exec_return` consumes it. (Slice 2: the snapshot's SOURCE
/// is the slot's per-slot `CrossSave` — the record `exec_replace`'s own
/// crossing will overwrite; the fork arm no longer needs this dance at all,
/// because its child has a record of its own.)
type ExecSave = CrossSave;

/// The nested child's `execve` — the handoff of the run-to-completion model.
///
/// busybox ash runs `sh -c '<one command>'` by execing the command in place
/// (the trailing-simple-command optimization skips the fork), and every
/// program line the stage-6 gates run reaches the kernel this way. There is
/// no fork to return to and no second runnable task: so exec REPLACES the
/// running child's image — same slot, fresh root, fresh fd table — and when
/// the exec'd image calls `exit_group`, [`child_exit`] returns here with its
/// status, this frame restores the SPAWN-level state, and control flows back
/// into [`spawn_arm`] exactly as if its own child had run to completion. The
/// exec'd image's stdout lands in the same pipe; its status is the child's.
///
/// A failure BEFORE the crossing is an ordinary `execve` error the caller
/// (ash) sees and reports; after the crossing there is no returning.
pub fn exec_replace(fr: &ring3::SyscallFrame) -> u64 {
    // Snapshot THIS slot's crossing record — the values the sshd→shell
    // crossing left, which the second `child_enter` below will overwrite
    // (same slot: an exec replaces the image in place, slice 2 or not).
    // SAFETY: syscall-path discipline (single core, IF clear).
    let save = unsafe { *save_ptr(crate::exec_twin::current_slot()) };
    let args = match read_spawn_args(fr.arg1(), fr.arg2(), fr.arg3()) {
        Ok(a) => a,
        Err(no) => return crate::e(no),
    };
    // PB5 evidence (session pb): the tick count before the new image's first
    // ring-3 instruction; "[TICK post]" at child_exit brackets the run.
    akuma_primitives::safe_print!(96, "[TICK pre] n={}\n", akuma_lapic_amd64::ticks());
    let effective = crate::resolve_effective(&args.path);
    let Ok(image) = crate::root_fs().read_file(&effective) else {
        akuma_primitives::safe_print!(
            192,
            "[exec] '{}' -> '{}': read failed\n",
            args.path,
            effective
        );
        return crate::e(crate::err::ENOENT);
    };
    if image.len() < 4 || image[..4] != [0x7f, b'E', b'L', b'F'] {
        akuma_primitives::safe_print!(160, "[exec] '{}': not an ELF image\n", effective);
        return crate::e(crate::err::ENOEXEC);
    }
    let argv: Vec<&str> = args.argv.iter().map(String::as_str).collect();
    let env: Vec<&str> = args.env.iter().map(String::as_str).collect();
    // POSIX exec keeps open fds across the image swap (cloexec swept below).
    // The tail-call exec IS how a non-builtin command runs under its shell's
    // redirects — `sh -c 'sha256sum x > f'` opens f, dup2's it onto fd 1,
    // then execs: with the fresh table `load_task` starts, the binding dies
    // here and the exec'd program's stdout rides the session pipe instead of
    // the file (the empty-file-with-rc-0 symptom one boot after the write
    // side was fixed). Snapshot before the load, restore after.
    let saved = {
        let t = crate::task();
        (t.fd_names.clone(), t.fd_cloexec.clone(), t.fd_nonblock.clone())
    };
    // The new image replaces the task: same slot, fresh root (the load runs
    // on the CURRENT child's CR3, all its writes through the physmap — the
    // same story as the SPAWN arm's load on the parent's CR3).
    let Some(ti) = crate::load_task(&argv, &env, &image) else {
        akuma_primitives::safe_print!(160, "[exec] '{}': load failed\n", effective);
        return crate::e(crate::err::ENOMEM);
    };
    // Restore the saved fds and sweep the cloexec ones — `sweep_cloexec`
    // returns the closed payloads; a swept socket releases its kernel side
    // exactly as close does. `load_task` had reset all three collections.
    {
        let t = crate::task();
        t.fd_names = saved.0;
        t.fd_cloexec = saved.1;
        t.fd_nonblock = saved.2;
        let mut table = crate::fd_table();
        for (_fd, payload) in table.sweep_cloexec() {
            if let crate::OpenFile::Socket(idx) = payload {
                akuma_net::socket::remove_socket(idx);
            } else {
                // A swept pipe end drops its table reference, exactly as
                // close does.
                let (ends, n) = payload.pipe_ends();
                for (id, w) in &ends[..n] {
                    crate::pipes::release(*id, *w);
                }
            }
        }
    }
    // The shell's registered Process named the PRE-exec root in its shared
    // view; `fork_process` would clone from the dead image. Re-point the
    // view at the fresh root — the same move amd64's execve makes
    // (`replace_image` swaps the address space under the same lock).
    {
        let cur = crate::exec_twin::current_slot();
        if cur != crate::exec_twin::NO_SLOT {
            let pid = slots()[cur].pid;
            if let Some(view) = akuma_mmu::UserAddressSpace::new_shared(ti.root_pa as usize) {
                let _ = akuma_exec::process::table::with_process(pid, |p| {
                    p.address_space = akuma_exec::process::ProcAddressSpace::new(view);
                });
            }
        }
    }
    let ef = ring3::EntryFrame {
        r15: 0, r14: 0, r13: 0, r12: 0, rbp: 0, rbx: 0, r10: 0, r8: 0, r9: 0,
        rdx: 0, rsi: 0, rdi: 0, rax: 0,
        rip: ti.entry,
        rflags: 0x202,
        rsp: ti.rsp,
    };
    // Lower the nested dispatch top BELOW this arm's live frame. Every child
    // syscall re-enters at `set_syscall_stack_top`'s current value — the same
    // region this frame occupies (the execve dispatch itself started there) —
    // so the exec'd child's first deep syscall would rewrite this frame,
    // `RET_RSP` included, and the exit `ret` would jump to whatever it found.
    // `spawn_arm`'s tail restores the real top after `child_enter` returns;
    // failure paths above never lowered it.
    let rsp_here: u64;
    // SAFETY: reads the current stack pointer, nothing else.
    unsafe {
        core::arch::asm!("mov {}, rsp", out(reg) rsp_here, options(nomem));
    }
    // SAFETY: syscall-path discipline; `set_syscall_stack_top` is a plain
    // atomic store the unsafe context of the arm already covers.
    ring3::set_syscall_stack_top(rsp_here - 0x1000);
    // The same stale-fs rule as exec_real_task's crossing: a fresh image
    // starts with fs = 0, never the outgoing task's TLS pointer.
    let _ = akuma_cpu::msr::set_fs_base(0);
    set_slot_fs(crate::exec_twin::current_slot(), 0);
    let status = unsafe { child_enter(ti.root_pa, &ef, core::ptr::addr_of_mut!(EXEC_SAVE)) };
    // The exec'd child exited: `child_exit` recorded its status in the slot,
    // cleared the nesting, put task slot 0 back, and restored THIS frame's
    // registers and CR3 (the parent's). Hand control to `spawn_arm`'s
    // continuation with the SPAWN-level registers, stack, and the exec'd
    // child's status in EAX.
    // SAFETY: the snapshot was taken at this arm's entry and is still live;
    // `ret` pops the return address `spawn_arm`'s `child_enter` call pushed.
    unsafe { exec_return(&save, (status & 0xFF) as u32) }
}

/// Restore the SPAWN-level saved state and `ret` into [`spawn_arm`].
///
/// # Safety
/// Only valid at the tail of [`exec_replace`], after the second
/// [`child_enter`] has returned here; the snapshot statics must be live and
/// CR3 must already be the parent's (set by [`child_exit`]).
#[cfg(target_arch = "x86_64")]
unsafe fn exec_return(save: &ExecSave, status: u32) -> ! {
    unsafe {
        // Explicit register bindings for every callee-saved restore, for the
        // reason `child_exit` spells out: an `in(reg)` input may be allocated
        // to a register the template itself overwrites. Here the loads read
        // the snapshot through `rdi` (bound explicitly), the callee-saveds
        // are PLACED as the restore, and `rax` carries the status the `ret`
        // hands back to `spawn_arm`.
        core::arch::asm!(
            "mov rbx, [rdi + 0x08]",
            "mov rbp, [rdi + 0x10]",
            "mov r12, [rdi + 0x18]",
            "mov r13, [rdi + 0x20]",
            "mov r14, [rdi + 0x28]",
            "mov r15, [rdi + 0x30]",
            "mov rsp, [rdi + 0x00]",
            "ret",
            in("rdi") save,
            in("rax") u64::from(status),
            options(noreturn),
        )
    }
}

/// Cross into the child and come back only when it exits.
///
/// Naked because the whole body is the contract: save the parent-phase
/// callee-saved registers and stack pointer into the **caller-chosen
/// per-slot record** (`save`, in `%rdx` — slice 2's replacement for the
/// crossing statics the old `sym` operands named), load `CR3`, then the
/// `enter_user` crossing. The exit arm reverses this exactly — restores the
/// registers from the same record, moves `rsp` back to the saved value, and
/// `ret`s, so this function *returns* the child's exit code into its
/// caller (`spawn_arm`, `fork_arm`, `exec_replace`).
///
/// The stores must precede every clobber of `%rdx`: the crossing loads below
/// spend it (`mov rdx, [rsi + 0x48]`) along with the frame pointer.
///
/// # Safety
/// `root` must be the child's page root (mapping the kernel half and the
/// child's image), `ef` a ring-3-enterable state in it, and `save` the
/// per-slot record the exit side will read (`save_ptr(slot)`).
#[cfg(target_arch = "x86_64")]
#[unsafe(naked)]
pub unsafe extern "C" fn child_enter(root: u64, ef: &ring3::EntryFrame, save: *mut CrossSave) -> u64 {
    core::arch::naked_asm!(
        // Save the caller's (parent-phase) state into the slot record first.
        "mov qword ptr [rdx + 0x00], rsp",
        "mov qword ptr [rdx + 0x08], rbx",
        "mov qword ptr [rdx + 0x10], rbp",
        "mov qword ptr [rdx + 0x18], r12",
        "mov qword ptr [rdx + 0x20], r13",
        "mov qword ptr [rdx + 0x28], r14",
        "mov qword ptr [rdx + 0x30], r15",
        // The child's address space: kernel half survives (shared PML4).
        "mov cr3, rdi",
        // The enter_user crossing, verbatim (`akuma-ring3-amd64`): every load
        // is spent against the frame pointer RSI, and the frame's own RSI is
        // restored only after the pointer's last use — loading it early made
        // every later read dereference the child's (null) RSI instead.
        "mov rax, [rsi + 0x60]",
        "mov rdx, [rsi + 0x48]",
        "mov rbp, [rsi + 0x20]",
        "mov r8,  [rsi + 0x38]",
        "mov r9,  [rsi + 0x40]",
        "mov rbx, [rsi + 0x28]",
        "mov r12, [rsi + 0x18]",
        "mov r13, [rsi + 0x10]",
        "mov r14, [rsi + 0x08]",
        "mov r15, [rsi + 0x00]",
        "mov rcx, [rsi + 0x68]",
        "mov r11, [rsi + 0x70]",
        "mov r10, [rsi + 0x30]",
        "mov rsp, [rsi + 0x78]",
        "mov rdi, [rsi + 0x58]",
        "mov rsi, [rsi + 0x50]", // last: spends the frame pointer
        "sysretq",
    )
}

/// The exec crossing's scratch save (session pg): `child_enter` stores the
/// caller's dead frame somewhere, and the M2 exec arm used to pass
/// `save_ptr(cur)` — the SPAWN crossing's saved sshd frame. An inner exec
/// (`sh -c "sh -c …"`, ash exec'ing the last command in place — every gate
/// command) then lost sshd's resume frame: the image's `child_exit` ran
/// `exit_cross` into the exec's dead save and sshd never resumed — no drain,
/// no `[SSH] Exit status`, a hung session. The exec's save is write-only by
/// construction (execve never returns to the exec'ing image), so ONE global
/// scratch is enough: single core, IF clear, and two exec crossings can never
/// be in flight at once.
static mut EXEC_SAVE: CrossSave = CrossSave::new();

/// Runs on the nested stack region, in the CHILD's address space, called
/// from the dispatcher's exit arm — which is why this is `!` from Rust's
/// point of view: it never returns *to the dispatcher*. It returns to
/// [`spawn_arm`], via the saved rsp and the return address
/// [`child_enter`]'s caller left.
pub fn child_exit(code: u64) -> ! {
    // PB5 evidence (session pb): the tick count after the child's last ring-3
    // stretch. The matching "[TICK pre]" line is at the exec crossing below.
    akuma_primitives::safe_print!(96, "[TICK post] n={}\n", akuma_lapic_amd64::ticks());
    // M3: the exiting task's pipe ends die with it — the fork child's exit
    // is what turns the last writer's close into the reader's EOF. Index
    // payloads hold no kernel reference (the close arms' convention); only
    // the counted pipe ends need the walk.
    crate::pipes::release_open_fds();
    // "Who am I" is `current_slot()` now (slice 2): the running child's own
    // slot names its record, its status row, and the crossing back.
    let cur = crate::exec_twin::current_slot();
    // Record first, while the Rust frame can still reach the tables.
    // SAFETY: syscall-path discipline. Depth-aware (M2): depth 1 is the
    // spawned shell handing the machine back to sshd; depth 2 is a fork
    // child handing it back to the shell — the record knows which.
    let (parent_root, status, parent_task_slot, parent_slot) = unsafe {
        let c = cross_mut(cur);
        let s = slots();
        s[cur].running = false;
        s[cur].status = ((code & 0xFF) << 8) as u32;
        let t = &mut *core::ptr::addr_of_mut!(crate::TASKS);
        t[c.task_slot].exit = Some(code);
        (c.parent_root, s[cur].status, c.parent_task_slot, c.parent_slot)
    };
    crate::set_task_slot(parent_task_slot);
    // The parent is current again: for a depth-1 exit that is the boot task
    // (`NO_SLOT`), for a depth-2 exit the shell's slot — what the old
    // decrement-`NESTED_DEPTH`-and-restore-`NESTED_SLOT` dance computed.
    crate::exec_twin::set_current_slot(parent_slot);
    // SAFETY: the record is live (this slot's own), the status is recorded,
    // and the tail never returns to this frame.
    unsafe { exit_cross(save_ptr(cur), parent_root, status) }
}

/// The exit tail's crossing back, naked because its whole body is the
/// contract: restore the parent's callee-saved registers and stack pointer
/// from the slot record, load the parent's CR3, put the status in `eax`, and
/// `ret` into the resuming arm. Arguments arrive in `rdi`/`rsi`/`rdx` per
/// the C ABI (`save`, `root`, `status`); everything the body writes is an
/// argument register or the register being restored — there is **no operand
/// allocation to get wrong**, which is the whole point of the naked form:
/// boot 2 caught the inline-asm version of this tail letting LLVM allocate
/// the `r13` input to the physical `rbx` the template's own first `mov`
/// destroyed, so the resumed frame came back with a user address in `r13`.
/// (`rbx`/`rbp` cannot be explicit inline-asm operands at all — LLVM
/// reserves them — which closes the inline form off entirely.)
///
/// # Safety
/// `save` must be this slot's own crossing record, `root` the parent's CR3,
/// `status` the packed wait status; never returns.
#[cfg(target_arch = "x86_64")]
#[unsafe(naked)]
unsafe extern "C" fn exit_cross(save: *mut CrossSave, root: u64, status: u32) -> ! {
    core::arch::naked_asm!(
        "mov rbp, [rdi + 0x10]",
        "mov r12, [rdi + 0x18]",
        "mov r13, [rdi + 0x20]",
        "mov r14, [rdi + 0x28]",
        "mov r15, [rdi + 0x30]",
        "mov rbx, [rdi + 0x08]",
        // CR3 before the stack switch: both spaces map the kernel half, so
        // the record reads above and the rsp below are safe in either.
        "mov cr3, rsi",
        "mov rsp, [rdi + 0x00]",
        "mov eax, edx",
        "ret",
    )
}

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
    if REAL_FORK && fr.rax != ring3::nr::CLONE {
        // The new path: plain fork (57/58) through `fork_process` over the
        // registered shell Process. `clone`/`vfork` keep the M2 arm — out of
        // scope this slice.
        return fork_arm_real(fr);
    }
    fork_arm_blocking(fr)
}

/// The M2 blocking arm, unchanged and selectable via [`REAL_FORK`].
fn fork_arm_blocking(fr: &ring3::SyscallFrame) -> u64 {
    // Fork lives only inside the spawned shell: nothing forks the sshd task,
    // and a fork inside a fork child has no stack region left (see above).
    // Per-slot (slice 2): the current slot's record carries the depth — the
    // boot task's `NO_SLOT` and a fork child's depth 2 both refuse.
    let cur = crate::exec_twin::current_slot();
    let shell_depth = match cur {
        crate::exec_twin::NO_SLOT => 0,
        // SAFETY: syscall-path discipline.
        _ => unsafe { cross_mut(cur).depth },
    };
    if shell_depth != 1 {
        return crate::e(crate::err::EPERM);
    }
    const CLONE_VM: u64 = 0x0000_0100;
    const CLONE_VFORK: u64 = 0x0000_4000;
    const SIGCHLD: u64 = 17;
    if fr.rax == ring3::nr::CLONE {
        let flags = fr.arg1();
        if flags & SIGCHLD == 0 || flags & !(CLONE_VM | CLONE_VFORK | SIGCHLD) != 0 {
            // Name the flags: each boot reads as a checklist of what the
            // caller asked for, not just that it was refused.
            akuma_primitives::safe_print!(
                160,
                "  [fork] clone flags=0x{:x} refused (not SIGCHLD[|VM|VFORK])\n",
                flags
            );
            return crate::e(ring3::err::EINVAL);
        }
    }

    // A free slot and a pid, exactly as SPAWN takes them — except the slot:
    // a fork child reuses a DEAD slot, never the spawn ring's blind next.
    // With NSLOTS = 4, a four-fork pipeline wraps the ring onto the LIVE
    // shell's slot and `SpawnSlot::new()` wipes the session buffer out of
    // it (boot 3, gate P3: the second `wc -c`'s `2\n` vanished and the
    // bridge reported the fork child's pid for the shell's slot). The
    // blocking-fork model keeps at most the shell plus ONE child alive at
    // any instant, so a dead slot always exists; NEXT_SLOT stays the spawn
    // arm's own ring and is not advanced here.
    let slot_idx = {
        // SAFETY: syscall-path discipline.
        let s = slots();
        let mut pick: Option<usize> = None;
        for i in 0..NSLOTS {
            if !s[i].running {
                pick = Some(i);
                break;
            }
        }
        let Some(idx) = pick else {
            return crate::e(ring3::err::EAGAIN);
        };
        s[idx] = SpawnSlot::new();
        s[idx].used = true;
        let pid = akuma_exec::process::allocate_pid();
        s[idx].pid = pid;
        s[idx].running = true;
        idx
    };
    let ash_root = crate::task().root_pa;

    // The child's address space: an eager copy of every mapped user page of
    // the shell's, each PTE keeping its own flags (RX text stays RX). Built
    // while the parent's CR3 is still active — the walk reads the shell's
    // tables and the copies go through the physmap, all kernel half.
    let Some(root) = crate::fork_share_memory(ash_root) else {
        slots()[slot_idx].used = false;
        return crate::e(crate::err::ENOMEM);
    };

    // The child's fd table is `akuma-fdtable`'s own fork decision — not a
    // hand copy. No kernel-side references to take on top of the cloned
    // payloads: this kernel's descriptions are indices (`File { inode }`,
    // `Socket(idx)`), the convention every close arm already runs on.
    let forked = crate::fd_table().fork_copy(|_| true);
    // A pipe end is a *counted* kernel resource: every payload the child's
    // table inherited needs one kernel-side reference taken on top of the
    // clone (`fork_copy`'s own doc — "iterate the values"). The index
    // payloads the old comment excused (File{inode}, Socket(idx)) stay free.
    for d in forked.names.values() {
        let (ends, n) = d.pipe_ends();
        for (id, w) in &ends[..n] {
            crate::pipes::clone_ref(*id, *w);
        }
    }

    // Task bookkeeping: the fork child owns slot 2 while it runs (1 stays
    // the shell's). Regions, brk/mmap cursors and the fd parts come from the
    // shell's record; `user_ok` then answers for the child exactly as for
    // the shell.
    {
        let t = crate::task();
        let regions = t.regions;
        let n_regions = t.n_regions;
        let (brk_base, brk_cur, brk_end, mmap_cur) =
            (t.brk_base, t.brk_cur, t.brk_end, t.mmap_cur);
        let cwd = t.cwd.clone();
        let ct = &mut crate::task_slots()[2];
        ct.cwd = cwd;
        ct.root_pa = root;
        ct.regions = regions;
        ct.n_regions = n_regions;
        ct.brk_base = brk_base;
        ct.brk_cur = brk_cur;
        ct.brk_end = brk_end;
        ct.mmap_cur = mmap_cur;
        ct.exit = None;
        ct.fd_names = forked.names;
        ct.fd_cloexec = forked.cloexec;
        ct.fd_nonblock = forked.nonblock;
    }
    crate::set_task_slot(2);

    // The child resumes the CALLER at its own RIP with every user register
    // it had — RAX forced to 0, the fork child's answer. The user RSP rides
    // the stub's stash (the shell's CLONE just wrote it); it is copied now,
    // because the child's first syscall overwrites the stash with its own.
    let ef = ring3::EntryFrame {
        r15: fr.r15, r14: fr.r14, r13: fr.r13, r12: fr.r12,
        rbp: fr.rbp, rbx: fr.rbx, r10: fr.r10, r8: fr.r8, r9: fr.r9,
        rdx: fr.rdx, rsi: fr.rsi, rdi: fr.rdi,
        rax: 0,
        rip: fr.user_rip,
        rflags: fr.user_rflags,
        rsp: ring3::user_rsp_stash(),
    };

    // The fork child gets a per-slot crossing record of its own (slice 2) —
    // which is why the old code's whole snapshot-and-restore dance around
    // the shared statics is gone: the shell's sshd→shell record lives in
    // `SLOT_STATE[cur]` and the fork never touches it, so the shell's own
    // exit (or its next exec's snapshot) reads exactly what was always
    // there. The child's record copies the shell's out_slot (a fork child's
    // fd 1 writes land in the buffer sshd drains — never its own slot),
    // inherits depth+1, and names the shell as the crossing back.
    // SAFETY: syscall-path discipline.
    let (_, save) = unsafe {
        let c = cross_mut(slot_idx);
        c.save = CrossSave::new();
        c.parent_root = ash_root;
        c.parent_stack_top = ring3::syscall_stack_top();
        c.parent_stash = ring3::user_rsp_stash();
        c.depth = shell_depth + 1;
        c.task_slot = 2;
        c.parent_task_slot = 1;
        c.parent_slot = cur;
        c.out_slot = cross_mut(cur).out_slot;
        crate::exec_twin::set_current_slot(slot_idx);
        // The child's dispatch stack: one quarter below the shell's top. The
        // shell's live fork frames sit just under ITS top, a few hundred
        // bytes deep against a 4 KiB quarter, so the two can never meet.
        ring3::set_syscall_stack_top(c.parent_stack_top - (ring3::SYSCALL_STACK_SIZE / 4) as u64);
        (c.parent_stack_top, save_ptr(slot_idx))
    };
    // Session pi: the M2 fork child inherits the shell's fs (same-image
    // semantics — the MSR already holds it at this crossing, the capture
    // comment below); stamp the shadow so its park-resumes re-install it.
    set_slot_fs(slot_idx, slot_fs_of(cur));
    let _code = unsafe { child_enter(root, &ef, save) };
    // The child exited into this frame: its status is in the slot, the task
    // slot is back to 1, CR3 is the shell's, `current_slot` is the shell's
    // slot is back to 1, CR3 is the shell's, `current_slot` is the shell's
    // slot again (the exit tail's `set_current_slot(parent_slot)`). Hand the
    // shell back its stack and stash from the child's own record, answer
    // fork.
    // SAFETY: syscall-path discipline.
    unsafe {
        let c = cross_mut(slot_idx);
        ring3::set_syscall_stack_top(c.parent_stack_top);
        ring3::set_user_rsp_stash(c.parent_stash);
    }
    let pid = slots()[slot_idx].pid;
    u64::from(pid)
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
    if REAL_FORK {
        return wait4_real(pid, status_va, options);
    }
    let want_pid = pid as u32;    let any = pid == u64::MAX || want_pid == 0; // -1, or "my process group"
    let mut found: Option<(u32, u32)> = None;
    // SAFETY: syscall-path discipline.
    let s = slots();
    for sl in s.iter_mut() {
        if !sl.used || sl.reaped || sl.running || (!any && sl.pid != want_pid) {
            continue;
        }
        found = Some((sl.pid, sl.status));
        sl.reaped = true;
        break;
    }
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
    let my_pid = slots()[my_tid].pid;
    loop {
        // SAFETY: syscall-path discipline.
        let s = slots();
        for sl in s.iter_mut() {
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
            let (child, status) = (sl.pid, sl.status);
            if status_va != 0
                && (!crate::user_ok(status_va, 4)
                    || !crate::copy_to_user(status_va, &status.to_le_bytes()))
            {
                return crate::e(crate::err::EFAULT);
            }
            return u64::from(child);
        }
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

/// The M2 body of [`wait4_arm`], kept for the boot task's non-blocking
/// answer (the bridge's reaper never parks).
fn wait4_arm_blocking_scan(pid: u64, status_va: u64) -> u64 {
    let want_pid = pid as u32;
    let any = pid == u64::MAX || want_pid == 0;
    let mut found: Option<(u32, u32)> = None;
    // SAFETY: syscall-path discipline.
    let s = slots();
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
        found = Some((sl.pid, sl.status));
        sl.reaped = true;
        break;
    }
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

/// Akuma's WAITPID (0x1000+303): `waitpid(pid, &status)`. Returns the pid
/// with the status filled (reaped), 0 (still running — impossible here, but
/// the honest answer for an unknown-but-plausible pid is the ABI's, not an
/// error), or negative (no such child).
pub fn waitpid_arm(pid: u64, status_va: u64) -> u64 {
    let pid = pid as u32;
    // SAFETY: syscall-path discipline.
    let found = {
        let s = slots();
        s.iter_mut().find(|sl| sl.used && sl.pid == pid).map(|sl| sl.status)
    };
    let Some(status) = found else {
        return 0; // "still running": the bridge polls until teardown
    };
    if status_va != 0
        && (!crate::user_ok(status_va, 4) || !crate::copy_to_user(status_va, &status.to_le_bytes()))
    {
        return crate::e(crate::err::EFAULT);
    }
    u64::from(pid)
}

/// Akuma's CLOSE_CHILD_STDIN (0x1000+326): deliver EOF to the child's
/// stdin. There is no stdin path here — the buffer model has no reader to
/// signal, and a command that read stdin would already have seen EOF.
/// Success is the honest answer: from the bridge's point of view the EOF
/// was delivered.
pub fn close_stdin_arm(_pid: u64) -> u64 {
    0
}

/// Drain the pipe behind `slot_idx` into the task's buffer: up to
/// `len` bytes from the front, then (buffer empty, child long gone) EOF.
pub fn read_child_out(slot_idx: usize, buf_va: u64, len: u64) -> u64 {
    if len == 0 || !crate::user_ok(buf_va, len) {
        return crate::e(crate::err::EFAULT);
    }
    // SAFETY: syscall-path discipline.
    let s = slots();
    let Some(slot) = s.get_mut(slot_idx) else {
        return crate::e(crate::err::EBADF);
    };
    let n = (len as usize).min(slot.out.len());
    if n == 0 {
        return 0; // drained: EOF, the same thing a real pipe reports
    }
    let head: Vec<u8> = slot.out[..n].to_vec();
    slot.out.drain(..n);
    if !crate::copy_to_user(buf_va, &head) {
        return crate::e(crate::err::EFAULT);
    }
    n as u64
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
    // SAFETY: syscall-path discipline (the claim path runs there in slice 3).
    let c = unsafe { cross_mut(slot) };
    if c.stacks.is_none() {
        c.stacks = Some(leak_stacks());
    }
    let s = c.stacks.as_ref()?;
    let ktop = (s.kernel.as_ptr() as usize + KSTACK_WORDS * 8) as u64;
    let ttop = ((s.trap.as_ptr() as usize + TSTACK_WORDS * 8) & !0xf) as u64;
    c.kstack_top = ktop;
    c.trap_stack_top = ttop;
    c.save = CrossSave::new();
    c.uctx = None;
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
    // SAFETY: syscall-path discipline; `slot` is not running.
    unsafe { cross_mut(slot).uctx = Some(*ctx) };
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
    // SAFETY: syscall-path discipline.
    unsafe { cross_mut(slot).uctx }
}
