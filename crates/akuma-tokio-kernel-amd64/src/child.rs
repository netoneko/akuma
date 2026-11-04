//! Processes on this kernel, the machine's half: init's thread, the `clone`
//! decode and the `execve` crossing on x86_64's syscall frame, the user-context
//! capture `fork_process` reads, the two signal arms this kernel keeps
//! (`rt_sigsuspend`, `rt_sigreturn`) and signal delivery. The slot ring, fork,
//! `execve`'s load, exit and the root ownership are
//! `akuma_tokio_kernel_core::proc`'s, shared with aarch64 (B21 A5.6c); the
//! wrappers below keep their names for this crate's callers.

use akuma_ring3_amd64 as ring3;
use akuma_tokio_kernel_core::proc;

use crate::lazy::Amd64;

pub use akuma_tokio_kernel_core::proc::{NSLOTS, is_thread_task, pid_for_slot};

pub fn bind_child_task(tid: usize, pid: u32, root_pa: u64, is_thread: bool) -> Result<(), &'static str> {
    proc::bind_child_task::<Amd64>(tid, pid, root_pa, is_thread)
}

pub fn sched_task_slot_of(tid: usize) -> usize {
    proc::sched_task_slot_of(tid)
}

pub fn sched_root_of(tid: usize) -> u64 {
    proc::sched_root_of::<Amd64>(tid)
}

pub fn is_real_task() -> bool {
    proc::is_real_task::<Amd64>()
}

pub fn task_exit_real(code: u64) -> ! {
    proc::task_exit_code::<Amd64>((code & 0xFF) as i32)
}

pub fn task_exit_thread() -> ! {
    proc::task_exit_thread::<Amd64>()
}

pub fn task_die_segv() -> ! {
    proc::die_segv::<Amd64>()
}

fn task_exit_code(code: i32) -> ! {
    proc::task_exit_code::<Amd64>(code)
}

pub fn on_thread_terminated(tid: usize) {
    proc::on_thread_terminated::<Amd64>(tid);
}

pub fn kill_requested() -> bool {
    proc::kill_requested::<Amd64>()
}

pub fn finish_killed() -> ! {
    proc::finish_killed::<Amd64>()
}

/// Free what reaped processes left (`proc::collect`), with the reap line under `trace-err`.
pub fn collect() {
    let before = akuma_exec::pmm::free_count();
    let c = proc::collect::<Amd64>();
    if cfg!(feature = "trace-err") && c.reaped != 0 {
        akuma_primitives::safe_print!(
            224,
            "[reap] reaped={} drained={} root={} free {} -> {} heap={}K cowrefs={} cow={:?} procs={}/{}\n",
            c.reaped,
            c.drained,
            c.pages,
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

/// The pool slot init runs on (`proc::bind_init` claimed it).
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

/// Run the loaded boot program as pid 1 on a real pool thread and park the boot thread. The slot is claimed, bound and
/// registered by `proc::bind_init` (the same row a fork child gets, with the task record copied from the one `load_task`
/// filled), seeded here to enter ring 3 and published. The boot thread then waits for init to finish and reports its status.
pub fn start_init_thread(path: &str, ti: &crate::TaskImage) -> ! {
    let Some(tid) = proc::bind_init::<Amd64>(path, ti) else {
        akuma_primitives::safe_print!(96, "  [init] no pool slot for init\n");
        crate::halt();
    };
    INIT_TID.store(tid, core::sync::atomic::Ordering::Relaxed);
    INIT_ENTRY.store(ti.entry, core::sync::atomic::Ordering::Relaxed);
    INIT_RSP.store(ti.sp, core::sync::atomic::Ordering::Relaxed);
    // Straight onto the heap: `Box::new([0; N])` builds the 32 KiB array on this stack first.
    let stack: &'static mut [u64] = alloc::boxed::Box::leak(alloc::vec![0u64; INIT_STACK_WORDS].into_boxed_slice());
    let top = stack.as_mut_ptr() as usize + INIT_STACK_WORDS * 8;
    akuma_threading::x86_seed_entry(tid, top, init_thread_entry);
    // The boot thread becomes pool slot 0 (the pool hands out 1..): it is about to
    // block, and a thread the pool has never heard of has no slot to park.
    let _ = akuma_sched_amd64::prepare_task_slot::<crate::sched::Tx>(0);
    akuma_threading::x86_adopt_running_thread(0);
    crate::exec_twin::set_current_slot(0);
    akuma_threading::x86_publish(tid);
    crate::PREEMPT_ARMED.store(true, core::sync::atomic::Ordering::Release);
    akuma_primitives::safe_print!(96, "[init] pid 1 on pool slot {}\n", tid);
    crate::task_done(proc::wait_init::<Amd64>())
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

/// CLONE/FORK/VFORK (56/57/58). On x86_64 musl issues `fork()` as
/// `clone(SIGCHLD)` and `posix_spawn` as `clone(SIGCHLD|CLONE_VM|CLONE_VFORK)`,
/// but **vfork as the raw SYS_vfork in assembly, no flags word at all**
/// (`vfork.s`), and busybox ash forks its non-final commands through exactly
/// that, so FORK/VFORK are accepted bare. x86_64's `clone` takes `child_tid`
/// before `tls`; `proc::clone` takes the asm-generic order.
pub fn fork_arm(fr: &ring3::SyscallFrame) -> u64 {
    if fr.rax == akuma_syscalls_abi::x86_64::CLONE {
        proc::clone::<Amd64>(fr.arg1(), fr.arg2(), fr.arg3(), fr.arg5(), fr.arg4())
    } else {
        proc::fork::<Amd64>(0) // fork (57) and vfork (58)
    }
}

/// `execve` for a fork child: `proc::exec` loads the new image (its fds survive,
/// its thread keeps running), then the crossing.
pub fn exec_real_task(fr: &ring3::SyscallFrame) -> u64 {
    let cur = crate::exec_twin::current_slot();
    let ti = match proc::exec::<Amd64>(fr.arg1(), fr.arg2(), fr.arg3()) {
        Ok(ti) => ti,
        Err(r) => return r,
    };
    let ef = ring3::EntryFrame {
        r15: 0, r14: 0, r13: 0, r12: 0, rbp: 0, rbx: 0, r10: 0, r8: 0, r9: 0,
        rdx: 0, rsi: 0, rdi: 0, rax: 0,
        rip: ti.entry,
        rflags: 0x202,
        rsp: ti.sp,
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
    // `proc::exec` returned, so the image (47 MB for cargo) and the argument
    // strings are already freed: nothing local is left to leak at the crossing.
    // The new image's root and first frame; the dispatcher frame on the
    // task's own band is abandoned (bounded by exec counts).
    crate::exec_twin::enter_user_on(ti.root_pa, &ef)
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

