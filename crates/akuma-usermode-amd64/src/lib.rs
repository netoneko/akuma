//! The full amd64 kernel's ring-3 crossing: the `syscall` landing stub, the
//! two ways into ring 3 (`sysretq` for a fresh image, a full register file for
//! a `fork`/`clone` child), the unwind back out, and the `IA32_*STAR` install.
//!
//! The counterpart of `akuma-ring3-amd64`, which is Taoxichuan's: that crate's
//! tasks return only through `sysretq` and die in the trap handler, so it has
//! no unwind, no fork entry and no per-task [`UserCtx`]. The two are meant to
//! become one crossing; until then each kernel keeps its own, and both keep
//! their `unsafe` here rather than in the kernel.
//!
//! # The contract with the kernel
//!
//! - **The handler.** The stub calls the [`SyscallHandler`] the kernel passed
//!   to [`install`]; no symbol is shared, so the kernel needs no `no_mangle`.
//! - **The per-CPU block.** `gs` (after `swapgs` on entry, before it on the
//!   way out) addresses the kernel's per-CPU block, with the running task's
//!   `*mut UserCtx` at [`PERCPU_CURRENT_UCTX`] and one scratch word at
//!   [`PERCPU_SCRATCH`]. The kernel pins its layout to these constants. The
//!   assembly checks the pointer is non-null; that it points at the running
//!   task's live `UserCtx` is the kernel's invariant, established where it
//!   writes `IA32_GS_BASE` and repointed by its scheduler on every switch.
//! - **The BKL** is the kernel's business: it releases it before [`enter`]
//!   and holds it again when [`enter`] returns.
//!
//! # What makes the entry points safe
//!
//! Each stub checks what it needs before it acts, and executes `ud2` if a
//! check fails: [`enter`] and [`enter_forked`] refuse a non-canonical or
//! kernel-half `rip` and a null per-CPU `UserCtx`; [`unwind`] refuses unless
//! the running task's `kernel_rsp` is live. `kernel_rsp` is private to this
//! crate, written only by the entry stubs and cleared by both exits.

#![no_std]
#![allow(clippy::too_long_first_doc_paragraph)]

/// `gs` offset of the running task's `*mut UserCtx` in the kernel's per-CPU
/// block. The stubs index it as `gs:[8]`.
pub const PERCPU_CURRENT_UCTX: usize = 8;
/// `gs` offset of the stubs' one scratch word in the kernel's per-CPU block
/// (`gs:[16]`).
pub const PERCPU_SCRATCH: usize = 16;

/// Where a task's kernel stack and saved user stack live.
///
/// One per task. `syscall_entry` reaches the running task's through the
/// per-CPU block (`gs:[8]`, `smp::current_uctx`); the scheduler repoints that
/// on every switch. They were two globals until multitasking made that wrong —
/// a syscall taken by one process would have written another's saved stack
/// pointer — and the pointer to them was one global until SMP made *that*
/// wrong the same way.
/// Field offsets are load-bearing: `syscall_entry` indexes this by hand as
/// `[rax + 0]`, `[rax + 8]`, `[rax + 16]` and `[rax + 32]`. Reordering the
/// fields silently changes what that assembly reads.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct UserCtx {
    /// Kernel stack to resume on: set by `enter_user_mode`, used by the syscall
    /// path and by the exit path, and cleared by both exits. Offset 0.
    ///
    /// Private, and that is the point: it is the stack [`unwind`] jumps to, so
    /// only this crate's assembly writes it. Non-zero means "this task is in
    /// ring 3, or in a syscall from it, and its `enter_user_mode` frame is live".
    kernel_rsp: u64,
    /// The task's user stack, saved on syscall entry. Offset 8.
    pub user_rsp: u64,
    /// Non-zero when this task should leave ring 3. Offset 16.
    pub leave: u64,
    /// Unused; keeps `user_rip` at offset 32, where `syscall_entry` writes it.
    /// It was `proc_slot`, the task's row in the `SPAWN` table that the process
    /// table replaced (2026-11-12): a task runs a process when
    /// `current_process()` answers.
    pub reserved_24: usize,
    /// The user instruction pointer captured on syscall entry — the address the
    /// `syscall` will return to. Offset 32. `syscall_entry` writes it (as
    /// `[rax + 32]`) so `sys_fork` can hand a child task the exact point the
    /// parent will resume from, which is what makes `vfork` "return twice".
    pub user_rip: u64,
    /// This task's `%fs` base (musl's TLS pointer). Offset 40 — not indexed by
    /// assembly. `arch_prctl(ARCH_SET_FS)` records it here and the scheduler
    /// `wrmsr`s it back on switch, because `IA32_FS_BASE` is one CPU-global
    /// register and two user tasks (a shell and the child it forked) each need
    /// their own. `0` means "never set" — the scheduler leaves the MSR alone.
    pub fs_base: u64,
    /// The user register set captured on every syscall entry, in the order
    /// `syscall_entry` writes it (offset 48):
    /// `[rdi, rsi, rdx, r10, r8, r9, rbx, rbp, r12, r13, r14, r15]`.
    /// `sys_fork` copies this into the child so it resumes as a true
    /// full-register copy of the parent — see `enter_user_mode_forked`.
    pub saved_regs: [u64; 12],
    /// This task's user `%gs` base. Offset 144 — not indexed by assembly.
    /// `arch_prctl(ARCH_SET_GS)` records it; the scheduler writes it to
    /// `IA32_KERNEL_GS_BASE` on every switch, which is the register `swapgs`
    /// turns into the program's `GS_BASE` on the way back to ring 3. The kernel
    /// keeps its own per-CPU block in the other half of that pair (`smp.rs`),
    /// so the program's value can never be written to `IA32_GS_BASE` directly.
    pub gs_base: u64,
    /// Unused; keeps the offsets below it where `syscall_entry` indexes them
    /// (`exec_pending` at 168). It was `thread_slot`, a task's row in the
    /// thread table that the process table replaced (2026-11-12): whether a
    /// task is a thread is `pid != tgid` of its `Process`.
    pub reserved_152: usize,
    /// Non-zero when this task's **first** ring-3 entry must resume with the
    /// parent's full register set (`enter_user_mode_forked`) rather than at a
    /// fresh `_start`. Offset 160 — not indexed by assembly.
    ///
    /// # Why it lives here and not on the process
    ///
    /// 5b slice 4 had to find this a home when `PROCS` was deleted, and this is
    /// the one place where the rest of the same fact already lives:
    /// [`crate::sched::seed_forked_task`] seeds `saved_regs`, `fs_base` and
    /// `gs_base` into this very struct, and `forked` says nothing more than
    /// "use them". Putting it on the registered `akuma_exec::Process` would
    /// have split one decision across two structures — and it is not a property
    /// of the *image* either: an `execve` in a `fork` child replaces the image
    /// and the flag is already spent by then, because [`run_process`] clears it
    /// on the first entry.
    ///
    /// On AArch64 there is no equivalent because there is nothing to say: that
    /// kernel `eret`s from `ProcessImage::context`, which either *is* the
    /// parent's register set or is not.
    pub forked: u64,
    /// Non-zero when `execve` has installed a new image on this task and
    /// [`run_process`] must re-enter ring 3 instead of treating the ring-3 exit
    /// as the process ending. Offset 168 — not indexed by assembly.
    ///
    /// This replaced `static mut PENDING_EXEC`, a second `PROC_SLOTS`-wide
    /// array of half-built processes. There is at most one pending `execve` per
    /// *task* — the task that called it, which is the same task that consumes
    /// it — so per-task state is what it always was; the array was addressing
    /// by process slot for want of anywhere else to put one bit.
    pub exec_pending: u64,
    /// The user `RFLAGS` captured on syscall entry — the `%r11` the `syscall`
    /// instruction delivered. Offset 176, and **indexed by hand**: the ordinary
    /// return pops `%r11` off the kernel stack, but `.Lsig_return` has abandoned
    /// that frame and has to read the flags from somewhere.
    ///
    /// It is also the only place the interrupted flags can be *read* from Rust,
    /// which a signal frame's `uc_mcontext.eflags` needs. Nothing wrote it
    /// before signals existed, because nothing needed it: `enter_user_mode` and
    /// `.Lexec_return` both synthesise `0x202` for a context that has no history.
    pub user_rflags: u64,
    /// Non-zero when this task must return to ring 3 with the register file in
    /// this `UserCtx` rather than the one on the kernel stack. Offset 184,
    /// indexed by hand (`.Lsig_return`'s selector).
    ///
    /// Set by both halves of signal handling, which is why there is one flag and
    /// one return path: entering a handler and returning from one are the same
    /// operation — install a register file and `sysret` into it — and differ
    /// only in who filled it (`signal::deliver_pending` vs
    /// `signal::sys_rt_sigreturn`).
    ///
    /// Consumed by `.Lsig_return` itself, like `exec_pending`. It can be left
    /// **set** on one path — a fatal default action sets `leave` in the same
    /// epilogue, and the assembly tests `leave` first — which for `leave` was
    /// once a real bug ("a second entry with it still set returns immediately
    /// after the first syscall"). It cannot be one here: a task that takes
    /// `.Lexit_to_kernel` never re-enters ring 3, and `sched::prepare_task_slot`
    /// assigns a whole fresh `UserCtx::new()` before a recycled slot runs
    /// anything, so the flag cannot outlive its task.
    pub sig_return: u64,
    /// The `%rax` `.Lsig_return` enters ring 3 with. Offset 192, indexed by hand.
    ///
    /// Separate from `saved_regs` because `rax` is not in it: the ABI clobbers
    /// `rax` across a `syscall`, so `syscall_entry` never saves it and the
    /// ordinary return supplies it from the handler's result. A signal return
    /// has no handler result — it carries either the handler's entry `rax` (0)
    /// or the `rax` the interrupted context had.
    pub sig_rax: u64,
    /// The `%rcx` `.Lsig_return` enters ring 3 with. Offset 200, indexed by hand.
    ///
    /// Not in `saved_regs` because `syscall_entry` cannot save it — the
    /// `syscall` instruction has already replaced it with the return address —
    /// and not derivable at return time either: a signal delivered at a **tick**
    /// or a **fault** interrupted an instruction with `%rcx` live, and
    /// `rt_sigreturn` must put that value back. The old `sysretq` return could
    /// not (it loads `%rcx` from `%rip`), which is how Go's `memmove` resumed
    /// with `RCX = RIP` (`docs/archive/AMD64_FORK_VS_THREADS_SEGV.md`); the
    /// return is an `iretq` now and reads this.
    pub sig_rcx: u64,
    /// The `%r11` `.Lsig_return` enters ring 3 with. Offset 208, indexed by
    /// hand. `%r11`'s story is `sig_rcx`'s with `rflags` for `rip`.
    pub sig_r11: u64,
}

/// **The offsets `syscall_entry` indexes by hand.**
///
/// Nine of them now. `exec_pending` (168) is the one that made this worth
/// spelling: the return path branches on it and then takes the program counter
/// it `sysret`s to from `user_rip`. A reordered field would not fail to compile,
/// would not fail a boot check, and would send `execve` into whatever word had
/// moved into 32 — which on this target is the entire mechanism by which any
/// program is ever replaced.
///
/// `sig_return` (184) is a second such branch and `user_rflags` (176) /
/// `sig_rax` (192) are two more hand-indexed loads, with the same property: a
/// signal handler entered with the wrong `%rip` is a `SIGSEGV` in a program that
/// was handling a signal correctly.
const _: () = {
    assert!(core::mem::offset_of!(UserCtx, kernel_rsp) == 0);
    assert!(core::mem::offset_of!(UserCtx, user_rsp) == 8);
    assert!(core::mem::offset_of!(UserCtx, leave) == 16);
    assert!(core::mem::offset_of!(UserCtx, user_rip) == 32);
    assert!(core::mem::offset_of!(UserCtx, saved_regs) == 48);
    assert!(core::mem::offset_of!(UserCtx, exec_pending) == 168);
    assert!(core::mem::offset_of!(UserCtx, user_rflags) == 176);
    assert!(core::mem::offset_of!(UserCtx, sig_return) == 184);
    assert!(core::mem::offset_of!(UserCtx, sig_rax) == 192);
    assert!(core::mem::offset_of!(UserCtx, sig_rcx) == 200);
    assert!(core::mem::offset_of!(UserCtx, sig_r11) == 208);
};

impl UserCtx {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            kernel_rsp: 0,
            user_rsp: 0,
            leave: 0,
            reserved_24: 0,
            user_rip: 0,
            fs_base: 0,
            saved_regs: [0; 12],
            gs_base: 0,
            reserved_152: 0,
            forked: 0,
            exec_pending: 0,
            user_rflags: 0,
            sig_return: 0,
            sig_rax: 0,
            sig_rcx: 0,
            sig_r11: 0,
        }
    }
}

impl Default for UserCtx {
    fn default() -> Self {
        Self::new()
    }
}

/// The head of a core's per-CPU block: the words read through `gs` before
/// anything else can be trusted. `syscall_entry` reads `current_uctx` and
/// `scratch` with no stack; `akuma_cpu::percpu` reads `index` and
/// `current_task` (`gs:[24]`, `gs:[32]`) for every core-id and tid lookup.
///
/// A kernel's per-CPU block embeds this as its **first** field, so the base
/// [`install_percpu`] writes is both the head and the block. Owning the type
/// here is what makes that write safe: the base can only be a `&'static`
/// `PerCpuHead`, whose layout the readers are pinned to below.
#[repr(C)]
pub struct PerCpuHead {
    /// Address of this head. `gs:[0]`.
    self_ptr: core::sync::atomic::AtomicU64,
    /// The running task's `*mut UserCtx`. `gs:[8]`. The scheduler repoints it
    /// on every switch; the entry stubs refuse it null.
    pub current_uctx: core::sync::atomic::AtomicU64,
    /// One word of scratch for the syscall stub. `gs:[16]`.
    scratch: core::sync::atomic::AtomicU64,
    /// This core's index. `gs:[24]`, written by [`install_percpu`] (and by a BSP
    /// preparing an AP). Every reader bounds-checks it.
    pub index: core::sync::atomic::AtomicU32,
    /// This core's LAPIC id. `gs:[28]`; read by no assembly, it fills the gap.
    pub lapic_id: core::sync::atomic::AtomicU32,
    /// The scheduler task slot running on this core. `gs:[32]`.
    pub current_task: core::sync::atomic::AtomicUsize,
}

const _: () = {
    assert!(core::mem::offset_of!(PerCpuHead, self_ptr) == 0);
    assert!(core::mem::offset_of!(PerCpuHead, current_uctx) == PERCPU_CURRENT_UCTX);
    assert!(core::mem::offset_of!(PerCpuHead, scratch) == PERCPU_SCRATCH);
    // `akuma_cpu::percpu::core_id` / `current_tid`.
    assert!(core::mem::offset_of!(PerCpuHead, index) == 24);
    assert!(core::mem::offset_of!(PerCpuHead, current_task) == 32);
};

impl PerCpuHead {
    #[must_use]
    pub const fn new() -> Self {
        use core::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize};
        Self {
            self_ptr: AtomicU64::new(0),
            current_uctx: AtomicU64::new(0),
            scratch: AtomicU64::new(0),
            index: AtomicU32::new(0),
            lapic_id: AtomicU32::new(0),
            current_task: AtomicUsize::new(0),
        }
    }

    /// This core's index, as [`install_percpu`] recorded it.
    #[must_use]
    pub fn index(&self) -> u32 {
        self.index.load(core::sync::atomic::Ordering::Relaxed)
    }
}

impl Default for PerCpuHead {
    fn default() -> Self {
        Self::new()
    }
}

/// Point this core's `%gs` at `head`, recording `index` in it, and zero
/// `IA32_KERNEL_GS_BASE` (the user's half of the pair; a fresh core has no
/// user yet).
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
pub fn install_percpu(head: &'static PerCpuHead, index: u32) {
    use core::sync::atomic::Ordering;
    let addr = core::ptr::from_ref::<PerCpuHead>(head) as u64;
    head.self_ptr.store(addr, Ordering::Relaxed);
    head.index.store(index, Ordering::Relaxed);
    switch::set_gs_base(addr);
    let _ = akuma_cpu::msr::set_user_gs_base(0);
}

/// Why [`enter`] or [`enter_forked`] would not enter ring 3.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    /// `rip` is not a canonical lower-half address. `sysretq` to it would
    /// fault in ring 0 on Intel parts.
    NotUserRip,
}

/// Whether `rip` is somewhere `sysretq` may go: canonical and in the lower
/// half, bits 63..47 clear.
#[must_use]
pub const fn is_user_rip(rip: u64) -> bool {
    rip >> 47 == 0
}

/// Flags `syscall` clears on entry (`IA32_FMASK`).
///
/// `IF`, so the stub never runs with interrupts on while `rsp` still points
/// into user memory. `DF`, so the kernel's string operations start from a known
/// direction. `AC`: with `CR4.SMAP` on, a program that sets `AC` and executes
/// `syscall` would otherwise enter the kernel with SMAP suspended. `TF`:
/// `syscall` is not an interrupt gate and does not clear it, and userspace may
/// set it (`sigreturn` permits it for debuggers), so a single-stepping program
/// would make the kernel single-step. `IOPL` and `NT`: `sysret` loads them from
/// `r11`, and `NT` set turns the kernel's next `iret` into a task switch. The
/// arithmetic flags are left alone; the ABI lets a syscall clobber them.
pub const FMASK: u64 = (1 << 9) | (1 << 10) | (1 << 18) | (1 << 8) | (3 << 12) | (1 << 14);

#[cfg(all(target_arch = "x86_64", target_os = "none"))]
mod switch {
    use akuma_cpu::msr::{self, Msr};

core::arch::global_asm!(
    r#"
    /* Naming the section is mandatory here — see akuma-kernel-amd64's sched.rs for why a missing
     * `.section` puts code in .bss and fails the link. */
    .section .text

.global syscall_entry
syscall_entry:
    /* Entered from ring 3. rcx = user rip, r11 = user rflags, rsp = the USER's
     * stack. Interrupts are off (IA32_FMASK clears IF), so this window cannot
     * be interrupted while rsp still points at user memory.
     *
     * `swapgs` first: %gs now holds this core's per-CPU block (`smp.rs`), and
     * the program's GS base is parked in IA32_KERNEL_GS_BASE until the matching
     * `swapgs` before `sysretq`. Everything this stub needs before it has a
     * stack — the running task's UserCtx, one word of scratch — is reached
     * through gs:[..], which is why two cores can be in here at once.
     *
     * Every saved slot is per-task, reached through the UserCtx pointer at
     * gs:[8]. With more than one process, globals would be wrong twice over: a
     * syscall by task A would overwrite task B's saved user stack, and a
     * context switch *inside* a syscall would corrupt whichever kernel stack
     * the two shared. */
    swapgs
    mov gs:[16], rax                /* percpu.scratch  = nr, while rax is a pointer */
    mov rax, gs:[8]                 /* percpu.current_uctx */
    mov [rax + 8], rsp              /* uctx.user_rsp   = user rsp   */
    mov [rax + 32], rcx             /* uctx.user_rip   = return addr (for vfork) */
    mov [rax + 176], r11            /* uctx.user_rflags = the flags `syscall` took */
    /* Full user register snapshot into uctx.saved_regs[12] (offset 48). Every
     * register the Linux syscall ABI preserves across `syscall` is still the
     * caller's here — `vfork` hands this exact set to the child so it resumes
     * as a true copy of the parent's context, not with garbage in r12-r15/rbx
     * that a C compiler assumed survived the call. rax (the nr) and rcx/r11
     * (clobbered by `syscall` itself) are not in the set. */
    mov [rax + 48], rdi
    mov [rax + 56], rsi
    mov [rax + 64], rdx
    mov [rax + 72], r10
    mov [rax + 80], r8
    mov [rax + 88], r9
    mov [rax + 96], rbx
    mov [rax + 104], rbp
    mov [rax + 112], r12
    mov [rax + 120], r13
    mov [rax + 128], r14
    mov [rax + 136], r15
    mov rsp, [rax + 0]              /* kernel stack    = uctx.kernel_rsp */
    mov rax, gs:[16]

    push rcx                        /* user rip    */
    push r11                        /* user rflags */
    push rax                        /* nr — kept only to balance the frame */

    /* The Linux x86_64 syscall ABI clobbers exactly three registers — rax
     * (the result), rcx and r11 (which the `syscall` instruction itself takes).
     * EVERYTHING ELSE IS PRESERVED, argument registers included, and a compiler
     * targeting that ABI relies on it: it will happily leave a live value in r8
     * across a syscall and never reload it.
     *
     * That is not hypothetical. It is what this stage's ELF program did — see
     * `docs/archive/AKUMA_FIRECRACKER_AMD64.md` §3.18.1. The hand-assembled
     * programs before it kept their state in r12/r13, which are callee-saved and
     * therefore preserved by `syscall_handler` for free, so the kernel got away
     * with clobbering the argument registers for five stages.
     *
     * rbx, rbp and r12-r15 need nothing here: `syscall_handler` is
     * `extern "C"`, so the compiler preserves them, and a context switch taken
     * inside it saves them too.
     *
     * Six pushes is 48 bytes, a multiple of 16, so the alignment the `sub rsp, 8`
     * below establishes is unchanged by adding them. */
    push rdi
    push rsi
    push rdx
    push r8
    push r9
    push r10

    /* a6, as System V's **seventh** argument — the first stack one, which at
     * the `call` sits at [rsp+0]. This replaces a bare `sub rsp, 8`: it moves
     * the stack by the same 8 bytes, so the 16-byte alignment that `sub`
     * established is unchanged, and it spends those 8 bytes on something.
     *
     * `futex` is why. It takes six arguments and the sixth is not optional —
     * Rust's `std` emits `FUTEX_WAIT_BITSET`, whose `val3` *is* the bitset, for
     * every timed wait, and a zero bitset is `EINVAL` by the crate's own decode
     * rule 2. `mmap`'s offset is the other user, still unreached (this target
     * has no file-backed mappings), but it costs nothing to be ready.
     *
     * r9 still holds the user's a6 here: the shuffle below is what clobbers it,
     * and it has not run yet. Moving this push after `mov r9, r8` would push a5
     * twice — a bug that would look like a futex whose bitset is its uaddr2. */
    push r9

    /* Linux arg registers into System V positions:
     *   Linux:    nr=rax  a1=rdi  a2=rsi  a3=rdx  a4=r10  a5=r8   a6=r9
     *   System V: 1 =rdi  2 =rsi  3 =rdx  4 =rcx  5 =r8   6 =r9   7 =[rsp]
     *
     * The order is load-bearing: every move must read its source before some
     * later move overwrites it. `r9 <- r8` precedes `r8 <- r10` for that reason,
     * and the rdx/rsi/rdi chain is assigned right-to-left for the same one.
     *
     * `rcx` is free even though `syscall` put the user return address there — it
     * was pushed above and is restored below. `r10` is free for the mirror
     * reason: it is caller-saved, was pushed above, and the ABI's a4 lives there
     * precisely because System V's 4th argument register is `rcx`, which
     * `syscall` destroys. */
    mov r9, r8
    mov r8, r10
    mov rcx, rdx
    mov rdx, rsi
    mov rsi, rdi
    mov rdi, rax
    call qword ptr [rip + {handler}] /* the kernel's handler (`install`); result in rax */

    add rsp, 8
    pop r10                         /* restore what the ABI promises userspace */
    pop r9
    pop r8
    pop rdx
    pop rsi
    pop rdi
    pop rcx                         /* discard the saved nr; rcx is scratch
                                       until the user rip is popped below */

    /* Whether to return to ring 3 is the handler's decision, not a property of
     * the syscall number: `exit` and `exit_group` are different numbers, and on
     * another architecture different again. Per-task, because a process that
     * exits must not make the *next* process return early from its own
     * syscall. */
    mov rcx, gs:[8]
    cmp qword ptr [rcx + 16], 0     /* uctx.leave */
    jne .Lexit_to_kernel
    cmp qword ptr [rcx + 168], 0    /* uctx.exec_pending */
    jne .Lexec_return
    cmp qword ptr [rcx + 184], 0    /* uctx.sig_return */
    jne .Lsig_return

    pop r11                         /* user rflags */
    pop rcx                         /* user rip    */
    /* rax holds the result and must survive the stack switch; rcx and r11 are
     * now live for sysretq, so the scratch slot is the only place left. */
    mov gs:[16], rax
    mov rax, gs:[8]
    mov rsp, [rax + 8]              /* back to this task's user stack */
    mov rax, gs:[16]
    /* The program's %gs back, the kernel's parked. The BKL was released in
     * `syscall_handler` already; nothing between there and here touches
     * shared state. */
    swapgs
    sysretq

.Lexec_return:
    /* **`execve` returns into the new image**, which is how Linux does it and
     * is what retires this target's `execve` loop.
     *
     * The pushed `user rip`/`user rflags` and the whole kernel frame are
     * abandoned deliberately: they describe the program that just ceased to
     * exist. `rsp` is overwritten below, and `syscall_entry` re-establishes the
     * kernel stack from `uctx.kernel_rsp` on every entry, so nothing leaks.
     *
     * **Every register is zeroed**, and that is not hygiene. Linux clears the
     * register file across `execve`, and System V says `%rdx` at process entry
     * is a function pointer to register with `atexit` — musl's `_start` passes
     * it straight to `__libc_start_main` as `rtld_fini`. Left holding the old
     * image's third syscall argument, the new program would call it on the way
     * out. `enter_user_mode` gets this for free (it is reached from Rust, so
     * `rdx` holds its own third argument, 0); this path has to say it.
     *
     * `r11 = 0x202` for the same reason: `IF` set, and `DF` **clear**, which
     * `execve` guarantees a fresh image. Returning the old program's flags
     * would hand the new one a direction flag it never set.
     *
     * Still in ring 0 with the kernel's `%gs`; the `swapgs` below is the same
     * one the ordinary return does. */
    mov qword ptr [rcx + 168], 0    /* consume uctx.exec_pending */
    mov rsp, [rcx + 8]              /* uctx.user_rsp — the new image's stack */
    mov rcx, [rcx + 32]             /* uctx.user_rip — its entry point */
    mov r11, 0x202
    xor eax, eax
    xor edi, edi
    xor esi, esi
    xor edx, edx
    xor r8d, r8d
    xor r9d, r9d
    xor r10d, r10d
    xor ebx, ebx
    xor ebp, ebp
    xor r12d, r12d
    xor r13d, r13d
    xor r14d, r14d
    xor r15d, r15d
    swapgs
    sysretq

.Lsig_return:
    /* **Return to ring 3 with the register file in `UserCtx`**, not the one on
     * the kernel stack. Both halves of signal handling come through here:
     * entering a handler (`signal::deliver_pending` filled the file with the
     * handler's ABI) and leaving one (`signal::sys_rt_sigreturn` filled it from
     * the frame on the user stack). Those are the same operation with two
     * authors, so they share one path.
     *
     * **`iretq`, not `sysretq`.** `sysretq` loads `rip` from `rcx` and
     * `rflags` from `r11`, so it cannot hand ring 3 a chosen `rcx`/`r11` — and
     * a signal delivered at a tick or a fault interrupted an instruction that
     * had both live. Returning through `sysretq` gave the interrupted code
     * `RCX = its own RIP`, which is the Go `memmove` crash in
     * `docs/archive/AMD64_FORK_VS_THREADS_SEGV.md`. `iretq` takes `rip`, `cs`,
     * `rflags`, `rsp`, `ss` off the stack and leaves every GPR alone, so all
     * sixteen come from the `UserCtx`. The frame is built on this task's
     * kernel stack, below the syscall frame it abandons; `syscall_entry`
     * re-establishes the kernel stack from `uctx.kernel_rsp` on every entry,
     * so nothing leaks. The selectors are the GDT's ring-3 rows, RPL 3,
     * exactly what `sysretq` derived from `IA32_STAR`.
     *
     * The pushed `user rip`/`user rflags` and the rest of the kernel frame are
     * abandoned exactly as `.Lexec_return` abandons them, and for the same
     * reason: they describe a context this return is not going to.
     *
     * `rcx` holds the uctx pointer on entry and is reloaded LAST, because every
     * other load is indexed off it.
     *
     * Still in ring 0 with the kernel's `%gs`; the `swapgs` below is the one the
     * ordinary return does. */
    mov qword ptr [rcx + 184], 0    /* consume uctx.sig_return */
    push {user_ss}                  /* iretq frame: ss                */
    push qword ptr [rcx + 8]        /*              rsp  = user_rsp   */
    push qword ptr [rcx + 176]      /*              rflags = user_rflags (sanitized by the author) */
    push {user_cs}                  /*              cs                */
    push qword ptr [rcx + 32]       /*              rip  = user_rip   */
    mov rax, [rcx + 192]            /* uctx.sig_rax */
    mov rdi, [rcx + 48]
    mov rsi, [rcx + 56]
    mov rdx, [rcx + 64]
    mov r10, [rcx + 72]
    mov r8,  [rcx + 80]
    mov r9,  [rcx + 88]
    mov rbx, [rcx + 96]
    mov rbp, [rcx + 104]
    mov r12, [rcx + 112]
    mov r13, [rcx + 120]
    mov r14, [rcx + 128]
    mov r15, [rcx + 136]
    mov r11, [rcx + 208]            /* uctx.sig_r11 */
    mov rcx, [rcx + 200]            /* uctx.sig_rcx — last: it is the base */
    swapgs
    iretq

.Lexit_to_kernel:
    /* Leave ring 3 for good. Restore what enter_user_mode saved and return from
     * it; rax still holds the handler's result. rcx already points at the
     * uctx. Still in ring 0, so %gs stays the kernel's: no swapgs. */
    mov rsp, [rcx + 0]
    mov qword ptr [rcx + 0], 0      /* the frame is being left: unwind may not use it */
    pop r15
    pop r14
    pop r13
    pop r12
    pop rbx
    pop rbp
    ret

.global enter_user_mode
enter_user_mode:
    /* The two checks that make this a `safe fn`. `sysretq` with a non-canonical
     * rcx raises #GP in ring 0 on Intel, with the user's rsp already loaded; a
     * ring-3 rip has bits 63..47 clear. And the per-CPU block must name a
     * UserCtx: everything below writes through it. */
    mov rax, rdi
    shr rax, 47
    jnz .Lrefuse_entry
    cmp qword ptr gs:[8], 0
    je .Lrefuse_entry
    /* rdi = user rip, rsi = user rsp, rdx = rax to enter ring 3 with. Returns
     * when userspace exits. The rdx value is 0 for a fresh program (_start
     * ignores rax) and 0 for a `vfork` child too — that 0 is what the child
     * sees as `vfork`'s return value. */
    push rbp
    push rbx
    push r12
    push r13
    push r14
    push r15

    mov rax, gs:[8]                 /* percpu.current_uctx */
    /* Publish this task's kernel stack: both the syscall path and the exit path
     * resume on it. */
    mov [rax + 0], rsp
    /* Clear the leave flag. Its lifetime is exactly one excursion, and it is set
     * on the way out — so a second entry with it still set returns immediately
     * after the first syscall, whatever that syscall was. That is not
     * hypothetical: it made a second process report its write() length (55) as
     * its exit status instead of 0x0B. */
    mov qword ptr [rax + 16], 0

    mov rcx, rdi                    /* sysret takes rip from rcx */
    mov r11, 0x202                  /* ...and rflags from r11: IF set, bit 1 reserved-1 */
    mov rsp, rsi                    /* user stack */
    mov rax, rdx                    /* ring-3 entry rax (vfork child return value) */
    swapgs                          /* the program's %gs; the kernel's is parked */
    sysretq

.global enter_user_mode_forked
enter_user_mode_forked:
    /* The two checks that make this a `safe fn`. `sysretq` with a non-canonical
     * rcx raises #GP in ring 0 on Intel, with the user's rsp already loaded; a
     * ring-3 rip has bits 63..47 clear. And the per-CPU block must name a
     * UserCtx: everything below writes through it. */
    mov rax, rdi
    shr rax, 47
    jnz .Lrefuse_entry
    cmp qword ptr gs:[8], 0
    je .Lrefuse_entry
    /* rdi = user rip, rsi = user rsp. Enter ring 3 as a `vfork` child: a
     * full-register copy of the parent's context at its `syscall` (read from
     * this task's own uctx.saved_regs, which `sys_fork` filled from the
     * parent), with rax = 0 (the child's `vfork` return value). Returns to the
     * caller when the child leaves ring 3, exactly like `enter_user_mode`. */
    push rbp
    push rbx
    push r12
    push r13
    push r14
    push r15

    mov rax, gs:[8]                /* rax = this task's uctx, kept until the end */
    mov [rax + 0], rsp             /* publish this task's kernel stack */
    mov qword ptr [rax + 16], 0    /* clear the leave flag */

    mov rcx, rdi                   /* sysret rip */
    mov r11, 0x202                 /* sysret rflags */

    /* Restore the parent's register set from uctx.saved_regs (offset 48):
     * [rdi, rsi, rdx, r10, r8, r9, rbx, rbp, r12, r13, r14, r15]. rsi (the
     * user stack) and rax (the uctx pointer) are consumed last. */
    mov rbp, [rax + 104]
    mov r12, [rax + 112]
    mov r13, [rax + 120]
    mov r14, [rax + 128]
    mov r15, [rax + 136]
    mov r8,  [rax + 80]
    mov r9,  [rax + 88]
    mov r10, [rax + 72]
    mov rdi, [rax + 48]
    mov rbx, [rax + 96]
    mov rdx, [rax + 64]
    mov rsp, rsi                   /* user stack (before rsi is reloaded) */
    mov rsi, [rax + 56]
    xor eax, eax                   /* child sees vfork() == 0 */
    swapgs
    sysretq

.Lrefuse_entry:
    ud2

/* `akuma_usermode_unwind(status)`: leave ring 3 from outside the syscall path
 * (an exception or a tick), returning `status` to whoever called
 * `enter_user_mode` -- the same sequence `.Lexit_to_kernel` runs. Refuses
 * (ud2) unless the per-CPU block names a UserCtx whose kernel_rsp is live,
 * which only `enter_user_mode` and `enter_user_mode_forked` set and both exits clear. */
.global akuma_usermode_unwind
akuma_usermode_unwind:
    mov rcx, gs:[8]
    test rcx, rcx
    jz .Lrefuse_entry
    mov rdx, [rcx + 0]
    test rdx, rdx
    jz .Lrefuse_entry
    mov qword ptr [rcx + 0], 0
    mov rax, rdi
    mov rsp, rdx
    pop r15
    pop r14
    pop r13
    pop r12
    pop rbx
    pop rbp
    ret
"#,
    // `.Lsig_return`'s `iretq` selectors: the same ring-3 rows `sysretq`
    // derives from `IA32_STAR`, taken from the GDT crate so they cannot drift.
    user_cs = const akuma_gdt::USER_CS as u64,
    user_ss = const akuma_gdt::USER_SS as u64,
    handler = sym SYSCALL_HANDLER,
);

    unsafe extern "C" {
        /// Drop to ring 3 at `rip` with stack `rsp` and `rax = entry_rax`;
        /// returns the status the task left ring 3 with. Checks `rip` and the
        /// per-CPU `UserCtx` itself (`ud2` otherwise); any `rsp` is the
        /// program's own problem.
        pub safe fn enter_user_mode(rip: u64, rsp: u64, entry_rax: u64) -> u64;
        /// As `enter_user_mode`, with the register file of the running task's
        /// `UserCtx::saved_regs` and `rax = 0`: a `fork`/`clone` child.
        pub safe fn enter_user_mode_forked(rip: u64, rsp: u64) -> u64;
        /// Return `status` from the running task's `enter_user_mode`; `ud2`
        /// if it has none live.
        pub safe fn akuma_usermode_unwind(status: u64) -> !;
        /// The `IA32_LSTAR` target. Never called from Rust: it expects the
        /// state `syscall` leaves (user `rsp`, `rcx`/`r11`), so it is declared
        /// unsafe and only its address is taken.
        fn syscall_entry();
    }

    /// `EFER.SCE`: without it, `syscall` raises `#UD`.
    const EFER_SCE: u64 = 1 << 0;

    /// `IA32_GS_BASE := addr`, where `addr` is a `&'static PerCpuHead`.
    pub fn set_gs_base(addr: u64) {
        // SAFETY: `install_percpu`'s only caller passes the address of a
        // `&'static PerCpuHead`, valid for the kernel's life and laid out as every
        // `gs:` reader expects (the offset asserts above). Every per-CPU read
        // dereferences this base; it can be no other pointer.
        unsafe { msr::write(Msr::GsBase, addr) };
    }

    /// The kernel's syscall handler, called by `syscall_entry` through this
    /// word. Written by `install` before it writes `LSTAR`, so no `syscall`
    /// can reach the stub while it is 0.
    static SYSCALL_HANDLER: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

    pub fn install(handler: super::SyscallHandler) {
        SYSCALL_HANDLER.store(handler as usize, core::sync::atomic::Ordering::Release);
        let star = akuma_gdt::star();
        // SAFETY: the four architectural MSRs of the fast-syscall path. LSTAR is
        // this crate's stub, STAR is the GDT crate's row order (what `sysretq`
        // derives the user selectors from), and FMASK clears every flag the
        // stub must not run with. Rewriting them with the same values is
        // idempotent.
        unsafe {
            msr::write(Msr::Efer, msr::read(Msr::Efer) | EFER_SCE);
            msr::write(Msr::Star, star);
            msr::write(Msr::Lstar, syscall_entry as *const () as usize as u64);
            msr::write(Msr::Fmask, super::FMASK);
        }
    }
}

/// The kernel side of a `syscall`: `(nr, a1..a6) -> rax`, System V, called by
/// `syscall_entry` on the task's kernel stack with interrupts off.
pub type SyscallHandler = extern "C" fn(u64, u64, u64, u64, u64, u64, u64) -> u64;

/// Enable `syscall`/`sysret` on this core: record `handler` for the stub, then
/// `EFER.SCE`, `STAR`, `LSTAR` (this crate's stub) and [`FMASK`].
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
pub fn install(handler: SyscallHandler) {
    switch::install(handler);
}

/// Enter ring 3 at `rip` on stack `rsp` with `rax = 0`. Returns the status the
/// task left with, when it leaves.
///
/// # Errors
/// [`Refused::NotUserRip`] for a `rip` that is not a lower-half address.
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
pub fn enter(rip: u64, rsp: u64) -> Result<u64, Refused> {
    if !is_user_rip(rip) {
        return Err(Refused::NotUserRip);
    }
    Ok(switch::enter_user_mode(rip, rsp, 0))
}

/// Enter ring 3 as a `fork`/`clone` child: the register file in the running
/// task's [`UserCtx::saved_regs`], `rax = 0`, at `rip` on `rsp`.
///
/// # Errors
/// As [`enter`].
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
pub fn enter_forked(rip: u64, rsp: u64) -> Result<u64, Refused> {
    if !is_user_rip(rip) {
        return Err(Refused::NotUserRip);
    }
    Ok(switch::enter_user_mode_forked(rip, rsp))
}

/// Leave ring 3 from outside the syscall path, returning `status` from the
/// running task's [`enter`]. The stack this is called on is abandoned:
/// nothing on it runs again, and no destructor on it runs at all.
///
/// Stops the machine (`ud2`) if the running task is not inside [`enter`].
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
pub fn unwind(status: u64) -> ! {
    switch::akuma_usermode_unwind(status)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_rip_is_the_lower_canonical_half() {
        assert!(is_user_rip(0));
        assert!(is_user_rip(0x40_0000));
        assert!(is_user_rip(0x0000_7fff_ffff_ffff));
        assert!(!is_user_rip(0x0000_8000_0000_0000));
        assert!(!is_user_rip(0xffff_8000_0000_0000));
        assert!(!is_user_rip(u64::MAX));
    }

    #[test]
    fn fmask_is_the_six_flags() {
        assert_eq!(FMASK, 0x4_7700);
    }

    #[test]
    fn a_new_context_is_not_in_ring_3() {
        assert_eq!(UserCtx::new().kernel_rsp, 0);
    }
}
