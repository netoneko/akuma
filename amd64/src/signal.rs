//! Signal delivery for this target — the epilogue every `syscall` return runs.
//!
//! # What was missing, and where
//!
//! Every *half* of signals except this one already worked here. `akuma-threading`
//! holds the per-thread pending set, the blocked mask and the sigaltstack;
//! `akuma-syscalls-glue` has `rt_sigaction`, `rt_sigprocmask`, `sigaltstack`,
//! `kill`, `tkill` and `tgkill`; `akuma-exec`'s `deliver_signal` pends on a
//! thread group and sets the interrupt flag `should_interrupt_blocking_syscall`
//! reads. What no amd64 code did was **look**: a signal could be pended and
//! never noticed, so `kill(2)` reported success and did nothing.
//!
//! On AArch64 the looking happens inside the EL0 sync handler, against a
//! `UserTrapFrame` — `akuma_exceptions::try_deliver_signal`, ~350 lines welded
//! to that architecture's `sigcontext`. This is the x86_64 counterpart, against
//! [`UserCtx`](crate::usermode::UserCtx), and it is a separate implementation on
//! purpose: the register file, the frame layout, the return instruction and the
//! restorer convention are all different, and the only thing the two could share
//! is a dispatch shape that neither would be shorter for.
//!
//! # Three places a signal is looked at
//!
//! A pending signal is only ever *acted on* at a boundary where this kernel has
//! the interrupted register file in its hands, and there are exactly three:
//!
//! | boundary | entry point | the register file comes from |
//! |---|---|---|
//! | a `syscall` return | [`deliver_pending`] | `UserCtx` (`syscall_entry` wrote it) |
//! | a `#PF`/`#GP` from ring 3 | [`deliver_fault_signal`] | `idt::TrapRegs` + the pushed frame |
//! | a LAPIC tick that interrupted ring 3 | [`deliver_pending_on_tick`] | the same |
//!
//! The **decision** is one function ([`next_delivery`]) for all three; they
//! differ only in where the file lives and in how the process leaves ring 3 if
//! the answer is a fatal default. Together they leave no shape of program a
//! signal cannot reach — the syscall return alone did (a compute loop), and the
//! first two together still did (`AKUMA_AMD64_FAULT_SIGNALS.md` §7).
//!
//! # Two things this target does differently, each pinned
//!
//! 1. **`uc_mcontext.fpstate` is `NULL` and no FP state is saved.** Linux always
//!    attaches an `xsave` area; a handler that reads `fpstate` would dereference
//!    zero. Nothing in this tree's userspace does (musl does not; Go would), and
//!    this kernel is both the writer *and* the reader — [`sys_rt_sigreturn`]
//!    never looks at the field — so the pair is self-consistent. Attaching one
//!    means saving/restoring the FPU across the excursion, which this target
//!    does at context-switch granularity (`sched.rs`'s `fxsave`) and not here.
//! 2. **`SA_RESTORER` is required.** So is it on real Linux/x86_64 — `sigaction`
//!    returns `EINVAL` without it, because the kernel has no signal trampoline
//!    page on this architecture. Here the check is at *delivery* rather than at
//!    registration (glue's `sys_rt_sigaction` is shared with AArch64, where
//!    `sa_restorer` does not exist), and a missing restorer declines delivery,
//!    which falls through to the default action.
//!
//! # The return path
//!
//! Redirecting a `sysret` needs a second exit from `syscall_entry`, for exactly
//! the reason `execve` needed one (`AKUMA_AMD64_EXECVE_RETURNS.md`): the
//! ordinary path takes `rip`/`rflags` from the `rcx`/`r11` the `syscall`
//! instruction delivered, off the kernel stack, and a signal changes both. So
//! `UserCtx` gains `sig_return`, and `.Lsig_return` takes `rip`, `rsp`,
//! `rflags`, `rax` and all twelve saved registers out of the `UserCtx`.
//!
//! **One path serves both directions**, which is why there is one and not two:
//! entering a handler and returning from one are the same operation — install a
//! register file and `sysret` into it — differing only in who filled the file.
//! [`deliver_pending`] fills it with the handler's ABI; [`sys_rt_sigreturn`]
//! fills it from the frame on the user stack.

use core::sync::atomic::{AtomicU64, Ordering};

use akuma_exec::process::{SignalHandler, current_process_shared};
use akuma_exec::threading;

use crate::fd::errno;
use crate::usermode::UserCtx;

/// `sa_flags` bits this module reads. The `SA_*` values are asm-generic and
/// identical on aarch64 and x86_64, so these are not a second vocabulary —
/// unlike the syscall numbers, which are (`akuma_syscalls_abi`).
mod sa {
    pub const SIGINFO: u64 = 0x0000_0004;
    pub const RESTORER: u64 = 0x0400_0000;
    pub const ONSTACK: u64 = 0x0800_0000;
    pub const NODEFER: u64 = 0x4000_0000;
    pub const RESETHAND: u64 = 0x8000_0000;
}

/// `SS_ONSTACK`, reported in the frame's `uc_stack.ss_flags` so a handler can
/// tell which stack it arrived on.
const SS_ONSTACK: i32 = 1;

/// The System V red zone: 128 bytes below `%rsp` that a leaf function may be
/// using without having adjusted the stack pointer. A signal frame that starts
/// inside it corrupts live data in the interrupted function.
const RED_ZONE: u64 = 128;

/// How many signals have been delivered to a handler.
///
/// All three counters are reported by [`smoke_test`] as suite notes, so a
/// `dmesg` from any boot carries them. They are relaxed adds on a cold path —
/// nothing pends a signal in the common syscall — so they are unconditional
/// rather than gated.
///
/// The interesting one is [`DECLINED`]. A delivery this kernel refuses falls
/// through to the **default action**, which for most signals is termination —
/// so a non-zero count is programs being killed where Linux would have run
/// their handler, and it is invisible from ring 3 (the process just dies).
pub static DELIVERED: AtomicU64 = AtomicU64::new(0);
/// Fatal default actions applied.
pub static DEFAULT_KILLS: AtomicU64 = AtomicU64::new(0);
/// Deliveries declined because the frame could not be written, the handler was
/// not a user address, or the action named no restorer. See [`DELIVERED`] for
/// why this is the one to read.
pub static DECLINED: AtomicU64 = AtomicU64::new(0);

/// `struct sigcontext_64` — the interrupted register file, as Linux lays it out
/// in `arch/x86/include/uapi/asm/sigcontext.h`. Field order is the ABI; a
/// reordering here is invisible to the compiler and visible to every handler
/// that reads `uc_mcontext.gregs[]`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct SigContext {
    r8: u64,
    r9: u64,
    r10: u64,
    r11: u64,
    r12: u64,
    r13: u64,
    r14: u64,
    r15: u64,
    rdi: u64,
    rsi: u64,
    rbp: u64,
    rbx: u64,
    rdx: u64,
    rax: u64,
    rcx: u64,
    rsp: u64,
    rip: u64,
    eflags: u64,
    cs: u16,
    gs: u16,
    fs: u16,
    ss: u16,
    err: u64,
    trapno: u64,
    oldmask: u64,
    cr2: u64,
    /// `struct _fpstate __user *`. Always 0 here — see the module header.
    fpstate: u64,
    reserved: [u64; 8],
}

/// `stack_t`, as the kernel writes it into `uc_stack`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct StackT {
    ss_sp: u64,
    ss_flags: i32,
    _pad: u32,
    ss_size: u64,
}

/// `struct ucontext`, **the kernel's** (`arch/x86/include/asm/ucontext.h`), whose
/// `uc_sigmask` is the 8-byte kernel `sigset_t` and not musl's 128-byte one.
///
/// That mismatch is Linux's, not this kernel's: the frame Linux builds is this
/// size, and a handler reading past the first 8 bytes of `uc_sigmask` reads
/// whatever follows on both. Reproducing it is the point — a *larger* mask field
/// here would move `si_addr` relative to nothing (the kernel passes `info` in
/// `%rsi`, so its offset is private) but would differ from the kernel every
/// other tool was written against.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct UContext {
    uc_flags: u64,
    uc_link: u64,
    uc_stack: StackT,
    uc_mcontext: SigContext,
    uc_sigmask: u64,
}

/// `siginfo_t`'s kernel form: three ints, then a 112-byte union. Written as a
/// byte array with the two fields this kernel fills placed by hand, because the
/// union arms differ per signal and only `si_pid`/`si_uid` (SI_USER) and
/// `si_addr` (a fault) are ever set.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct SigInfo {
    si_signo: i32,
    si_errno: i32,
    si_code: i32,
    _pad: i32,
    /// The union, 112 bytes. `si_pid`/`si_uid` live at its first two words for
    /// `SI_USER`; `si_addr` at its first word for `SIGSEGV`/`SIGBUS`.
    fields: [u64; 14],
}

/// `struct rt_sigframe` — what `%rsp` points at when the handler starts.
///
/// `pretcode` first is the whole convention: the handler is entered with a
/// `sysret`, not a `call`, so the return address has to be *already on the
/// stack* for its closing `ret` to find. That word is `sa_restorer`, and the
/// restorer issues `rt_sigreturn`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct RtSigFrame {
    pretcode: u64,
    uc: UContext,
    info: SigInfo,
}

/// **The offsets a handler and a debugger index by hand.**
///
/// `uc_mcontext` is `ucontext_t`'s only interesting field and every runtime
/// reaches it as a fixed displacement; `si_code` at 8 is what tells `SI_USER`
/// from a fault. A field inserted above either of them compiles, boots, passes
/// the suite, and hands every signal handler in the system a register file
/// shifted by eight bytes.
const _: () = {
    assert!(core::mem::offset_of!(SigContext, rip) == 128);
    assert!(core::mem::offset_of!(SigContext, eflags) == 136);
    assert!(core::mem::offset_of!(SigContext, fpstate) == 184);
    assert!(core::mem::size_of::<SigContext>() == 256);
    assert!(core::mem::size_of::<StackT>() == 24);
    assert!(core::mem::offset_of!(UContext, uc_stack) == 16);
    assert!(core::mem::offset_of!(UContext, uc_mcontext) == 40);
    assert!(core::mem::size_of::<UContext>() == 304);
    assert!(core::mem::offset_of!(SigInfo, si_code) == 8);
    assert!(core::mem::size_of::<SigInfo>() == 128);
    assert!(core::mem::offset_of!(RtSigFrame, uc) == 8);
    assert!(core::mem::offset_of!(RtSigFrame, info) == 312);
    assert!(core::mem::size_of::<RtSigFrame>() == 440);
};

/// What the `siginfo_t` should say about where the signal came from.
///
/// The two arms are the two entry points. A `kill`/`tkill`/INTR signal is
/// `SI_USER` with no payload; a CPU fault carries the address that faulted and
/// a code saying why, which is the whole of what a `SIGSEGV` handler has to work
/// with — `mprotectlb`'s handler reads neither, but Rust's stack-overflow
/// handler and Go's `sigpanic` read both.
#[derive(Clone, Copy)]
enum Cause {
    /// `SI_USER`.
    User,
    /// `si_code` is `SEGV_MAPERR`/`SEGV_ACCERR`/`SI_KERNEL`; `addr` is `%cr2`.
    Fault { si_code: i32, addr: u64 },
}

/// `si_code` values this kernel produces for a fault.
pub mod segv {
    /// The address is not mapped at all.
    pub const MAPERR: i32 = 1;
    /// It is mapped, and the access was not permitted.
    pub const ACCERR: i32 = 2;
    /// `SI_KERNEL` — a fault with no meaningful address (`#GP`).
    pub const SI_KERNEL: i32 = 0x80;
}

/// The interrupted ring-3 register file, gathered out of `UserCtx` so the frame
/// builder and the restorer speak about one thing.
///
/// `regs` is `UserCtx::saved_regs` verbatim — `[rdi, rsi, rdx, r10, r8, r9, rbx,
/// rbp, r12, r13, r14, r15]`, the order `syscall_entry` writes.
#[derive(Clone, Copy)]
struct Regs {
    rip: u64,
    rsp: u64,
    rflags: u64,
    rax: u64,
    regs: [u64; 12],
}

/// Index into [`Regs::regs`] / `UserCtx::saved_regs`.
mod r {
    pub const RDI: usize = 0;
    pub const RSI: usize = 1;
    pub const RDX: usize = 2;
    pub const R10: usize = 3;
    pub const R8: usize = 4;
    pub const R9: usize = 5;
    pub const RBX: usize = 6;
    pub const RBP: usize = 7;
    pub const R12: usize = 8;
    pub const R13: usize = 9;
    pub const R14: usize = 10;
    pub const R15: usize = 11;
}

impl Regs {
    /// Read the file the running task will `sysret` with.
    ///
    /// `rax` is not in `UserCtx` on the ordinary path — it is the syscall's own
    /// result, still in flight — so the caller supplies it.
    fn load(uctx: &UserCtx, rax: u64) -> Self {
        Self {
            rip: uctx.user_rip,
            rsp: uctx.user_rsp,
            rflags: uctx.user_rflags,
            rax,
            regs: uctx.saved_regs,
        }
    }

    /// Install this file as the one `.Lsig_return` will `sysret` with.
    fn store(&self, uctx: &mut UserCtx) {
        uctx.user_rip = self.rip;
        uctx.user_rsp = self.rsp;
        uctx.user_rflags = self.rflags;
        uctx.sig_rax = self.rax;
        uctx.saved_regs = self.regs;
        uctx.sig_return = 1;
    }

    /// The same file, gathered off the **exception** stub instead of `UserCtx`.
    ///
    /// Twelve hand-written positions again, and the third place this mapping is
    /// spelled (`syscall_entry`'s store order and `to_sigcontext` are the other
    /// two). A transposition here is invisible to the compiler and shows up as a
    /// signal handler — or a `rt_sigreturn` — seeing two registers swapped, so
    /// `signal::smoke_test` checks it against `TrapRegs` field by field.
    fn from_trap(frame: &crate::idt::InterruptStackFrame, regs: &crate::idt::TrapRegs) -> Self {
        Self {
            rip: frame.rip,
            rsp: frame.rsp,
            rflags: frame.rflags,
            rax: regs.rax,
            regs: [
                regs.rdi, regs.rsi, regs.rdx, regs.r10, regs.r8, regs.r9,
                regs.rbx, regs.rbp, regs.r12, regs.r13, regs.r14, regs.r15,
            ],
        }
    }

    fn to_sigcontext(self) -> SigContext {
        SigContext {
            r8: self.regs[r::R8],
            r9: self.regs[r::R9],
            r10: self.regs[r::R10],
            // `rcx` and `r11` are destroyed by the `syscall` instruction itself,
            // so the interrupted values are genuinely gone: `r11` held the
            // caller's `rflags` (which `eflags` below reports) and `rcx` the
            // return address (`rip`). Zero rather than a stale copy — the ABI
            // says a `syscall` clobbers both, so no correct program can observe
            // the difference, and a wrong value in a core-dump-shaped structure
            // is worse than an obvious zero.
            r11: 0,
            r12: self.regs[r::R12],
            r13: self.regs[r::R13],
            r14: self.regs[r::R14],
            r15: self.regs[r::R15],
            rdi: self.regs[r::RDI],
            rsi: self.regs[r::RSI],
            rbp: self.regs[r::RBP],
            rbx: self.regs[r::RBX],
            rdx: self.regs[r::RDX],
            rax: self.rax,
            rcx: 0,
            rsp: self.rsp,
            rip: self.rip,
            eflags: self.rflags,
            // The ring-3 selectors this target's `sysret` installs
            // (`gdt.rs`'s `STAR`): a handler that reads them must see the values
            // it will be returned to, not zero.
            // `sysret` derives both from `IA32_STAR[63:48]`: `CS = base + 16`,
            // `SS = base + 8` (`gdt.rs`'s `SYSRET_BASE`, RPL included). A
            // handler that reads them must see the values it will be returned
            // to, not zero.
            cs: crate::gdt::SYSRET_BASE + 16,
            ss: crate::gdt::SYSRET_BASE + 8,
            gs: 0,
            fs: 0,
            err: 0,
            trapno: 0,
            oldmask: 0,
            cr2: 0,
            fpstate: 0,
            reserved: [0; 8],
        }
    }

    fn from_sigcontext(sc: &SigContext) -> Self {
        let mut regs = [0u64; 12];
        regs[r::RDI] = sc.rdi;
        regs[r::RSI] = sc.rsi;
        regs[r::RDX] = sc.rdx;
        regs[r::R10] = sc.r10;
        regs[r::R8] = sc.r8;
        regs[r::R9] = sc.r9;
        regs[r::RBX] = sc.rbx;
        regs[r::RBP] = sc.rbp;
        regs[r::R12] = sc.r12;
        regs[r::R13] = sc.r13;
        regs[r::R14] = sc.r14;
        regs[r::R15] = sc.r15;
        Self { rip: sc.rip, rsp: sc.rsp, rflags: sc.eflags, rax: sc.rax, regs }
    }
}

/// `rflags` a program is allowed to set through a signal frame.
///
/// `sysret` loads `%r11` straight into `RFLAGS`, so whatever a frame says
/// becomes the flags of a ring-3 thread — and two of those bits are not the
/// program's to choose. `IF` must stay set (a ring-3 thread with interrupts
/// masked cannot be preempted and the machine stops scheduling; on this target
/// that is a hang, not a slowdown) and `IOPL`/`NT` must stay clear. Linux
/// applies the same filter in `restore_sigcontext`.
///
/// The kept set is the arithmetic and direction flags plus `TF` — the ones a
/// debugger or a `longjmp`-style handler legitimately restores.
const RFLAGS_USER_MASK: u64 = 0x0000_0000_0001_0DD5 | 0x100 /* TF */ | 0x400 /* DF */;
/// `IF` (0x200) and the reserved-one bit (0x2), forced on every return.
const RFLAGS_FORCED: u64 = 0x202;

fn sanitize_rflags(f: u64) -> u64 {
    (f & RFLAGS_USER_MASK) | RFLAGS_FORCED
}

/// Is `va` an address this kernel will `sysret` to?
///
/// `sysretq` takes `rip` from `%rcx` and **faults in ring 0** if that value is
/// non-canonical — so an unchecked `rt_sigreturn` frame is a ring-3 program
/// choosing where the kernel takes a `#GP`. Linux sidesteps it by returning
/// through `iret` when the frame is suspect; this target has one return
/// instruction, so the check is the guard.
fn user_rip_ok(va: u64) -> bool {
    (0x1000..crate::uaccess::USER_END).contains(&va)
}

/// Run the pending-signal check at a `syscall` return. Returns the value ring 3
/// should see in `%rax`.
///
/// Called from `syscall_handler` with the BKL held, interrupts off, on the
/// task's kernel stack — the same conditions the syscall body ran under, and the
/// reason the frame write below can take a `#PF` safely (`idt.rs` services
/// demand paging and CoW breaks from ring 0).
///
/// Returns early, and the early returns are the cheap path: one relaxed load of
/// this thread's pending word for the overwhelmingly common "nothing pending".
pub fn deliver_pending(uctx: *mut UserCtx, syscall_result: u64) -> u64 {
    if uctx.is_null() {
        return syscall_result;
    }
    // SAFETY: the running task's own `UserCtx`; only this task writes it, and
    // it is inside its own syscall.
    let uctx = unsafe { &mut *uctx };

    // A task on its way out of ring 3 for good has no register file worth
    // redirecting, and `execve` has just installed one that must not be.
    if uctx.leave != 0 || uctx.exec_pending != 0 {
        return syscall_result;
    }

    let tid = threading::current_thread_id();
    if threading::pending_signals_raw(tid) == 0 {
        return syscall_result;
    }

    // `sig_return != 0` on entry means `rt_sigreturn` has already installed the
    // file this return will use; its `sig_rax` is the interrupted `rax`, not the
    // syscall's result. Nested delivery builds the next frame on top of that
    // restored context, which is what Linux does too.
    let restored = uctx.sig_return != 0;
    let cur = Regs::load(uctx, if restored { uctx.sig_rax } else { syscall_result });

    let Some(proc) = current_process_shared() else {
        return syscall_result;
    };

    match next_delivery(proc, tid, &cur) {
        Next::Nothing => {
            if restored { cur.rax } else { syscall_result }
        }
        Next::Handler(next) => {
            next.store(uctx);
            uctx.sig_rax
        }
        // **This target's own exit**, not glue's `sys_exit_group`: setting
        // `leave` returns the task into `crate::usermode::run_process`, whose
        // epilogue drains sibling threads, closes the fd table, stamps the
        // `SPAWN` row and removes the channel. And the status must be the
        // **returned** value, not just `EXIT_STATUS`: `run_process` reads the
        // exit status off `enter_user`'s return — the `%rax` the
        // `.Lexit_to_kernel` path carries out — and stamps it into the child's
        // exit channel, which is what the parent's `wait4` decodes. Returning
        // the interrupted syscall's own result instead reported a `SIGTERM`
        // death as whatever that syscall answered.
        Next::Fatal(sig) => {
            crate::usermode::exit_current_from_signal(sig);
            signal_status(sig)
        }
    }
}

/// The bookkeeping every delivered handler needs, whichever path built its
/// frame: the `SA_RESETHAND` one-shot, the blocked-signal mask for the duration
/// of the handler, and the sticky delivered-record that lets a blocking syscall
/// learn it was interrupted after the pending bit is gone
/// (`current_thread_has_pending_interrupt`, and
/// `PTHREAD_KILL_EINTR_DELIVERY_STARVATION.md` for why the record is separate).
///
/// Factored out when the fault path arrived rather than duplicated, because two
/// copies of a mask update is exactly the shape that lets one path forget
/// `sa_mask` and reenter a handler that asked not to be.
fn enter_handler(
    sig: u32,
    idx: usize,
    action: &akuma_exec::process::SignalAction,
    proc: &akuma_exec::process::Process,
    tid: usize,
) {
    if action.flags & sa::RESETHAND != 0 {
        proc.signal_actions.actions.lock()[idx] = akuma_exec::process::SignalAction::default();
    }
    // Block the delivered signal for the duration of the handler (unless
    // `SA_NODEFER`), plus the action's own `sa_mask`. SIGKILL(9) and SIGSTOP(19)
    // can never be masked.
    const UNMASKABLE: u64 = (1u64 << 8) | (1u64 << 18);
    let mut add = action.mask & !UNMASKABLE;
    if action.flags & sa::NODEFER == 0 && (1..=64).contains(&sig) {
        add |= (1u64 << (sig - 1)) & !UNMASKABLE;
    }
    threading::or_thread_signal_mask(add);
    threading::note_delivered_signal(tid, sig);
    // The interrupt bit has done its job: this signal has reached userspace.
    // Without this, the **next syscall the program makes** — any syscall,
    // including ones that cannot block — answers `EINTR` from glue's prologue,
    // because nothing consumed a flag raised for a blocking wait the thread
    // was never in. Measured here with `alarm(1); pause(); getpid()`, which
    // returned `-4`. See `akuma_exec::process::signal_frame_installed`.
    akuma_exec::process::signal_frame_installed(tid);
    DELIVERED.fetch_add(1, Ordering::Relaxed);
}

/// **Turn a ring-3 CPU fault into a `SIGSEGV` its own handler can catch.**
///
/// Called from `idt.rs`'s `#PF` and `#GP` dispatchers for a fault that nothing
/// serviced. Returns whether a handler took it: `true` means the stub's `iretq`
/// now enters that handler and the fault is over; `false` means the caller must
/// kill the process, which is what this target did unconditionally until
/// 2026-09-11.
///
/// # Why this is not `deliver_pending` with different arguments
///
/// Three things differ, and each is the reason the two are separate functions:
///
/// - **The register file is somewhere else.** A syscall's is in `UserCtx`
///   (`syscall_entry` put it there); a fault's is the `TrapRegs` block the
///   exception stub pushed, plus the `rip`/`rsp`/`rflags` the CPU pushed. The
///   frame builder speaks `Regs`, so the difference stops here.
/// - **The return is an `iretq`, not a `sysret`.** So there is no `sig_return`
///   flag and no `.Lsig_return`: rewriting the pushed frame in place *is* the
///   redirect.
/// - **Only three registers are written back** — see [`install_handler_frame`],
///   which the tick path shares.
///
/// # What it refuses, and why each refusal ends in a kill
///
/// No process (a fault with no identity), no `UserFn` disposition, or the signal
/// **blocked**. Linux force-unblocks a synchronous fault signal and then applies
/// the default action if the handler cannot run; refusing here reaches the same
/// place by the caller's route. Declining is always safe: the caller kills, and
/// a killed process is what happened before this function existed.
pub fn deliver_fault_signal(
    frame: &mut crate::idt::InterruptStackFrame,
    regs: &mut crate::idt::TrapRegs,
    sig: u32,
    si_code: i32,
    addr: u64,
) -> bool {
    let idx = (sig as usize).wrapping_sub(1);
    if idx >= akuma_exec::process::MAX_SIGNALS {
        return false;
    }
    let Some(proc) = current_process_shared() else {
        return false;
    };
    let action = { proc.signal_actions.actions.lock()[idx] };
    let SignalHandler::UserFn(entry) = action.handler else {
        return false;
    };
    let tid = threading::current_thread_id();
    // A blocked synchronous fault cannot be deferred — the faulting instruction
    // would simply re-execute and fault again. Kill instead.
    if threading::thread_signal_mask() & (1u64 << idx) != 0 {
        return false;
    }

    let cur = Regs::from_trap(frame, regs);
    let Some(next) = build_frame(&cur, sig, entry, &action, tid, Cause::Fault { si_code, addr })
    else {
        DECLINED.fetch_add(1, Ordering::Relaxed);
        return false;
    };

    install_handler_frame(frame, regs, &next);
    enter_handler(sig, idx, &action, proc, tid);
    true
}

/// Apply signal `sig`'s default action. Returns `Some(status)` when it
/// terminated the process — in which case the caller must stop looking at
/// signals, **and must return that value**, because the task is leaving ring 3
/// and `%rax` is how the status gets there.
///
/// That last part is not bookkeeping. `run_process` reads the exit status off
/// `enter_user`'s return value — the `%rax` the `.Lexit_to_kernel` path carries
/// out of the syscall — and stamps it into the child's exit channel, which is
/// what the parent's `wait4` decodes. Returning the interrupted syscall's own
/// result instead reported a `SIGTERM` death as whatever that syscall answered.
///
/// The negative value is the tree's "killed by signal" encoding, which
/// `encode_wait_status` turns into `WIFSIGNALED`/`WTERMSIG` — the same spelling
/// the AArch64 fatal-signal path uses (`sys_exit_group(-(sig as i32))`).
///
/// **Termination goes through this target's own exit**, not glue's
/// `sys_exit_group`: setting `leave` returns the task into
/// [`crate::usermode::run_process`], whose epilogue is what drains sibling
/// threads, closes the fd table, stamps the `SPAWN` row and removes the channel.
/// Glue's version does most but not all of that and then parks the thread in a
/// `yield_now` loop, so reaching ring 3's exit through it would leave a child
/// whose parent's `sys_waitpid` never sees a status.
fn fatal_default(sig: u32, proc: &akuma_exec::process::Process) -> bool {
    if !akuma_syscalls_glue::signal::signal_is_fatal_default(sig) {
        return false;
    }
    akuma_primitives::safe_print!(128,
        "[signal] pid={} killed by signal {} (default action)\n", proc.pid, sig);
    DEFAULT_KILLS.fetch_add(1, Ordering::Relaxed);
    true
}

/// The exit status a signal death carries. See [`fatal_default`].
fn signal_status(sig: u32) -> u64 {
    (-(i64::from(sig))) as u64
}

/// What the pending set says to do next.
///
/// Three outcomes because there are three *paths out*, and each caller leaves
/// ring 3 its own way: a syscall return sets `UserCtx::leave`, a timer tick has
/// no syscall to return from and unwinds through `kill_current_from_fault`. The
/// decision is the same on both, so it is made once here and acted on twice.
enum Next {
    /// Nothing deliverable; the caller's ordinary return stands.
    Nothing,
    /// Enter this register file.
    Handler(Regs),
    /// This signal's default action terminates the process.
    Fatal(u32),
}

/// Drain this thread's pending set until one signal produces a handler frame, a
/// fatal default is reached, or the set is empty.
///
/// Consumes ignored and non-fatal-default signals on the way, which is what
/// keeps the pending word from filling up with `SIGCHLD`s nobody asked for.
fn next_delivery(proc: &akuma_exec::process::Process, tid: usize, cur: &Regs) -> Next {
    while let Some(sig) = threading::take_pending_signal(threading::thread_signal_mask()) {
        let idx = (sig as usize).wrapping_sub(1);
        if idx >= akuma_exec::process::MAX_SIGNALS {
            continue;
        }
        let action = { proc.signal_actions.actions.lock()[idx] };
        match action.handler {
            SignalHandler::Ignore => {}
            SignalHandler::UserFn(entry) => {
                if let Some(next) = build_frame(cur, sig, entry, &action, tid, Cause::User) {
                    enter_handler(sig, idx, &action, proc, tid);
                    return Next::Handler(next);
                }
                DECLINED.fetch_add(1, Ordering::Relaxed);
                // Fall through to the default action — a handler that cannot be
                // entered must not silently swallow a fatal signal.
                if fatal_default(sig, proc) {
                    return Next::Fatal(sig);
                }
            }
            SignalHandler::Default => {
                if fatal_default(sig, proc) {
                    return Next::Fatal(sig);
                }
            }
        }
    }
    Next::Nothing
}

/// Install a handler's register file into an **interrupt/exception** frame, so
/// the stub's `iretq` enters it.
///
/// Only three registers are written back. Linux's `setup_rt_frame` sets
/// `di`/`si`/`dx` (and `ip`/`sp`) and leaves the rest of the file alone, so a
/// handler that looks at `%rbx` sees what the interrupted code had. The syscall
/// path zeroes more because a `syscall` has already clobbered `rcx`/`r11` and
/// the ABI makes the argument registers dead.
fn install_handler_frame(
    frame: &mut crate::idt::InterruptStackFrame,
    regs: &mut crate::idt::TrapRegs,
    next: &Regs,
) {
    frame.rip = next.rip;
    frame.rsp = next.rsp;
    // `iretq` restores these flags to ring 3. `IF` set is not optional (a ring-3
    // thread with interrupts masked stops being preemptible) and `DF` clear is
    // what the ABI promises the C function about to run. `cs`/`ss` are left
    // alone: they are already the ring-3 selectors this frame came from.
    frame.rflags = RFLAGS_FORCED;
    regs.rdi = next.regs[r::RDI];
    regs.rsi = next.regs[r::RSI];
    regs.rdx = next.regs[r::RDX];
}

/// **The pending-signal check at a LAPIC tick** — the third and last place a
/// signal can be looked at, and the one that closes "a program that never
/// syscalls and never faults takes no signal".
///
/// Called from `idt::timer_dispatch` for a tick that interrupted **ring 3**,
/// before the preemption decision. Only for a ring-3 origin, and that gate is
/// load-bearing rather than an optimisation: the interrupted code then provably
/// holds no BKL, so taking it here cannot deadlock against itself.
///
/// **Does not terminate the process itself**, and that is not a style choice:
/// the caller is holding the BKL for the frame write, `kill_current_from_fault`
/// takes it again and never returns, so killing from in here would leave the
/// lock one level deep for the rest of the boot. `TickOutcome::Fatal` hands the
/// decision back so `timer_dispatch` can drop its hold first.
#[derive(Clone, Copy)]
pub enum TickOutcome {
    /// Nothing pending, or nothing deliverable. The tick returns as it was.
    Unchanged,
    /// The frame now enters a handler.
    Redirected,
    /// This signal's default action terminates the process — **after** the
    /// caller releases the BKL.
    Fatal(u32),
}

/// Leave ring 3 because a tick found a fatal default. Separate from
/// [`deliver_pending_on_tick`] so the caller can drop the BKL first; see
/// [`TickOutcome`].
pub fn kill_current_from_tick(sig: u32) -> ! {
    crate::usermode::kill_current_from_fault(signal_status(sig))
}

pub fn deliver_pending_on_tick(
    frame: &mut crate::idt::InterruptStackFrame,
    regs: &mut crate::idt::TrapRegs,
) -> TickOutcome {
    let tid = threading::current_thread_id();
    if threading::pending_signals_raw(tid) == 0 {
        return TickOutcome::Unchanged;
    }
    let Some(proc) = current_process_shared() else {
        return TickOutcome::Unchanged;
    };
    let cur = Regs::from_trap(frame, regs);
    match next_delivery(proc, tid, &cur) {
        Next::Nothing => TickOutcome::Unchanged,
        Next::Handler(next) => {
            install_handler_frame(frame, regs, &next);
            TickOutcome::Redirected
        }
        Next::Fatal(sig) => TickOutcome::Fatal(sig),
    }
}

/// Build the `rt_sigframe` for `sig` and return the register file that enters
/// the handler, or `None` if it cannot be delivered.
fn build_frame(
    cur: &Regs,
    sig: u32,
    entry: usize,
    action: &akuma_exec::process::SignalAction,
    tid: usize,
    cause: Cause,
) -> Option<Regs> {
    // x86_64 has no kernel signal trampoline: the restorer is the program's.
    if action.flags & sa::RESTORER == 0 || action.restorer == 0 {
        akuma_primitives::safe_print!(128,
            "[signal] sig {} declined: no SA_RESTORER (flags={:#x})\n", sig, action.flags);
        return None;
    }
    if !user_rip_ok(entry as u64) {
        akuma_primitives::safe_print!(128,
            "[signal] sig {} declined: handler {:#x} is not a user address\n", sig, entry);
        return None;
    }

    let (alt_sp, alt_size, _) = threading::get_sigaltstack(tid);
    let on_alt = action.flags & sa::ONSTACK != 0
        && alt_sp != 0
        && alt_size >= core::mem::size_of::<RtSigFrame>() as u64;
    let top = if on_alt {
        alt_sp + alt_size
    } else {
        // The red zone is only live on the interrupted thread's own stack; an
        // alternate stack has nothing below its top to preserve.
        cur.rsp.checked_sub(RED_ZONE)?
    };
    // System V wants `%rsp + 8` 16-byte aligned at a function's first
    // instruction, and the handler is entered as if `call`ed — `pretcode` is the
    // pushed return address. So the frame base must be 16-byte aligned minus 8.
    let base = top.checked_sub(core::mem::size_of::<RtSigFrame>() as u64)? & !0xF;
    let base = base.checked_sub(8)?;
    if base < 0x1000 {
        return None;
    }

    let mut frame = RtSigFrame {
        pretcode: action.restorer as u64,
        ..Default::default()
    };
    frame.uc.uc_stack.ss_sp = alt_sp;
    frame.uc.uc_stack.ss_size = alt_size;
    frame.uc.uc_stack.ss_flags = if on_alt { SS_ONSTACK } else { 0 };
    frame.uc.uc_sigmask = threading::thread_signal_mask();
    frame.uc.uc_mcontext = cur.to_sigcontext();
    frame.uc.uc_mcontext.oldmask = frame.uc.uc_sigmask;
    frame.info.si_signo = sig.cast_signed();
    match cause {
        // `si_pid`/`si_uid` stay 0: `deliver_signal` does not carry the sender's
        // identity, and 0 is `init`, which is who a kernel-raised signal is from.
        Cause::User => frame.info.si_code = 0,
        Cause::Fault { si_code, addr } => {
            frame.info.si_code = si_code;
            // `si_addr` is the first word of the union, which is what the
            // `_sigfault` arm puts there.
            frame.info.fields[0] = addr;
        }
    }

    if !crate::uaccess::write_val(base, frame) {
        akuma_primitives::safe_print!(128,
            "[signal] sig {} declined: frame write to {:#x} failed\n", sig, base);
        return None;
    }

    // The handler's ABI: `void h(int)` or, with SA_SIGINFO,
    // `void h(int, siginfo_t *, void *)`.
    let mut regs = [0u64; 12];
    regs[r::RDI] = u64::from(sig);
    if action.flags & sa::SIGINFO != 0 {
        regs[r::RSI] = base + core::mem::offset_of!(RtSigFrame, info) as u64;
        regs[r::RDX] = base + core::mem::offset_of!(RtSigFrame, uc) as u64;
    }
    Some(Regs {
        rip: entry as u64,
        rsp: base,
        // A fresh handler gets fresh flags: `DF` clear is what the ABI promises a
        // C function, and inheriting the interrupted `DF` is the same bug
        // `.Lexec_return` documents for `execve`.
        rflags: RFLAGS_FORCED,
        rax: 0,
        regs,
    })
}

/// `rt_sigreturn()` — x86_64 syscall 15.
///
/// Restores the register file the frame at `%rsp - 8` describes (the restorer
/// has already `ret`ted past `pretcode`) and re-arms `.Lsig_return`, so the
/// return value of this "syscall" is the interrupted `%rax` rather than
/// anything this function computes.
///
/// A frame this kernel did not write is a ring-3 program choosing a register
/// file, so every field that can hurt the kernel is filtered: `rip` must be a
/// user address (`sysret` faults in ring 0 on a non-canonical one), and `rflags`
/// goes through [`sanitize_rflags`].
pub fn sys_rt_sigreturn() -> u64 {
    let uctx_ptr = crate::smp::current_uctx();
    if uctx_ptr.is_null() {
        return errno::EFAULT;
    }
    // SAFETY: the running task's own `UserCtx`.
    let uctx = unsafe { &mut *uctx_ptr };

    // `%rsp` on entry to `rt_sigreturn` points just past `pretcode`, so the
    // frame starts 8 bytes below it.
    let Some(base) = uctx.user_rsp.checked_sub(8) else {
        return errno::EFAULT;
    };
    let Some(frame) = crate::uaccess::read_val::<RtSigFrame>(base) else {
        return errno::EFAULT;
    };

    let mut restored = Regs::from_sigcontext(&frame.uc.uc_mcontext);
    if !user_rip_ok(restored.rip) {
        akuma_primitives::safe_print!(128,
            "[signal] rt_sigreturn: bad rip {:#x} — killing\n", restored.rip);
        crate::usermode::exit_current_from_signal(11);
        return errno::EFAULT;
    }
    restored.rflags = sanitize_rflags(restored.rflags);

    // The mask the handler ran under is discarded and the frame's is installed —
    // that is what makes `SA_NODEFER`-less nesting unwind correctly.
    threading::set_thread_signal_mask(frame.uc.uc_sigmask);
    restored.store(uctx);
    uctx.sig_rax
}

/// `tkill(tid, sig)` / `tgkill(tgid, tid, sig)` — x86_64 200 and 234.
///
/// **Not folded to `akuma_syscalls_glue::sys_tkill`, and the reason is a
/// divergence worth keeping visible.** That function decides fatality *inline*:
/// a `SIG_DFL` signal whose default action is termination calls
/// `sys_exit_group` from inside the `tkill`, which on this target would leave
/// ring 3 through glue's epilogue instead of `run_process`'s — no `SPAWN` row
/// stamped, no `thread::drain`, the thread parked in a `yield_now` loop. Here
/// every signal is *pended* and every fatality decision belongs to
/// [`deliver_pending`], which is the one place that knows how this kernel
/// leaves ring 3.
///
/// The dispositions it does reproduce are glue's, because they are POSIX:
/// `SIG_IGN` drops the signal, `SIGKILL` is unconditional, and a blocked signal
/// pends rather than acting.
pub fn sys_tkill(tid: u32, sig: u32) -> u64 {
    if sig == 0 {
        return 0;
    }
    if sig as usize > akuma_exec::process::MAX_SIGNALS {
        return errno::EINVAL;
    }
    let tid = tid as usize;
    if tid >= threading::MAX_THREADS {
        return errno::ESRCH;
    }

    // SIGKILL cannot be caught, blocked or ignored, and it is the one signal the
    // epilogue must not be trusted with: the target may never make another
    // syscall. Kill the whole group the way `exit_group` does.
    if sig == 9 {
        crate::usermode::exit_current_from_signal(9);
        return 0;
    }

    let handler = akuma_exec::process::find_pid_by_thread(tid)
        .and_then(akuma_exec::process::lookup_process_shared)
        .map_or(SignalHandler::Default, |p| {
            p.signal_actions.actions.lock()[(sig - 1) as usize].handler
        });
    if matches!(handler, SignalHandler::Ignore) {
        return 0;
    }

    // **Pend, and do not touch `ProcessChannel::interrupted`.**
    //
    // That flag is the Ctrl-C sledgehammer, not a signal: glue's dispatch
    // prologue reads it on *every* syscall, marks the process a `Zombie(130)`
    // and returns `EINTR` — `SA_RESTART`-blind and disposition-blind by design,
    // because Ctrl-C's job is to end the foreground job. Setting it here made
    // `raise(3)` fail in the most confusing possible way: musl's `raise` is
    // `block-all` / `tgkill` / `restore`, and the `EINTR` landed on the
    // **restore**, so the signal stayed blocked forever and the handler never
    // ran. Glue's own `sys_tkill` does not set it either; the per-thread `EINTR`
    // decision belongs to `current_thread_has_pending_interrupt`, which reads
    // the pending set and honours `SA_RESTART`.
    threading::pend_signal_for_thread(tid, sig);
    0
}

/// See [`sys_tkill`]. The `tgid` check is what stops a recycled tid taking a
/// signal meant for the thread that used to own the slot.
pub fn sys_tgkill(tgid: u32, tid: u32, sig: u32) -> u64 {
    if let Some(pid) = akuma_exec::process::find_pid_by_thread(tid as usize)
        && let Some(proc) = akuma_exec::process::lookup_process_shared(pid)
        && proc.tgid != tgid
    {
        return errno::ESRCH;
    }
    sys_tkill(tid, sig)
}

// ===========================================================================
// Boot self-tests
// ===========================================================================

/// The pieces of signal delivery a kernel-side test can reach.
///
/// **Most of it cannot be tested from here and the split is deliberate.** The
/// interesting half is a *musl program's* view — `sigaction` recording a
/// handler, a frame the handler's `ret` returns through, `rt_sigreturn`
/// restoring a register file this kernel did not author — and the boot suite
/// runs inside the kernel on init's task, where none of that exists. That half
/// is `userspace/forktest/c_stress/sigprobe.c`, eight rungs, run by
/// `scripts/utils/amd64_ring3_check.py` and A/B'd against real Linux.
///
/// What is left here is exactly the code whose failure would be *silent* in
/// that probe as well: a transposed register index produces a program that
/// resumes with two values swapped, which a probe notices only if it happens to
/// have live data in both, and a frame offset that moves produces a handler
/// reading the wrong field of a structure it was handed a pointer to.
pub fn smoke_test(t: &mut akuma_selftest::Suite) {
    // The ABI, as sizes. The `const _` above already fails the build on a
    // reorder; these report the numbers in the tally, where a reader comparing
    // against `arch/x86/include/uapi/asm/sigcontext.h` can check them.
    t.check_eq("signal: sigcontext is 256 bytes",
        core::mem::size_of::<SigContext>() as u64, 256);
    t.check_eq("signal: ucontext is 304 bytes",
        core::mem::size_of::<UContext>() as u64, 304);
    t.check_eq("signal: rt_sigframe is 440 bytes",
        core::mem::size_of::<RtSigFrame>() as u64, 440);
    t.check_eq("signal: uc_mcontext at +40 in ucontext",
        core::mem::offset_of!(UContext, uc_mcontext) as u64, 40);
    t.check_eq("signal: siginfo at +312 in the frame",
        core::mem::offset_of!(RtSigFrame, info) as u64, 312);

    // **The register shuffle, both ways.** `UserCtx::saved_regs` is a 12-entry
    // array with a positional meaning (`syscall_entry`'s store order) and
    // `sigcontext` is a named struct; the mapping between them is twelve hand
    // written lines in each direction and a transposition in either is invisible
    // to the compiler. Distinct values, so a swap cannot cancel out.
    let mut regs = [0u64; 12];
    for (i, r) in regs.iter_mut().enumerate() {
        *r = 0x1000 + i as u64;
    }
    let before = Regs { rip: 0x4000, rsp: 0x7fff_0000, rflags: 0x246, rax: 0x99, regs };
    let sc = before.to_sigcontext();
    t.check("signal: sigcontext names the right registers",
        sc.rdi == regs[r::RDI] && sc.rsi == regs[r::RSI] && sc.rdx == regs[r::RDX]
            && sc.r10 == regs[r::R10] && sc.r8 == regs[r::R8] && sc.r9 == regs[r::R9]
            && sc.rbx == regs[r::RBX] && sc.rbp == regs[r::RBP]
            && sc.r12 == regs[r::R12] && sc.r13 == regs[r::R13]
            && sc.r14 == regs[r::R14] && sc.r15 == regs[r::R15]);
    let after = Regs::from_sigcontext(&sc);
    t.check("signal: sigcontext round-trips the register file",
        after.regs == before.regs
            && after.rip == before.rip
            && after.rsp == before.rsp
            && after.rax == before.rax
            && after.rflags == before.rflags);
    // `sysret` installs ring-3 selectors from `IA32_STAR`; a handler that reads
    // `uc_mcontext.cs` must see what it will be returned to.
    t.check_eq("signal: sigcontext cs is the sysret user selector",
        u64::from(sc.cs), u64::from(crate::gdt::SYSRET_BASE + 16));

    // **The `rflags` filter.** `sysret` loads `%r11` straight into `RFLAGS`, so
    // a frame is a ring-3 program choosing its own flags. `IF` clear in ring 3
    // is a machine that stops scheduling.
    t.check("signal: sanitize_rflags forces IF", sanitize_rflags(0) & 0x200 != 0);
    t.check("signal: sanitize_rflags strips IOPL and NT",
        sanitize_rflags(0x7000) & 0x7000 == 0);
    t.check("signal: sanitize_rflags keeps DF and TF",
        sanitize_rflags(0x500) & 0x500 == 0x500);
    t.check("signal: sanitize_rflags keeps the carry/zero flags",
        sanitize_rflags(0x41) & 0x41 == 0x41);

    // **The `sysret` guard.** A non-canonical `%rcx` faults in ring 0, so an
    // `rt_sigreturn` frame naming one is a ring-3 program choosing where this
    // kernel takes a `#GP`.
    t.check("signal: user_rip_ok refuses the null page", !user_rip_ok(0x800));
    t.check("signal: user_rip_ok refuses the kernel half",
        !user_rip_ok(0xFFFF_8000_0000_0000));
    t.check("signal: user_rip_ok refuses non-canonical",
        !user_rip_ok(0x0000_8000_0000_0000));
    t.check("signal: user_rip_ok admits a user address", user_rip_ok(0x40_0000));

    // **Delivery declines rather than jumping into nothing.** x86_64 Linux has
    // no kernel trampoline page, so `SA_RESTORER` is mandatory — the check is at
    // delivery here because glue's `rt_sigaction` is shared with AArch64, where
    // the field does not exist.
    let cur = Regs { rip: 0x40_0000, rsp: 0x7fff_f000, rflags: 0x202, rax: 0, regs: [0; 12] };
    let no_restorer = akuma_exec::process::SignalAction {
        handler: SignalHandler::UserFn(0x40_1000),
        flags: 0,
        mask: 0,
        restorer: 0,
    };
    t.check("signal: delivery declines without SA_RESTORER",
        build_frame(&cur, 10, 0x40_1000, &no_restorer, 0, Cause::User).is_none());
    let bad_handler = akuma_exec::process::SignalAction {
        handler: SignalHandler::UserFn(0),
        flags: sa::RESTORER,
        mask: 0,
        restorer: 0x40_2000,
    };
    t.check("signal: delivery declines a non-user handler",
        build_frame(&cur, 10, 0, &bad_handler, 0, Cause::User).is_none());

    // The fatal-default table this target shares with AArch64, spot-checked at
    // the two ends that matter: `SIGINT` must kill a foreground job, `SIGCHLD`
    // must not kill anything (every `fork` parent gets one).
    t.check("signal: SIGINT is fatal by default",
        akuma_syscalls_glue::signal::signal_is_fatal_default(2));
    t.check("signal: SIGCHLD is not",
        !akuma_syscalls_glue::signal::signal_is_fatal_default(17));

    // **The exception stub's register block, and the mapping off it.** This is
    // the *third* place the register order is spelled — `syscall_entry`'s store
    // order and `to_sigcontext` are the other two — and the only one whose
    // source is assembly in another file. Distinct values again, so a swap
    // cannot cancel out.
    let trap = crate::idt::TrapRegs {
        r15: 0x0f, r14: 0x0e, r13: 0x0d, r12: 0x0c,
        r11: 0x0b, r10: 0x0a, r9: 0x09, r8: 0x08,
        rbp: 0x05, rdi: 0x01, rsi: 0x02, rdx: 0x03,
        rcx: 0x04, rbx: 0x06, rax: 0x07,
    };
    let tframe = crate::idt::InterruptStackFrame {
        rip: 0x40_2000, cs: 0x23, rflags: 0x246, rsp: 0x7fff_1000, ss: 0x1b,
    };
    let from_trap = Regs::from_trap(&tframe, &trap);
    t.check("signal: the trap register file maps by name",
        from_trap.regs[r::RDI] == trap.rdi
            && from_trap.regs[r::RSI] == trap.rsi
            && from_trap.regs[r::RDX] == trap.rdx
            && from_trap.regs[r::R10] == trap.r10
            && from_trap.regs[r::R8] == trap.r8
            && from_trap.regs[r::R9] == trap.r9
            && from_trap.regs[r::RBX] == trap.rbx
            && from_trap.regs[r::RBP] == trap.rbp
            && from_trap.regs[r::R12] == trap.r12
            && from_trap.regs[r::R13] == trap.r13
            && from_trap.regs[r::R14] == trap.r14
            && from_trap.regs[r::R15] == trap.r15);
    t.check("signal: the trap frame carries rip/rsp/rflags/rax",
        from_trap.rip == tframe.rip
            && from_trap.rsp == tframe.rsp
            && from_trap.rflags == tframe.rflags
            && from_trap.rax == trap.rax);
    // `%rcx` is deliberately absent from `Regs`: it is the one register the
    // *syscall* path can never recover (the `syscall` instruction takes it), so
    // the shared shape does not carry it and the fault path drops it too. Stated
    // as a check so the asymmetry is a decision rather than an oversight.
    t.check_eq("signal: rcx is not carried (sigcontext reports 0)",
        from_trap.to_sigcontext().rcx, 0);
    t.check_eq("signal: the trap register block is 120 bytes",
        core::mem::size_of::<crate::idt::TrapRegs>() as u64, 120);

    // **The two checks above each print a `[signal] sig 10 declined: …` line and
    // each bump `DECLINED`.** That is the decline path doing its job, but two
    // unexplained lines in every boot's `dmesg` is exactly the noise that costs
    // somebody an hour later — so they are named here, and the counter is put
    // back to zero so the note below means what it says on a real workload.
    t.note("signal: the two `declined` lines above are this test's", 2);
    DECLINED.store(0, Ordering::Relaxed);

    // The counters, so a `dmesg` from any boot carries them. All three read 0
    // here, which is the point: a non-zero `declined` on a real workload is
    // programs being killed where Linux would have run their handler, and
    // nothing in ring 3 can see that happen.
    t.note("signal: delivered to a handler", DELIVERED.load(Ordering::Relaxed));
    t.note("signal: fatal default actions", DEFAULT_KILLS.load(Ordering::Relaxed));
    t.note("signal: deliveries declined", DECLINED.load(Ordering::Relaxed));
}
