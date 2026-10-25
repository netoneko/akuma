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
}

impl SpawnSlot {
    const fn new() -> Self {
        Self { used: false, pid: 0, running: false, status: 0, out: Vec::new() }
    }
}

const NSLOTS: usize = 4;
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
static mut NESTED_SLOT: usize = 0;
static mut NESTED_DEPTH: u32 = 0;

/// -- saved parent state for the nested run (written by [`spawn_run`], read
/// -- by the exit arm's asm tail). Single core, `IF` off: no other writer.
static mut PARENT_ROOT: u64 = 0;
static mut PARENT_STACK_TOP: u64 = 0;
static mut PARENT_STASH: u64 = 0;
static mut RET_RSP: u64 = 0;
static mut SAVE_RBX: u64 = 0;
static mut SAVE_RBP: u64 = 0;
static mut SAVE_R12: u64 = 0;
static mut SAVE_R13: u64 = 0;
static mut SAVE_R14: u64 = 0;
static mut SAVE_R15: u64 = 0;

/// SAFETY: single core, `IF` clear (syscall path or boot) — the discipline
/// that owns every static in this module.
fn slots() -> &'static mut [SpawnSlot; NSLOTS] {
    unsafe { &mut *core::ptr::addr_of_mut!(SLOTS) }
}

/// Is a nested child running right now? The `exit_group` arm's fork in the
/// dispatcher.
pub fn nested_active() -> bool {
    // SAFETY: as `slots`.
    unsafe { core::ptr::read_volatile(&raw const NESTED_DEPTH) > 0 }
}

/// The pid the current task should report (`getpid`): 1 for the boot task,
/// the spawn slot's pid while the child runs.
pub fn current_pid() -> u32 {
    if nested_active() {
        // SAFETY: as `slots`.
        unsafe { slots()[core::ptr::read_volatile(&raw const NESTED_SLOT)].pid }
    } else {
        1
    }
}

/// The nested child's write to fd 1: into the pipe (the buffer).
pub fn child_write(buf_va: u64, len: u64) -> bool {
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
            &mut s[core::ptr::read_volatile(&raw const NESTED_SLOT)]
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

/// Akuma's SPAWN (0x1000+301): `spawn(path, argv, envp, stdin_ptr, stdin_len, flags)`.
///
/// Runs the child to completion inside the call (the model this module
/// exists to state), then returns `pid | stdout_fd << 32` — the same packed
/// word the full kernel's SPAWN returns, read by the same `spawn_full`.
pub fn spawn_arm(fr: &ring3::SyscallFrame) -> u64 {
    let (path_va, argv_va, envp_va, _stdin_ptr, _stdin_len, flags) =
        (fr.arg1(), fr.arg2(), fr.arg3(), fr.arg4(), fr.arg5(), fr.arg6());
    if nested_active() {
        return crate::e(crate::err::EPERM); // one level only; nothing nests a nest
    }
    let mut pbuf = [0u8; 128];
    let Some(path) = crate::user_str(path_va, &mut pbuf) else {
        return crate::e(crate::err::EFAULT);
    };
    if !path.starts_with('/') {
        return crate::e(crate::err::ENOENT);
    }
    let Some(argv_ptrs) = read_ptr_array(argv_va, ARGV_MAX) else {
        return crate::e(crate::err::EFAULT);
    };
    let mut argv: Vec<&str> = Vec::new();
    let mut argv_bufs: Vec<String> = Vec::new();
    for p in &argv_ptrs {
        let Some(s) = read_user_string(*p) else {
            return crate::e(crate::err::EFAULT);
        };
        argv_bufs.push(s);
    }
    for s in &argv_bufs {
        argv.push(s.as_str());
    }
    if argv.is_empty() {
        argv.push(path); // Linux would EFAULT; a path-only spawn is kinder
    }
    let mut env: Vec<&str> = Vec::new();
    let mut env_bufs: Vec<String> = Vec::new();
    if envp_va != 0 {
        let Some(env_ptrs) = read_ptr_array(envp_va, ENV_MAX) else {
            return crate::e(crate::err::EFAULT);
        };
        for p in &env_ptrs {
            let Some(s) = read_user_string(*p) else {
                return crate::e(crate::err::EFAULT);
            };
            env_bufs.push(s);
        }
        for s in &env_bufs {
            env.push(s.as_str());
        }
    }
    // `stdin` (sshd passes None for exec) and the pty/console flag bits are
    // accepted and ignored: there is no input path and no pty in this kernel.
    let _ = flags;

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

    // Snapshot the parent and point the syscall machinery at the child.
    // SAFETY: syscall-path discipline for the statics; `child_enter`'s
    // contract for the crossing.
    unsafe {
        PARENT_ROOT = crate::task_slots()[0].root_pa;
        PARENT_STACK_TOP = ring3::syscall_stack_top();
        PARENT_STASH = ring3::user_rsp_stash();
        *core::ptr::addr_of_mut!(NESTED_SLOT) = slot_idx;
        *core::ptr::addr_of_mut!(NESTED_DEPTH) += 1;
        // The nested stack: the lower half of the syscall stack. The live
        // dispatch frames sit in the upper half — the SPAWN arm's depth from
        // the top is a few hundred bytes, and the stub's per-syscall frame is
        // 128 — so the two halves cannot meet.
        let half = (ring3::SYSCALL_STACK_SIZE / 2) as u64;
        ring3::set_syscall_stack_top(PARENT_STACK_TOP - half);
    }

    let ef = ring3::EntryFrame {
        r15: 0, r14: 0, r13: 0, r12: 0, rbp: 0, rbx: 0, r10: 0, r8: 0, r9: 0,
        rdx: 0, rsi: 0, rdi: 0, rax: 0,
        rip: ti.entry,
        rflags: 0x202,
        rsp: ti.rsp,
    };
    let code = unsafe { child_enter(ti.root_pa, &ef) };
    // `code` (the child's exit status) was recorded into the slot by the
    // exit arm before it returned here; SPAWN's own result is the packed
    // pid/fd word below.
    let _ = code;

    // Back in the parent, in every sense: registers and stack top restored
    // by the exit arm, CR3 back, the stash pointing at the parent again.
    // SAFETY: syscall-path discipline.
    unsafe {
        *core::ptr::addr_of_mut!(NESTED_DEPTH) = 0;
        ring3::set_syscall_stack_top(PARENT_STACK_TOP);
        ring3::set_user_rsp_stash(PARENT_STASH);
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

/// Cross into the child and come back only when it exits.
///
/// Naked because the whole body is the contract: save the parent-phase
/// callee-saved registers and stack pointer, load `CR3`, then the
/// `enter_user` crossing. The exit arm reverses this exactly — restores the
/// registers, moves `rsp` back to the saved value, and `ret`s, so this
/// function *returns* the child's exit code into [`spawn_arm`].
///
/// # Safety
/// `root` must be the child's page root (mapping the kernel half and the
/// child's image), and `ef` a ring-3-enterable state in it.
#[cfg(target_arch = "x86_64")]
#[unsafe(naked)]
pub unsafe extern "C" fn child_enter(root: u64, ef: &ring3::EntryFrame) -> u64 {
    core::arch::naked_asm!(
        // Save the caller's (parent-phase) state first.
        "mov qword ptr [rip + {ret_rsp}], rsp",
        "mov qword ptr [rip + {s_rbx}], rbx",
        "mov qword ptr [rip + {s_rbp}], rbp",
        "mov qword ptr [rip + {s_r12}], r12",
        "mov qword ptr [rip + {s_r13}], r13",
        "mov qword ptr [rip + {s_r14}], r14",
        "mov qword ptr [rip + {s_r15}], r15",
        // The child's address space: kernel half survives (shared PML4).
        "mov cr3, rdi",
        // The enter_user crossing, verbatim (`akuma-ring3-amd64`): the frame
        // pointer is now RSI.
        "mov rax, [rsi + 0x60]",
        "mov rdx, [rsi + 0x48]",
        "mov rsi, [rsi + 0x50]",
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
        "sysretq",
        ret_rsp = sym RET_RSP,
        s_rbx = sym SAVE_RBX,
        s_rbp = sym SAVE_RBP,
        s_r12 = sym SAVE_R12,
        s_r13 = sym SAVE_R13,
        s_r14 = sym SAVE_R14,
        s_r15 = sym SAVE_R15,
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
    // Record first, while the Rust frame can still reach the slot table.
    // SAFETY: syscall-path discipline.
    let (parent_root, status) = unsafe {
        let slot_idx = core::ptr::read_volatile(&raw const NESTED_SLOT);
        let s = &mut *core::ptr::addr_of_mut!(SLOTS);
        s[slot_idx].running = false;
        s[slot_idx].status = ((code & 0xFF) << 8) as u32;
        let t = &mut *core::ptr::addr_of_mut!(crate::TASKS);
        t[1].exit = Some(code);
        *core::ptr::addr_of_mut!(NESTED_DEPTH) = 0;
        (core::ptr::read_volatile(&raw const PARENT_ROOT), s[slot_idx].status)
    };
    crate::set_task_slot(0);
    let ret_rsp = unsafe { core::ptr::read_volatile(&raw const RET_RSP) };
    let (bx, bp, r12, r13, r14, r15) = unsafe {
        (
            core::ptr::read_volatile(&raw const SAVE_RBX),
            core::ptr::read_volatile(&raw const SAVE_RBP),
            core::ptr::read_volatile(&raw const SAVE_R12),
            core::ptr::read_volatile(&raw const SAVE_R13),
            core::ptr::read_volatile(&raw const SAVE_R14),
            core::ptr::read_volatile(&raw const SAVE_R15),
        )
    };
    // SAFETY: the values were saved by `child_enter` from the frame this
    // restores; `ret` pops the return address that call pushed. After the
    // `cr3` load this code is running in the parent's address space — both
    // map the kernel half, so the registers land before the jump matters.
    unsafe {
        core::arch::asm!(
            "mov cr3, {root}",
            "mov rbx, {bx}",
            "mov rbp, {bp}",
            "mov r12, {r12}",
            "mov r13, {r13}",
            "mov r14, {r14}",
            "mov r15, {r15}",
            "mov rsp, {sp}",
            "mov eax, {code:e}",
            "ret",
            root = in(reg) parent_root,
            bx = in(reg) bx,
            bp = in(reg) bp,
            r12 = in(reg) r12,
            r13 = in(reg) r13,
            r14 = in(reg) r14,
            r15 = in(reg) r15,
            sp = in(reg) ret_rsp,
            code = in(reg) u64::from(status),
            options(noreturn),
        )
    }
}

// ---------------------------------------------------------------------------
// The wait family, as the bridge drives it.
// ---------------------------------------------------------------------------

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
