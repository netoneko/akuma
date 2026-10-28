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
}

impl SpawnSlot {
    const fn new() -> Self {
        Self { used: false, pid: 0, running: false, status: 0, out: Vec::new(), reaped: false }
    }
}

pub const NSLOTS: usize = 4;
/// Stdout ceiling per child: after this, writes are dropped (the paws
/// fallback truncates at 1 MiB too). The gate's output is two orders below.
const OUT_CAP: usize = 1024 * 1024;

static mut SLOTS: [SpawnSlot; NSLOTS] = [
    SpawnSlot::new(),
    SpawnSlot::new(),
    SpawnSlot::new(),
    SpawnSlot::new(),
];
static mut NEXT_SLOT: usize = 0;
static mut NEXT_PID: u32 = 2; // 1 is sshd (the boot's init task)

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
static mut SLOT_STATE: [SlotCross; NSLOTS] =
    [SlotCross::new(), SlotCross::new(), SlotCross::new(), SlotCross::new()];

/// The crossing record of `slot`, for the syscall-path writers.
///
/// SAFETY: as `slots`.
unsafe fn cross_mut(slot: usize) -> &'static mut SlotCross {
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
const ARGV_MAX: usize = 16;
const ENV_MAX: usize = 16;
const STR_MAX: usize = 256;

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

    // A free slot and a pid, before anything expensive.
    let slot_idx = {
        // SAFETY: syscall-path discipline.
        let s = slots();
        let idx = unsafe { core::ptr::read_volatile(&raw const NEXT_SLOT) };
        let idx = idx % NSLOTS;
        s[idx] = SpawnSlot::new();
        s[idx].used = true;
        let pid = unsafe {
            let p = &mut *core::ptr::addr_of_mut!(NEXT_PID);
            let v = *p;
            *p = p.wrapping_add(1);
            v
        };
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
        ring3::set_syscall_stack_top(c.parent_stack_top - half);
        (c.parent_stack_top, save_ptr(slot_idx))
    };

    let ef = ring3::EntryFrame {
        r15: 0, r14: 0, r13: 0, r12: 0, rbp: 0, rbx: 0, r10: 0, r8: 0, r9: 0,
        rdx: 0, rsi: 0, rdi: 0, rax: 0,
        rip: ti.entry,
        rflags: 0x202,
        rsp: ti.rsp,
    };
    let code = unsafe { child_enter(ti.root_pa, &ef, save) };
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
            } else if let crate::OpenFile::Pipe(id, w) = payload {
                // A swept pipe end drops its table reference, exactly as
                // close does.
                crate::pipes::release(id, w);
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
    let status = unsafe { child_enter(ti.root_pa, &ef, save_ptr(crate::exec_twin::current_slot())) };
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

/// The nested child's `exit_group`: record the status, hand the machine back
/// to the parent, and return into [`spawn_arm`].
///
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
    let (parent_root, status, parent_task_slot, parent_slot, save) = unsafe {
        let c = cross_mut(cur);
        let s = slots();
        s[cur].running = false;
        s[cur].status = ((code & 0xFF) << 8) as u32;
        let t = &mut *core::ptr::addr_of_mut!(crate::TASKS);
        t[c.task_slot].exit = Some(code);
        (c.parent_root, s[cur].status, c.parent_task_slot, c.parent_slot, c.save)
    };
    crate::set_task_slot(parent_task_slot);
    // The parent is current again: for a depth-1 exit that is the boot task
    // (`NO_SLOT`), for a depth-2 exit the shell's slot — what the old
    // decrement-`NESTED_DEPTH`-and-restore-`NESTED_SLOT` dance computed.
    crate::exec_twin::set_current_slot(parent_slot);
    let ret_rsp = save.ret_rsp;
    // SAFETY: the values were saved by `child_enter` into this slot's record
    // from the frame this restores; `ret` pops the return address that call
    // pushed. After the `cr3` load this code is running in the parent's
    // address space — both map the kernel half, so the registers land before
    // the jump matters.
    //
    // The callee-saved restores are **explicit register bindings**
    // (`in("rbx")` …), not `mov` statements fed by `in(reg)` inputs: an
    // `in(reg)` input may be allocated to ANY register — boot 2 caught LLVM
    // handing the `r13` input the physical `rbx`, which the template's own
    // `mov rbx, …` destroyed first, so `r13` came back holding the parent's
    // saved `rbx` (a user address) and the resumed frame faulted on its
    // first `(%r13)` load. With the binding form the value IS in the named
    // register before the block runs — the placement is the restore — and
    // LLVM must keep the free inputs (`root`, `sp`) out of the bound ones.
    // `rax` carries the exit status the same way: it is what the `ret`
    // returns into the resuming arm.
    unsafe {
        core::arch::asm!(
            "mov cr3, {root}",
            "mov rsp, {sp}",
            "ret",
            root = in(reg) parent_root,
            sp = in(reg) ret_rsp,
            in("rax") u64::from(status),
            in("rbx") save.rbx,
            in("rbp") save.rbp,
            in("r12") save.r12,
            in("r13") save.r13,
            in("r14") save.r14,
            in("r15") save.r15,
            options(noreturn),
        )
    }
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
        let pid = unsafe {
            let p = &mut *core::ptr::addr_of_mut!(NEXT_PID);
            let v = *p;
            *p = p.wrapping_add(1);
            v
        };
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
        if let crate::OpenFile::Pipe(id, w) = d {
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
        let ct = &mut crate::task_slots()[2];
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
    let _code = unsafe { child_enter(root, &ef, save) };
    // The child exited into this frame: its status is in the slot, the task
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
pub fn wait4_arm(pid: u64, status_va: u64, _options: u64, _rusage_va: u64) -> u64 {
    let want_pid = pid as u32;
    let any = pid == u64::MAX || want_pid == 0; // -1, or "my process group"
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
