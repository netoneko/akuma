//! Ring 3, and the `syscall`/`sysret` transition.
//!
//! Stage F. This is the first code in the amd64 port that runs *unprivileged*,
//! and the first use of `PteProt::USER_RX` / `PteProt::USER_RW` — which have existed
//! since Stage B and been unit-checked but never actually mapped.
//!
//! # The transition, and what the hardware does not do for you
//!
//! `syscall` is fast because it does almost nothing: it puts the return address
//! in `rcx` and `RFLAGS` in `r11`, loads `CS`/`SS` from `IA32_STAR`, masks the
//! flags named in `IA32_FMASK` — and **leaves `rsp` pointing at the user
//! stack**. There is no automatic stack switch, no pushed frame, and no saved
//! registers. Everything below is what the kernel has to do by hand.
//!
//! `sysret` is the mirror: it restores `rip` from `rcx` and `RFLAGS` from `r11`,
//! and computes `CS`/`SS` from `IA32_STAR[63:48]` rather than taking selectors.
//! `gdt.rs` documents the layout constraint that follows from that.
//!
//! # Leaving ring 3 for good
//!
//! A `syscall` normally returns to userspace, so the exit path needs somewhere
//! else to go. [`enter_user_mode`] saves the kernel's callee-saved registers and
//! stack pointer before dropping to ring 3; syscall 0 restores them and `ret`s,
//! so `enter_user_mode` returns to its caller as if it were an ordinary
//! function. It is the same trick as `sched.rs`'s context switch, with ring 3 in
//! the middle.

use akuma_syscalls_abi::Syscall;

#[cfg(not(feature = "no-tests"))]
use akuma_selftest::Suite;

use crate::gdt;
use akuma_mmap::{MmapRegion, PhysFrame};
use alloc::vec::Vec;

use akuma_exec::process::ProcAddressSpace;
use akuma_mmu::{LeafAction, PteProt, UserAddressSpace};

use crate::loader;
use crate::phys::phys_ptr;
use crate::serial;
use core::sync::atomic::{AtomicU64, Ordering};
use spinning_top::Spinlock;

const IA32_EFER: u32 = 0xC000_0080;
const IA32_STAR: u32 = 0xC000_0081;
const IA32_LSTAR: u32 = 0xC000_0082;
const IA32_FMASK: u32 = 0xC000_0084;

/// `EFER.SCE` — without it, `syscall` raises `#UD`.
const EFER_SCE: u64 = 1 << 0;

/// Where the test's user code and stack live.
///
/// `0x40_0000` is where a static Linux x86_64 binary is linked by default, and
/// since Stage K the kernel no longer occupies the lower half, so a program can
/// simply be mapped where it expects to be. Before that this had to be
/// `0x5000_0000` — chosen to dodge the kernel's identity map — which is exactly
/// the constraint the higher-half move removed.
#[cfg(not(feature = "no-tests"))]
const USER_CODE_VA: usize = 0x40_0000;
#[cfg(not(feature = "no-tests"))]
const USER_STACK_VA: usize = 0x41_0000;

/// Top of the stack given to an ELF-loaded process.
///
/// Near the ceiling of the lower half rather than just above the image, which is
/// where the hand-assembled program's stack goes: an ELF decides its own extent,
/// and `0x41_0000` is inside `hello`'s. Linux puts the stack at the top of the
/// user address space for the same reason — it is the one place a program's
/// segments cannot already be.
const ELF_STACK_TOP: u64 = 0x7FFF_FFFF_F000;
/// Pages of stack. The initial frame is under 200 bytes, but the stack is
/// eagerly allocated and there is no growth policy or guard page — so it has to
/// be sized for the *largest* program the loader runs, not the smallest.
/// `sshd`'s key exchange (curve25519, ed25519, AES) drives the deepest stack
/// here and #PF'd on two pages within a few calls of `main`; 128 pages
/// (512 KiB) clears it with room to spare. A small program pays 512 KiB of
/// eagerly-zeroed frames it never touches — the cost of not having demand
/// paging for the stack yet.
///
/// `pub` since C1 step 3 batch 3: this is `RLIMIT_STACK`. Folding
/// `prlimit64` into glue made `ExecConfig::user_stack_size` the number ring 3
/// reads, and it had been set to `sched::STACK_SIZE` — the *kernel* stack —
/// because until then nothing on this target read it. See
/// `exec_runtime.rs`'s `user_stack_size`.
pub const ELF_STACK_PAGES: usize = 128;

/// The guest program, linked at [`USER_CODE_VA`] and embedded in the kernel
/// image.
///
/// **The fallback since Stage N, not the primary.** `elf_test` reads
/// `/bin/hello` off the ext2 root when there is one, which is the interesting
/// case: an image the kernel opened by path, from a filesystem it mounted, on a
/// disk it discovered. This copy is what runs when there is no disk — `DISK=none`,
/// and every stage before Stage M — so the loader is still exercised on a
/// machine with no storage.
///
/// The two are byte-identical: `amd64/build.rs` compiles the program into
/// `OUT_DIR` and `amd64/mkdisk.sh` copies that same file into the image. That is
/// what makes the fallback honest rather than a second, drifting program — and
/// `elf_test` checks it, because "identical" is an assumption about two build
/// steps agreeing.
#[cfg(not(feature = "no-tests"))]
const HELLO_ELF: &[u8] = include_bytes!(env!("USER_HELLO_ELF"));

/// The `clone`/`futex` probe. See `usermode::thread_test`.
#[cfg(not(feature = "no-tests"))]
const THREADPROBE_ELF: &[u8] = include_bytes!(env!("USER_THREADPROBE_ELF"));

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
    /// path and by the exit path. Offset 0.
    pub kernel_rsp: u64,
    /// The task's user stack, saved on syscall entry. Offset 8.
    pub user_rsp: u64,
    /// Non-zero when this task should leave ring 3. Offset 16.
    pub leave: u64,
    /// The process slot this task is running, or `usize::MAX` for a task that
    /// is not an ELF process (the boot task). Offset 24 — past everything the
    /// assembly indexes, so it is free to be an ordinary field. `sys_read` /
    /// `sys_write` use it to route fd 0/1/2 to a spawned child's pipes instead
    /// of the console.
    pub proc_slot: usize,
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
    /// This task's `thread::THREADS` slot, or [`crate::thread::NO_THREAD`] if
    /// it is a process's main thread. Offset 152 — past everything the
    /// assembly indexes, like `proc_slot` and for the same reason.
    ///
    /// This is what lets every `clone` child share **one** entry function, and
    /// since 2026-09-06 `proc_slot` above does the same job for processes —
    /// `usermode::proc_entry` replaced sixteen hand-written trampolines that
    /// existed only because a process index had nowhere to live but the `fn`
    /// pointer. Both indices are seeded before the task is published.
    pub thread_slot: usize,
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
}

impl UserCtx {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            kernel_rsp: 0,
            user_rsp: 0,
            leave: 0,
            proc_slot: usize::MAX,
            user_rip: 0,
            fs_base: 0,
            saved_regs: [0; 12],
            gs_base: 0,
            thread_slot: crate::thread::NO_THREAD,
            forked: 0,
            exec_pending: 0,
        }
    }
}

/// How many syscalls arrived.
static CALLS: AtomicU64 = AtomicU64::new(0);

/// Print every syscall number and its result. Off during the self-tests, turned
/// on by `run_init` when `strace` is on the command line — a bring-up aid for
/// running a program the tree did not compile (busybox).
pub static SYSCALL_TRACE: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);
/// Bytes accepted by `write`, across all processes.
static WRITTEN: AtomicU64 = AtomicU64::new(0);
/// Status the last process exited with.
static EXIT_STATUS: AtomicU64 = AtomicU64::new(u64::MAX);

/// Which task performed each `write`, in order. Proving that two processes ran
/// concurrently needs the *interleaving*, not just the totals: three writes from
/// A followed by three from B would satisfy every count-based check and would
/// mean the scheduler never switched.
static WRITE_SEQ: [AtomicU64; 32] = [const { AtomicU64::new(u64::MAX) }; 32];
static WRITE_SEQ_LEN: AtomicU64 = AtomicU64::new(0);
/// Which *core* served each `write`, alongside [`WRITE_SEQ`]. Two processes
/// whose writes arrive from two cores ran on two cores — the ring-3 half of
/// the SMP self-test, and the only witness of user-mode parallelism the kernel
/// can take without instrumenting the programs.
static WRITE_CPU: [AtomicU64; 32] = [const { AtomicU64::new(u64::MAX) }; 32];

/// Largest `write` this kernel will accept. A bound rather than trust: `len`
/// comes from ring 3, and an unbounded length would walk off the mapped page
/// into whatever follows.
///
/// Raised from 4096 in Stage O to match `fd::MAX_IO`: a shell streaming a file
/// to the console writes in whatever chunks its buffer holds, and a limit lower
/// than the read limit turns a legitimate write into `EFAULT` halfway through
/// an output line.
const MAX_WRITE: u64 = 64 * 1024;

core::arch::global_asm!(
    r#"
    /* Naming the section is mandatory here — see sched.rs for why a missing
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
    call syscall_handler            /* result in rax */

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

.Lexit_to_kernel:
    /* Leave ring 3 for good. Restore what enter_user_mode saved and return from
     * it; rax still holds the handler's result. rcx already points at the
     * uctx. Still in ring 0, so %gs stays the kernel's: no swapgs. */
    mov rsp, [rcx + 0]
    pop r15
    pop r14
    pop r13
    pop r12
    pop rbx
    pop rbp
    ret

.global enter_user_mode
enter_user_mode:
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
"#
);

unsafe extern "C" {
    /// Drop to ring 3 at `rip` with stack `rsp` and `rax = entry_rax`; returns
    /// when userspace calls syscall 0. `entry_rax` is 0 for a fresh program and
    /// 0 for a `vfork` child (the value it sees returned from `vfork`).
    ///
    /// # Safety
    /// `rip` must point at a page mapped user-executable and `rsp` at a page
    /// mapped user-writable, both in the address space that is live.
    fn enter_user_mode(rip: u64, rsp: u64, entry_rax: u64) -> u64;
    /// Enter ring 3 as a `vfork` child — a full-register copy of the parent at
    /// its `syscall`, `rax = 0`. Reads the register set from the running task's
    /// `UserCtx::saved_regs`, which [`sys_fork`] populated. Returns when the
    /// child leaves ring 3.
    ///
    /// # Safety
    /// As [`enter_user_mode`], and the running task's `saved_regs` must hold the
    /// parent's snapshot.
    fn enter_user_mode_forked(rip: u64, rsp: u64) -> u64;
    fn syscall_entry();
}

/// # Safety
/// Writing an MSR reconfigures the CPU; the four written here set up the
/// `syscall` path and nothing else.
unsafe fn wrmsr(msr: u32, val: u64) {
    // SAFETY: caller's obligation.
    unsafe {
        core::arch::asm!("wrmsr", in("ecx") msr, in("eax") val as u32,
                         in("edx") (val >> 32) as u32,
                         options(nostack, preserves_flags));
    }
}

fn rdmsr(msr: u32) -> u64 {
    let (lo, hi): (u32, u32);
    // SAFETY: reading an architectural MSR has no side effect.
    unsafe {
        core::arch::asm!("rdmsr", in("ecx") msr, out("eax") lo, out("edx") hi,
                         options(nomem, nostack, preserves_flags));
    }
    (u64::from(hi) << 32) | u64::from(lo)
}

/// Load `%fs` base (`IA32_FS_BASE`) — the scheduler calls this to restore an
/// incoming task's TLS pointer (`UserCtx::fs_base`). See `sys_arch_prctl`.
pub fn set_fs_base(base: u64) {
    const IA32_FS_BASE: u32 = 0xC000_0100;
    // SAFETY: a linear address for `%fs:` accesses; a bad one only faults the
    // task's own TLS reads, exactly as on Linux.
    unsafe { wrmsr(IA32_FS_BASE, base) };
}

/// Set the *program's* `%gs` base — `IA32_KERNEL_GS_BASE`, which is where it
/// lives while the kernel runs and what `swapgs` installs on the way back to
/// ring 3. Never `IA32_GS_BASE`: in ring 0 that is the per-CPU block.
pub fn set_user_gs_base(base: u64) {
    // SAFETY: the parked half of the `%gs` pair; a bad value only faults the
    // program's own `%gs:` accesses.
    unsafe { wrmsr(crate::smp::IA32_KERNEL_GS_BASE, base) };
}

/// Leave ring 3 from an exception handler as if the program had called
/// `exit_group(status)`.
///
/// The syscall path's `.Lexit_to_kernel`, done by hand: back onto the task's
/// kernel stack at the point `enter_user_mode` saved its callee-saved registers,
/// pop them, and return into `run_process` with `status` in `rax`. The trap
/// stack the exception arrived on is simply abandoned — nothing on it is
/// needed again. Takes the BKL first, because the code it returns into is
/// kernel code and expects to hold it; the exception stub already `swapgs`'d,
/// so the per-CPU block is in place.
pub fn kill_current_from_fault(status: u64) -> ! {
    crate::smp::bkl_enter();
    EXIT_STATUS.store(status, Ordering::Relaxed);
    let uctx = crate::smp::current_uctx();
    assert!(!uctx.is_null(), "ring-3 fault with no current UserCtx");
    // SAFETY: `uctx.kernel_rsp` is the stack `enter_user_mode` published for
    // this task, with the six callee-saved registers and its return address on
    // top; this is exactly the sequence `.Lexit_to_kernel` runs.
    unsafe {
        core::arch::asm!(
            "mov rsp, [{uctx}]",
            "pop r15",
            "pop r14",
            "pop r13",
            "pop r12",
            "pop rbx",
            "pop rbp",
            "ret",
            uctx = in(reg) uctx,
            in("rax") status,
            options(noreturn)
        );
    }
}

/// Drop to ring 3 for `entry`/`stack`, holding the BKL on the way in and out.
///
/// Ring 3 does not hold the lock: it is released here, just before the
/// `sysret`, and the task returns holding it again — `syscall_handler` took it
/// for the syscall that decided to leave ring 3 and deliberately did not let go.
/// Nothing between the release and the `sysretq` touches shared state; the
/// assembly reads only this task's `UserCtx` and this core's per-CPU block.
/// Name the `clone` flags in a trace line. Bounded and allocation-free — this
/// runs on the console path, where `format!` is banned.
fn trace_clone_flags(flags: u64) {
    const NAMED: [(u64, &str); 10] = [
        (0x0000_0100, "VM"),
        (0x0000_0200, "FS"),
        (0x0000_0400, "FILES"),
        (0x0000_0800, "SIGHAND"),
        (0x0001_0000, "THREAD"),
        (0x0004_0000, "SYSVSEM"),
        (0x0008_0000, "SETTLS"),
        (0x0010_0000, "PARENT_SETTID"),
        (0x0020_0000, "CHILD_CLEARTID"),
        (0x0100_0000, "CHILD_SETTID"),
    ];
    let mut first = true;
    let mut known = 0u64;
    for (bit, name) in NAMED {
        if flags & bit != 0 {
            if !first {
                serial::puts("|");
            }
            serial::puts(name);
            first = false;
            known |= bit;
        }
    }
    if first {
        serial::puts("0");
    }
    // The residue matters: an unnamed bit is a flag this kernel silently
    // ignored, which is the failure mode `clone` is famous for.
    let rest = flags & !known;
    if rest != 0 {
        serial::puts("|0x");
        serial::put_hex(rest);
    }
}

/// Enter ring 3 as a `clone(CLONE_VM)` child.
///
/// The same `enter_user_mode_forked` a `fork` child takes — a full-register
/// copy of the parent at its `syscall` with `rax = 0` — on the stack the caller
/// supplied. `crate::thread` cannot call `enter_user` itself because the BKL
/// hand-off on the way out is this module's business.
pub fn enter_user_from_thread(rip: u64, rsp: u64) -> u64 {
    enter_user(rip, rsp, true)
}

fn enter_user(entry: u64, stack: u64, forked: bool) -> u64 {
    crate::smp::bkl_leave();
    // SAFETY: the caller's obligation, stated on `enter_user_mode` — the
    // addresses come from the loader for the space the scheduler installed.
    unsafe {
        if forked {
            enter_user_mode_forked(entry, stack)
        } else {
            enter_user_mode(entry, stack, 0)
        }
    }
}

/// The kernel side of a `syscall`.
///
/// Runs on the dedicated syscall stack with interrupts off. Returns the value
/// userspace sees in `rax`.
///
/// Dispatches on [`Syscall`] rather than on a raw number. That is proposal item
/// 5's point made load-bearing: the same `write` is 1 here and 64 on aarch64, so
/// a handler written against numbers cannot be shared and a handler written
/// against names can.
#[unsafe(no_mangle)]
extern "C" fn syscall_handler(
    nr: u64,
    a1: u64,
    a2: u64,
    a3: u64,
    a4: u64,
    a5: u64,
    a6: u64,
) -> u64 {
    // Ring 3 does not hold the Big Kernel Lock; kernel code does. Taken here,
    // released at the bottom unless this syscall is the one that leaves ring 3
    // for good — that path returns into kernel code (`run_process`) which
    // expects to hold it.
    crate::smp::bkl_enter();
    CALLS.fetch_add(1, Ordering::Relaxed);
    let trace = SYSCALL_TRACE.load(Ordering::Relaxed);
    // Entry line: a syscall that blocks forever has no result line, so the
    // entry line is what names it (a bring-up aid — without it a hang inside
    // `read` is invisible and reads as "no syscalls at all"). Path-taking
    // syscalls also get their path, bounded by the same decode the syscall
    // itself uses.
    if trace {
        serial::puts("[sc>] cpu=");
        serial::put_dec(crate::smp::cpu_index() as u64);
        serial::puts(" task=");
        serial::put_dec(crate::sched::current_task() as u64);
        serial::puts(" nr=");
        serial::put_dec(nr);
        serial::puts(" a1=0x");
        serial::put_hex(a1);
        match nr {
            // The first-arg-path syscalls: `open`, `stat`, `lstat`, `access`,
            // `chdir`. All of them answer ENOENT for a path this kernel does
            // not serve, and without the path in the trace an ENOENT is a
            // number with nothing attached — which is exactly how long it took
            // to see that `ps` was failing on `stat("/proc/<pid>")` rather than
            // on anything to do with `getdents64`.
            2 | 4 | 6 | 21 | 80 => {
                serial::puts(" \"");
                crate::fd::trace_user_cstr(a1);
                serial::puts("\"");
            }
            257 | 258 | 263 | 269 => {
                serial::puts(" at=");
                serial::put_dec(a1);
                serial::puts(" \"");
                crate::fd::trace_user_cstr(a2);
                serial::puts("\"");
            }
            // `clone`'s flag word and `futex`'s op decide everything about the
            // call and neither is legible as a hex blob. Added 2026-09-06 after
            // decoding `a1=0x7d0f00` by hand off a trace was the step that
            // identified the wall; the second time would have been waste.
            56 => {
                serial::puts(" flags=");
                trace_clone_flags(a1);
            }
            202 => {
                serial::puts(" op=");
                serial::put_dec(a2 & 0x7f);
                if a2 & 128 != 0 {
                    serial::puts("|PRIV");
                }
                serial::puts(" val=");
                serial::put_dec(a3);
            }
            264 => {
                serial::puts(" at=");
                serial::put_dec(a1);
                serial::puts(" \"");
                crate::fd::trace_user_cstr(a2);
                serial::puts("\" -> at=");
                serial::put_dec(a3);
                serial::puts(" \"");
                crate::fd::trace_user_cstr(a4);
                serial::puts("\"");
            }
            _ => {}
        }
        serial::puts("\n");
    }
    // A sibling thread of a process that has called `exit_group` leaves here,
    // before the syscall runs: its address space is about to be freed and
    // anything it does with it from now on is a race with the reaper.
    let r = if crate::thread::should_leave_now() {
        // SAFETY: single core, interrupts off inside a syscall; this task's own
        // `UserCtx`. Same `leave` mechanism `exit` uses.
        unsafe {
            let uctx = crate::smp::current_uctx();
            if !uctx.is_null() {
                (*uctx).leave = 1;
            }
        }
        crate::fd::errno::EINTR
    } else {
        syscall_dispatch(nr, a1, a2, a3, a4, a5, a6)
    };
    if trace {
        serial::puts("[sc] cpu=");
        serial::put_dec(crate::smp::cpu_index() as u64);
        serial::puts(" task=");
        serial::put_dec(crate::sched::current_task() as u64);
        serial::puts(" nr=");
        serial::put_dec(nr);
        serial::puts(" -> 0x");
        serial::put_hex(r);
        serial::puts("\n");
    }
    let uctx = crate::smp::current_uctx();
    // SAFETY: this task's own `UserCtx`, which only this task writes.
    let leaving = unsafe { !uctx.is_null() && (*uctx).leave != 0 };
    if !leaving {
        crate::smp::bkl_leave();
    }
    r
}

fn syscall_dispatch(nr: u64, a1: u64, a2: u64, a3: u64, a4: u64, a5: u64, a6: u64) -> u64 {
    use crate::fd::errno;

    /// `AT_FDCWD` — "relative to the current directory", which on a target with
    /// no per-process cwd means relative to the root. What every legacy,
    /// non-`at` path syscall below passes to its `*at` implementation.
    const AT_FDCWD: u64 = (-100i64) as u64;

    // Akuma's own syscalls, before the Linux table.
    //
    // They live at `0x1000 +` their AArch64 number (see `libakuma`'s
    // `AKUMA_PRIVATE_BASE`), far above any allocated Linux number. Checking this
    // range first is what keeps a shell's keystroke poll from dispatching into
    // whatever Linux happens to have at 313 — `finit_module`, as it turns out.
    /// How long a `resolve_host` may take before it gives up. Generous: a
    /// cold cache on this target means a real UDP round trip to whatever
    /// `/etc/resolv.conf`'s first reachable server is, and the alternative to
    /// waiting is a program that reports "no such host" for a working name.
    const DNS_TIMEOUT_US: u64 = 5_000_000;

    const AKUMA_PRIVATE_BASE: u64 = 0x1000;
    if nr >= AKUMA_PRIVATE_BASE {
        return match nr - AKUMA_PRIVATE_BASE {
            // `spawn(path, argv, envp, stdin, stdin_len[, flags])` — the sixth
            // argument (the PTY flag) is dropped by `syscall_entry` and this
            // target ignores it anyway (a pipe has no line discipline).
            301 => sys_spawn(a1, a2, a3, a4, a5),
            // `resolve_host(name_ptr, name_len, out4)` — Akuma's own 300.
            //
            // Wired 2026-09-06 because `hget` (and anything else built on
            // `libakuma-tls`) resolves through this, not through musl: it is a
            // `no_std` binary with no resolver of its own. Without it, TLS on
            // this target failed at `DNS resolution failed` while `busybox
            // wget http://…` resolved the same name perfectly — because
            // busybox is musl and goes out over UDP itself.
            //
            // The kernel already had the resolver; `clock.rs`'s SNTP bootstrap
            // uses the same `dns::resolve_a`. Only the syscall was missing.
            300 => {
                // A hostname is passed as `(ptr, len)` and is **not**
                // NUL-terminated, so `read_cstr` is the wrong tool — it would
                // run past the end looking for a terminator that is not there.
                const MAX_HOST: u64 = 255; // RFC 1035's limit on a domain name
                if a2 == 0 || a2 > MAX_HOST {
                    return errno::EINVAL;
                }
                let mut buf = [0u8; MAX_HOST as usize];
                let n = a2 as usize;
                if !crate::uaccess::read_bytes(a1, &mut buf[..n]) {
                    return errno::EFAULT;
                }
                let Ok(name) = core::str::from_utf8(&buf[..n]) else {
                    return errno::EINVAL;
                };
                let Some(ip) = crate::dns::resolve_a(name, DNS_TIMEOUT_US) else {
                    // `EAI_FAIL`'s errno cousin: the name did not resolve. Not
                    // `EFAULT` — the caller's pointers were fine.
                    return errno::ENOENT;
                };
                if !crate::uaccess::write_bytes(a3, &ip) {
                    return errno::EFAULT;
                }
                0
            }
            313 => crate::fd::sys_poll_input_event(a1, a2, a3),
            303 => sys_waitpid(a1, a2, a3),
            // `uptime()` — microseconds since boot, matching
            // `akuma_syscalls_time::sys_uptime` on the AArch64 side. `herd`'s
            // whole supervision loop is keyed on it (restart delays, start
            // delays, the 20 s config reload), so without this its clock
            // reads `-ENOSYS` as a colossal timestamp and every delay is
            // already in the past.
            319 => crate::net::uptime_us(),
            326 => sys_close_child_stdin(a1),
            // `kill` is accepted as a no-op success: `sshd` sends SIGHUP/SIGTERM
            // to a session's shell on teardown, and there is nothing here to
            // deliver a signal to, but failing the call makes it log an error.
            302 => 0,
            // `console_notify(ptr, len)` — Akuma-private 322, feature
            // `console-notify` (default-on for this target). Prints a
            // caller-supplied line straight to the framebuffer/serial console,
            // framed so it stands out in a scrolling boot log. The HP box has a
            // screen and no keyboard, so this is how a program — or a person
            // driving one over ssh — puts a word in front of whoever is watching
            // that screen. `/bin/wall` is the userspace front end.
            //
            // Hard-capped at 512 bytes, and every byte below space (bar `\t`) is
            // rendered as '.': the framebuffer console interprets some control
            // bytes, and a careless or hostile caller should not be able to move
            // its cursor or scroll it from here. A trailing '\n' the caller
            // included is dropped — the framing adds its own.
            #[cfg(feature = "console-notify")]
            322 => {
                const MAX_MSG: usize = 512;
                let n = (a2 as usize).min(MAX_MSG);
                if n == 0 {
                    return errno::EINVAL;
                }
                let mut buf = [0u8; MAX_MSG];
                if !crate::uaccess::read_bytes(a1, &mut buf[..n]) {
                    return errno::EFAULT;
                }
                let mut end = n;
                while end > 0 && (buf[end - 1] == b'\n' || buf[end - 1] == b'\r') {
                    end -= 1;
                }
                for b in &mut buf[..end] {
                    if (*b < 0x20 && *b != b'\t') || *b == 0x7f {
                        *b = b'.';
                    }
                }
                let msg = core::str::from_utf8(&buf[..end]).unwrap_or("<console_notify: not UTF-8>");
                serial::puts("\n>>> ");
                serial::puts(msg);
                serial::puts(" <<<\n");
                0
            }
            _ => errno::ENOSYS,
        };
    }

    // The **x86-only legacy spellings**, and nothing else.
    //
    // Each of these numbers exists on x86_64 and has no `asm-generic` twin, so
    // `akuma_syscalls_abi::Syscall` deliberately cannot name it (rule 2 in that
    // crate's header): giving one an aarch64 number would mean inventing a fact
    // about Linux. They are handled here, ahead of the neutral table, and every
    // one of them narrows to a modern call the neutral table *does* name —
    // usually by supplying the `AT_FDCWD` that the `*at` form wants.
    //
    // Not redundant with the `*at` arms below: musl issues whichever spelling
    // the architecture has, and x86_64 has both, so which one arrives is a
    // property of the *caller*. busybox `mkdir` uses 83 and got `ENOSYS` while
    // `mkdirat` sat implemented and working — reported as `mkdir: can't create
    // directory: Function not implemented`, which reads as a filesystem that
    // cannot make directories rather than a dispatch table missing a number.
    //
    // **Adding an arm here is a claim that the call is x86-only.** Check
    // `akuma_syscalls_abi`'s table first; if the call has an asm-generic number,
    // it belongs in the `match call` below, where the AArch64 kernel's
    // implementation can eventually serve it.
    match nr {
        // x86_64 158: the TLS-base primitive. No aarch64 number, and no
        // equivalent either — `arch_prctl(ARCH_SET_FS)` is what `set_tpidr_el0`
        // is on the other side, and that one is Akuma-private.
        158 => return sys_arch_prctl(a1, a2),
        // Path-based `struct stat`. `stat` (4) and `lstat` (6) are x86-only —
        // `asm-generic` dropped them — and both narrow to `newfstatat`, which
        // is in the neutral table. `stat` follows a final symlink, `lstat` does
        // not (`AT_SYMLINK_NOFOLLOW` == 0x100); on this target that changes
        // nothing (see `fd::sys_newfstatat`). `AT_FDCWD` is -100. busybox `sh`
        // stats every PATH entry before it will run an applet — without this it
        // saw `ENOSYS` and reported "Function not implemented" for a working
        // builtin.
        4 => return crate::fd::sys_newfstatat(AT_FDCWD, a1, a2, 0),
        6 => return crate::fd::sys_newfstatat(AT_FDCWD, a1, a2, 0x100),
        // `open(path, flags, mode)` — x86_64 2. x86_64 musl issues this directly
        // (it only falls back to `openat` on architectures without `open`, like
        // aarch64), so `busybox cat` hit `ENOSYS` here until now. `openat`
        // ignores the dirfd for absolute paths and treats a relative one as
        // root-relative, which is what `AT_FDCWD` means on a target with no cwd.
        2 => return crate::fd::sys_openat(AT_FDCWD, a1, a2, a3),
        // `access(path, mode)` — existence only. This target has one user (root)
        // and no per-file exec tracking worth trusting, so "the path resolves"
        // is the honest answer; a real permission check would be a guess. The
        // `faccessat` spelling is in the neutral table and answers identically.
        21 => return crate::fd::sys_access(a1),
        // `poll(fds, nfds, timeout_ms)` — x86_64 7, narrowing to the same core
        // `ppoll` uses. An interactive `busybox sh` polls its stdin on every
        // keystroke; `ENOSYS` here was a forever-loop of "sh: poll: Function
        // not implemented".
        7 => return crate::fd::sys_poll(a1, a2, a3),
        // `select(nfds, readfds, writefds, exceptfds, timeout)` — x86_64 23.
        // asm-generic has only `pselect6`. `apk` waits for post-connect socket
        // writability through this syscall; `ENOSYS` here wedged its TLS fetch
        // mid-handshake (see `fd::sys_select`).
        23 => return crate::fd::sys_select(a1, a2, a3, a4, a5),
        // `pipe` (22) and `dup2` (33) — the legacy halves of the four calls that
        // make a shell a shell (`pipe2`/`dup3` are the neutral pair below).
        // Every one of them was `ENOSYS` until 2026-09-06, which is why
        // `cmd | cmd` reported *can't create pipe* and `echo x > file` left a
        // zero-length file: a shell builds both out of `pipe` plus `dup2`, and
        // neither existed.
        22 => return crate::fd::sys_pipe2(a1, 0),
        33 => return crate::fd::sys_dup2(a1, a2),
        // The legacy, non-`at` path calls — x86_64 82/83/84/87/88/89 — as thin
        // `AT_FDCWD` shims. `rmdir` (84) is `unlinkat` with `AT_REMOVEDIR`
        // (0x200). Since 4b step 2 batch 1 the `*at` arms live in glue, so the
        // shims hand it the asm-generic number (`to_glue` owns the hop) with
        // `AT_FDCWD` in the dirfd slots — `AT_FDCWD` is -100 in both ABIs.
        // `AKUMA_AMD64_4B_FOLD_BATCH1.md` records the divergences the fold
        // adopted.
        82 => return to_glue(Syscall::Renameat, [AT_FDCWD, a1, AT_FDCWD, a2, 0, 0]),
        83 => return to_glue(Syscall::Mkdirat, [AT_FDCWD, a1, a2, 0, 0, 0]),
        84 => return to_glue(Syscall::Unlinkat, [AT_FDCWD, a1, 0x200, 0, 0, 0]),
        87 => return to_glue(Syscall::Unlinkat, [AT_FDCWD, a1, 0, 0, 0, 0]),
        // `symlink(target, linkpath)` — x86_64 88.
        //
        // **This arm used to call `sys_utimensat`.** Its comment read
        // "`utimensat` (280) / `futimens` (88)", and there is no `futimens`
        // syscall in Linux at all — libc implements it as `utimensat(fd, NULL,
        // times, 0)`. x86_64 88 is `symlink`, sitting between `unlink` (87) and
        // `readlink` (89), both of which are correct shims two lines up. So
        // `ln -s` handed its *link path* to `utimensat` as a `struct
        // timespec[2]` pointer. Found 2026-09-07 while widening
        // `akuma-syscalls-abi` for C1 — exactly the wrong-answer-not-a-compile-
        // error class that widening exists to prevent, one layer up from the
        // aarch64/x86_64 crossing.
        //
        // `symlinkat(target, newdirfd, linkpath)` takes the dirfd *second*.
        88 => return to_glue(Syscall::Symlinkat, [a1, AT_FDCWD, a2, 0, 0, 0]),
        // `readlink(path, buf, size)` — x86_64 89. Was a flat EINVAL while no
        // symlink could exist; `symlinkat` made package symlinks real.
        89 => return to_glue(Syscall::Readlinkat, [AT_FDCWD, a1, a2, a3, 0, 0]),
        // `fork` (57) / `vfork` (58) — a real eager-copy fork; see `sys_fork`
        // (`vfork` gets the same, its "don't touch the parent" contract is moot
        // once the address space is copied). asm-generic has neither: `clone`
        // with `CLONE_VM` clear is the only spelling there, and it is in the
        // neutral table below.
        57 | 58 => return sys_fork(),
        // `getpgrp()` — x86_64 111, x86-only; asm-generic callers use
        // `getpgid(0)`. One process, so it is its own group leader.
        111 => return 1,
        // The x86-only halves of the clock. `clock_settime` (227) and
        // `adjtimex` (159) are in the neutral table; these two are not, and
        // `busybox ntpd -q` reaches for whichever musl offers.
        //
        // Why it matters beyond `date` being wrong: at the epoch **every TLS
        // certificate on earth is not-yet-valid**, and `apk` reports that as
        // `server certificate not trusted` — sending you to look at the CA
        // bundle, which is fine.
        164 => {
            // `settimeofday(tv, tz)`. `tz` is ignored: it has been meaningless
            // since the 1980s and Linux itself does nothing useful with it.
            if a1 == 0 {
                return 0;
            }
            let Some(tv) = crate::uaccess::read_val::<akuma_syscalls_linux::time::Timeval>(a1)
            else {
                return errno::EFAULT;
            };
            if tv.tv_sec < 0 || tv.tv_usec < 0 || tv.tv_usec >= 1_000_000 {
                return errno::EINVAL;
            }
            crate::clock::set_unix_us(
                (tv.tv_sec.cast_unsigned()).saturating_mul(1_000_000) + tv.tv_usec.cast_unsigned(),
            );
            return 0;
        }
        // `gettimeofday(*timeval, *timezone)` — x86_64 96. `timezone` (a2)
        // is always NULL from every real caller and is not consulted.
        96 => {
            if a1 != 0 {
                let us = crate::clock::now_us();
                // A user `struct timeval { i64 tv_sec, i64 tv_usec }`.
                let tv = [(us / 1_000_000).cast_signed(), (us % 1_000_000).cast_signed()];
                if !crate::uaccess::write_val(a1, tv) {
                    return errno::EFAULT;
                }
            }
            return 0;
        }
        // `time(*time_t)` — x86_64 201. Writes through `tloc` when non-null,
        // in addition to the return value, matching `time(2)`'s own contract.
        201 => {
            let secs = (crate::clock::now_us() / 1_000_000).cast_signed();
            if a1 != 0 && !crate::uaccess::write_val::<i64>(a1, secs) {
                return errno::EFAULT;
            }
            return secs as u64;
        }
        _ => {}
    }

    // Everything else goes through `akuma_syscalls_abi::Syscall` — the
    // architecture-neutral name, decoded from the x86_64 number here and
    // encodable back to the asm-generic one `akuma-syscalls-glue` dispatches on.
    //
    // That second half is the point: C1 folds these arms into glue one at a
    // time (`proposals/NEXT_AGENT_AMD64_C1_USERMODE_FOLD.md`), and glue's table
    // is asm-generic. Handing it `1` meaning `write` would find the *wrong*
    // handler rather than none — so the vocabulary hop happens once, here,
    // through a table whose two halves are round-trip tested against each other.
    let Some(call) = Syscall::from_x86_64(nr) else {
        return errno::ENOSYS;
    };

    match call {
        Syscall::Write => sys_write(a1, a2, a3),
        Syscall::Read => crate::fd::sys_read(a1, a2, a3),
        // `pread64(fd, buf, count, offset)` — a read that does not move the
        // cursor. Not dispatched at all until 2026-09-07: every `pread` on this
        // target was `ENOSYS`, which `mmapsum`'s reference arm hit at offset 0.
        Syscall::Pread64 => crate::fd::sys_pread64(a1, a2, a3, a4),
        // busybox prints through `writev`, not `write`. Walk the iovec array and
        // forward each segment; a short write on any segment stops the walk, as
        // `writev(2)` specifies.
        Syscall::Writev => sys_writev(a1, a2, a3),
        Syscall::Readv => sys_readv(a1, a2, a3),
        Syscall::Openat => crate::fd::sys_openat(a1, a2, a3, a4),
        // `close(fd)` — **served by glue** (4b batch 2b). Two prerequisites had
        // to land before this arm could move, and neither was in the plan:
        // the two kernels had to share **one pipe table** (glue's `sys_close`
        // closes a `PipeRead`/`PipeWrite` through `glue::pipe`, and this
        // kernel's ids used to name a different table — a wrong pipe, not an
        // error), and the boot suite had to have a **process identity**, since
        // every descriptor-freeing arm in glue resolves
        // `current_process_shared()` first.
        Syscall::Close => to_glue(call, [a1, 0, 0, 0, 0, 0]),
        // `lseek(fd, offset, whence)` — **glue's arm behind one preamble**
        // (4b batch 3a): a `/dev` character node, which Linux seeks to the
        // offset asked for and glue answers `0`/`ESPIPE` for. Not routed
        // through `to_glue` for that reason — see `fd::sys_lseek`.
        Syscall::Lseek => crate::fd::sys_lseek(a1, a2, a3),
        Syscall::Fstat => crate::fd::sys_fstat(a1, a2),
        Syscall::Ioctl => crate::fd::sys_ioctl(a1, a2, a3),
        // `getdents64(fd, dirp, count)` — x86_64 217. `ls`/`find`.
        // **Served by glue** (4b batch 3a) with no preamble at all: the record
        // layout was already `akuma_syscalls_linux::dirent` and the snapshot
        // was already `KernelFile::dir_cache`, so the two arms differed only
        // in what this one got wrong (`/dev` nodes listed as `DT_REG`, no
        // up-front validation of `dirp`).
        Syscall::Getdents64 => to_glue(call, [a1, a2, a3, 0, 0, 0]),
        // `a5` is mmap's fd and is deliberately unused: only anonymous mappings
        // are supported, so a file-backed request must fail rather than quietly
        // return zeroed memory that the caller believes holds a file.
        // `a6` is the file offset. It was dropped until 2026-09-07 because only
        // anonymous mappings were served and an anonymous `mmap`'s offset is
        // ignored; a file mapping cannot ignore it.
        Syscall::Mmap => crate::mm::sys_mmap(a1, a2, a3, a4, a5, a6),
        Syscall::Munmap => crate::mm::sys_munmap(a1, a2),
        // `madvise(addr, len, advice)`. Answered `ENOSYS` for everything until
        // 2026-09-07, which is the *wrong* way to say "not implemented": Linux
        // says `EINVAL` for unsupported advice and redis exits on anything else.
        Syscall::Madvise => crate::mm::sys_madvise(a1, a2, a3),
        // `mremap(old, old_len, new_len, flags)`. `a5` is `MREMAP_FIXED`'s
        // `new_address` and is deliberately not passed: this target does not
        // support `MREMAP_FIXED`, and the decision crate does not decode it.
        Syscall::Mremap => crate::mm::sys_mremap(a1, a2, a3, a4),
        Syscall::Socket => crate::sock::sys_socket(a1, a2, a3),
        Syscall::Bind => crate::sock::sys_bind(a1, a2, a3),
        Syscall::Listen => crate::sock::sys_listen(a1, a2),
        Syscall::Accept => crate::sock::sys_accept(a1, a2, a3),
        Syscall::Connect => crate::sock::sys_connect(a1, a2, a3),
        // `a5` is the destination/source `struct sockaddr *` — Linux's 5th
        // argument, which the entry asm's shuffle keeps (only the 6th,
        // `addrlen`, is dropped; see `syscall_entry`'s comment on why that was
        // safe until now). Needed for UDP: unlike a connected TCP socket, a
        // UDP `sendto` has no peer to fall back on, and musl's DNS resolver
        // never `connect()`s its query socket — it addresses every nameserver
        // by hand on each `sendto`. See `sock::sys_sendto`.
        Syscall::Sendto => crate::sock::sys_sendto(a1, a2, a3, a5),
        Syscall::Recvfrom => crate::sock::sys_recvfrom(a1, a2, a3, a5),
        Syscall::Setsockopt => crate::sock::sys_setsockopt(a1, a2, a3, a4, a5),
        // `exit` (60) and `exit_group` (231) are the same call for a
        // single-threaded process and emphatically not for a threaded one:
        // `exit` ends the calling thread, `exit_group` ends every thread in the
        // group. musl's `pthread_exit` uses the first and `main` returning uses
        // the second, so conflating them makes the first thread to finish take
        // the whole process with it.
        Syscall::ExitGroup => {
            crate::thread::set_group_exiting(current_proc_slot());
            EXIT_STATUS.store(a1, Ordering::Relaxed);
            // SAFETY: single core, interrupts off inside a syscall. The
            // running task's context is what `syscall_entry` will read on the
            // way out, and only this task can be inside a syscall.
            unsafe {
                let uctx = crate::smp::current_uctx();
                if !uctx.is_null() {
                    (*uctx).leave = 1;
                }
            }
            a1
        }
        Syscall::Exit => {
            // A main thread's `exit` is a process exit: nothing else runs in
            // its address space, and `run_process` is the only frame below it.
            // A non-main thread's is not — it must leave ring 3 without
            // touching `EXIT_STATUS`, the fd table or the spawn record, all of
            // which belong to the process it is only one thread of.
            if crate::thread::current_is_main() {
                EXIT_STATUS.store(a1, Ordering::Relaxed);
            }
            // SAFETY: single core, interrupts off inside a syscall; the
            // per-CPU `UserCtx` is this task's own.
            unsafe {
                let uctx = crate::smp::current_uctx();
                if !uctx.is_null() {
                    (*uctx).leave = 1;
                }
            }
            a1
        }
        Syscall::Getpid => 1,
        Syscall::Fcntl => crate::fd::sys_fcntl(a1, a2, a3),
        // `getrandom(buf, len, flags)` — **served by glue** (C1 step 3, batch 3).
        //
        // A leaf by `akuma_syscalls::fast_path`'s reckoning and the last one on
        // the C1 hand-off's step-3 list that could not be folded, because
        // glue's body named `akuma_virtio::rng::fill_bytes` outright and this
        // target has no virtio-rng on any rig — the answer would have been
        // `EIO` to every caller, `sshd`'s key exchange included. The seam is
        // `akuma_primitives::rng`, registered with `net::rng_fill_checked` in
        // `boot::install_shared_sinks`; glue still falls back to the virtio
        // device when nothing is registered, so the AArch64 kernel keeps the
        // behaviour it had.
        //
        // Two divergences close on the way, and both were silent here:
        // glue **loops** where this capped at one 256-byte chunk, and it
        // returns `EIO` on a short fill where this returned the byte count
        // regardless — so a `RDRAND` that ran out of entropy handed ring 3 a
        // buffer whose tail was kernel stack and called it random.
        Syscall::Getrandom => to_glue(call, [a1, a2, a3, a4, a5, a6]),
        // `nanosleep(req, rem)`.
        //
        // This was a bare `yield_now()` — "no high-resolution sleep: this
        // target has a coarse, uncalibrated clock", which is true and was the
        // wrong conclusion. A `nanosleep` that returns immediately is not a
        // coarse sleep, it is **no sleep**, and every program that uses one to
        // sequence against another thread silently loses its ordering.
        //
        // Found 2026-09-06 by `scripts/futex_suite.py`'s `futexops`, which
        // reported a requeue bug that did not exist: the probe parks a thread
        // with a 400 ms timeout, then `nanosleep`s three times for a second
        // each and checks whether it fired. All three returned at once, ~0 ms
        // of guest time in, so of course it had not. The futex was correct and
        // the clock the probe was steering by was not moving.
        //
        // So: sleep for real, to the 10 ms tick this target's clock has.
        // `allow_tick` is what makes the deadline reachable at all — see its
        // own comment; without it a sleeper spinning while any other task also
        // spins in the kernel freezes the very counter it is waiting on.
        Syscall::Nanosleep => {
            let Some([sec, nsec]) = crate::uaccess::read_val::<[i64; 2]>(a1) else {
                return errno::EFAULT;
            };
            if sec < 0 || !(0..1_000_000_000).contains(&nsec) {
                return errno::EINVAL;
            }
            let want_us = (sec.cast_unsigned())
                .saturating_mul(1_000_000)
                .saturating_add(nsec.cast_unsigned() / 1000);
            let deadline = crate::net::uptime_us().saturating_add(want_us);
            while crate::net::uptime_us() < deadline {
                crate::sched::yield_now();
                crate::sched::allow_tick();
                // A thread whose group is exiting must not finish its nap
                // first: `thread::drain` is waiting on it. Same argument as the
                // futex wait loop's own check, and the same errno.
                if crate::thread::should_leave_now() {
                    return errno::EINTR;
                }
            }
            // `rem` is only written on an interrupted sleep, and this one
            // cannot be interrupted except by the group exit above (which does
            // not return here). A completed sleep leaves it untouched, as Linux
            // does.
            0
        }
        // The child-tid futex address a threaded libc registers on startup.
        // Single-address-space, no `CLONE_THREAD` here, so it is recorded
        // nowhere and the return value (the caller's tid) is ignored.
        Syscall::SetTidAddress => 1,
        Syscall::SchedYield => {
            // The switch happens on *this task's* kernel stack, which is the
            // whole reason UserCtx is per-task: two processes sharing one
            // syscall stack would clobber each other's saved frame here.
            crate::sched::yield_now();
            0
        }
        // `sysinfo(struct sysinfo *)` — x86_64 99. `busybox free`/`top` read
        // total/free RAM from here, not from `/proc/meminfo`, and a missing one
        // means `used = total - free - …` underflows to 18 quintillion.
        Syscall::Sysinfo => sys_sysinfo(a1),
        // `syslog(type, buf, len)` — x86_64 103 (`klogctl`). Backed by the
        // console ring buffer in `serial.rs`, so `busybox dmesg` returns the
        // kernel's own boot/diagnostic output over ssh — the only way to read
        // it on the reference box, whose console is a write-only framebuffer.
        Syscall::Syslog => sys_syslog(a1, a2, a3),
        // `statfs(path, buf)` / `fstatfs(fd, buf)` — x86_64 137 / 138. `busybox
        // df` reads `/proc/mounts` and then calls `statfs` once per line; with
        // neither of them it printed a header and nothing else. Both report the
        // mount that actually serves the path, out of `fs.rs`'s mount table,
        // rather than one set of hardcoded numbers for the whole kernel.
        Syscall::Statfs => crate::fd::sys_statfs(a1, a2),
        Syscall::Fstatfs => crate::fd::sys_fstatfs(a1, a2),
        // `reboot(magic1, magic2, cmd, arg)` — x86_64 169. The ABI decode is
        // shared with the aarch64 kernel (`akuma-boot`); the x86 machine reset
        // under it is `reboot.rs`. `busybox reboot`/`halt`/`poweroff` all land
        // here.
        Syscall::Reboot => crate::reboot::sys_reboot(a1, a2, a3, a4),
        Syscall::Newfstatat => crate::fd::sys_newfstatat(a1, a2, a3, a4),
        // `statx` — arch-neutral struct, so this is glue's arm with no
        // preamble. New on this target (4b batch 3b); was `ENOSYS`.
        Syscall::Statx => crate::fd::sys_statx(a1, a2, a3, a4, a5),
        Syscall::Faccessat => crate::fd::sys_access(a2),
        // `dup(fd)` — x86_64 32. `apk` dups a reopened index fd during
        // signature-verification I/O setup; `ENOSYS` here made it report
        // `UNTRUSTED signature` over a fetch that was fine (see
        // `fd::sys_dup`; the aarch64 twin is `APK_MISSING_SYSCALLS.md`).
        Syscall::Dup => crate::fd::sys_dup(a1),
        Syscall::Dup3 => crate::fd::sys_dup3(a1, a2, a3),
        Syscall::Pipe2 => crate::fd::sys_pipe2(a1, a2),
        // `mkdirat` (258) / `unlinkat` (263) / `renameat` (264) — **served by
        // glue** (4b step 2, batch 1: the path-only `*at` family). `apk`'s
        // cache write is a named `.tmp.<pid>` file plus a rename; without
        // these the cache write fails and the index fetch is unusable (the
        // aarch64 table in `APK_MISSING_SYSCALLS.md` lists all three). The
        // VFS underneath is the same `akuma_vfs_glue` mount walk both kernels
        // share, so the arm answers identically; the dirfd resolution and
        // errno-table divergences the fold adopted are stated in
        // `AKUMA_AMD64_4B_FOLD_BATCH1.md`.
        Syscall::Mkdirat => to_glue(call, [a1, a2, a3, 0, 0, 0]),
        Syscall::Unlinkat => to_glue(call, [a1, a2, a3, 0, 0, 0]),
        Syscall::Renameat => to_glue(call, [a1, a2, a3, a4, 0, 0]),
        // `ppoll(fds, nfds, *timespec, sigmask, sigsetsize)` — x86_64 271. Same
        // core; a NULL timespec means wait forever, otherwise fold sec+nsec to
        // milliseconds (this target has no finer clock to honour anyway).
        Syscall::Ppoll => {
            let timeout_ms = if a3 == 0 {
                (-1i64) as u64
            } else {
                // A user `struct timespec` { i64 tv_sec, i64 tv_nsec }.
                let Some([sec, nsec]) = crate::uaccess::read_val::<[i64; 2]>(a3) else {
                    return errno::EFAULT;
                };
                (sec.max(0) as u64)
                    .saturating_mul(1000)
                    .saturating_add((nsec.max(0) as u64) / 1_000_000)
            };
            crate::fd::sys_poll(a1, a2, timeout_ms)
        }
        // `execve(path, argv, envp)` — x86_64 59: the current (spawned or
        // forked) task replaces its own image in place. See `sys_execve`.
        Syscall::Execve => sys_execve(a1, a2, a3),
        Syscall::Clone => {
            // `CLONE_VM` is the fork/thread fork in the road, and the only one:
            // with it the caller wants to share an address space
            // (`crate::thread`), without it it wants a copy (`sys_fork`).
            //
            // x86_64's argument order is its own: `(flags, child_stack,
            // parent_tid, child_tid, tls)` — `tls` **last**, after `child_tid`,
            // where most architectures put it fourth.
            if a1 & clone_flags::CLONE_VM != 0 {
                return crate::thread::sys_clone_thread(a1, a2, a3, a4, a5);
            }
            sys_fork()
        }
        // `gettid` — x86_64 186. Its own tid for a thread, its pid for a main
        // thread. Rust's `std` prints it in a panic message, which is how its
        // absence announced itself: `thread 'main' (18446744073709551615)`.
        Syscall::Gettid => u64::from(crate::thread::current_tid()),
        // `futex` — x86_64 202. Six arguments, which is why `syscall_entry`
        // now forwards `a6`.
        Syscall::Futex => crate::futex::sys_futex(a1, a2, a3, a4, a5, a6),
        // `wait4(pid, wstatus, options, rusage)` — x86_64 61. Route into the
        // Akuma-private `waitpid` table, but **block** (unless `WNOHANG`): a
        // forked shell calls `wait4(pid, &st, 0, 0)` expecting to sleep until
        // the child is done, where `sys_waitpid` alone just returns 0.
        Syscall::Wait4 => {
            const WNOHANG: u64 = 0x0000_0001;
            let me = crate::sched::current_task();
            loop {
                // Arm, then join the waiter set, then ask. Both steps go before
                // the question for the same reason: a child that exits between
                // the answer and the park must leave something behind, and
                // `wait4_wake_all` reaching an armed, registered task is that
                // something. Registering *after* asking would reopen the window
                // this ordering exists to close.
                wait4_register(me);
                // 0 = a matching child exists but has not exited; anything else
                // is a reaped pid or `-ESRCH`.
                let r = sys_waitpid(a1, a2, a3);
                if r != 0 || a3 & WNOHANG != 0 {
                    wait4_unregister(me);
                    return r;
                }
                crate::sched::block_current();
                wait4_unregister(me);
            }
        }
        // uname(2) — **the first arm served by `akuma-syscalls-glue`** (C1 step 3).
        //
        // It was already "the same answer the aarch64 kernel gives, machine
        // string aside"; now it *is* that answer. The machine string was the one
        // real difference and it moved into glue as `UTS_MACHINE`, derived from
        // `target_arch` — a shared `uname` reporting `aarch64` on this box would
        // be a wrong answer nothing refuses.
        //
        // Two fields change what they print, deliberately: `release` and
        // `version` now come from glue's build identity (`<git-sha>-<profile>`)
        // rather than `banner::RELEASE`/`VERSION_DESC`. That is the fold working
        // — one answer, not two — and it is a gain: `uname -a` here now names the
        // commit it is running. `banner::print()` keeps the local strings for the
        // boot banner, which is where the target's name belongs.
        Syscall::Uname => to_glue(call, [a1, a2, a3, a4, a5, a6]),
        // Credentials — **served by glue** (C1 step 3, second batch).
        //
        // Chosen next because they are the whole of `akuma_syscalls::fast_path`'s
        // `Leaf` tier that this target dispatches: they take no arguments and
        // consult no `Process`, so glue's prologue skips the identity resolve
        // and there is nothing here for a process table this target does not
        // populate to answer wrongly. Both kernels already returned a literal
        // `0` (glue's `geteuid` is a function whose entire body is `0`), so this
        // is the rare fold with no divergence to pin: the answer is identical
        // and only the number of places it is written down changes.
        //
        // **Not folded, deliberately:** `getpid`/`gettid`/`getppid`/`getpgid`/
        // `getsid`/`getcwd` below. Those *are* identity — glue reads them out of
        // `akuma_exec`'s process table, which this target does not fill, so it
        // would answer confidently and wrongly rather than not at all. They wait
        // for C1 step 5, the same step `exec_runtime.rs`'s `futex_wake` stub
        // names. See `docs/archive/AKUMA_AMD64_C1_STEP3_PREREQUISITES.md`.
        Syscall::Getuid
        | Syscall::Getgid
        | Syscall::Geteuid
        | Syscall::Getegid
        // `setuid`/`setgid`: already root, accept. Glue folds them in with
        // `capset`/`setres[ug]id`/`setgroups` under one stated stance —
        // "success" here means *not implemented*, not "privileges dropped".
        | Syscall::Setuid
        | Syscall::Setgid
        // `getgroups(size, list)` — x86_64 115. Not a fold: this target had no
        // arm for it at all, and glue's has been there all along. Found by the
        // ring-3 check this batch was verified with — `busybox id` on the metal
        // printed `uid=0 gid=0` (the folded arms answering) and then
        // `id: can't get groups` and exited 1. No supplementary groups exist
        // here, so the answer is the count `0`, and `size == 0` is the probe
        // form every caller actually uses.
        | Syscall::Getgroups => to_glue(call, [a1, a2, a3, a4, a5, a6]),
        // Signals: this kernel has none, so "the mask is empty and stays empty"
        // is the correct result, not a stub. `rt_sigprocmask` writes the old
        // (empty) set back if asked.
        Syscall::RtSigaction => 0, // rt_sigaction
        Syscall::RtSigprocmask => {
            if a3 != 0 {
                let n = (a4 as usize).min(8);
                // The old set, empty, into a user `sigset_t` bounded by sigsetsize.
                // This was the last raw user write in the kernel; SMAP found it
                // (`memset` → `#PF err=3` at a user stack address) the first boot
                // it was on.
                if !crate::uaccess::write_bytes(a3, &[0u8; 8][..n]) {
                    return errno::EFAULT;
                }
            }
            0
        }
        // Best-effort robustness/rlimit hooks musl pokes on startup.
        Syscall::SetRobustList => 0,          // set_robust_list
        // `prlimit64(pid, resource, new, old)` — **served by glue** (C1 step 3,
        // batch 3). A leaf: it reads a fixed table and `ExecConfig`, never a
        // `Process`.
        //
        // This arm was `=> 0`, which is not a stub but a **wrong answer**:
        // `prlimit64` returning success without writing `old_rlim` leaves the
        // caller reading whatever was on its stack as its own stack and
        // file-descriptor limits. musl's `getrlimit` is this syscall, and a
        // build system asking `RLIMIT_NOFILE` before sizing a poll set is
        // exactly the shape that then fails somewhere else entirely.
        // Glue fills it: `RLIMIT_STACK` from `ExecConfig::user_stack_size`
        // (real on this target — `exec_runtime.rs`), `RLIMIT_NOFILE` 1024,
        // everything else `RLIM_INFINITY`.
        Syscall::Prlimit64 => to_glue(call, [a1, a2, a3, a4, a5, a6]),
        // `readlinkat` / `symlinkat` — **served by glue** (4b step 2, batch 1).
        // Glue's readlinkat is strictly more than the arm it replaces: it
        // distinguishes `EINVAL` (path exists, not a symlink) from `ENOENT`
        // where the local arm collapsed both to `ENOENT`, serves
        // `/proc/self/exe` from the registered image, and describes
        // non-file fds under `/proc/<pid>/fd`.
        Syscall::Readlinkat => to_glue(call, [a1, a2, a3, a4, 0, 0]),
        // `symlinkat(target, newdirfd, link_path)` — x86_64 266. Package
        // contents are full of `.so.1` versioned-library symlinks; ENOSYS
        // here turned each into a counted `apk add` error.
        Syscall::Symlinkat => to_glue(call, [a1, a2, a3, 0, 0, 0]),
        // `utimensat(dirfd, path, times, flags)` — timestamp preservation for
        // `apk add`'s post-extract pass. NULL times = both set to now. There is
        // no `futimens` syscall to pair it with — libc spells that
        // `utimensat(fd, NULL, times, 0)` — which is what the x86_64 88 arm
        // above used to be mistaken for.
        Syscall::Utimensat => crate::fd::sys_utimensat(a1, a2, a3, a4),
        // Process-group / session ids. One process, so it is its own group and
        // session leader; `setpgid`/`setsid` accept and report id 1.
        Syscall::Getppid => 1,
        Syscall::Getpgid | Syscall::Getsid => 1,
        // `setpgid`/`setsid` accept and report id 1.
        Syscall::Setpgid | Syscall::Setsid => 0,
        // `getcwd(buf, size)` — this target has no per-process cwd; it is always
        // root. Linux returns the length *including* the NUL.
        Syscall::Getcwd => {
            if a1 == 0 || a2 < 2 {
                return errno::EINVAL;
            }
            // A user buffer of at least `a2` bytes, `a2 >= 2` checked.
            if !crate::uaccess::write_bytes(a1, b"/\0") {
                return errno::EFAULT;
            }
            2
        }
        // `mprotect` — real since the region table landed (2026-09-07). It
        // splits the regions the range crosses and re-permissions the pages
        // that are present; see `mm::sys_mprotect` for why it was `return 0`
        // for so long and what that cost.
        Syscall::Mprotect => crate::mm::sys_mprotect(a1, a2, a3),
        // `flock(fd, op)` — x86_64 73. One user, one process at a time on
        // this target (no fork-based package-manager concurrency exists to
        // race against), so there is nothing an advisory lock could actually
        // protect — accept and do nothing, the same stance `mprotect` above
        // takes. Without this, `apk`'s database lock (`flock` on
        // `/lib/apk/db/lock`) came back `ENOSYS` and it treated that as fatal:
        // `apk update` printed "Unable to lock database: Function not
        // implemented" and exited before ever reaching the network.
        Syscall::Flock => 0,
        // `sendmsg`/`recvmsg` — x86_64 46/47. musl's DNS resolver on this
        // build uses these, not `sendto`/`recvfrom`; without them `poll`
        // correctly reported a UDP reply readable and `recvmsg` came back
        // `ENOSYS`, so `apk`'s own name resolution spun forever. See
        // `sock::sys_sendmsg`/`sock::sys_recvmsg`.
        Syscall::Sendmsg => crate::sock::sys_sendmsg(a1, a2, a3),
        Syscall::Recvmsg => crate::sock::sys_recvmsg(a1, a2, a3),
        // `clock_gettime(clockid, *timespec)` — x86_64 228. `CLOCK_REALTIME`
        // (0) reads `clock::now_us()` — `0` until `clock::sync_via_sntp`
        // succeeds, exactly the "every real TLS certificate looks not-yet-
        // valid" bug this syscall existing at all closes
        // (`docs/archive/AKUMA_FIRECRACKER_AMD64.md` §3.29.5/§3.30).
        // `CLOCK_MONOTONIC` (1) and anything else read `net::uptime_us`
        // instead: always available with no SNTP dependency, which is all a
        // monotonic clock ever promised (an arbitrary epoch, not the Unix
        // one) — busybox `sh`'s own `poll` timeout math and similar callers
        // that just want *a* moving clock get one either way.
        Syscall::ClockGettime => {
            const CLOCK_REALTIME: u64 = 0;
            let us = if a1 == CLOCK_REALTIME { crate::clock::now_us() } else { crate::net::uptime_us() };
            // A user `struct timespec { i64 tv_sec, i64 tv_nsec }`.
            let ts = [(us / 1_000_000).cast_signed(), ((us % 1_000_000) * 1000).cast_signed()];
            if !crate::uaccess::write_val(a2, ts) {
                return errno::EFAULT;
            }
            0
        }
        // The write side of the clock — x86_64 227 `clock_settime`, 164
        // `settimeofday`, 159 `adjtimex`.
        //
        // This is how the machine gets a usable time when the kernel's own
        // SNTP does not manage it: `busybox ntpd -q` fetches the time and
        // steps the clock through these. Without them it fetches correctly and
        // then fails to apply the answer, which looks exactly like a network
        // problem and is not.
        //
        // Why it matters beyond `date` being wrong: at the epoch **every TLS
        // certificate on earth is not-yet-valid**, and `apk` reports that as
        // `server certificate not trusted` — sending you to look at the CA
        // bundle, which is fine.
        Syscall::ClockSettime => {
            const CLOCK_REALTIME: u64 = 0;
            if a1 != CLOCK_REALTIME {
                return errno::EINVAL;
            }
            let Some(ts) = crate::uaccess::read_val::<akuma_syscalls_linux::time::Timespec>(a2)
            else {
                return errno::EFAULT;
            };
            if ts.tv_sec < 0 || ts.tv_nsec < 0 || ts.tv_nsec >= 1_000_000_000 {
                return errno::EINVAL;
            }
            crate::clock::set_unix_us(
                (ts.tv_sec.cast_unsigned()).saturating_mul(1_000_000)
                    + (ts.tv_nsec.cast_unsigned() / 1000),
            );
            0
        }
        Syscall::Adjtimex => {
            // `adjtimex(buf)`. This target has no frequency discipline — the
            // tick comes from a PIT-calibrated LAPIC and nothing slews it — so
            // the honest implementation reports the current time and an
            // otherwise zeroed state, and accepts a step through `ADJ_SETOFFSET`
            // because that is a real capability here.
            //
            // Returning `ENOSYS` instead is what makes `ntpd` give up before it
            // ever sends a packet: it probes the clock's state on startup.
            const ADJ_SETOFFSET: u32 = 0x0100;
            const TIME_OK: u64 = 0;
            let Some(mut tx) = crate::uaccess::read_val::<akuma_syscalls_linux::time::Timex>(a1)
            else {
                return errno::EFAULT;
            };
            if tx.modes & ADJ_SETOFFSET != 0 {
                let now = crate::clock::now_us();
                let delta = tx.time_sec.saturating_mul(1_000_000).saturating_add(tx.time_usec);
                let stepped = now.cast_signed().saturating_add(delta).max(0);
                crate::clock::set_unix_us(stepped.cast_unsigned());
            }
            let now = crate::clock::now_us();
            tx = akuma_syscalls_linux::time::Timex {
                time_sec: (now / 1_000_000).cast_signed(),
                time_usec: (now % 1_000_000).cast_signed(),
                // A tick of exactly `US_PER_TICK_TARGET`: it is what the
                // calibration makes true, and reporting the Linux default of
                // 10000 by accident would be right only by coincidence.
                tick: i64::from(crate::lapic::US_PER_TICK_TARGET),
                ..akuma_syscalls_linux::time::Timex::default()
            };
            if !crate::uaccess::write_val(a1, tx) {
                return errno::EFAULT;
            }
            TIME_OK
        }
        _ => errno::ENOSYS,
    }
}

/// `writev(fd, iov, iovcnt)` — `struct iovec` is `{ base: *const u8, len: usize }`,
/// 16 bytes. Forwards each segment through `sys_write`; a short or failing
/// segment ends the walk and the total so far (or the error, if nothing was
/// written) is returned, per POSIX.
fn sys_writev(fd: u64, iov: u64, cnt: u64) -> u64 {
    use crate::fd::errno;
    let cnt = (cnt as usize).min(1024);
    let mut total: u64 = 0;
    for i in 0..cnt {
        let e = iov + (i as u64) * 16;
        // One `struct iovec { void *base; size_t len }` of the user's array.
        let Some([base, len]) = crate::uaccess::read_val::<[u64; 2]>(e) else {
            return if total == 0 { errno::EFAULT } else { total };
        };
        if len == 0 {
            continue;
        }
        let n = sys_write(fd, base, len);
        if errno::is_err(n) {
            // An errno (top of the u64 range). Return it only if nothing has
            // gone out yet; otherwise report the partial success.
            return if total == 0 { n } else { total };
        }
        total += n;
        if n < len {
            break;
        }
    }
    total
}

/// `readv(fd, iov, iovcnt)` — the mirror of [`sys_writev`].
fn sys_readv(fd: u64, iov: u64, cnt: u64) -> u64 {
    use crate::fd::errno;
    let cnt = (cnt as usize).min(1024);
    let mut total: u64 = 0;
    for i in 0..cnt {
        let e = iov + (i as u64) * 16;
        let Some([base, len]) = crate::uaccess::read_val::<[u64; 2]>(e) else {
            return if total == 0 { errno::EFAULT } else { total };
        };
        if len == 0 {
            continue;
        }
        let n = crate::fd::sys_read(fd, base, len);
        if errno::is_err(n) {
            return if total == 0 { n } else { total };
        }
        total += n;
        if n < len {
            break;
        }
    }
    total
}

/// `sysinfo(2)` — memory totals for `busybox free` / `top`. The struct layout
/// is `akuma_syscalls_linux::proc::Sysinfo` (LP64, host-tested, shared with the
/// AArch64 kernel). `mem_unit = 1` so every `*ram` field is already in bytes;
/// a `0` there is what makes `free` print `16.0E`.
fn sys_sysinfo(out: u64) -> u64 {
    use akuma_syscalls_linux::proc::Sysinfo;
    if out == 0 {
        return crate::fd::errno::EFAULT;
    }
    let page = 4096u64;
    let heap = akuma_alloc::stats();
    let info = Sysinfo {
        uptime: i64::try_from(crate::net::uptime_us() / 1_000_000).unwrap_or(i64::MAX),
        totalram: akuma_pmm::total_count() as u64 * page,
        freeram: akuma_pmm::free_count() as u64 * page,
        bufferram: heap.allocated as u64,
        procs: proc_count(),
        mem_unit: 1,
        ..Sysinfo::default()
    };
    // SAFETY: `Sysinfo` is `repr(C)` and `Copy` with no padding of interest;
    // `write_bytes` bounds-checks the user pointer.
    let bytes = unsafe {
        core::slice::from_raw_parts(
            core::ptr::addr_of!(info).cast::<u8>(),
            core::mem::size_of::<Sysinfo>(),
        )
    };
    if !crate::uaccess::write_bytes(out, bytes) {
        return crate::fd::errno::EFAULT;
    }
    0
}

/// A rough count of live processes, for `sysinfo`'s `procs`. Best-effort — the
/// number is cosmetic in `free`/`top` on this target.
fn proc_count() -> u16 {
    // `akuma-exec`'s table since 5b slice 4, which is the only process table
    // there is now. It answers a slightly different question than the array it
    // replaced — registered processes rather than occupied process slots — and
    // for a cosmetic `sysinfo` field the registered count is the better one:
    // it is what `ps` lists.
    akuma_exec::process::process_count().min(u16::MAX as usize) as u16
}

/// `syslog(type, bufp, len)` — the `klogctl(2)` operations `busybox dmesg`
/// uses, served from `serial.rs`'s console history ring.
///
/// `SIZE_BUFFER`/`SIZE_UNREAD` report what is retrievable; `READ`/`READ_ALL`
/// copy history into the caller's buffer (the ring only ever keeps the tail, so
/// both behave the same here); `READ_CLEAR`/`CLEAR` also discard it. The
/// console-level and open/close actions are accepted as no-ops — there is no
/// priority filtering on this target.
///
/// The action numbers are [`akuma_dmesg::Action`] rather than local `const`s:
/// an action mapped to the wrong arm makes `dmesg` clear the log instead of
/// reading it and reports no error, and the decode is the half that a host test
/// can pin. The AArch64 kernel decodes through the same table.
fn sys_syslog(action: u64, bufp: u64, len: u64) -> u64 {
    use crate::fd::errno;
    use akuma_dmesg::Action;

    let Some(action) = Action::decode(action) else {
        return errno::EINVAL;
    };

    if action.is_noop() {
        return 0;
    }
    if action.sizes() {
        return crate::serial::klog_len() as u64;
    }
    if !action.reads() {
        // `Clear` — the only remaining non-reading action.
        crate::serial::klog_clear();
        return 0;
    }

    if bufp == 0 || len == 0 {
        return errno::EINVAL;
    }
    // Bounded staging buffer, but **not** a bound on the answer: copy the ring
    // out a chunk at a time until the caller's buffer is full or the history
    // runs out.
    //
    // It used to be `min(4096)` with a single pass, which made this buffer a
    // hard ceiling on `dmesg` — `SIZE_BUFFER` advertised 64 KiB (the real ring),
    // `busybox dmesg` allocated that and asked for it, and got the last 4 KiB
    // back with no indication anything was missing. Every boot-time diagnostic
    // older than the last few seconds was unreachable on the one target where
    // the console has no scrollback: an xHCI bring-up printed its whole trace
    // and then the NIC's stall dumps pushed it out of what could be read.
    const CHUNK: usize = 4096;
    let mut stage = [0u8; CHUNK];
    let want = (len as usize).min(crate::serial::klog_len());
    let mut done = 0usize;
    while done < want {
        let take = (want - done).min(CHUNK);
        let n = crate::serial::klog_snapshot_from(done, &mut stage[..take]);
        if n == 0 {
            break;
        }
        if !crate::uaccess::write_bytes(bufp + done as u64, &stage[..n]) {
            return errno::EFAULT;
        }
        done += n;
    }
    if action.clears() {
        crate::serial::klog_clear();
    }
    done as u64
}

/// `arch_prctl(code, addr)` — x86_64 syscall 158, the TLS-base primitive.
///
/// musl's `__init_tp` calls `arch_prctl(ARCH_SET_FS, tp)` as its very first
/// syscall and `hlt`s (crashes) if it fails, so this is the wall for running
/// any musl binary. It writes `IA32_FS_BASE` (or `GS_BASE`) directly — the x86
/// analogue of AArch64's `set_tpidr_el0`.
///
/// **Single-TLS-user assumption:** the kernel itself never touches FS/GS base
/// and there is no per-task save/restore, so this value simply persists across
/// preemption. Correct while one program at a time uses TLS (the shell); two
/// concurrent musl processes would clobber each other and need the base saved
/// in `UserCtx` and reloaded on switch. Deferred — noted in
/// `AKUMA_FIRECRACKER_AMD64.md`.
fn sys_arch_prctl(code: u64, addr: u64) -> u64 {
    const ARCH_SET_GS: u64 = 0x1001;
    const ARCH_SET_FS: u64 = 0x1002;
    const ARCH_GET_FS: u64 = 0x1003;
    const ARCH_GET_GS: u64 = 0x1004;
    const IA32_FS_BASE: u32 = 0xC000_0100;
    match code {
        ARCH_SET_FS => {
            // SAFETY: sets the current CPU's FS base to a userspace-supplied
            // linear address. A bad value can only fault EL0's own `%fs:`
            // accesses, exactly as on Linux. Also record it in this task's
            // `UserCtx` so the scheduler can restore it on the way back in — the
            // MSR is CPU-global, and without the per-task copy a forked child
            // that `execve`s (and re-`arch_prctl`s) leaves the parent running on
            // the child's TLS base. That crash (`cr2` a tiny offset off a
            // garbage pointer) is what made this field load-bearing.
            unsafe {
                wrmsr(IA32_FS_BASE, addr);
                let uctx = crate::smp::current_uctx();
                if !uctx.is_null() {
                    (*uctx).fs_base = addr;
                }
            }
            0
        }
        ARCH_SET_GS => {
            // The program's GS lives in IA32_KERNEL_GS_BASE while the kernel
            // runs — IA32_GS_BASE is this core's per-CPU block right now, and
            // writing the program's value there would point every `gs:[..]` in
            // the kernel at user memory. Recorded per task for the same reason
            // as FS.
            set_user_gs_base(addr);
            // SAFETY: this task's own `UserCtx`.
            unsafe {
                let uctx = crate::smp::current_uctx();
                if !uctx.is_null() {
                    (*uctx).gs_base = addr;
                }
            }
            0
        }
        ARCH_GET_FS => {
            if !crate::uaccess::write_val::<u64>(addr, rdmsr(IA32_FS_BASE)) {
                return crate::fd::errno::EFAULT;
            }
            0
        }
        ARCH_GET_GS => {
            if !crate::uaccess::write_val::<u64>(addr, rdmsr(crate::smp::IA32_KERNEL_GS_BASE)) {
                return crate::fd::errno::EFAULT;
            }
            0
        }
        _ => crate::fd::errno::EINVAL,
    }
}

/// `write(fd, buf, len)` — the serial console, and **`akuma-syscalls-glue`'s
/// arm** for everything else (4b batch 3a).
///
/// # The console is why this function still exists
///
/// Two reasons, and only the first is about the console.
///
/// Glue's `Stdout`/`Stderr`/`DevTty` arm writes through
/// `akuma_exec::process::current_channel()` — a `ProcessChannel`, which is the
/// SSH/PTY plumbing — and **silently writes nothing** when there is none. No
/// process on this target has one: a spawned child's output is a pipe
/// (`fd::bind_stdio`), and init on the serial line has no channel at all. So
/// delegating fd 1 would make `INIT=/bin/hello` print nothing while reporting
/// every byte written, which is exactly the failure `fd::console_end`'s own
/// header describes from the other direction.
///
/// And the `WRITE_SEQ`/`WRITE_CPU` bookkeeping below: the multitasking and
/// preemption tests prove interleaving by recording *which task and which core*
/// performed each write, and that instrumentation belongs next to the tests
/// that read it.
///
/// # The order is the fix, not a tidy-up
///
/// A console descriptor is asked about **first**. A registered process's table
/// has `Stdout`/`Stderr` at 1/2 (`SharedFdTable::with_stdio`), so any
/// "is it bound?" test is *true* for init — the pre-batch-2a version of this
/// function fell through to the file path on exactly that and answered `EBADF`.
/// See [`crate::fd::console_end`] for the two spellings (a bound descriptor,
/// and an unbound 0/1/2) and why both have to be asked.
///
/// # What the fold gained
///
/// - **Concurrent writers cannot corrupt each other.** Glue reserves the file
///   position with `reserve_write_pos` — read-and-advance in one lock hold —
///   before any I/O. Two `CLONE_FILES` siblings writing one descriptor each
///   read a stale cursor here and wrote over each other on disk.
/// - **A socket write re-arms the `EPOLLET` edge** on a short write, which this
///   kernel's `sock::send` path never did.
/// - **A pipe write blocks with the scheduler's backstop.** Glue parks through
///   `akuma_threading::park_indefinitely`, which this target now gives a 1 s
///   deadline (`sched::BACKSTOP_US`) — the prerequisite that had to land before
///   any blocking arm could fold at all.
fn sys_write(fd: u64, buf: u64, len: u64) -> u64 {
    const EFAULT: u64 = (-14i64) as u64;

    // See the header: first, and by both spellings.
    if crate::fd::console_end(fd) != Some(crate::fd::ConsoleEnd::Write) {
        // The `O_ACCMODE` refusal glue's `File` arm does not make — see
        // `fd::write_mode_refusal`, which is where the finding is written down.
        if let Some(e) = crate::fd::write_mode_refusal(fd) {
            return e;
        }
        return akuma_syscalls_glue::fs::sys_write(fd, buf, len as usize);
    }
    // An unbound 1 or 2, or a bound `Stdout`/`Stderr`: the console, full stop.
    if len > MAX_WRITE {
        return EFAULT;
    }
    // Fault-safe since 2026-09-05: a bad `buf` is EFAULT, not a halt. Chunked
    // through a stack buffer so the copy is one `rep movsb` per 256 bytes rather
    // than a recovered fault per byte, and so nothing here allocates.
    let mut chunk = [0u8; 256];
    let mut done = 0u64;
    while done < len {
        let n = ((len - done) as usize).min(chunk.len());
        if !crate::uaccess::read_bytes(buf + done, &mut chunk[..n]) {
            return if done == 0 { EFAULT } else { done };
        }
        for &byte in &chunk[..n] {
            serial::putb(byte);
        }
        done += n as u64;
    }
    let n = WRITE_SEQ_LEN.fetch_add(1, Ordering::Relaxed) as usize;
    if let Some(slot) = WRITE_SEQ.get(n) {
        slot.store(crate::sched::current_task() as u64, Ordering::Relaxed);
    }
    if let Some(slot) = WRITE_CPU.get(n) {
        slot.store(crate::smp::cpu_index() as u64, Ordering::Relaxed);
    }
    WRITTEN.fetch_add(len, Ordering::Relaxed);
    len
}

/// Enable `syscall`/`sysret`.
pub fn init_syscall() {
    let star = (u64::from(gdt::SYSRET_BASE) << 48) | (u64::from(gdt::KERNEL_CODE) << 32);
    // SAFETY: the four architectural MSRs of the fast-syscall path, written
    // before any `syscall` can be executed (userspace does not exist yet).
    unsafe {
        wrmsr(IA32_EFER, rdmsr(IA32_EFER) | EFER_SCE);
        wrmsr(IA32_STAR, star);
        wrmsr(IA32_LSTAR, syscall_entry as *const () as usize as u64);
        // Clear IF on entry, so a syscall handler never runs with interrupts on
        // while `rsp` still points into user memory. Also clear DF, so the
        // kernel's string operations start from a known direction — userspace
        // can set it and the ABI does not require it cleared on entry.
        // And AC (bit 18): with `CR4.SMAP` on, a program that sets AC and then
        // executes `syscall` would otherwise enter the kernel with SMAP
        // suspended — the one way userspace could grant itself the kernel's
        // access to user pages. Linux masks it for the same reason.
        wrmsr(IA32_FMASK, (1 << 9) | (1 << 10) | (1 << 18));
    }
}

/// Emit the user program into `out`, returning its total length.
///
/// Built rather than written as a byte literal so the message address and the
/// loop displacement are *computed*. A hand-assembled blob with hardcoded
/// operands stays correct until someone edits the message by one character.
///
/// ```text
///   mov r12, <rounds>
/// loop:
///   mov rax, <write>; mov rdi, 1; movabs rsi, <msg>; mov rdx, <len>; syscall
///   mov rax, <sched_yield>; syscall     ; hand the CPU to the other process
///   dec r12
///   jnz loop
///   mov rax, <exit_group>; mov rdi, <status>; syscall
///   jmp $                               ; a guard, not a fallthrough
///   <message bytes>
/// ```
#[cfg(not(feature = "no-tests"))]
fn build_user_program(
    out: &mut [u8],
    base_va: u64,
    msg: &[u8],
    rounds: u32,
    delay: u32,
    status: u32,
) -> usize {
    let mut n = 0;
    let mut emit = |bytes: &[u8], n: &mut usize| {
        out[*n..*n + bytes.len()].copy_from_slice(bytes);
        *n += bytes.len();
    };

    // mov r64, imm32 (sign-extended). The ModRM byte selects the destination.
    let mov_imm = |modrm: u8, v: u32| {
        let b = v.to_le_bytes();
        [0x48, 0xC7, modrm, b[0], b[1], b[2], b[3]]
    };
    const RAX: u8 = 0xC0;
    const RDI: u8 = 0xC7;
    const RDX: u8 = 0xC2;

    // mov r12, imm32 needs REX.WB (0x49) since r12 is an extended register.
    let r = rounds.to_le_bytes();
    emit(&[0x49, 0xC7, 0xC4, r[0], r[1], r[2], r[3]], &mut n);

    let loop_start = n;
    emit(&mov_imm(RAX, Syscall::Write.to_x86_64() as u32), &mut n);
    emit(&mov_imm(RDI, 1), &mut n);
    let movabs_at = n;
    emit(&[0x48, 0xBE, 0, 0, 0, 0, 0, 0, 0, 0], &mut n); // movabs rsi, msg
    emit(&mov_imm(RDX, msg.len() as u32), &mut n);
    emit(&[0x0F, 0x05], &mut n); // syscall

    if delay == 0 {
        // Cooperative: hand the CPU over explicitly.
        emit(&mov_imm(RAX, Syscall::SchedYield.to_x86_64() as u32), &mut n);
        emit(&[0x0F, 0x05], &mut n); // syscall
    } else {
        // Preemptive: burn time in ring 3 and never yield, so the only way this
        // process can stop running is the timer taking it off the CPU.
        //   mov r13, delay
        // spin:
        //   dec r13
        //   jnz spin
        let d = delay.to_le_bytes();
        emit(&[0x49, 0xC7, 0xC5, d[0], d[1], d[2], d[3]], &mut n);
        let spin = n;
        emit(&[0x49, 0xFF, 0xCD], &mut n); // dec r13
        let back = (n + 2) - spin;
        emit(&[0x75, (back as u8).wrapping_neg()], &mut n);
    }

    emit(&[0x49, 0xFF, 0xCC], &mut n); // dec r12
    // jnz rel8, back to loop_start. The displacement is measured from the *end*
    // of the jump, hence the +2 for the instruction's own bytes. Computed as a
    // positive distance and negated, so no signed cast is needed and the range
    // check is on a value that cannot already have wrapped.
    let back = (n + 2) - loop_start;
    debug_assert!(back <= 127, "loop body outgrew a rel8 jump");
    emit(&[0x75, (back as u8).wrapping_neg()], &mut n);

    emit(&mov_imm(RAX, Syscall::ExitGroup.to_x86_64() as u32), &mut n);
    emit(&mov_imm(RDI, status), &mut n);
    emit(&[0x0F, 0x05], &mut n); // syscall
    emit(&[0xEB, 0xFE], &mut n); // jmp $

    let msg_off = n;
    emit(msg, &mut n);

    let msg_va = base_va + msg_off as u64;
    out[movabs_at + 2..movabs_at + 10].copy_from_slice(&msg_va.to_le_bytes());
    n
}

/// A freshly built program image: an address space, where ring 3 starts in it,
/// and the `mmap` regions that come with it.
///
/// # It was `struct Process`, and the difference is that this is a *value*
///
/// Until 5b slice 4 this type was the process on this target, and it lived in
/// `static mut PROCS: [Option<Process>; 128]` indexed by process slot. Every
/// field of it now belongs to the registered [`akuma_exec::process::Process`]
/// that slices 1-2 already put in `PROCESS_TABLE`:
///
/// | was | is |
/// |---|---|
/// | `space: ProcAddressSpace` | `Process::address_space` (**owning**, was a `new_shared` view) |
/// | `entry: u64` | `Process::entry_point`, and `ProcessImage::context.pc` |
/// | `stack: u64` | `ProcessImage::context.sp` |
/// | `regions: Spinlock<Vec<MmapRegion>>` | `Process::mmap_regions` |
/// | `forked: bool` | [`UserCtx::forked`] — per *task*, beside the registers it refers to |
///
/// What is left is the handful of things a loader produces and a registration
/// consumes, so this is never stored in a table: [`register_exec_process`] takes
/// it by value and moves each field into the registered process. A failed
/// spawn simply drops it, which is what frees the half-built space —
/// the same `Drop` contract the old type had, minus the array.
struct Image {
    /// The address space the loader built, with the program in it.
    ///
    /// A plain [`UserAddressSpace`], not a [`ProcAddressSpace`]: nothing can
    /// reach this space concurrently until it is registered, and the lock and
    /// the lock-free root mirror are exactly what registration adds.
    space: UserAddressSpace,
    /// Where ring 3 starts executing.
    entry: u64,
    /// Initial `rsp`.
    stack: u64,
    /// The `mmap` regions this image starts life with.
    ///
    /// Empty for everything but a `fork` child, which inherits the parent's
    /// extents (`akuma_mmap::inherit_mmap_regions_for_cow_child`). Carrying the
    /// extent is the part that has been dropped before and the part that
    /// matters: a grandchild whose parent's regions read as zero-length shares
    /// nothing and faults on its first touch
    /// (`docs/archive/FORK_EXEC_HEAP_LAZY_REGION_SIGSEGV.md`).
    ///
    /// `MmapRegion::frames` is left **empty** on this target and `pages`
    /// carries the extent — the CoW-inherited shape the crate documents. Frame
    /// ownership is `akuma_user_space::FrameLedger`'s job — the one **inside**
    /// [`Image::space`] — which counts VAs per frame and is what teardown
    /// walks; a second frame list in the region would be a second answer to the
    /// same question.
    regions: Vec<MmapRegion>,
}

impl Image {
    /// Build an image that prints `msg` `rounds` times then exits with
    /// `status`.
    ///
    /// `delay == 0` yields between rounds (cooperative); anything else spins
    /// that many iterations in ring 3 and never yields, so only preemption can
    /// take it off the CPU.
    ///
    /// The program is assembled byte by byte by [`build_user_program`] rather
    /// than loaded from an image. That is deliberate even now that
    /// [`Self::from_elf`] exists: these two tests are about the scheduler and
    /// the timer, and a blob with no file format between it and the page table
    /// cannot fail for a loader's reasons.
    #[cfg(not(feature = "no-tests"))]
    fn new(msg: &[u8], rounds: u32, delay: u32, status: u32) -> Option<Self> {
        let mut space = UserAddressSpace::new()?;

        // Dropping `space` on either failure arm below releases its tables and
        // whatever the ledger has taken on — the explicit `space.free()` +
        // `free_all_frames` pair this replaces.
        let (Some(code), Some(stack)) = (akuma_pmm::alloc_page(), akuma_pmm::alloc_page()) else {
            return None;
        };

        // SAFETY: PMM frames are reachable through the physmap, so the program
        // can be staged *before* the address space that will hold it is ever
        // activated.
        unsafe {
            core::ptr::write_bytes(phys_ptr::<u8>(code as u64), 0, 4096);
            core::ptr::write_bytes(phys_ptr::<u8>(stack as u64), 0, 4096);
            let page = core::slice::from_raw_parts_mut(phys_ptr::<u8>(code as u64), 4096);
            build_user_program(page, USER_CODE_VA as u64, msg, rounds, delay, status);
        }

        // Tracked as they are mapped: `map_and_track_pte` records the frame
        // first and untracks it again if the map fails, so a refusal here leaves
        // nothing behind for `Drop` to double-free.
        if !space.map_and_track_pte(USER_CODE_VA, PhysFrame::new(code), PteProt::USER_RX, false)
            || !space.map_and_track_pte(
                USER_STACK_VA,
                PhysFrame::new(stack),
                PteProt::USER_RW,
                false,
            )
        {
            akuma_pmm::free_page(code, 0);
            akuma_pmm::free_page(stack, 0);
            return None;
        }
        Some(Self {
            space,
            entry: USER_CODE_VA as u64,
            stack: (USER_STACK_VA + 4096 - 16) as u64,
            regions: Vec::new(),
        })
    }

    /// Build a process from a linked ELF image.
    ///
    /// The address space comes back **from** the loader rather than going into
    /// it: since C1 step 6 the loading is `akuma-elf`'s, and that crate builds
    /// the space itself because the image's own headers decide what it needs.
    /// A failed load therefore drops the space inside the crate, where its
    /// destructor returns every frame the half-finished load had taken — the
    /// property `elf: rejected loads leak nothing` checks.
    /// Returns the process and what the loader found, so a caller can check the
    /// placement as well as the outcome.
    #[cfg(not(feature = "no-tests"))]
    fn from_elf(image: &[u8]) -> Result<(Self, loader::LoadedImage), &'static str> {
        Self::from_elf_argv(image, &[b"hello"])
    }

    /// As [`Self::from_elf`], with the argv the program sees on its initial
    /// stack. `sys_spawn` passes the real one (`sh -c "<cmd>"`); the tests pass
    /// a single element because `hello.rs` only checks `argv[0]`.
    fn from_elf_argv(
        image: &[u8],
        argv: &[&[u8]],
    ) -> Result<(Self, loader::LoadedImage), &'static str> {
        Self::from_elf_argv_envp(image, argv, &[])
    }

    /// As [`Self::from_elf_argv`], plus the environment. `execve` (Stage T)
    /// hands the new image the caller's whole `envp`; `spawn` passes none, and a
    /// program with an empty environment falls back to its own default `PATH`.
    fn from_elf_argv_envp(
        image: &[u8],
        argv: &[&[u8]],
        envp: &[&[u8]],
    ) -> Result<(Self, loader::LoadedImage), &'static str> {
        let (mut space, img) = loader::load(image)?;
        // `space` drops here on a stack failure, which frees the frames the
        // loader placed along with the page tables it built. That is exactly
        // the `free_all_frames` + `space.free()` pair this arm used to run by
        // hand, and it can no longer be forgotten on a new arm.
        let stack = loader::build_stack(&mut space, ELF_STACK_TOP, ELF_STACK_PAGES, argv, envp, &img)?;

        let entry = img.entry;
        Ok((Self { space, entry, stack, regions: Vec::new() }, img))
    }

    // Teardown is `Drop`, not a `free(self)` method.
    //
    // # What replaced it, and why it is not merely a rename
    //
    // `Process::free` was `loader::free_all_frames(&self.frames)` followed by
    // `self.space.free()` — two halves of one job, on two objects, that every
    // bail-out arm had to remember to run in that order. There were six such
    // arms. They are now the one `UserAddressSpace` destructor, which:
    //
    // * frees each **distinct** user frame once through `free_page_at`, whose
    //   first line is the same `cow_ref_dec` gate `free_all_frames` applied — so
    //   a page a `fork` sibling still maps is not released;
    // * frees the page-table frames from the **ledger's tracked set** rather
    //   than by re-walking the tables, which is the leak `map_and_track`'s
    //   mandatory ledger closed;
    // * and runs both through `free_or_defer_as_frames`, which parks everything
    //   if `any_core_on_l0` or `any_saved_ctx_on_l0` says a core or a preempted
    //   thread is still standing on this L0. `paging::activate` publishes into
    //   the first of those, which is the prerequisite that made a destructor
    //   safe on this target.
    //
    // The old two-paths-and-one-of-them-leaked history the removed doc comment
    // recorded — a `fork` child's post-fork `mmap` pages tracked by neither the
    // ledger nor the bump-window walk — is settled by the same change: there is
    // one ledger, inside the address space, and nothing else to consult.

    /// A `fork` child: `parent`'s address space **shared copy-on-write**,
    /// resuming at `entry`/`stack`
    /// (the parent's post-`fork` RIP/RSP) as a register-complete copy.
    ///
    /// `parent` is the **registered** `akuma_exec::Process` since 5b slice 4 —
    /// the only process there is now. The two things read out of it are the
    /// same two this function always read, under the same lock order
    /// (**regions → address space**): the region extents first, in one hold,
    /// then one walk of the tables.
    ///
    /// `None` if a frame for the child's PML4 or one of its page tables runs
    /// out — the shell then sees `fork` fail with `ENOMEM`, which is a
    /// survivable "can't fork" rather than a corrupt child. Both failure paths
    /// print which one they took: an `ENOMEM` that names the wrong resource is
    /// what made the scheduler's slot leak look like memory exhaustion for an
    /// afternoon (`docs/archive/AKUMA_AMD64_COW.md`).
    fn fork_of(
        parent: &akuma_exec::process::Process,
        entry: u64,
        stack: u64,
    ) -> Option<Self> {
        let Some(mut space) = UserAddressSpace::new() else {
            serial::puts("  [fork] no frame for a child PML4; pmm free=");
            serial::put_dec(akuma_pmm::free_count() as u64);
            serial::puts("\n");
            return None;
        };
        let mut ok = true;

        // The child's region list, and — separately — the VA ranges that must be
        // shared **by identity** rather than copy-on-write. Both are read out of
        // the parent's list in one hold, before the page walk below, because the
        // walk maps pages and must not run under the region lock.
        let (regions, shared_ranges) = {
            let _irq = akuma_primitives::irq::IrqGuard::new();
            let parent_regions = parent.mmap_regions.lock();
            let shared: Vec<(usize, usize)> = parent_regions
                .iter()
                .filter(|r| r.shared_anon)
                .map(|r| (r.start_va, r.start_va + r.len_bytes()))
                .collect();
            // The child maps every page of every parent region — read-only and
            // CoW-shared by the pass below — but *owns* none of them, which is
            // exactly the shape `inherit_mmap_regions_for_cow_child` produces.
            // Carrying the **extent** across is the part that matters and the
            // part that has been dropped before: a grandchild whose parent's
            // regions read as zero-length shares nothing and faults on its first
            // touch (`docs/archive/FORK_EXEC_HEAP_LAZY_REGION_SIGSEGV.md`).
            (akuma_mmap::inherit_mmap_regions_for_cow_child(&parent_regions), shared)
        };

        // One walk of the parent's tables, with the parent's own PTE edited in
        // place where it has to be demoted.
        //
        // `rewrite_leaves_in_range` replaces `for_each_user_leaf` + a second
        // `map_page_in` per demoted page: the walk already has the leaf slot in
        // hand, so a `Reprotect` is one store rather than a fresh four-level
        // descent. The child's pages are mapped inside the closure into
        // `space`, a **different** address space, so nothing here edits the
        // table the walk is standing on.
        //
        // The parent's hold is taken for the whole walk. That is the longest
        // `ProcAddressSpace` hold on this target and it is bounded by the
        // parent's residency; it has to be one hold, because a demote that
        // published halfway would leave the parent writable on pages the child
        // already shares.
        let mut parent_as = parent.address_space.lock();
        parent_as.rewrite_leaves_in_range(0, akuma_mmu::USER_HALF_END, |_ledger, leaf| {
            if !ok {
                return LeafAction::Keep;
            }
            let (va, pa) = (leaf.va, leaf.pa);
            let frame = PhysFrame::new(pa);

            // `MAP_SHARED | MAP_ANONYMOUS`: one object, not two copies.
            //
            // Everything else in an address space is private, so fork demotes it
            // to read-only and lets the first write break the sharing. Doing that
            // to a shared anonymous mapping gives parent and child separate pages
            // — the child's write becomes invisible to the parent, which is the
            // exact opposite of what the flag asks for, and it is how a process
            // pool coordinating through shared memory silently measures nothing
            // (`userspace/forktest/c_stress/shmanon.c`).
            //
            // So: same frame, writable in **both**, no CoW marker, and the
            // parent's own PTE deliberately left alone (`LeafAction::Keep`).
            if shared_ranges.iter().any(|(start, end)| va >= *start && va < *end) {
                let shared_rw = PteProt { write: true, ..leaf.prot };
                akuma_pmm::cow_ref_inc(frame.addr);
                space.track_user_frame(frame);
                if !space.map_page_pte(va, pa, shared_rw, false) {
                    if space.remove_user_frame(frame) && akuma_pmm::cow_ref_dec(frame.addr) {
                        akuma_pmm::free_page(frame.addr, 0);
                    }
                    ok = false;
                }
                return LeafAction::Keep;
            }

            // A page that is already read-only and *not* CoW stays exactly as it
            // is in both spaces — `.rodata`, an `mprotect(PROT_READ)` region.
            // Marking it would turn a legitimate `SIGSEGV` into a silent write.
            let share_writable = leaf.prot.write || leaf.cow;

            if share_writable {
                // Demote in **both** address spaces. Demoting only the child
                // leaves the parent writing straight through to memory the
                // child can see change — the entire point of CoW, missed.
                //
                // The parent's own PTE is rewritten by the `Reprotect` returned
                // below, in its live address space, and the walk issues the
                // `invlpg` plus the shootdown IPI (`TlbFlush::drop` waits for
                // the peers' acknowledgements), so a peer holding a stale
                // writable translation has it invalidated before `fork`
                // returns. The deadlock argument is on `set_shootdown_hooks`
                // in `akuma-mmu`: the sender holds the BKL, and the one
                // IRQ-masked state a peer can be stranded in — the BKL ticket
                // wait — services shootdowns inline.
                let demoted = PteProt { write: false, ..leaf.prot };
                akuma_pmm::cow_ref_inc(frame.addr);
                space.track_user_frame(frame);
                if !space.map_page_pte(va, pa, demoted, true) {
                    if space.remove_user_frame(frame) && akuma_pmm::cow_ref_dec(frame.addr) {
                        akuma_pmm::free_page(frame.addr, 0);
                    }
                    ok = false;
                    // The parent keeps its writable mapping: the child does not
                    // share this page, so demoting the parent would cost it a
                    // fault for nothing.
                    return LeafAction::Keep;
                }
                LeafAction::Reprotect(demoted, true)
            } else {
                // Read-only and unshared-by-marker: the child maps the same
                // frame at the same permissions. It still takes a reference,
                // because teardown of either process must not free a page the
                // other still maps.
                akuma_pmm::cow_ref_inc(frame.addr);
                space.track_user_frame(frame);
                if !space.map_page_pte(va, pa, leaf.prot, leaf.cow) {
                    if space.remove_user_frame(frame) && akuma_pmm::cow_ref_dec(frame.addr) {
                        akuma_pmm::free_page(frame.addr, 0);
                    }
                    ok = false;
                }
                LeafAction::Keep
            }
        });
        drop(parent_as);

        if !ok {
            serial::puts("  [fork] share pass failed; pmm free=");
            serial::put_dec(akuma_pmm::free_count() as u64);
            serial::puts(" pages=");
            serial::put_dec(space.user_frame_count() as u64);
            serial::puts("\n");
            // `space` drops on return, releasing every frame the pass above
            // managed to claim and every page table it built.
            return None;
        }
        Some(Self { space, entry, stack, regions })
    }
}

/// The registered process the running task belongs to, or `None`.
///
/// **This is the fault path's lookup**, and the whole cost question of 5b
/// slice 4 is in this one line. It used to be `current_proc_slot()` — a field
/// read from the per-CPU `UserCtx` — followed by one index into `static mut
/// PROCS`. It is now `akuma-exec`'s per-thread identity cache, which is the
/// same shape one level along: a per-CPU read of the task slot, then
/// `THREAD_IDENTITY[tid]`'s process-table slot and generation, then
/// `SlotTable::ref_if_current`. Both are O(1); the new one is a few atomic
/// loads rather than an array index.
///
/// The naive fold — `pid_for_thread` then a `find_process` scan of 256 slots —
/// is what the hand-off warned about, and it is not what this is. That cache
/// exists because the syscall boundary paid for it once already: 410 ns → 150 ns
/// by resolving identity per *thread* instead of per call
/// (`akuma_exec::process::table::THREAD_IDENTITY`, commit `c2a0e630`). Its key
/// is the thread id, which on this target **is** the scheduler task slot
/// (`X86ArchHooks::current_slot`), and that is the same key
/// `register_exec_process` inserts under — so the cache resolves on the first
/// try for every process this kernel starts.
///
/// `own`, not `tgid`: a `CLONE_VM` thread is published into the map under its
/// *process's* pid (`crate::thread`), so the own half already answers "my
/// process" for threads and costs one lookup instead of two.
///
/// `None` means the caller is not a registered user task — a kernel thread, or
/// the boot task driving the self-tests. Every caller must handle that rather
/// than defaulting: "no process" and "a process with no regions" are different
/// answers and only the second one may be served.
#[inline]
/// The registered `Process` behind the running task, if any.
///
/// `pub` since C2 slice 4: `fd.rs` resolves the registered `SharedFdTable`
/// through it (its `shared_table()`), the same accessor the fork path uses to
/// build the child's copy.
pub fn current_process() -> Option<&'static akuma_exec::process::Process> {
    akuma_exec::process::current_thread_own_process().map(|(_pid, p)| p)
}

/// Run `f` with the running process's `mmap` region list, under its lock.
///
/// # The lock is held for the whole closure
///
/// So do nothing inside it that can fault or take the PMM. `sys_mmap` reserves
/// its VA range here and then populates it *outside*, which is what makes a
/// concurrent `mmap` on another core impossible to collide with while keeping
/// the allocator out of the hold.
pub fn with_current_regions<R>(f: impl FnOnce(&mut Vec<MmapRegion>) -> R) -> Option<R> {
    let p = current_process()?;
    let _irq = akuma_primitives::irq::IrqGuard::new();
    Some(f(&mut p.mmap_regions.lock()))
}

/// Run `f` with the running process's user address space, under its lock.
///
/// The address-space counterpart of [`with_current_regions`], and the same
/// contract: `None` means the caller is not a registered user task — a kernel
/// thread, or the boot task driving the self-tests — and every caller must
/// handle that rather than acting on the kernel's own tables. That distinction
/// is not new but it is worth restating: until step 5a this file's callers
/// reached the page tables through `paging::active_root()`, which answers with
/// **`CR3`** whoever asks: on a kernel thread that is the kernel's own root, so
/// `munmap` walking a user range in it was at best a no-op and at worst an
/// unmap of something the kernel put there. `mm.rs` guarded that with a
/// separate `have_address_space()` probe; it cannot be forgotten now, because
/// there is no root to pass.
///
/// # The hold
///
/// Short. Every caller here holds it across one PTE edit or one bounded range
/// walk and nothing that blocks. `ProcAddressSpace::lock` **does** mask IRQs on
/// this target — `kernel_smp_shared` is a required feature of it since the
/// unblock (`docs/archive/AKUMA_AMD64_SMP_SHARED_UNBLOCK.md`) and `akuma-cpu`'s
/// `daif` has real x86 arms.
///
/// Lock order is **regions → address space**: `fault_in` and `dontneed_range`
/// take the region list first and reach the tables inside it. Nothing takes them
/// the other way round, and nothing may start.
pub fn with_current_address_space<R>(f: impl FnOnce(&mut akuma_mmu::UserAddressSpace) -> R) -> Option<R> {
    let p = current_process()?;
    Some(f(&mut p.address_space.lock()))
}

/// A copy-on-write break replaced `old` with `new` in the running process:
/// update its ledger so teardown frees what it actually holds.
///
/// Without this the private copy is untracked (leaked at exit, forever) and the
/// shared frame is still claimed by a process that no longer maps it (freed
/// twice, or freed while a sibling still reads it). Both are silent, and both
/// arrive long after the fault that caused them.
///
/// `remove_user_frame` reporting "last reference" is ignored on purpose: the
/// fault handler has already done the `cow_ref_dec` and freed the frame if that
/// was its call to make. This only edits the per-process ledger.
pub fn cow_swap_frame(old: usize, new: usize) {
    if let Some(p) = current_process() {
        let ledger = p.address_space.lock();
        let _ = ledger.remove_user_frame(akuma_mmap::PhysFrame::new(old));
        ledger.track_user_frame(akuma_mmap::PhysFrame::new(new));
    }
}

/// How many process slots this target has.
///
/// A process slot is **not** a process any more — since 5b slice 4 the process
/// itself is an `akuma_exec::Process` in `PROCESS_TABLE`. What is left keyed by
/// slot is this target's own mechanism: the `SPAWN` row (stdio pipes and the
/// scheduler task slot), `fd.rs`'s descriptor row, and `crate::thread`'s
/// group-exit flag and thread list. `UserCtx::proc_slot` is how a running task
/// names its own.
///
/// Slots 0..=6 are the self-tests and `run_init`; slots [`SPAWN_SLOT_BASE`]..
/// are for `sys_spawn`'d children (an `sshd` session's shell).
///
/// **Raised 16 → 128 on 2026-09-06.** Nine concurrent spawns was "past what the
/// cooperative `sshd` serves" only while a session stayed idle: a real one
/// (`apk add`, then any command) exhausted it and `sshd` reported
/// `failed to spawn '/bin/sh' for exec`. Slots *are* recycled on
/// `waitpid`/exit, so this is headroom against leaks and bursts, not a true
/// concurrency bound.
pub const PROC_SLOTS: usize = 128;

/// First process slot `sys_spawn` may use; everything below is the self-tests
/// and `run_init`.
pub const SPAWN_SLOT_BASE: usize = 7;

/// Where ring 3 starts for the running task: `(entry, stack)` off its
/// registered process, or `None` if it has none.
///
/// The pair is `ProcessImage::context`'s `pc`/`sp` — the field `akuma-exec`
/// already calls "the register state the first entry to ring 3 uses", which is
/// exactly what these two are. Slice 1 registered it zeroed with the note that
/// amd64 "never `eret`s from it"; that stays true and is beside the point, since
/// what is read here is the two scalars, not a register file.
///
/// Taken under `image`'s lock, which is also what makes an `execve` atomic
/// against this read: `sys_execve` writes both halves in one hold, so a task
/// re-entering ring 3 cannot pair a new entry point with an old stack.
fn current_entry_stack() -> Option<(u64, u64)> {
    let p = current_process()?;
    let img = p.image.lock();
    Some((img.context.pc, img.context.sp))
}

/// Read and clear this task's [`UserCtx::forked`] / [`UserCtx::exec_pending`]
/// flags. Both are one-shot and both are consumed by [`run_process`] only.
fn take_uctx_flag(read: impl Fn(&mut UserCtx) -> &mut u64) -> bool {
    // SAFETY: under the BKL; the per-CPU `UserCtx` pointer is this task's own
    // slot, and only this task reads or clears these two fields.
    unsafe {
        let uctx = crate::smp::current_uctx();
        if uctx.is_null() {
            return false;
        }
        let f = read(&mut *uctx);
        let was = *f != 0;
        *f = 0;
        was
    }
}

/// Enter ring 3 for process slot `idx`, then mark the task finished.
///
/// The scheduler has already installed this task's address space by the time
/// this runs — `spawn_in_space_unpublished` recorded the root, and `yield_now`
/// writes `CR3` before switching stacks.
///
/// The entry point and stack come from the registered process rather than from
/// module constants. They were constants while every process was the same
/// hand-assembled blob at the same address; an ELF's entry is `e_entry` and its
/// stack is wherever the loader could put one.
fn run_process(idx: usize) -> ! {
    // Tell the syscall path which process this is, so fd 0/1/2 route to this
    // task's pipes (if it is a spawned child) rather than the console.
    // SAFETY: under the BKL; the per-CPU `UserCtx` pointer is this task's own slot.
    unsafe {
        let uctx = crate::smp::current_uctx();
        if !uctx.is_null() {
            (*uctx).proc_slot = idx;
        }
    }
    // A `fork` child's first entry re-enters ring 3 at the parent's post-`fork`
    // instruction with the parent's full register set; the `execve` it usually
    // does next installs a plain image, and every later loop iteration uses the
    // ordinary entry path. Read once, here, because it is spent by the first
    // entry whatever happens after it.
    let mut forked_child = take_uctx_flag(|u| &mut u.forked);
    let mut status = 0;
    // Whether ring 3 was ever entered. The teardown below reports an exit —
    // `spawn_record_exit` publishes a status a parent's `wait4` will believe —
    // so a task that never ran a program must not run it. Unreachable by
    // construction since 5b slice 4 (every process task is registered before it
    // is published) and checked rather than assumed, because the old shape got
    // this for free: it wrapped the whole teardown in `if let Some(start)`.
    let mut ran = false;
    // The loop is `execve`. `sys_execve` has already done the swap — it
    // installs the new address space on the registered process, switches `CR3`
    // and drops the old space, then asks the task to leave ring 3 — so all that
    // is left here is to re-read where the new image starts and go back in.
    //
    // That is a change of *place*, not of order: the swap used to happen right
    // here, out of `static mut PENDING_EXEC`, because a `mov cr3` and a frame
    // free "do not belong inside the syscall asm". They still do not, and they
    // still are not: `sys_execve` runs on the kernel stack in ordinary Rust,
    // and `sched::set_current_space_root` is documented safe mid-flight
    // precisely because every address space shares the kernel's upper half.
    // What the old shape bought was an array; what it cost was a second copy of
    // every image field.
    while let Some((entry, stack)) = current_entry_stack() {
        // SAFETY: both are addresses the loader (or `Image::new`) mapped
        // user-accessible in the address space the scheduler installed for this
        // task, and every program this kernel runs ends in exit_group.
        ran = true;
        status = {
            let forked = forked_child;
            forked_child = false;
            enter_user(entry, stack, forked)
        };
        if !take_uctx_flag(|u| &mut u.exec_pending) {
            break;
        }
    }
    if !ran {
        serial::puts("  [proc] slot ");
        serial::put_dec(idx as u64);
        serial::puts(" has no registered process; not entering ring 3\n");
        crate::sched::finish();
    }
    EXIT_STATUS.store(status, Ordering::Relaxed);
    // Real Linux closes every fd a process still holds at exit. Since step 4b
    // the registered table is the only descriptor authority and every entry in
    // it owns a real pipe/socket reference, so the sweep is the table's own
    // `close_all()` — the same walk its `Drop` would run later — captured
    // **before** `thread::drain`, which may retire the identity the lookup
    // below resolves through. Done before `spawn_record_exit` so a parent's
    // `waitpid` never observes the child as reaped while its fds are still
    // charged against the shared table.
    let exit_fds = current_process().map(|p| p.fds.clone());
    // Every thread of this process, gone, before anything downstream can reap.
    // `sys_waitpid` retires the `Process` — and with it the page tables, which
    // the reclaim then frees — once this task is `Finished`, so a sibling still
    // running in that space would be walking freed page tables. This is the
    // only place that ordering can be enforced: the reaper is another process
    // and has no idea threads exist.
    crate::thread::drain(idx);
    if let Some(fds) = exit_fds {
        fds.close_all();
    }
    if idx >= SPAWN_SLOT_BASE {
        spawn_record_exit(idx, status as i32);
    }
    // 5b slice 1: terminal teardown is a vetted drain site (`process::reclaim`
    // site 1). On AArch64 `unregister_process`'s RETIRED slots are collected
    // from the exit paths, the idle loop and the PMM pressure ladder; this
    // target had none of the three, so before this line every reaped child's
    // `Box<Process>` parked in the table forever and a long session would have
    // panicked `register_process` at the 256-slot ceiling.
    //
    // Since 5b slice 4 the drain is also what returns a dead process's *memory*:
    // the registered `Process` owns the address space now, so its `drop` is
    // `UserAddressSpace::drop` — every user frame and every page table. This
    // target configures `process_reclaim_cooldown_us: 0` (`exec_runtime.rs`),
    // so an eligible slot is collected on the first sweep that reaches it.
    akuma_exec::process::reclaim::drain_retired_if_requested();
    crate::sched::finish();
}

/// Every process task starts here.
///
/// **One entry function for all of them**, the same shape `thread::thread_entry`
/// has used since threads existed. Until 2026-09-06 this was sixteen
/// hand-written trampolines generated by a macro, each with a process index
/// baked into its `fn` pointer, behind a `proc_entry_for(idx)` that handed out
/// only nine of them (7..=15). That was the machine's real process ceiling:
/// `PROC_SLOTS` is 128, `sys_spawn` searched all of them, and `sys_fork`
/// refused any parent slot `>= 16` — so nine concurrent processes, which
/// `cargo -j4` exceeds before it has finished starting.
///
/// The index now lives where a thread's already did: `UserCtx::proc_slot`,
/// written by `sched::seed_proc_slot` while the task is still unpublished and
/// read back here. Nothing else changed — the ceiling was never about memory
/// or scheduling, only about where one `usize` could be kept.
extern "C" fn proc_entry() -> ! {
    let slot = current_proc_slot();
    if slot >= PROC_SLOTS {
        // Reached only if a task was published without being seeded. Say so:
        // `run_process` would name no process at all, and a panic here reads
        // as a scheduler fault rather than a missing seed.
        serial::puts("  [proc] entry with no slot\n");
        crate::sched::finish();
    }
    run_process(slot);
}

/// Reserve and seed — but do **not** publish — a task to run process slot
/// `proc_slot` in the address space `root`.
///
/// Two steps in a fixed order, which is why it is a function rather than two
/// lines at each call site: reserve the task, then seed the slot it serves. A
/// task published before it is seeded can be scheduled, reach [`proc_entry`],
/// and find `usize::MAX` there.
///
/// **It published, until 5b slice 4.** Every caller now registers the process
/// with `akuma-exec` before calling [`crate::sched::publish_task`], because
/// `run_process` reads its entry point and stack *out of* that registration:
/// a task published first could reach ring 3 before it exists as a process and
/// find nowhere to start. Slices 1-2 tolerated that window — an unregistered
/// child's syscalls merely fell back to the pre-slice answers — and this is
/// what closes it, for the reason `spawn_in_space_unpublished` has no
/// publish-immediately variant at all.
fn spawn_process_task(proc_slot: usize, root: u64) -> Option<usize> {
    let task_slot = crate::sched::spawn_in_space_unpublished(proc_entry, root)?;
    crate::sched::seed_proc_slot(task_slot, proc_slot);
    Some(task_slot)
}

/// Start a **self-test** process: register it, then publish its task.
///
/// The six boot self-tests that run ring-3 programs used to write an
/// `Option<Process>` into `PROCS[slot]` and spawn a task over it. They register
/// with `akuma-exec` now, like every other process — not for tidiness but
/// because they must: `with_current_regions` and `with_current_address_space`
/// resolve through the process table since 5b slice 4, and `fdprobe` and
/// `threadprobe` both `mmap`. An unregistered self-test process would have got
/// `None` from the accessors and failed with no explanation.
///
/// Registered before [`crate::sched::publish_task`], the same order every other
/// caller uses, and returning the `(pid, task_slot)` pair the teardown needs.
/// The parent is pid 1: these run before `run_init`, so there is no real
/// parent, and 1 is what `current_pid()` answers for the boot task driving them.
#[cfg(not(feature = "no-tests"))]
fn start_test_process(
    slot: usize,
    image: Image,
    image_top: u64,
    name: &str,
) -> Option<(u32, usize)> {
    let root = image.space.ttbr0();
    let task_slot = spawn_process_task(slot, root)?;
    let pid = alloc_pid();
    let mut cmdline = alloc::vec::Vec::with_capacity(name.len() + 1);
    cmdline.extend_from_slice(name.as_bytes());
    cmdline.push(0);
    register_exec_process(pid, 1, task_slot, image, image_top, name, &cmdline, None);
    crate::sched::publish_task(task_slot);
    Some((pid, task_slot))
}

/// Tear a self-test process down and free its address space **before the next
/// line runs**.
///
/// Every one of those tests ends by asserting that teardown leaked nothing —
/// `akuma_pmm::free_count()` back where it started — and until 5b slice 4 that
/// worked because the test dropped the `Process` itself. The address space
/// belongs to the registered process now, and `unregister_process` only
/// *retires* it; something has to collect. This target sets
/// `process_reclaim_cooldown_us: 0` (`exec_runtime.rs`), so the drain here
/// frees immediately, which is what keeps those assertions meaning what they
/// said. A test that merely retired would report a leak of the whole image.
///
/// `drain_retired_if_requested`, not the `_force` variant: the cooldown is
/// already zero, so forcing would only remove the guard, and the same call is
/// what every production drain site uses. It also drains the TTBR-deferred
/// frame list, which is where an address space's frames go if another core's
/// `CR3` still stands on its L0.
#[cfg(not(feature = "no-tests"))]
fn finish_test_process(pid: u32, task_slot: usize) {
    reap_exec_process(pid, task_slot);
    akuma_exec::process::reclaim::drain_retired_if_requested();
}

// ===========================================================================
// Stage R: sys_spawn, a process table with pids, and waitpid
// ===========================================================================
//
// `sshd` authenticates a session and then calls `spawn`/`spawn_pty` to start a
// shell, bridging the child's stdout back to the SSH channel and the client's
// keystrokes forward to its stdin. This is the amd64 half of that: load an ELF
// (the loader already exists), give the child a stdout pipe and a stdin pipe,
// run it as a scheduler task, and hand `sshd` back a pid plus a descriptor that
// reads the stdout pipe. `waitpid` reports the exit status; `/proc/<pid>/fd/0`
// (in `fd::sys_openat`) resolves to the stdin pipe's write end, which is how
// `sshd`'s `bridge_process` feeds the shell.
//
// No `fork`, no per-process fd table, no real process hierarchy — one spawn per
// `SPAWN` slot, and fd 0/1/2 are routed per task through `UserCtx::proc_slot`.

use crate::pipe::{self, PipeId};

/// One spawned child.
///
/// **C2 slice 6 deleted three of the four stdio fields.**
/// `stdout_pipe`/`borrowed_io`/`console_io` existed because a spawned child's
/// stdio was routed *by number*: every unbound fd 0/1/2 read or write asked
/// the spawn row which pipe (or console) served it, and the exit path closed
/// the child's stdout end by hand, guarded by `borrowed_io` so a `fork` child
/// did not close its parent's. The child's stdio is now **bound descriptors**
/// in its own row and registered table (`fd::bind_stdio`), so every one of
/// those jobs is done by machinery that already existed:
///
/// - routing: `pipe_read_id`/`pipe_write_id` resolve fd 0/1/2 like any pipe;
/// - `borrowed_io`: a `fork` child's `clone_deep_for_fork` bumps the shared
///   ends' refcounts, and only the last reference's close reaches the pipe;
/// - `console_io`: a child of a console task inherits an empty table, so its
///   0/1/2 are unbound and fall through to the console exactly as before;
/// - exit EOF: `close_all` releases the child's ends before
///   `spawn_record_exit` runs — the parent's reader sees EOF with no
///   per-spawn code (the manual `close_write` this replaced would now be a
///   *double* close).
///
/// **`stdin_pipe` stays, and that is a carried decision rather than a
/// leftover.** Its *read* end is the child's fd 0 and dies with the child's
/// row; its **write** end is reached by *path* — `sshd` opens
/// `/proc/<pid>/fd/0` — and a path is not a reference, so nothing refcounts
/// it. Left to the counts alone, a spawn whose stdin nobody ever opened (every
/// `run_sh_capture` in the boot suite) would keep one writer forever and the
/// pipe would never be destroyed: a leak against a 64-pipe ceiling. The reap
/// is the one place that knows the child is gone, so the reap drops it. What
/// changed is *how*: `pipe::close_write`, not the old `pipe::free` —
/// `free` destroyed the pipe under `sshd`'s still-open descriptor, and
/// `close_write` lets the end counts decide, which is the rule everywhere else
/// in this module. A `fork` child owns no pipe of its own and carries `None`.
struct Spawn {
    pid: u32,
    /// The stdin pipe this spawn created and still owns the *write* end of;
    /// `None` for a `fork` child, which shares its parent's by descriptor. See
    /// the type's header for why this one field outlived the other three.
    stdin_pipe: Option<PipeId>,
    /// The scheduler task slot running this child, recorded at spawn so the
    /// `waitpid` reap can remove the `THREAD_PID_MAP` entry it published
    /// (`reap_exec_process`). A `usize` is wider than `sched::MAX_TASKS` needs,
    /// but `usize::MAX` is not a sentinel here — every Spawn row has one.
    exec_slot: usize,
}

const SPAWN_SLOTS: usize = PROC_SLOTS - SPAWN_SLOT_BASE;

/// The spawn table. `static mut` reached through raw pointers on one core, same
/// discipline the deleted `PROCS` had: the only writers run inside a syscall (non-preemptible
/// on this target) and none of them yield while touching it.
static mut SPAWN: [Option<Spawn>; SPAWN_SLOTS] = [const { None }; SPAWN_SLOTS];

/// Next pid to hand out. `sshd` itself is pid 1 (`Getpid` returns 1), so
/// children start at 2.
static NEXT_PID: AtomicU64 = AtomicU64::new(2);

/// The next pid **or tid**.
///
/// One counter for both, as on Linux: a thread's tid and a process's pid live
/// in one number space there, and `gettid` returning a value that collides with
/// a live pid is the kind of thing that reads as a scheduler bug three layers
/// later. `crate::thread` is the other caller.
pub fn alloc_pid() -> u32 {
    NEXT_PID.fetch_add(1, Ordering::Relaxed) as u32
}

/// The `clone(2)` flag bits this kernel names.
///
/// Spelled out rather than reached through `akuma-syscalls-linux`: these are
/// architecture-independent Linux constants, and the one that matters here —
/// `CLONE_SETTLS` — pairs with an x86_64-specific *argument position*, so
/// keeping the two facts in one file is worth more than the deduplication.
pub mod clone_flags {
    pub const CLONE_VM: u64 = 0x0000_0100;
    pub const CLONE_THREAD: u64 = 0x0001_0000;
    pub const CLONE_SETTLS: u64 = 0x0008_0000;
    pub const CLONE_PARENT_SETTID: u64 = 0x0010_0000;
    pub const CLONE_CHILD_CLEARTID: u64 = 0x0020_0000;
    pub const CLONE_CHILD_SETTID: u64 = 0x0100_0000;
}

/// Bytes of argv kept per process for `/proc/<pid>/cmdline`.
///
/// A shell command line, not a program's whole argument vector: 256 bytes holds
/// every command a person types and every `sh -c "..."` a session runs, and
/// caps what an `execve` with a pathological argv can add to the process table.
/// `ps` shows the COMMAND column truncated, which is what `ps` does anyway.
const CMDLINE_MAX: usize = 256;

/// The init program's `/proc/1/cmdline`, recorded by [`run_init`].
///
/// Init has no `Spawn` entry — it runs in process slot 6, below
/// [`SPAWN_SLOT_BASE`], and predates the spawn table entirely — so its name
/// lives here. Without it `ps` listed every session's shell and not the thing
/// that started them, which is the one line a person checks first.
static INIT_CMDLINE: Spinlock<alloc::vec::Vec<u8>> = Spinlock::new(alloc::vec::Vec::new());

/// Flatten argv into the `/proc/<pid>/cmdline` form: each element
/// NUL-terminated, the whole thing capped at [`CMDLINE_MAX`].
///
/// The cap truncates **whole bytes, not whole elements** — same as Linux, whose
/// `cmdline` is just the argv block clipped at the page it lives on, and same
/// as what a reader that splits on NUL expects.
fn flatten_cmdline<'a>(args: impl IntoIterator<Item = &'a [u8]>) -> alloc::vec::Vec<u8> {
    let mut out = alloc::vec::Vec::new();
    for a in args {
        if out.len() >= CMDLINE_MAX {
            break;
        }
        let room = CMDLINE_MAX - out.len();
        let take = a.len().min(room.saturating_sub(1));
        out.extend_from_slice(&a[..take]);
        out.push(0);
    }
    out
}

/// One process, as `/proc` needs to describe it.
///
/// Owned rather than borrowed: every reader of the process table is a syscall
/// rendering a virtual file, and the table is a `static mut` behind raw
/// pointers — handing out a reference into it would outlive the single-core
/// reasoning that makes touching it sound at all.
pub struct ProcEntry {
    /// argv, NUL-separated. Never empty: falls back to the program name.
    ///
    /// The only field left. This type used to carry `pid`, `ppid` and `exit`
    /// as well, and render `/proc/<pid>/stat` through them — that was this
    /// target's own `/proc`, deleted in 4b batch 2c in favour of the mounted
    /// `ProcFilesystem`, which reads the same facts off `Process` directly.
    /// What survives is one question `fork` asks: what did my parent's command
    /// line say.
    pub cmdline: alloc::vec::Vec<u8>,
}

impl ProcEntry {
    /// argv[0], for `comm` and the `Name:` field.
    #[must_use]
    pub fn name(&self) -> &str {
        let first = self.cmdline.split(|&b| b == 0).next().unwrap_or(&[]);
        core::str::from_utf8(first).unwrap_or("?")
    }
}

/// Record init's argv so `/proc/1` can describe it. Called once by [`run_init`].
fn set_init_cmdline<'a>(args: impl IntoIterator<Item = &'a [u8]>) {
    *INIT_CMDLINE.lock() = flatten_cmdline(args);
}

/// One `akuma-exec` `Process`'s command line.
///
/// This rendered a whole `/proc` entry — pid, ppid, exit status — until 4b
/// batch 2c deleted this target's own `/proc`. The mounted `ProcFilesystem`
/// reads those facts off `Process` directly; what is left is the one question
/// `sys_fork` asks about a parent it is about to copy.
fn proc_entry_of(p: &akuma_exec::process::Process) -> ProcEntry {
    let img = p.image.lock();
    // `image.args` is `Vec<String>`; `/proc/<pid>/cmdline` is NUL-terminated
    // bytes. Rebuilt here rather than stored twice.
    let mut cmdline = alloc::vec::Vec::new();
    for a in &img.args {
        cmdline.extend_from_slice(a.as_bytes());
        cmdline.push(0);
    }
    if cmdline.is_empty() {
        cmdline.extend_from_slice(img.name.as_bytes());
        cmdline.push(0);
    }
    ProcEntry { cmdline }
}

/// One process by pid, or `None` if no such process is live.
#[must_use]
pub fn proc_by_pid(pid: u32) -> Option<ProcEntry> {
    // 5b slice 2: one table, and init is in it — `run_init` registers pid 1,
    // so no special case is needed *once the machine is running*.
    if let Some(e) = akuma_exec::process::find_process(|p| (p.pid == pid).then(|| proc_entry_of(p)))
    {
        return Some(e);
    }
    // ...but the boot self-tests run **before** `run_init`, on a task that is
    // registered nowhere, and `current_pid()` answers 1 for it. So every
    // `/proc/self` check in the suite asks for a pid 1 that does not exist yet.
    //
    // This fallback is that window and nothing else, which is why it is here
    // rather than a `pid == 1` arm ahead of the lookup: once init is registered
    // the table answers first and this is dead. Removing it during slice 2
    // failed three `proc: /proc/self/...` checks immediately — the synthetic
    // entry it replaces was load-bearing for a reason nobody had written down.
    if pid == 1 {
        let cmdline = INIT_CMDLINE.lock().clone();
        return Some(ProcEntry {
            cmdline: if cmdline.is_empty() { alloc::vec![b'i', b'n', b'i', b't', 0] } else { cmdline },
        });
    }
    None
}

/// The pid of the process making the current syscall — what `/proc/self`
/// resolves to. `1` for init and for anything not in the spawn table, matching
/// `getpid`, which returns 1 on this target for exactly the same reason.
#[must_use]
pub fn current_pid() -> u32 {
    // 5b slice 2: through `akuma-exec`'s `THREAD_PID_MAP`, which slice 1
    // populates at every fork/spawn/execve/`run_init` and slice 2 extended to
    // `clone_thread`. This *is* the identity `current_process_shared()` uses,
    // so the two can no longer disagree — before, the spawn table answered here
    // and the map answered there, and nothing checked them against each other.
    //
    // The key is the scheduler task slot, because on this target the tid **is**
    // that slot. A thread resolves to its process's pid, which is what makes
    // this correct for `CLONE_VM` where the old slot walk was correct by a
    // different route (the shared `proc_slot` in the per-CPU `UserCtx`).
    //
    // Unmapped means init: the self-tests run before `run_init` registers
    // anything, and they are pid 1's work. That is the same answer the slot
    // walk gave for `slot < SPAWN_SLOT_BASE`.
    akuma_exec::process::pid_for_thread(crate::sched::current_task()).unwrap_or(1)
}

fn spawn_table() -> *mut [Option<Spawn>; SPAWN_SLOTS] {
    &raw mut SPAWN
}

// ===========================================================================
// 5b slice 1: akuma-exec's process table, populated (2026-09-08)
//
// `SPAWN` above is what is left of this target's *mechanism* — which pipes a
// process slot reads and writes, and which scheduler task serves it. `akuma_exec`'s `PROCESS_TABLE` +
// `THREAD_PID_MAP` are its *identity* — which pid is calling this syscall —
// and until now nothing here populated them, so every
// `akuma_exec::process::current_process_shared()` a folded glue arm reached
// answered `None`. Each `sys_spawn`/`sys_fork`/`run_init` below now also
// builds an `akuma_exec::Process` and registers it, and each reap unregisters.
//
// # The field decisions, each stated rather than defaulted
//
// The `Process` has 45 pub fields and the sanctioned constructor shape is
// `image.rs::from_image`'s literal, copied here. Where this target has no
// answer, the honest one is written down:
//
// * `channel: None`, empty stdio — `sshd`'s stdio bridge is `crate::pipe`, a
//   different namespace; the `StdioBuffer`s stay empty and unread.
// * `fds: SharedFdTable::with_stdio()` — empty. The live descriptor table is
//   `fd.rs`'s own; folding it is C2.
// * `namespace: global_namespace()` — there are no boxes here.
// * `signal_actions` empty, `signal_mask` 0 — no signal delivery on this
//   target; `rt_sigaction`/`rt_sigprocmask` are local arms that never consult
//   this.
// * `image.context: UserContext::new(0, 0)` — amd64 keeps ring-3 registers in
//   its own `UserCtx` and `enter_user` is its own entry path; this context is
//   never `eret`n from.
// * `lazy_regions` empty — demand paging here comes from `mmap_regions`
//   (`akuma-mmap`), not a lazy-region map.
// * `memory: ProcessMemory::new(end_va, stack_bottom, ELF_STACK_TOP,
//   mm::MMAP_BASE)` — the same constants `loader.rs`/`mm.rs` place with, so
//   the registered view and the real one cannot disagree about where the heap,
//   the stack and the mmap window are.
//
// Two of those decisions were **spent by 5b slice 4**, and the entries are
// left here rather than deleted because the reasoning is what dates them:
// `image.context` is no longer a zeroed placeholder (it is the `(entry, stack)`
// pair `run_process` re-enters ring 3 with), and `address_space` is no longer a
// non-owning `new_shared` view (it owns the space, because nothing else does).
// * `thread_id: None` — deliberately. On AArch64 it names an `akuma-threading`
//   thread slot `unregister_process` may mark TERMINATED; this target's task
//   lifecycle is `sched.rs`'s, and identity comes from `THREAD_PID_MAP`
//   (`thread_pid_map_insert(task_slot, pid)` below) exactly as the vfork
//   fast-path resolves it. Leaving the field `None` also keeps
//   `unregister_process`'s thread-termination arm out of this target's
//   scheduler, which it does not model.
// * `process_info_phys: 0` — the ProcessInfo page is **not mapped, not
//   allocated, and not missed**. `read_current_pid`'s page-read tail is gated
//   on `ttbr0_el1() != boot_ttbr0()`, and on x86_64 the register read is a
//   `0`-returning stub against `get_boot_ttbr0() == 0`, so the tail returns
//   `None` before any access; identity resolves through `THREAD_PID_MAP` and
//   the identity cache alone. The only writer, `prepare_for_execution`, is not
//   on this target's path — and `write_phys` range-checks against PMM RAM and
//   no-ops on `0` regardless. Mapping + ledger-tracking a real page would leak
//   4 KiB per process, which the ring-3 leak checks would catch.
//
// # What registration changes on a live syscall
//
// `to_glue`'s folded arms (uname, getrandom, getgroups, prlimit64) get a
// resolved identity in glue's prologue: `last_syscall`/`current_syscall` and
// the per-process syscall stats start being stamped. Every other consumer of
// `current_process_shared()` in glue is behind an arm this target does not
// dispatch yet, so slice 1 changes no visible answer — it is the foundation
// slices 2–4 (`Spawn` deletion, the mounted `ProcFilesystem`) build on.
//
// # The smp-shared question, decided before this landed
//
// It stopped being open on 2026-09-08: `smp-shared` is a **required** default
// feature of this target (`Cargo.toml`, const-asserted in `smp.rs`), so
// `kernel_smp_shared` is on and `ProcAddressSpace::lock()` really masks IRQs
// (`akuma-cpu`'s `daif` has real x86 arms now). The shared table behind that
// lock is therefore exclusion-correct from day one, and this slice adds no
// lock of its own — `register_process`'s `SlotTable` CAS and the IRQ-masked
// `THREAD_PID_MAP` are akuma-exec's own.
// ===========================================================================

/// Build and register the `akuma_exec::Process` standing behind `pid`, and
/// publish `task_slot → pid` in `THREAD_PID_MAP`.
///
/// `image` is **consumed**: its address space, entry point, stack pointer and
/// region list are moved onto the registered process, which owns them from here
/// on. Slice 1 took a bare `root: u64` and wrapped a non-owning `new_shared`
/// view of it, because `PROCS` still owned the real one; slice 4 deleted that
/// array and this took over the ownership with it.
///
/// `image_top` is where the heap starts (`LoadedImage::end_va`), `name` the
/// `argv[0]`-ish display name `/proc/<pid>/comm` renders.
///
/// Register order is **table, then map**: `thread_pid_map_insert` refreshes
/// the identity cache, whose lazy re-stamp handles the reverse order, but this
/// order is the one the cache resolves on the first try. The registering task
/// runs under the BKL inside a syscall; the child is usually not scheduled
/// yet, and if it is (it was published first), the window before registration
/// resolves identity exactly as it did before this slice — `None`, and the
/// same fallbacks as ever.
fn register_exec_process(
    pid: u32,
    ppid: u32,
    task_slot: usize,
    image: Image,
    image_top: u64,
    name: &str,
    cmdline: &[u8],
    // **C2 slice 3 (registration).** `None` builds the fresh `with_stdio()`
    // table a spawned or self-test process starts with; `sys_fork` passes the
    // parent's `clone_deep_for_fork()`, so the child's registered table is a
    // real POSIX copy — its own `BTreeMap`, naming the same descriptions —
    // rather than a second fresh stdio triple that merely *looks* inherited.
    //
    // Nothing reads this table yet: the fd syscalls still serve `fd.rs`'s
    // `FDS`/`FILES`, so today the fork-side clone differs from a fresh
    // `with_stdio()` only when the parent had opened something *through the
    // crate's table*, which no path does yet. That is what makes this slice
    // reversible — registration lands without any behaviour changing, and the
    // slices that repoint `open`/`dup`/`close` at this table inherit a fork
    // path that is already correct.
    //
    // One obligation recorded for those slices: `clone_deep_for_fork` runs
    // `clone_fd_refs`, whose refcounted arms call the `ExecRuntime`
    // pipe/socket/sock clone hooks — all `not_wired!` here until C2 slice 6.
    // Innocent today because a table this target builds can only hold the
    // `Stdin`/`Stdout`/`Stderr` arms (the `_ => {}` fall-through); the moment
    // slice 5 starts landing `File` descriptors here it is still innocent
    // (`KernelFile` is not hook-refcounted), and slice 6 is the one that has
    // to wire the hooks before a `PipeRead`/`PipeWrite` can be cloned.
    fds: Option<alloc::sync::Arc<akuma_exec::process::SharedFdTable>>,
) {
    use alloc::boxed::Box;
    use alloc::collections::BTreeMap;
    use alloc::string::String;
    use alloc::sync::Arc;
    use core::sync::atomic::{
        AtomicBool, AtomicI32, AtomicUsize,
    };

    use akuma_exec::process::{
        AtomicProcessState, LazyRegionMap, Process, ProcessImage, ProcessMemory,
        ProcessState, ProcessSyscallStats, SharedFdTable, SharedSignalTable,
        StdioBuffer, UserContext, register_process, thread_pid_map_insert,
    };

    let stack_bottom = ELF_STACK_TOP - (ELF_STACK_PAGES as u64 * 4096);
    let proc = Box::new(Process {
        pid,
        pgid: pid,
        tgid: pid, // group leader = self; no CLONE_THREAD groups in the table yet
        state: AtomicProcessState::new(ProcessState::Ready),
        // 5b slice 4: the **owning** address space, moved in from the loader.
        // Slice 1 registered a `new_shared` view here — non-owning, no `Drop`,
        // pointed at a root `PROCS` really owned — because two structures
        // claiming one L0 would double-free it. There is one structure now, so
        // the registration owns what it names, and `Process::drop` is what
        // frees a dead process's frames and page tables (through
        // `free_or_defer_as_frames`, which parks them while any core's `CR3` or
        // any preempted thread's saved context still stands on the L0).
        address_space: ProcAddressSpace::new(image.space),
        image: Spinlock::new(ProcessImage {
            name: String::from(name),
            // 5b slice 2: the argument vector, so `/proc/<pid>/cmdline` can be
            // rendered from the registered process rather than from the spawn
            // row. Slice 1 left this empty, which was invisible only because
            // nothing read it yet — moving the procfs readers over without
            // filling it would have blanked every `ps` COMMAND column, the
            // exact shape of silent divergence the fold is supposed to avoid.
            //
            // The wire format is the one `Spawn::cmdline` already carries and
            // `/proc/<pid>/cmdline` wants: each argument NUL-terminated. A
            // trailing NUL therefore yields an empty final element, which
            // `split` produces and `filter` drops. Non-UTF-8 argv is possible
            // on Linux and impossible in a `String`, so it is lossy-converted
            // rather than dropped — a mangled argument still identifies the
            // process; a missing one does not.
            args: cmdline
                .split(|b| *b == 0)
                .filter(|a| !a.is_empty())
                .map(|a| String::from_utf8_lossy(a).into_owned())
                .collect(),
            // 5b slice 4: **where ring 3 starts**, which is what this field
            // has always meant — `UserContext::new(entry_point, stack_pointer)`
            // writes exactly `pc`/`sp`. Slice 1 registered it zeroed and noted
            // that amd64 "never `eret`s from it"; that stays true, and is
            // beside the point. `run_process` reads these two scalars back
            // (`current_entry_stack`) instead of indexing a second table, and
            // an `execve` rewrites them in this same lock so a re-entry cannot
            // pair a new entry point with an old stack.
            context: UserContext::new(image.entry as usize, image.stack as usize),
        }),
        parent_pid: ppid,
        brk: AtomicUsize::new(image_top as usize),
        initial_brk: AtomicUsize::new(image_top as usize),
        entry_point: AtomicUsize::new(image.entry as usize),
        memory: ProcessMemory::new(
            image_top as usize,
            stack_bottom as usize,
            ELF_STACK_TOP as usize,
            crate::mm::MMAP_BASE,
        ),
        // Stated decision, not a default: see the module header — the page is
        // never read on this target and mapping it would leak 4 KiB/process.
        process_info_phys: AtomicUsize::new(0),
        cwd: String::from("/"),
        stdin: Arc::new(Spinlock::new(StdioBuffer::new())),
        stdout: Arc::new(Spinlock::new(StdioBuffer::new())),
        exited: AtomicBool::new(false),
        exit_code: AtomicI32::new(0),
        dynamic_page_tables: Vec::new(),
        // 5b slice 4: the region list this target demand-pages from, moved in
        // from the loader. Empty for everything but a `fork` child, which
        // arrives carrying the parent's extents.
        mmap_regions: Spinlock::new(image.regions),
        lazy_regions: Spinlock::new(LazyRegionMap::new()),
        // **`with_stdio()` since 4b batch 2a** — it was `new()`, and the
        // comment here recorded why: a table holding `Stdin`/`Stdout`/`Stderr`
        // made `is_bound(1)` true and routed the write into `sys_write_file`,
        // which refuses a non-`File` descriptor, so every test process went
        // silent. That was a missing *arm*, not a wrong table:
        // `fd::console_end` now answers both spellings — the by-number one
        // this target invented and the descriptor one the tree uses — so the
        // triple routes to the console instead of to the file path.
        //
        // Flipping it is a **prerequisite for the `openat` fold**, not a
        // tidy-up. Glue allocates with `alloc_fd`, which is
        // `alloc_fd_from(0)`: with 0/1/2 absent, the first `open` in a process
        // whose stdio is unbound returns **fd 0**, and every later write to
        // fd 1 lands in whatever file the process opened next. Occupying the
        // triple is what makes the tree's allocator safe here, and it is also
        // what `SharedFdTable::with_stdio` exists for.
        fds: fds.unwrap_or_else(|| alloc::sync::Arc::new(SharedFdTable::with_stdio())),
        thread_id: None,
        spawner_pid: None,
        terminal_state: Arc::new(Spinlock::new(akuma_terminal::TerminalState::default())),
        box_id: 0,
        namespace: akuma_isolation::global_namespace(),
        channel: None,
        delegate_pid: None,
        grabbed_by: None,
        clear_child_tid: AtomicU64::new(0),
        robust_list_head: 0,
        robust_list_len: 0,
        signal_actions: Arc::new(SharedSignalTable::new()),
        signal_mask: 0,
        fault_mutex: Spinlock::new(BTreeMap::new()),
        sigaltstack_sp: AtomicU64::new(0),
        sigaltstack_flags: AtomicI32::new(2), // SS_DISABLE
        sigaltstack_size: AtomicU64::new(0),
        start_time_us: (akuma_exec::runtime::runtime().uptime_us)(),
        current_syscall: AtomicU64::new(!0),
        last_syscall: AtomicU64::new(0),
        syscall_stats: ProcessSyscallStats::new(),
    });
    register_process(pid, proc);
    thread_pid_map_insert(task_slot, pid);
}

/// Tear a reaped child's registration down: retire the `Process` (the table's
/// deferred reclaim frees it after its cooldown) and remove the
/// `task_slot → pid` map entry **only if the slot still names this pid**.
///
/// That guard is load-bearing rather than tidy: task slots are recycled, and a
/// zombie can sit in the `SPAWN` table long past the moment its finished task
/// slot was handed to an unrelated process. Removing the entry unconditionally
/// would strip a live process's identity — every syscall it makes would
/// degrade to the pre-slice fallbacks until its own insert re-stamps, which is
/// exactly the silent-wrongness this table exists to prevent. Compare, then
/// remove; a stale entry is self-correcting (the new owner's insert overwrote
/// it), a matching one is ours.
fn reap_exec_process(pid: u32, task_slot: usize) {
    use akuma_exec::process::{pid_for_thread, thread_pid_map_remove, unregister_process};
    unregister_process(pid);
    if pid_for_thread(task_slot) == Some(pid) {
        thread_pid_map_remove(task_slot);
    }
}

/// Read a NUL-terminated string from user memory, bounded.
fn user_cstr(ptr: u64, max: usize) -> Option<alloc::vec::Vec<u8>> {
    crate::uaccess::read_cstr(ptr, max)
}

/// Parse a NULL-terminated array of C-string pointers into owned bytes.
fn user_argv(ptr: u64) -> alloc::vec::Vec<alloc::vec::Vec<u8>> {
    let mut argv = alloc::vec::Vec::new();
    if ptr == 0 {
        return argv;
    }
    for i in 0..loader::MAX_ARGV {
        // A bad array pointer ends the list, like a NULL entry would.
        let p = crate::uaccess::read_val::<u64>(ptr + (i as u64) * 8).unwrap_or(0);
        if p == 0 {
            break;
        }
        match user_cstr(p, 512) {
            Some(s) => argv.push(s),
            None => break,
        }
    }
    argv
}

/// Parse a NULL-terminated array of C-string pointers, bounded at `max` entries.
fn user_strv(ptr: u64, max: usize) -> alloc::vec::Vec<alloc::vec::Vec<u8>> {
    let mut out = alloc::vec::Vec::new();
    if ptr == 0 {
        return out;
    }
    for i in 0..max {
        // A bad array pointer ends the list, like a NULL entry would.
        let p = crate::uaccess::read_val::<u64>(ptr + (i as u64) * 8).unwrap_or(0);
        if p == 0 {
            break;
        }
        match user_cstr(p, 512) {
            Some(s) => out.push(s),
            None => break,
        }
    }
    out
}

/// `execve(path, argv, envp)` — x86_64 syscall 59.
///
/// This target has no `fork`, so `execve` is only ever the tail of a spawned
/// task: `sshd`/`run_init` start a shell with `sys_spawn`, and the shell running
/// `sh -c "<cmd>"` `execve`s the command directly (ash does not fork for the
/// single-command `-c` form — verified with `strace`). The running task keeps
/// its process slot, its pipes and its pid; only the image behind it changes.
///
/// # The swap happens here now
///
/// It used to be parked in `static mut PENDING_EXEC` and performed by
/// [`run_process`] after the `leave` path returned, "because a page-table
/// switch and a frame free do not belong inside the syscall asm". They still do
/// not, and they still are not: this function runs on the kernel stack in
/// ordinary Rust, well outside `syscall_entry`, and
/// [`crate::sched::set_current_space_root`] is documented safe mid-flight —
/// every address space shares the kernel's upper half, so the stack this runs
/// on stays mapped across the `mov cr3`.
///
/// What the deferral actually bought was somewhere to *keep* the new image, and
/// 5b slice 4 deleted the array it was kept in. The order below is the part
/// that matters and is the same order `run_process` used:
///
/// 1. install the new address space on the registered process (which hands back
///    the old one, rather than dropping it);
/// 2. switch `CR3` off the old space;
/// 3. **then** drop the old space, freeing its frames and page tables.
///
/// Nothing between (1) and (3) touches user memory — the syscall return path
/// with `leave` set restores a kernel stack pointer and returns into kernel
/// code — so there is no window in which a fault could reach a table that has
/// been freed or a root that has been replaced.
///
/// On success this does not really "return": it sets `leave` and `exec_pending`
/// and the next thing the task does is re-enter ring 3 at the new entry. On
/// failure it returns a negative errno and the caller runs on.
fn sys_execve(path_ptr: u64, argv_ptr: u64, envp_ptr: u64) -> u64 {
    use crate::fd::errno;

    let slot = current_proc_slot();
    if slot >= PROC_SLOTS {
        // Not a slotted user task — nothing to replace. (`current_proc_slot`
        // answers `usize::MAX` off a user task, which this same bound catches.)
        return errno::ENOSYS;
    }

    let Some(path_bytes) = user_cstr(path_ptr, 256) else {
        return errno::EFAULT;
    };
    let Ok(path) = core::str::from_utf8(&path_bytes) else {
        return errno::EINVAL;
    };
    let Ok(image) = crate::fs::read_file(path) else {
        return errno::ENOENT;
    };

    let argv_owned = {
        let mut v = user_strv(argv_ptr, loader::MAX_ARGV);
        if v.is_empty() {
            v.push(path_bytes.clone());
        }
        v
    };
    let envp_owned = user_strv(envp_ptr, loader::MAX_ENVP);
    let argv_refs: alloc::vec::Vec<&[u8]> =
        argv_owned.iter().map(alloc::vec::Vec::as_slice).collect();
    let envp_refs: alloc::vec::Vec<&[u8]> =
        envp_owned.iter().map(alloc::vec::Vec::as_slice).collect();

    let (next, ld) = match Image::from_elf_argv_envp(&image, &argv_refs, &envp_refs) {
        Ok(p) => p,
        Err(e) => {
            serial::puts("  [execve] load failed: ");
            serial::puts(e);
            serial::puts("\n");
            // The image was rejected; the caller's own image is untouched, so
            // this is a real errno return, not a leave.
            return errno::ENOMEM;
        }
    };

    // `/proc/<pid>/cmdline` follows the new image. Without this, `ps` reported
    // every process under the name of whatever `fork`ed it — a shell session
    // showed a column of `sh`, which is the shape that makes `ps` useless
    // rather than merely incomplete.
    //
    // 5b slice 2 deleted `Spawn::cmdline`, the second copy, and refreshed the
    // registered process's display `name` here. **It did not refresh `args`**,
    // and `args` is what `/proc/<pid>/cmdline` — and therefore `ps`'s COMMAND
    // column, through `ProcEntry::name`— is actually rendered from
    // (`proc_entry_of`), so an `execve`d process still listed the argv of
    // whatever spawned it. Slice 4 refreshes both, which is what that slice's
    // note already claimed. Recorded once and used once, in the same lock as
    // the entry point, so no reader can pair a new image with an old argv.
    let exec_cmdline = flatten_cmdline(argv_refs.iter().copied());
    // **The resolved path, not `argv[0]`** — same reason as `sys_spawn`'s
    // registration: `image.name` is what `/proc/<pid>/exe` reports, and
    // `argv[0]` is a name the caller chose rather than something that opens.
    // `ps` is unaffected — `akuma_procfs::ProcStat::comm` takes the basename.
    let new_name = alloc::string::String::from(path);

    let pid = current_pid();
    let new_root = next.space.ttbr0();
    let new_brk = ld.end_va;
    let new_entry = next.entry;
    let new_stack = next.stack;
    let stack_bottom = (ELF_STACK_TOP - (ELF_STACK_PAGES as u64 * 4096)) as usize;

    // **POSIX: `execve` destroys every other thread of the calling process.**
    // This target did not, and slice 4 is what made the omission concrete: the
    // address space the swap below replaces and frees is the one a `CLONE_VM`
    // sibling is still executing in. `free_or_defer_as_frames` parks the frames
    // while another core's `CR3` stands on that L0, so the sibling is not
    // reading freed memory — it is running the *old program* in a process that
    // has become a different one, and the parked frames come back only when it
    // finally leaves. `akuma-exec`'s own `replace_image` calls
    // `kill_exec_siblings` at exactly this point, and this is that.
    //
    // `drain` sets the group-exit flag, wakes every sibling so a parked
    // `FUTEX_WAIT` does not have to wait out the scheduler backstop, and spins
    // (bounded) until none is live. It is safe to yield here: the new image is
    // built but nothing has been swapped, so a failure at this point leaves the
    // caller's own image untouched. `clear_group_exiting` below is what lets
    // the new program's first thread run.
    //
    // **Only from the main thread**, and the restriction is carried rather than
    // hidden: `THREADS` holds non-main threads, so `live_count` includes a
    // *non-leader* caller and `drain` would spin its full budget waiting for
    // the thread that is calling it, then print `DRAIN INCOMPLETE`. POSIX says
    // that caller becomes the group leader and the others die — leader transfer
    // is a thing this target's thread model does not have, so the honest
    // behaviour for that case is the one it has always had, said out loud.
    if crate::thread::current_is_main() {
        crate::thread::drain(slot);
    } else if crate::thread::live_count(slot) > 1 {
        serial::puts("  [execve] from a non-leader thread with live siblings; \
                      they are not killed (no leader transfer on this target)\n");
    }

    // Built **before** the hold below. `with_process` runs its closure with
    // interrupts disabled and states that it must not allocate on the heap, so
    // everything that allocates is done here and only moved in there.
    let new_args: alloc::vec::Vec<alloc::string::String> = exec_cmdline
        .split(|b| *b == 0)
        .filter(|a| !a.is_empty())
        .map(|a| alloc::string::String::from_utf8_lossy(a).into_owned())
        .collect();

    // Step 1: install the new image on the registered process, taking the old
    // address space and region list back out rather than letting them drop
    // under that same hold — `UserAddressSpace::drop` frees every user frame and
    // page table, which is not work for a closure that runs with interrupts off.
    let taken = akuma_exec::process::with_process(pid, |p| {
        {
            let mut im = p.image.lock();
            im.name = new_name;
            im.args = new_args;
            im.context = akuma_exec::process::UserContext::new(
                new_entry as usize,
                new_stack as usize,
            );
        }
        p.brk.store(new_brk as usize, Ordering::Relaxed);
        p.initial_brk.store(new_brk as usize, Ordering::Relaxed);
        p.entry_point.store(new_entry as usize, Ordering::Relaxed);
        // The heap, the stack and the mmap window all move with the image.
        // Nothing on this target places through `ProcessMemory` yet — `mm.rs`
        // has its own placer over the region list — but a registration that
        // states the *previous* image's arena is a wrong answer waiting for the
        // first folded arm that reads it.
        p.memory.reset(new_brk as usize, stack_bottom, ELF_STACK_TOP as usize, crate::mm::MMAP_BASE);
        // A new image inherits no mappings. This was implicit while `execve`
        // replaced a whole `Process` — the new one simply had an empty list —
        // and has to be explicit now that the process outlives its image: a
        // `fork` child's inherited extents would otherwise survive into the
        // program it `execve`s and reserve VA ranges nothing maps.
        let old_regions = core::mem::take(&mut *p.mmap_regions.lock());
        (p.address_space.replace(next.space), old_regions)
    });
    let Some((old_space, old_regions)) = taken else {
        // Unreachable by construction since 5b slice 4: every process task is
        // registered before it is published, so a task running `execve` has a
        // registered process. Say so rather than proceeding — the new image
        // would have nowhere to be recorded and the task would re-enter ring 3
        // at the old entry point in the new space.
        serial::puts("  [execve] no registered process for pid ");
        serial::put_dec(u64::from(pid));
        serial::puts("\n");
        return errno::ESRCH;
    };

    // A new program in an existing slot starts with a clean group: a stale
    // `exit_group` flag from the image just replaced would kill its first
    // thread at its first syscall.
    crate::thread::clear_group_exiting(slot);
    // Step 2: `CR3` off the old space, onto the new one. After this the old
    // tables are unreferenced by this core.
    crate::sched::set_current_space_root(new_root);
    // Step 3: and now the old image can go. `free_or_defer_as_frames` still
    // parks it if another core's `CR3` or a preempted thread's saved context
    // stands on that L0 — a `CLONE_VM` sibling, which this target does not
    // terminate on `execve`.
    drop(old_space);
    drop(old_regions);

    // Ask the entry path to leave ring 3, and tell `run_process` this is an
    // `execve` rather than an exit.
    // SAFETY: under the BKL, interrupts off inside a syscall; the per-CPU
    // `UserCtx` is this task's slot. Same `leave` mechanism `exit` uses.
    unsafe {
        let uctx = crate::smp::current_uctx();
        if !uctx.is_null() {
            (*uctx).leave = 1;
            (*uctx).exec_pending = 1;
        }
    }
    0
}

/// `fork` (57) / `vfork` (58) / plain `clone(SIGCHLD, 0)` (56).
///
/// A real fork: the child **shares** the parent's address space copy-on-write,
/// resumes at the parent's post-`fork` instruction as a register- and TLS-
/// complete copy, and runs as its own scheduler task. The parent is **not**
/// suspended — it gets the child pid back immediately and both run; a shell
/// blocks on the child itself, in `wait4`.
///
/// This is what an interactive `busybox sh` needs for every external command
/// (`fork(); if (child) execvp(...)`), and CoW is what makes it cheap: the
/// child usually `execve`s microseconds later and throws the whole space away,
/// so almost nothing is ever copied. Measured 2026-09-06: 2000 forks, zero
/// memory drift, constant time — the eager copy this replaced died at ~500.
///
/// **SMP-safe as of 2026-09-09.** The share pass demotes the *parent's* live
/// PTEs, and that demote's flush (`x86_walk_leaves`'s ranged shootdown)
/// invalidates every peer's stale writable translation before `fork` returns —
/// which is what makes `cowstale` deterministic at `SMP=4`. Until then this was
/// SMP=1 by construction: `invlpg` is core-local and there was no shootdown
/// (`smp.rs`). See `docs/archive/AKUMA_AMD64_COW.md`.
///
/// Returns the child pid in the parent; the child never returns from here.
fn sys_fork() -> u64 {
    use crate::fd::errno;

    let parent_slot = current_proc_slot();
    // `>= PROC_SLOTS`, not `>= 16`. The old bound existed because
    // `proc_entry_for` had only sixteen trampolines and handed out nine, so a
    // parent in a higher slot had no entry function for its child. There is one
    // entry function now (`proc_entry`), and `usize::MAX` — the answer off a
    // user task — is caught by the same comparison.
    if parent_slot >= PROC_SLOTS {
        return errno::ENOSYS;
    }

    // The point the child resumes from — the parent's own user RIP/RSP as
    // captured on the way into this syscall — plus its TLS base and register
    // snapshot.
    // SAFETY: raw-pointer read under the BKL; the per-CPU `UserCtx` is this task's.
    let (user_rip, user_rsp, parent_fs_base, parent_gs_base, parent_regs) = unsafe {
        let uctx = crate::smp::current_uctx();
        if uctx.is_null() {
            (0, 0, 0, 0, [0u64; 12])
        } else {
            (
                (*uctx).user_rip,
                (*uctx).user_rsp,
                (*uctx).fs_base,
                (*uctx).gs_base,
                (*uctx).saved_regs,
            )
        }
    };
    if user_rip == 0 || user_rsp == 0 {
        return errno::ENOSYS;
    }

    // A free child slot. The `SPAWN` row is the whole answer since 5b slice 4:
    // it used to be `PROCS[s].is_none() && SPAWN[s].is_none()`, and the
    // diagnostic below used to count both halves because they could diverge —
    // `fork` searched them together, `sys_spawn` searched only `PROCS`. There
    // is one array left, so they cannot.
    // SAFETY: raw-pointer read; single core.
    let slot = unsafe {
        let spawn = spawn_table();
        (SPAWN_SLOT_BASE..PROC_SLOTS).find(|&s| (*spawn)[s - SPAWN_SLOT_BASE].is_none())
    };
    let Some(slot) = slot else {
        // A bare `ENOMEM` here reaches the user as `sh: can't fork: Out of
        // memory`, which names the wrong resource: the table is full, and the
        // machine may have gigabytes free. Say which, and how full.
        // SAFETY: raw-pointer read; single core.
        let spawn_used = unsafe {
            let spawn = spawn_table();
            (0..SPAWN_SLOTS).filter(|&s| (*spawn)[s].is_some()).count()
        };
        serial::puts("  [fork] no free process slot: SPAWN ");
        serial::put_dec(spawn_used as u64);
        serial::puts("/");
        serial::put_dec(SPAWN_SLOTS as u64);
        serial::puts(" registered ");
        serial::put_dec(akuma_exec::process::process_count() as u64);
        serial::puts("\n");
        return errno::ENOMEM;
    };

    // The parent's pid and command line, for the child's `/proc` entry. Read
    // before the child exists, because `proc_by_pid` walks the same table the
    // registration below is about to touch.
    let parent_pid = current_pid();
    let parent_cmdline = proc_by_pid(parent_pid).map_or_else(alloc::vec::Vec::new, |p| p.cmdline);
    let parent_name = proc_by_pid(parent_pid).map_or_else(
        || alloc::string::String::from("fork"),
        |p| alloc::string::String::from(p.name()),
    );

    // Copy the parent's whole address space, off the parent's **registered**
    // process — the only process there is since 5b slice 4.
    let Some(parent) = current_process() else {
        return errno::ESRCH;
    };
    let Some(child) = Image::fork_of(parent, user_rip, user_rsp) else {
        return errno::ENOMEM;
    };
    let child_root = child.space.ttbr0();

    // The child gets its own descriptor table naming the same open
    // descriptions. Before per-process tables existed there was nothing to do
    // here and that was the bug: parent and child shared one flat table, so a
    // child that closed fd 1 to redirect its own output closed the parent's
    // too. Done before the task is published — a child that runs with an empty
    // table cannot open anything and does not say why. (Since step 4b the copy
    // IS the registration's `clone_deep_for_fork` below; there is no second,
    // legacy table to copy alongside it.)

    let Some(task_slot) = crate::sched::spawn_in_space_unpublished(proc_entry, child_root) else {
        // `child` drops here, releasing every frame the CoW share pass claimed
        // and every page table it built — what `take_proc_slot` used to do by
        // taking the slot back out of `PROCS`.
        return errno::ENOMEM;
    };
    crate::sched::seed_proc_slot(task_slot, slot);
    crate::sched::seed_forked_task(task_slot, parent_fs_base, parent_gs_base, &parent_regs);

    let pid = NEXT_PID.fetch_add(1, Ordering::Relaxed) as u32;
    // 5b slice 4: registered **before** the task is published, which is the
    // ordering rule this slice made mandatory — `run_process` reads the child's
    // entry point and stack out of this registration, so a child scheduled
    // first would have nowhere to start. Slices 1-2 registered after publishing
    // and merely left a window where the child's identity did not resolve.
    //
    // `image_top` 0 is deliberate for a `fork` child: it has no heap of its own
    // — it shares the parent's image CoW until the `execve` that virtually
    // always follows refreshes the registered view (`sys_execve`) — and a `brk`
    // naming the parent's heap would answer a grow request into a space the
    // child does not own.
    register_exec_process(
        pid,
        parent_pid,
        task_slot,
        child,
        0,
        parent_name.as_str(),
        // A `fork` child runs the parent's image until it `execve`s, so it
        // shows the parent's command line — the reason `ps` briefly lists two
        // `sh`s.
        &parent_cmdline,
        // Step 4b: the child's registered fd table is the *only* table — a
        // real POSIX copy of the parent's through `clone_deep_for_fork`, whose
        // `clone_fd_refs` bumps one pipe-end/socket reference per inherited
        // descriptor. The bump used to live in the deleted `inherit_fds` and
        // the mirror copied **without** it (running both double-bumped every
        // forked pipeline's pipes and `yes` blocked forever — found by the
        // suite's `redirect` test, first boot of slice 4); with one authority
        // there is one place for it.
        Some(alloc::sync::Arc::new(parent.fds.clone_deep_for_fork())),
    );

    // SAFETY: raw-pointer write; single core.
    unsafe {
        (*spawn_table())[slot - SPAWN_SLOT_BASE] = Some(Spawn {
            pid,
            // Owns no pipe: the child's fd 0/1/2 are *names* for the parent's
            // descriptions, copied by `inherit_fds` above and released by this
            // child's own row sweep at exit. This `None` is what `borrowed_io`
            // used to say.
            stdin_pipe: None,
            exec_slot: task_slot,
        });
    }

    // Published last: the child's register/TLS snapshot, its identity and its
    // stdio row must all be in place before anything can schedule it — the same
    // ordering rule `spawn_in_space_unpublished` exists to enforce (a tick
    // between spawn and seed used to run the child on garbage).
    crate::sched::publish_task(task_slot);

    u64::from(pid)
}

/// `spawn(path, argv, envp, stdin, stdin_len, flags)` — Akuma's own syscall 301.
///
/// Returns `pid | (stdout_fd << 32)` on success, or a negative errno. `flags`
/// bit 0 (`SPAWN_FLAG_PTY`) is accepted and currently ignored: this target has
/// no pty line discipline for a pipe, so an interactive shell gets raw bytes
/// and does its own editing (`paws` already does).
pub fn sys_spawn(path_ptr: u64, argv_ptr: u64, _envp: u64, stdin_ptr: u64, stdin_len: u64) -> u64 {
    use crate::fd::errno;

    let Some(path_bytes) = user_cstr(path_ptr, 256) else {
        return errno::EFAULT;
    };
    let Ok(path) = core::str::from_utf8(&path_bytes) else {
        return errno::EINVAL;
    };

    let Ok(image) = crate::fs::read_file(path) else {
        return errno::ENOENT;
    };

    // argv[0] defaults to the path if the caller passed none.
    let argv_owned = {
        let mut v = user_argv(argv_ptr);
        if v.is_empty() {
            v.push(path_bytes.clone());
        }
        v
    };
    let argv_refs: alloc::vec::Vec<&[u8]> =
        argv_owned.iter().map(alloc::vec::Vec::as_slice).collect();
    // Kept for `/proc/<pid>/cmdline`: the loader writes argv onto the child's
    // stack and this vector is dropped, so this is the last chance to record it.
    let spawn_cmdline = flatten_cmdline(argv_refs.iter().copied());
    let spawner_pid = current_pid();

    // A free process slot in the spawn range. The `SPAWN` row is the oracle
    // since 5b slice 4 — it was `PROCS`, and `fork` consulted both, which is
    // the divergence that made this function and that one disagree about how
    // full the machine was.
    let slot = {
        // SAFETY: raw-pointer read; single core.
        let spawn = unsafe { &*spawn_table() };
        (SPAWN_SLOT_BASE..PROC_SLOTS).find(|&s| spawn[s - SPAWN_SLOT_BASE].is_none())
    };
    let Some(slot) = slot else {
        return errno::ENOMEM;
    };

    let (child, img) = match Image::from_elf_argv(&image, &argv_refs) {
        Ok(p) => p,
        Err(e) => {
            serial::puts("  [spawn] load failed: ");
            serial::puts(e);
            serial::puts("\n");
            return errno::ENOMEM;
        }
    };

    let (Some(stdout_pipe), Some(stdin_pipe)) = (pipe::alloc(), pipe::alloc()) else {
        drop(child);
        return errno::ENOMEM;
    };

    // Seed the child's stdin, if the caller supplied any (`spawn_with_stdin`).
    if stdin_ptr != 0 && stdin_len != 0 {
        // A bad seed pointer seeds nothing rather than failing the spawn: the
        // child is already built, and an empty stdin is a state it handles.
        if let Some(seed) = crate::fd::copy_in(stdin_ptr, stdin_len.min(64 * 1024)) {
            // The pipe was created two statements ago and has both ends, so the
            // broken-pipe answer is unreachable; a short write is not, and the
            // seed is capped at the pipe's own capacity above.
            let _ = pipe::write(stdin_pipe, &seed);
        }
    }

    let root = child.space.ttbr0();

    // **C2 slice 6:** the child's stdio becomes real descriptors — fd 0 = the
    // read end of `stdin_pipe`, fd 1 **and fd 2** = the write end of
    // `stdout_pipe` (one description, two names, which is what keeps stderr on
    // the session after a `dup2(file, 1)`) — in its own registered table,
    // which since step 4b is the only table, replacing the by-number routing
    // the `Spawn` row used to carry. See `fd::bind_stdio` for what this buys.
    // Done before the child can be scheduled: the table is seeded before
    // `publish_task` and handed to the registration below. There is no legacy
    // row to reset defensively — a fresh `SharedFdTable` starts empty.
    let child_fds = alloc::sync::Arc::new(akuma_exec::process::SharedFdTable::new());
    let bind = crate::fd::bind_stdio(&child_fds, stdin_pipe, stdout_pipe);
    if bind != 0 {
        drop(child);
        cleanup_spawn_slot(stdout_pipe, stdin_pipe);
        return bind;
    }

    let Some(task_slot) = spawn_process_task(slot, root) else {
        // `child` drops here: the image is freed by the same destructor that
        // would have freed it out of `PROCS`.
        //
        // The table is unwound **before** the pipes go: it holds three real
        // references now, and `child_fds` is about to drop unregistered —
        // `SharedFdTable::drop` runs `close_all()` anyway, but doing it
        // explicitly keeps the release before `cleanup_spawn_slot` destroys
        // the ends by id, the same ordering the exit path keeps.
        child_fds.close_all();
        cleanup_spawn_slot(stdout_pipe, stdin_pipe);
        return errno::ENOMEM;
    };

    let pid = NEXT_PID.fetch_add(1, Ordering::Relaxed) as u32;
    // 5b slice 1: register the child with `akuma-exec`'s process table and
    // publish `task_slot → pid`, so `current_process_shared()` resolves for
    // this child from its first syscall. The heap starts where the loaded image
    // ends — the same `LoadedImage::end_va` the loader reported and `mm.rs`'s
    // placer agrees with.
    //
    // 5b slice 4: the image itself is moved in here, and this happens **before**
    // `publish_task` below, because `run_process` reads the child's entry point
    // and stack back out of it.
    register_exec_process(
        pid,
        spawner_pid,
        task_slot,
        child,
        img.end_va,
        // **The path, not `argv[0]`.** `Process::image_name` is what
        // `/proc/<pid>/exe` reports — a symlink to the binary since the
        // interception in `akuma-syscalls-glue` moved into procfs — and
        // `argv[0]` is a *name a caller chose*, not a path: `readlink
        // /proc/self/exe` answered `readlink`, and `ls -l` showed
        // `/proc/self/exe -> ls`, neither of which opens anything. `comm`
        // is unaffected: `akuma_procfs::ProcStat::comm` takes the basename,
        // so `ps` still shows `ls` while `exe` names `/bin/ls`.
        path,
        &spawn_cmdline,
        Some(child_fds),
    );

    // SAFETY: raw-pointer write; single core.
    unsafe {
        (*spawn_table())[slot - SPAWN_SLOT_BASE] = Some(Spawn {
            pid,
            stdin_pipe: Some(stdin_pipe),
            exec_slot: task_slot,
        });
    }

    // Last, as in `sys_fork`: identity and stdio in place before anything can
    // schedule the child.
    crate::sched::publish_task(task_slot);

    let stdout_fd = crate::fd::alloc_pipe_fd(stdout_pipe, false, true);
    let Some(stdout_fd) = stdout_fd else {
        // The child is already running; it will just write into a pipe nobody
        // reads. Report the failure — `sshd` drops the session.
        return errno::EMFILE;
    };

    u64::from(pid) | (stdout_fd << 32)
}

/// Release the pipes of a spawn that never got a task. The image itself is a
/// local value now and drops on the way out of [`sys_spawn`], which is why this
/// no longer takes a slot: there is nothing parked under one to take back.
fn cleanup_spawn_slot(stdout_pipe: PipeId, stdin_pipe: PipeId) {
    pipe::free(stdout_pipe);
    pipe::free(stdin_pipe);
}

/// Tasks parked inside `wait4`, as a bitmap over scheduler task slots.
///
/// A set rather than a per-child parent link, because `wait4(-1)` waits for
/// *any* child and the waiter is frequently not in the spawn table at all
/// (`sshd` is pid 1). Waking the whole set on any child's exit is a handful of
/// spurious wakes at most — each parked task re-runs `sys_waitpid` and parks
/// again if the exit was not its child — against the alternative of threading a
/// parent task slot through every spawn, fork and vfork path.
///
/// Relaxed ordering throughout: every reader and writer runs under the BKL, and
/// the atomics are here to make the `static` sound rather than to order
/// anything.
static WAIT4_PARKED: [AtomicU64; crate::sched::MAX_TASKS.div_ceil(64)] =
    [const { AtomicU64::new(0) }; crate::sched::MAX_TASKS.div_ceil(64)];

fn wait4_register(task: usize) {
    if let Some(w) = WAIT4_PARKED.get(task / 64) {
        w.fetch_or(1 << (task % 64), Ordering::Relaxed);
    }
}

fn wait4_unregister(task: usize) {
    if let Some(w) = WAIT4_PARKED.get(task / 64) {
        w.fetch_and(!(1 << (task % 64)), Ordering::Relaxed);
    }
}

/// Make every task parked in `wait4` runnable.
///
/// Called from [`spawn_record_exit`], which is the single place in this kernel
/// where a child's exit status becomes visible — so this is the whole wake path
/// for `wait4`, and it is one call site rather than a rule to remember.
fn wait4_wake_all() {
    for (word, bits) in WAIT4_PARKED.iter().enumerate() {
        let mut set = bits.load(Ordering::Relaxed);
        while set != 0 {
            let bit = set.trailing_zeros() as usize;
            set &= set - 1;
            crate::sched::wake(word * 64 + bit);
        }
    }
}

/// Called from `run_process` when a spawned child leaves ring 3.
pub fn spawn_record_exit(proc_slot: usize, status: i32) {
    // SAFETY: raw-pointer access; single core.
    //
    // The row is read only for the pid that names the process. It used to
    // also carry the child's stdout write end, closed here by hand so the
    // parent's reader saw EOF — **C2 slice 6 deleted that**, because the
    // child's stdio is bound descriptors now: `close_all` (which runs
    // earlier in `run_process`'s exit path) already released the child's
    // ends, and the refcounts did the EOF. A manual `close_write` here would
    // be a *second* decrement of a ref the child no longer holds — it would
    // close a live parent-side end.
    let dying = unsafe {
        (*spawn_table())
            .get_mut(proc_slot - SPAWN_SLOT_BASE)
            .and_then(|s| s.as_ref())
            .map_or(0, |s| s.pid)
    };

    // Reparent this process's children onto init, the way Linux does at exit.
    //
    // This is the other half of `sys_waitpid`'s `ppid` filter and it landed
    // with it: once a wait only considers *your own* children, a child whose
    // parent died has nobody left who may reap it, and its row and its pipes
    // sit in the table until the slots run out. Before the filter the global
    // scan let anyone reap anything, so orphans were collected by accident.
    //
    // Init is pid 1 and is not itself a table row (`current_pid()` answers 1
    // for anything below `SPAWN_SLOT_BASE`), so a reparented child is reapable
    // by whatever runs as init — which on this target is the console shell or
    // `sshd`, both of which do reap.
    if dying != 0 {
        // 5b slice 2: the exit status becomes visible on the **registered**
        // process, which is what `sys_waitpid` now reads. Set before the wake
        // for the same reason the row's was: a parent woken by `wait4_wake_all`
        // re-runs `sys_waitpid` immediately and must find this already true.
        akuma_exec::process::with_process(dying, |p| {
            p.exit_code.store(status, core::sync::atomic::Ordering::Release);
            p.exited.store(true, core::sync::atomic::Ordering::Release);
        });
        // Reparent this process's children onto init, the way Linux does at
        // exit — on the same table that answers the wait, so a child cannot be
        // reparented in one view and orphaned in the other.
        //
        // Two passes rather than a mutation inside `for_each_process`, which
        // hands out `&Process`: writing through that reference would need a
        // const-to-mut cast, and `with_process` is the accessor that exists so
        // it does not have to be. The `Vec` is empty for a process with no
        // children — which is nearly all of them — and `Vec::new` does not
        // allocate until something is pushed, so the common exit path stays
        // allocation-free.
        for orphan in akuma_exec::process::collect_pids(|p| p.parent_pid == dying) {
            akuma_exec::process::with_process(orphan, |p| p.parent_pid = 1);
        }
    }
    // Outside the `unsafe` block and after the status is recorded, both
    // deliberately: a woken parent re-runs `sys_waitpid` immediately, and it
    // must find `exit` already set or it parks again for nothing.
    wait4_wake_all();
}

// **C2 slice 6 deleted `spawn_stdio`, `current_stdin_pipe` and
// `current_stdout_pipe`.** They answered "which pipe serves this task's fd
// 0/1/2" out of the `Spawn` row, and every read, write and poll of an unbound
// 0/1/2 called one of them. A spawned child's stdio is descriptors now, so
// `fd::pipe_read_id`/`pipe_write_id` answer the same question from the table
// the rest of the module already trusts, and an unbound 0/1/2 means exactly
// one thing again: the console.
//
// Removing them is the point of the slice rather than tidying after it. The
// row said "fd 1 and 2 are *this* pipe" **whatever fd 1 currently named**, so
// it kept answering after a `dup2(file, 1)` — `prog > out.txt` sent stderr to
// a pipe fd 1 no longer had — and the descriptor said something different. Two
// sources, one of them ignoring redirection.

pub fn current_proc_slot() -> usize {
    // SAFETY: under the BKL; the per-CPU `UserCtx` pointer is the running task's slot.
    unsafe {
        let uctx = crate::smp::current_uctx();
        if uctx.is_null() {
            usize::MAX
        } else {
            (*uctx).proc_slot
        }
    }
}

/// The stdin pipe write end for pid `pid`, for `fd::sys_openat`'s
/// `/proc/<pid>/fd/0` handling.
///
/// Off the spawn row, not off the child's fd table, and the difference is the
/// direction of the question. `sshd` is asking for the end **it** writes — the
/// end no descriptor names — and the child's fd 0 is the *other* end. Reading
/// the child's table for it would work only for as long as fd 0 still named
/// that pipe: a shell that redirects its own stdin, or a child already past
/// `close_all` on the exit path, would silently answer `ENOENT` to a
/// bridge that is still live. The row outlives both, up to the reap.
pub fn stdin_pipe_for_pid(pid: u32) -> Option<PipeId> {
    // SAFETY: raw-pointer read; single core, no row mutated.
    unsafe {
        (*spawn_table())
            .iter()
            .flatten()
            .find(|s| s.pid == pid)
            .and_then(|s| s.stdin_pipe)
    }
}

/// `close_child_stdin(pid)` — Akuma's syscall 326. `sshd` calls it when the
/// client sends EOF on the channel, so the shell sees end-of-input.
pub fn sys_close_child_stdin(pid: u64) -> u64 {
    match stdin_pipe_for_pid(pid as u32) {
        Some(p) => {
            pipe::close_write(p);
            0
        }
        None => crate::fd::errno::ESRCH,
    }
}

/// `waitpid(pid, status_ptr, options)` — Akuma's syscall 303.
///
/// Non-blocking regardless of `options`: `sshd`'s bridge polls it every tick and
/// must keep draining the child's stdout while it waits. Returns `pid` and
/// writes the wait status (`exit_code << 8`) once the child has exited, `0`
/// while it is still running, `-ESRCH` for an unknown pid.
pub fn sys_waitpid(pid: u64, status_ptr: u64, _options: u64) -> u64 {
    use crate::fd::errno;
    use core::sync::atomic::Ordering;
    let want = pid as u32;
    // `wait4(-1)` / `waitpid(0)` — any child. `-1` arrives as `u32::MAX`.
    let any = want == u32::MAX || want == 0;
    // **Whose** children. Answered by `akuma-exec`'s process table since 5b
    // slice 2; it was the global spawn table, which has a row for every live
    // process rather than for the caller's descendants — so a scan without a
    // parent filter answered "are there any processes?" instead of "do I have
    // any children?".
    //
    // That was a hang, and a bad one: a subshell (`( ls; true )`) forks `ls`,
    // reaps it, and asks once more. Its own row was still there, so the scan
    // said "a child exists, none has exited" — the process saw *itself* as its
    // unexited child and the `Wait4` arm parked it forever
    // (`docs/archive/AKUMA_AMD64_WAIT4_OWNERSHIP.md`). Moving the question onto
    // the registered process keeps the filter and drops the second copy of the
    // parent link: `parent_pid` is now the only place it is written.
    let me = current_pid();

    // Does any matching child exist at all? (For the `-ECHILD` vs `0` decision.)
    let exists = akuma_exec::process::find_process(|p| {
        (p.parent_pid == me && (any || p.pid == want)).then_some(())
    })
    .is_some();
    if !exists {
        // ECHILD, not ESRCH: POSIX gives "you have no such child" its own errno
        // and a shell tests for exactly it to stop reaping. ESRCH here happened
        // to end ash's loop too, but anything that checks — `wait`, `system()`,
        // make's jobserver — reads a wrong answer from it.
        return errno::ECHILD;
    }

    // A matching child that has exited — reap the first one found. `exited` is
    // published by `spawn_record_exit` before it wakes the waiters, so a parent
    // that gets here after a wake finds it set.
    let Some((child_pid, code)) = akuma_exec::process::find_process(|p| {
        (p.parent_pid == me && (any || p.pid == want) && p.exited.load(Ordering::Acquire))
            .then(|| (p.pid, p.exit_code.load(Ordering::Acquire)))
    }) else {
        return 0; // matching child(ren) exist, none has exited yet
    };

    // The spawn row is now consulted for the task slot the identity map is
    // keyed by, and for the one pipe end no descriptor names. Everything else
    // about the child is read above — since C2 slice 6 its stdout pipe is a
    // refcounted descriptor whose ends died with the child's own `close_all`
    // at exit, so the reap's old "spare the stdout pipe for
    // `sshd`'s final drain, and mind `borrowed_io`" bookkeeping is exactly what
    // the refcounts already do.
    let Some(slot_off) = spawn_row_of(child_pid) else {
        // A child known to the process table with no spawn row is not a
        // condition this target has: every registration is paired with a row.
        // Report the reap rather than wedging the parent — losing a pipe leaks
        // a buffer; refusing here would hang a shell.
        reap_exec_process(child_pid, usize::MAX);
        return u64::from(child_pid);
    };
    // SAFETY: `slot_off` came from a live row and nothing yields between.
    let (exec_slot, stdin_pipe) = unsafe {
        let s = (*spawn_table())[slot_off].as_ref().unwrap();
        (s.exec_slot, s.stdin_pipe)
    };

    // Drop the spawn's own writer reference on the child's stdin pipe — the
    // one `sshd` reaches by path rather than by descriptor, so nothing else
    // will. `close_write`, **not** the `pipe::free` this replaced: `free`
    // destroys the pipe whatever the end counts say, and `sshd` may still hold
    // an open `/proc/<pid>/fd/0` descriptor over it whose own close would then
    // land on a stranger's pipe id. Letting the counts decide means the last
    // end out destroys it, which is the rule every other pipe here follows. A
    // `fork` child carries `None` and borrows its parent's, exactly as
    // `borrowed_io` used to say.
    if let Some(p) = stdin_pipe {
        pipe::close_write(p);
    }

    if status_ptr != 0 {
        let raw = ((u64::from((code as u32) & 0xff)) << 8) as i32;
        // A bad `status` pointer loses the status, not the reap: the child is
        // already gone and `wait4` reporting its pid is the useful half.
        let _ = crate::uaccess::write_val::<i32>(status_ptr, raw);
    }

    // SAFETY: raw-pointer access; single core. The child task is Finished — it
    // called `sched::finish()` in `run_process` after recording its exit.
    unsafe {
        (*spawn_table())[slot_off] = None;
    }
    // 5b slice 1: the child is gone from this target's tables, so it goes from
    // akuma-exec's too. 5b slice 4 gave that line a second job: the registered
    // `Process` **owns** the address space now, so retiring it is also what
    // frees the child's frames and page tables — the `drop(p)` that used to
    // happen here, out of `PROCS`, deferred by one reclaim sweep. This target
    // configures `process_reclaim_cooldown_us: 0`, so the next drain site to
    // run collects it; `run_process`'s own terminal drain, the idle loop and
    // the PMM pressure ladder are all of them.
    //
    // The `THREAD_PID_MAP` entry is removed only while the recorded task slot
    // still names this pid (`reap_exec_process`).
    reap_exec_process(child_pid, exec_slot);
    // **The reap is a drain site on this target**, added by 5b slice 4 because
    // that slice is what gave the registered process something worth freeing.
    //
    // `process::reclaim`'s vetted list has three entries here — the exit path's
    // terminal drain, the idle loop, and the PMM pressure ladder — and none of
    // them covers the moment a *parent* collects a child: the child's own
    // terminal drain ran before this retire, the idle loop does not run while a
    // busy shell reaps, and parking one image is not pressure. The boot suite
    // is the extreme case that shows it, and did: at `SMP=1` thread 0 is the
    // test rather than idling, so `spawn`/`busybox`/`fork`'s "teardown leaks
    // nothing" checks each reported a whole image outstanding.
    //
    // The lock context is the one the module docs require: inside a syscall,
    // holding the BKL and nothing else — no PMM lock, no VFS lock, no address
    // space. The reaping thread is alive rather than terminated, so the sweep
    // is not even the pinned variant. It is the same work `drop(p)` did here
    // before the address space moved onto the registered process.
    akuma_exec::process::reclaim::drain_retired_if_requested();
    u64::from(child_pid)
}

/// The spawn-row index for `pid`, or `None`.
///
/// The row is addressed by pid rather than by slot since 5b slice 2: the
/// process table decides *which* child is being reaped, and this finds the
/// stdio that goes with it.
fn spawn_row_of(pid: u32) -> Option<usize> {
    // SAFETY: raw-pointer read; single core, no row mutated.
    unsafe { (*spawn_table()).iter().position(|e| e.as_ref().is_some_and(|s| s.pid == pid)) }
}

#[cfg(not(feature = "no-tests"))]
/// Stage R: `sys_spawn` runs a child, its stdout comes back through a pipe, and
/// `waitpid` reports its exit status.
///
/// Spawns `/bin/hello` — the same image `elf_test` runs, but this time its
/// stdout is a pipe rather than the console and its exit status arrives through
/// `waitpid` rather than the `EXIT_STATUS` global. The self-test calls
/// `sys_spawn` with kernel pointers, which is fine: `user_cstr` just does
/// volatile reads and the kernel may read its own memory.
pub fn spawn_test(t: &mut Suite) {
    /// Every check `hello.rs` reports, all passing (bits 0..=6).
    const HELLO_ALL_OK: u64 = 0x7F;

    let free_before = akuma_pmm::free_count();

    // A syscall's "negative" return is an errno in `-1..=-4095`, i.e. a u64 at
    // the very top of the range; anything below that is a real value.
    const ERRNO_FLOOR: u64 = 0xFFFF_FFFF_FFFF_F000;

    let path = b"/bin/hello\0";
    let arg0 = b"hello\0";
    let argv: [u64; 2] = [arg0.as_ptr() as u64, 0];
    let r = sys_spawn(path.as_ptr() as u64, argv.as_ptr() as u64, 0, 0, 0);
    if !t.check("spawn: sys_spawn returned a handle", r < ERRNO_FLOOR) {
        return;
    }
    let pid = (r & 0xFFFF_FFFF) as u32;
    let stdout_fd = (r >> 32) & 0xFFFF_FFFF;
    t.check("spawn: pid is a real child pid", pid >= 2);

    // **C2 slice 6's central claim, pinned.** The child's stdio is descriptors
    // in its own registered table — fd 0 the read end of its stdin pipe, fd 1
    // and fd 2 the write end of its stdout pipe — and *not* a `Spawn` row
    // consulted by number. Everything slice 6 deleted (`spawn_stdio`,
    // `current_stdin_pipe`, `current_stdout_pipe`, `Spawn::stdout_pipe`,
    // `borrowed_io`, `console_io`) depends on this being true, and the
    // observable failures if it stops being true are all indirect: a shell
    // that reads the console instead of the channel, an `EBADF` from a
    // `dup2` target, a lost stderr. Asked here, of the live child, before it
    // is drained — the one moment the table is guaranteed populated and not
    // yet swept by `close_all`.
    {
        use akuma_exec::process::FileDescriptor;
        let stdio = akuma_exec::process::with_process(pid, |p| {
            let t = p.fds.table.lock();
            (
                matches!(t.get(&0), Some(FileDescriptor::PipeRead(_))),
                matches!(t.get(&1), Some(FileDescriptor::PipeWrite(_))),
                matches!(t.get(&2), Some(FileDescriptor::PipeWrite(_))),
            )
        });
        t.check(
            "spawn: the child's registered table holds fd 0 as its stdin pipe",
            stdio.is_some_and(|(r, _, _)| r),
        );
        t.check(
            "spawn: fd 1 as its stdout pipe",
            stdio.is_some_and(|(_, w, _)| w),
        );
        // fd 2 is a second *name* for fd 1's description, which is what keeps
        // stderr on the session after a `dup2(file, 1)`.
        t.check(
            "spawn: and fd 2 as a second name for the same end",
            stdio.is_some_and(|(_, _, e)| e),
        );
    }

    // Non-blocking reads so the driver keeps polling `waitpid` too.
    crate::fd::sys_fcntl(stdout_fd, 4, 0x800);

    let mut out: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
    let mut status = u64::MAX;
    let mut buf = [0u8; 64];
    let mut spins = 0u64;
    loop {
        spins += 1;
        if spins > 500_000 {
            break;
        }
        let n = crate::fd::sys_read(stdout_fd, buf.as_mut_ptr() as u64, buf.len() as u64);
        if n != 0 && n < ERRNO_FLOOR {
            out.extend_from_slice(&buf[..n as usize]);
        }
        let mut st: i32 = -1;
        if sys_waitpid(u64::from(pid), core::ptr::addr_of_mut!(st) as u64, 0) == u64::from(pid) {
            status = ((st >> 8) & 0xff) as u64;
            break;
        }
        crate::sched::yield_now();
    }

    t.check(
        "spawn: the child's stdout came back through the pipe",
        out.windows(5).any(|w| w == b"[elf]"),
    );
    t.check_eq("spawn: waitpid reported the child's exit status", status, HELLO_ALL_OK);
    // A drained, EOF pipe read returns 0.
    t.check_eq(
        "spawn: reading the child's stdout past EOF returns 0",
        crate::fd::sys_read(stdout_fd, buf.as_mut_ptr() as u64, buf.len() as u64),
        0,
    );
    crate::fd::sys_close(stdout_fd);
    t.check_eq(
        "spawn: teardown leaks nothing",
        akuma_pmm::free_count() as u64,
        free_before as u64,
    );
}

#[cfg(not(feature = "no-tests"))]
/// The `console_notify` syscall (Akuma-private 322), the kernel half of
/// `/bin/wall`. Only the argument-validation edges are checkable from here — a
/// success needs a mapped user page this context does not have — but those edges
/// are what pin the ABI `libakuma::console_notify` is written against: that the
/// length is `a2`, that a zero length is `EINVAL` and a bad pointer is `EFAULT`,
/// and that the number resolves at all rather than falling through to `ENOSYS`.
#[cfg(feature = "console-notify")]
pub fn console_notify_test(t: &mut Suite) {
    use crate::fd::errno;
    const NR: u64 = 0x1000 + 322;

    t.check_eq(
        "console_notify: a zero-length message is EINVAL",
        syscall_dispatch(NR, 0, 0, 0, 0, 0, 0),
        errno::EINVAL,
    );
    // An unmapped low user address: the `rep movsb` user copy faults and
    // `idt.rs`'s `user_copy_fixup` turns that into a returned error, exactly as
    // it does for `resolve_host` and `spawn`'s argument reads.
    t.check_eq(
        "console_notify: an unreadable pointer is EFAULT",
        syscall_dispatch(NR, 0x1000, 16, 0, 0, 0, 0),
        errno::EFAULT,
    );
}

#[cfg(not(feature = "no-tests"))]
/// Stage S: a real static musl **busybox** — a program the tree did not compile
/// — runs an applet and its output comes back.
///
/// `busybox uname -m` exercises the whole "run a foreign binary" surface: the
/// ELF loader on a ~1 MB image, `arch_prctl` for the TLS base, SSE made legal in
/// `boot.s` (busybox's startup `movups` #UD'd without it), `uname(2)`, and
/// `writev` (busybox prints through it, not `write`). Skipped when busybox is
/// not on the disk.
pub fn busybox_test(t: &mut Suite) {
    const ERRNO_FLOOR: u64 = 0xFFFF_FFFF_FFFF_F000;

    if crate::fs::read_file("/bin/busybox").is_err() {
        t.note("busybox: not on the disk; skipped", 0);
        return;
    }

    let free_before = akuma_pmm::free_count();
    let path = b"/bin/busybox\0";
    let (a0, a1a, a2a) = (b"busybox\0", b"uname\0", b"-m\0");
    let argv: [u64; 4] = [
        a0.as_ptr() as u64,
        a1a.as_ptr() as u64,
        a2a.as_ptr() as u64,
        0,
    ];
    let r = sys_spawn(path.as_ptr() as u64, argv.as_ptr() as u64, 0, 0, 0);
    if !t.check("busybox: spawned", r < ERRNO_FLOOR) {
        return;
    }
    let pid = (r & 0xFFFF_FFFF) as u32;
    let stdout_fd = (r >> 32) & 0xFFFF_FFFF;
    crate::fd::sys_fcntl(stdout_fd, 4, 0x800);

    let mut out: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
    let mut status = u64::MAX;
    let mut buf = [0u8; 64];
    let mut spins = 0u64;
    loop {
        spins += 1;
        if spins > 2_000_000 {
            break;
        }
        let n = crate::fd::sys_read(stdout_fd, buf.as_mut_ptr() as u64, buf.len() as u64);
        if n != 0 && n < ERRNO_FLOOR {
            out.extend_from_slice(&buf[..n as usize]);
        }
        let mut st: i32 = -1;
        if sys_waitpid(u64::from(pid), core::ptr::addr_of_mut!(st) as u64, 0) == u64::from(pid) {
            status = ((st >> 8) & 0xff) as u64;
            break;
        }
        crate::sched::yield_now();
    }
    crate::fd::sys_close(stdout_fd);

    t.check_eq("busybox: exited 0", status, 0);
    t.check(
        "busybox: `uname -m` printed x86_64",
        out.windows(6).any(|w| w == b"x86_64"),
    );
    t.check_eq(
        "busybox: teardown leaks nothing",
        akuma_pmm::free_count() as u64,
        free_before as u64,
    );
}

#[cfg(not(feature = "no-tests"))]
/// Stage T: `execve` with no `fork`.
///
/// `busybox sh -c "uname"` is spawned; ash resolves `uname` on `PATH`,
/// `stat`s `/bin/uname` (the path-`stat` half of this stage) and `execve`s it
/// **in place** — no fork. The spawned task keeps its slot, pid and stdout
/// pipe, so `uname`'s output ("Akuma") comes back the same way the shell's would
/// and `waitpid` reaps one child, not two.
pub fn execve_test(t: &mut Suite) {
    const ERRNO_FLOOR: u64 = 0xFFFF_FFFF_FFFF_F000;

    if crate::fs::read_file("/bin/busybox").is_err() || crate::fs::read_file("/bin/sh").is_err() {
        t.note("execve: busybox /bin/sh not on the disk; skipped", 0);
        return;
    }

    let free_before = akuma_pmm::free_count();
    let path = b"/bin/sh\0";
    // The exact shape `sshd`'s exec sessions use: `sh -c "<cmd with args>"`.
    // ash runs a single simple command (even with arguments) by `execve` in
    // place, no fork — a `;`/`|` sequence is what forces the fork it does not
    // have yet.
    let (a0, a1a, a2a) = (b"sh\0", b"-c\0", b"uname -a\0");
    let argv: [u64; 4] = [a0.as_ptr() as u64, a1a.as_ptr() as u64, a2a.as_ptr() as u64, 0];
    let r = sys_spawn(path.as_ptr() as u64, argv.as_ptr() as u64, 0, 0, 0);
    if !t.check("execve: sh spawned", r < ERRNO_FLOOR) {
        return;
    }
    let pid = (r & 0xFFFF_FFFF) as u32;
    let stdout_fd = (r >> 32) & 0xFFFF_FFFF;
    crate::fd::sys_fcntl(stdout_fd, 4, 0x800);

    let mut out: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
    let mut status = u64::MAX;
    let mut buf = [0u8; 64];
    let mut spins = 0u64;
    loop {
        spins += 1;
        if spins > 3_000_000 {
            break;
        }
        let n = crate::fd::sys_read(stdout_fd, buf.as_mut_ptr() as u64, buf.len() as u64);
        if n != 0 && n < ERRNO_FLOOR {
            out.extend_from_slice(&buf[..n as usize]);
        }
        let mut st: i32 = -1;
        if sys_waitpid(u64::from(pid), core::ptr::addr_of_mut!(st) as u64, 0) == u64::from(pid) {
            status = ((st >> 8) & 0xff) as u64;
            break;
        }
        crate::sched::yield_now();
    }
    crate::fd::sys_close(stdout_fd);

    t.check_eq("execve: `sh -c \"uname -a\"` exited 0", status, 0);
    t.check(
        "execve: the exec'd program's output came back",
        out.windows(5).any(|w| w == b"Akuma") && out.windows(6).any(|w| w == b"x86_64"),
    );
    t.check_eq(
        "execve: teardown leaks nothing (one child reaped, not two)",
        akuma_pmm::free_count() as u64,
        free_before as u64,
    );
}

/// Run `busybox sh -c <cmd>` and collect its stdout and exit status.
///
/// The drive loop `fork_test` and `execve_test` each spell out longhand. Split
/// out for [`redirect_test`], which needs it twice; the two older tests are
/// deliberately left alone so a failure there still bisects to their own code.
#[cfg(not(feature = "no-tests"))]
fn run_sh_capture(cmd: &[u8]) -> Option<(u64, alloc::vec::Vec<u8>)> {
    const ERRNO_FLOOR: u64 = 0xFFFF_FFFF_FFFF_F000;
    let path = b"/bin/sh\0";
    let (a0, adash) = (b"sh\0", b"-c\0");
    let argv: [u64; 4] = [a0.as_ptr() as u64, adash.as_ptr() as u64, cmd.as_ptr() as u64, 0];
    let r = sys_spawn(path.as_ptr() as u64, argv.as_ptr() as u64, 0, 0, 0);
    if r >= ERRNO_FLOOR {
        return None;
    }
    let pid = (r & 0xFFFF_FFFF) as u32;
    let stdout_fd = (r >> 32) & 0xFFFF_FFFF;
    crate::fd::sys_fcntl(stdout_fd, 4, 0x800);

    let mut out: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
    let mut status = u64::MAX;
    let mut buf = [0u8; 64];
    let mut spins = 0u64;
    loop {
        spins += 1;
        if spins > 4_000_000 {
            break;
        }
        let n = crate::fd::sys_read(stdout_fd, buf.as_mut_ptr() as u64, buf.len() as u64);
        if n != 0 && n < ERRNO_FLOOR {
            out.extend_from_slice(&buf[..n as usize]);
        }
        let mut st: i32 = -1;
        if sys_waitpid(u64::from(pid), core::ptr::addr_of_mut!(st) as u64, 0) == u64::from(pid) {
            status = ((st >> 8) & 0xff) as u64;
            break;
        }
        crate::sched::yield_now();
    }
    crate::fd::sys_close(stdout_fd);
    Some((status, out))
}

#[cfg(not(feature = "no-tests"))]
/// Shell redirection and pipelines — `dup2` and `pipe(2)`, end to end.
///
/// These are the two things a build system cannot do without, and until
/// 2026-09-06 neither worked: descriptors 0/1/2 were routed by number below
/// `fd`'s table, so `dup2` had nowhere to land, and `pipe`/`pipe2` were not
/// dispatched at all. The symptoms were `echo x > file` leaving a
/// **zero-length file** and `cmd | cmd` reporting *can't create pipe* — both
/// of which read as filesystem or resource problems and are neither.
///
/// Driven through the real `busybox ash` rather than by calling the syscalls
/// directly, because what has to work is the shell's *own* sequence:
/// `open(file)` → `dup2(fd,1)` → `close(fd)` for a redirect, and `pipe()` plus
/// two `dup2`s across a `fork` for a pipeline. A unit test of `dup2` in
/// isolation passed on kernels where neither of those worked.
///
/// The redirect is checked by reading the file **back through the kernel's own
/// filesystem**, not by trusting the shell's exit status: the pre-fix failure
/// exited 0 and left an empty file, so a status check alone scores it green.
pub fn redirect_test(t: &mut Suite) {
    if crate::fs::read_file("/bin/busybox").is_err() || crate::fs::read_file("/bin/sh").is_err() {
        t.note("redirect: busybox /bin/sh not on the disk; skipped", 0);
        return;
    }
    let free_before = akuma_pmm::free_count();

    // 0. `/proc/self/exe` — a **magic symlink served by procfs**, not by a
    //    syscall-layer interception.
    //
    //    It was two `if path == "/proc/self/exe"` arms in
    //    `akuma-syscalls-glue` (`sys_openat` and `sys_readlinkat`) patching a
    //    gap in `akuma_vfs_glue::proc`, which is the same shape as this
    //    target's own `/proc` view and the reason both are being deleted. The
    //    gap is closed in procfs now, so the generic paths serve it — and the
    //    answer generalised from *self only* to any visible pid.
    //
    //    Checked from ring 3 because that is the only place it can work: the
    //    boot row is registered nowhere, and procfs renders from the process
    //    table.
    //
    //    Two assertions, because the first one passed while the file was
    //    useless. `readlink` answered `ls`, `ps`-style — `image.name` was
    //    `argv[0]`, a name the caller chose rather than a path — so the
    //    symlink existed and opened nothing. Reading four bytes through it and
    //    finding `\x7fELF` is what says the link names the binary.
    if let Some((status, out)) = run_sh_capture(b"readlink /proc/self/exe\0") {
        t.check_eq("proc: `readlink /proc/self/exe` exited 0", status, 0);
        t.check(
            "proc: and it names an absolute path",
            out.starts_with(b"/"),
        );
    } else {
        t.check("proc: sh spawned for the exe symlink", false);
    }
    if let Some((_, out)) = run_sh_capture(b"head -c 4 /proc/self/exe\0") {
        t.check(
            "proc: opening through /proc/self/exe reads the ELF it names",
            out.windows(4).any(|w| w == b"\x7fELF"),
        );
    }

    // 1. `>` — the shell's open/dup2/close sequence onto fd 1.
    let Some((status, _)) = run_sh_capture(b"echo REDIROK > /tmp/redir.txt\0") else {
        t.check("redirect: sh spawned for `>`", false);
        return;
    };
    t.check_eq("redirect: `echo … > file` exited 0", status, 0);
    let written = crate::fs::read_file("/tmp/redir.txt");
    t.check("redirect: the redirected file exists", written.is_ok());
    t.check(
        "redirect: the redirected bytes reached the file",
        written.is_ok_and(|d| d.windows(7).any(|w| w == b"REDIROK")),
    );

    // 2. `>>` — the same sequence with `O_APPEND`, which is a *different* open
    // and was the same one until 2026-09-06: `open_flags` read neither
    // `O_APPEND` nor `O_TRUNC`, so every `O_CREAT` began with an empty buffer
    // and `>>` silently behaved as `>`. Checked here rather than beside the
    // `open` unit tests because it is invisible without working redirection —
    // there was no way for a program to reach it before `dup2` landed.
    let Some((astatus, _)) = run_sh_capture(b"echo ONE > /tmp/app.txt\0") else {
        t.check("redirect: sh spawned for `>>`", false);
        return;
    };
    t.check_eq("redirect: `>` for the append case exited 0", astatus, 0);
    let _ = run_sh_capture(b"echo TWO >> /tmp/app.txt\0");
    let appended = crate::fs::read_file("/tmp/app.txt");
    t.check(
        "redirect: `>>` kept the first line",
        appended.as_ref().is_ok_and(|d| d.windows(3).any(|w| w == b"ONE")),
    );
    t.check(
        "redirect: `>>` added the second",
        appended.as_ref().is_ok_and(|d| d.windows(3).any(|w| w == b"TWO")),
    );

    // 3. `|` — `pipe(2)` plus a `dup2` on each side of a fork.
    let Some((pstatus, pout)) = run_sh_capture(b"echo PIPEOK | cat\0") else {
        t.check("redirect: sh spawned for `|`", false);
        return;
    };
    t.check_eq("redirect: `cmd | cmd` exited 0", pstatus, 0);
    t.check(
        "redirect: the piped bytes came out the far end",
        pout.windows(6).any(|w| w == b"PIPEOK"),
    );

    // 4. A **deep** pipeline. One `|` proves `pipe`+`dup2`; twelve proves the
    // process ceiling is really gone (the old `proc_entry_for` handed out nine
    // slots) and that nothing leaks a pipe, a process slot or a task per stage
    // — each of which is a fixed-size table that a shell can exhaust.
    let Some((dstatus, dout)) = run_sh_capture(b"echo DEEPOK | cat | cat | cat | cat | cat | cat | cat | cat | cat | cat | cat\0")
    else {
        t.check("redirect: sh spawned for the deep pipeline", false);
        return;
    };
    t.check_eq("redirect: a 12-stage pipeline exited 0", dstatus, 0);
    t.check(
        "redirect: the bytes survived 12 stages",
        dout.windows(6).any(|w| w == b"DEEPOK"),
    );

    // 5. A pipeline whose **reader leaves first**. `head -n 1` prints its line
    // and exits, dropping the last read end while `yes` is still pushing at a
    // buffer that is already full — so `yes` can only learn the pipe broke by
    // its next write reporting it.
    //
    // This is the rule the amd64 pipe table could not express until it became
    // `akuma-pipes` on 2026-09-06: with a single `write_closed` flag and no end
    // reference counts, a dead reader was indistinguishable from a full buffer,
    // and `write_pipe`'s retry loop span here forever. It is the whole reason
    // this case is a boot check and not a unit test — nothing short of the real
    // shell arranges the exit order.
    let Some((ystatus, _)) = run_sh_capture(b"busybox yes | busybox head -n 1\0") else {
        t.check("redirect: sh spawned for the early-exit reader", false);
        return;
    };
    t.check_eq("redirect: `yes | head -n 1` terminates", ystatus, 0);

    // 6. `>` over a file that is **longer than what replaces it**, and two
    // redirects that write **no bytes at all**. Every case above only ever
    // wrote to a path that did not exist yet, which is precisely why they all
    // passed while `O_TRUNC` and `O_CREAT` did nothing (C2 slice 5's
    // `write_at(path, 0, &[])`, short-circuited by `write_at`'s own
    // `data.is_empty()` guard before it resolved anything — see `sys_openat`).
    //
    // The failures that hid behind that: `echo x >` over 31 bytes left
    // `x\nAAAA…`, keeping the tail of the old file behind the new head; and
    // `: > f` — the idiom for "make this empty", and what `2> err` does for a
    // command that prints no errors — created nothing at all. Found from ring
    // 3 over ssh, not by any check in this suite, which is the lesson the
    // plan's § "The lesson C1 step 3 paid for" states: a redirect whose
    // *exit status* is 0 proves nothing about the bytes.
    let _ = run_sh_capture(b"echo AAAAAAAAAAAAAAAAAAAAAAAAAAAAAA > /tmp/tr.txt\0");
    let _ = run_sh_capture(b"echo x > /tmp/tr.txt\0");
    let truncated = crate::fs::read_file("/tmp/tr.txt");
    t.check_eq(
        "redirect: `>` truncates a longer file to just the new bytes",
        truncated.as_ref().map_or(u64::MAX, |d| d.len() as u64),
        2,
    );
    t.check(
        "redirect: nothing of the old contents survives the truncate",
        truncated.as_ref().is_ok_and(|d| !d.windows(2).any(|w| w == b"AA")),
    );
    let _ = run_sh_capture(b": > /tmp/empty.txt\0");
    let empty = crate::fs::read_file("/tmp/empty.txt");
    t.check("redirect: a zero-byte `>` still creates the file", empty.is_ok());
    t.check_eq(
        "redirect: and creates it empty",
        empty.map_or(u64::MAX, |d| d.len() as u64),
        0,
    );
    let _ = run_sh_capture(b"echo quiet 2> /tmp/noerr.txt\0");
    t.check(
        "redirect: `2>` creates its file even when nothing is written to it",
        crate::fs::read_file("/tmp/noerr.txt").is_ok(),
    );

    t.check_eq(
        "redirect: teardown leaks nothing",
        akuma_pmm::free_count() as u64,
        free_before as u64,
    );
}

#[cfg(not(feature = "no-tests"))]
/// Stage T: `fork` (with `vfork` semantics) + `execve` + `wait4`.
///
/// `busybox sh -c "uname; echo DONE"` — the `;` makes ash a command list, and
/// it **forks** to run `uname` (an external command, not the last in the list)
/// before running the `echo` builtin. So this exercises the whole path: the
/// shell forks, the child `execve`s `/bin/uname` in the shared address space,
/// the parent blocks in `wait4` until the child is done, then finishes the
/// list. Output must carry both `Akuma` (from the forked `uname`) and `DONE`
/// (from the parent shell), and nothing may leak.
/// `wait4` must answer for **your own** children only, and say `ECHILD` when
/// there are none.
///
/// Pins the 2026-09-08 subshell hang. The spawn table is global, so a scan
/// without a `ppid` filter answers "does any process exist?" — and a forked
/// process, whose own row is in that table, saw itself as an unexited child and
/// parked in `wait4` forever. `( ls; true )` was enough to wedge the machine.
///
/// Both halves are checked here because either alone is still broken: a filter
/// that returns the wrong errno leaves a shell reaping in a loop, and the right
/// errno without the filter never gets asked.
///
/// The suite runs as init, whose `current_pid()` is 1, so a child spawned here
/// is genuinely this caller's — the same relationship a shell has to its own.
pub fn wait4_ownership_test(t: &mut Suite) {
    const ERRNO_FLOOR: u64 = 0xFFFF_FFFF_FFFF_F000;
    use crate::fd::errno;

    // No children yet: the answer must be ECHILD, not "block" and not ESRCH.
    let mut st: i32 = -1;
    let r = sys_waitpid(u64::MAX, core::ptr::addr_of_mut!(st) as u64, 0);
    t.check_eq("wait4: no children -> ECHILD", r, errno::ECHILD);

    if crate::fs::read_file("/bin/hello").is_err() {
        t.note("wait4: /bin/hello not on the disk; ownership half skipped", 0);
        return;
    }

    // One child of our own. While it is alive, `wait4(-1)` must report "exists,
    // not exited" (0) rather than reaping something that is not ours.
    let path = b"/bin/hello\0";
    let arg0 = b"hello\0";
    let argv: [u64; 2] = [arg0.as_ptr() as u64, 0];
    let r = sys_spawn(path.as_ptr() as u64, argv.as_ptr() as u64, 0, 0, 0);
    if !t.check("wait4: child spawned", r < ERRNO_FLOOR) {
        return;
    }
    let pid = (r & 0xFFFF_FFFF) as u32;
    let stdout_fd = (r >> 32) & 0xFFFF_FFFF;

    let mut spins = 0u64;
    let reaped = loop {
        spins += 1;
        if spins > 4_000_000 {
            break 0;
        }
        let mut st: i32 = -1;
        let r = sys_waitpid(u64::MAX, core::ptr::addr_of_mut!(st) as u64, 0);
        if r != 0 {
            break r;
        }
        crate::sched::yield_now();
    };
    crate::fd::sys_close(stdout_fd);
    t.check_eq("wait4: -1 reaps our own child by pid", reaped, u64::from(pid));

    // And once it is reaped we are childless again. This is the assertion that
    // fails on the pre-fix kernel: the caller's own row (or any other live
    // process) kept `exists` true, so this returned 0 and the caller parked.
    let mut st: i32 = -1;
    let r = sys_waitpid(u64::MAX, core::ptr::addr_of_mut!(st) as u64, 0);
    t.check_eq("wait4: after the last reap -> ECHILD again", r, errno::ECHILD);
}

#[cfg(not(feature = "no-tests"))]
pub fn fork_test(t: &mut Suite) {
    const ERRNO_FLOOR: u64 = 0xFFFF_FFFF_FFFF_F000;

    if crate::fs::read_file("/bin/busybox").is_err() || crate::fs::read_file("/bin/sh").is_err() {
        t.note("fork: busybox /bin/sh not on the disk; skipped", 0);
        return;
    }

    let free_before = akuma_pmm::free_count();
    let path = b"/bin/sh\0";
    let (a0, a1a, a2a) = (b"sh\0", b"-c\0", b"uname; echo DONE\0");
    let argv: [u64; 4] = [a0.as_ptr() as u64, a1a.as_ptr() as u64, a2a.as_ptr() as u64, 0];
    let r = sys_spawn(path.as_ptr() as u64, argv.as_ptr() as u64, 0, 0, 0);
    if !t.check("fork: sh spawned", r < ERRNO_FLOOR) {
        return;
    }
    let pid = (r & 0xFFFF_FFFF) as u32;
    let stdout_fd = (r >> 32) & 0xFFFF_FFFF;
    crate::fd::sys_fcntl(stdout_fd, 4, 0x800);

    let mut out: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
    let mut status = u64::MAX;
    let mut buf = [0u8; 64];
    let mut spins = 0u64;
    loop {
        spins += 1;
        if spins > 4_000_000 {
            break;
        }
        let n = crate::fd::sys_read(stdout_fd, buf.as_mut_ptr() as u64, buf.len() as u64);
        if n != 0 && n < ERRNO_FLOOR {
            out.extend_from_slice(&buf[..n as usize]);
        }
        let mut st: i32 = -1;
        if sys_waitpid(u64::from(pid), core::ptr::addr_of_mut!(st) as u64, 0) == u64::from(pid) {
            status = ((st >> 8) & 0xff) as u64;
            break;
        }
        crate::sched::yield_now();
    }
    crate::fd::sys_close(stdout_fd);

    t.check_eq("fork: `sh -c \"uname; echo DONE\"` exited 0", status, 0);
    t.check(
        "fork: the forked child's output came back",
        out.windows(5).any(|w| w == b"Akuma"),
    );
    t.check(
        "fork: the parent shell finished the command list",
        out.windows(4).any(|w| w == b"DONE"),
    );
    t.check_eq(
        "fork: teardown leaks nothing",
        akuma_pmm::free_count() as u64,
        free_before as u64,
    );
}

/// Hand a syscall to `akuma-syscalls-glue` — the implementation the AArch64
/// kernel serves it from.
///
/// **This is the C1 seam**, and the one place the number vocabulary changes.
/// Userspace passed an x86_64 number; glue's dispatch is a `match` over
/// asm-generic constants, so `to_aarch64()` is not a formality — handing glue
/// the number that arrived would find the *wrong* arm, not none
/// (`docs/archive/AKUMA_AMD64_C1_DISPATCH_VOCABULARY.md`). Taking a `Syscall`
/// rather than a `u64` is what makes that unskippable.
///
/// Three things had to exist before the first arm could come through here, none
/// of which the C1 hand-off prompt listed
/// (`docs/archive/AKUMA_AMD64_C1_STEP3_PREREQUISITES.md`):
/// `akuma_exec::runtime::register` (`exec_runtime.rs`), `stac`/`clac` in the
/// shared user-copy loop, and an x86 arm for `akuma_mmu::get_current_ttbr0`.
/// The prologue itself needed nothing: it reaches `akuma_exec::threading`,
/// which **is** `akuma-threading`, the scheduler this target has run since A1.
fn to_glue(call: Syscall, args: [u64; 6]) -> u64 {
    akuma_syscalls_glue::handle_syscall(call.to_aarch64(), &args)
}

#[cfg(not(feature = "no-tests"))]
/// The dispatch **vocabulary**: two tables that must stay disjoint, and the
/// number hop that must keep happening.
///
/// Added 2026-09-07 with C1 step 2, which split one 98-arm dispatcher into an
/// x86-only legacy list and the architecture-neutral `Syscall` table. Both
/// failures this guards against are silent:
///
/// - **A number in both tables.** Whichever match runs first wins, and which
///   one that is becomes an accident of ordering rather than a decision.
/// - **The two ABIs agreeing.** `akuma-syscalls-glue` dispatches on
///   asm-generic numbers; this kernel is handed x86_64 ones. If `to_aarch64`
///   ever starts returning the number that came in, the fold is feeding an
///   x86_64 number to an asm-generic `match` — the wrong handler, not none.
///
/// The `symlink` case at the end is the concrete bug this test was written
/// after: x86_64 88 dispatched to `sys_utimensat` for months because a comment
/// called it `futimens`, which is not a syscall at all. `utimensat` on those
/// arguments returns 0 and creates nothing, so only reading the link back tells
/// the two apart — which is exactly why it went unnoticed.
pub fn dispatch_smoke_test(t: &mut Suite, have_fs: bool) {
    // Every number the legacy `match nr` above claims to own. Each must be
    // x86-only; a number that also decodes through `Syscall` is handled twice.
    const X86_ONLY: [u64; 21] = [
        2, 4, 6, 7, 21, 22, 23, 33, 57, 58, 82, 83, 84, 87, 88, 89, 96, 111, 158, 164, 201,
    ];
    let mut overlap = 0u64;
    for n in X86_ONLY {
        if Syscall::from_x86_64(n).is_some() {
            overlap += 1;
        }
    }
    t.check_eq("dispatch: no number is in both tables", overlap, 0);

    // The hop. `write` arrives as 1 and reaches glue as 64.
    t.check_eq("dispatch: write arrives as x86_64 1", Syscall::Write.to_x86_64(), 1);
    t.check_eq("dispatch: write reaches glue as 64", Syscall::Write.to_aarch64(), 64);
    // The crossing that would be loudest and least explicable: x86_64 63 is
    // `uname`, asm-generic 63 is `read`.
    t.check(
        "dispatch: 63 decodes as uname, not read",
        Syscall::from_x86_64(63) == Some(Syscall::Uname),
    );
    t.check_eq("dispatch: uname reaches glue as 160", Syscall::Uname.to_aarch64(), 160);

    // The arms folded into glue, driven **through the real dispatcher** by the
    // x86_64 number userspace would send.
    //
    // Two properties, and the second is the one that needs a test. The answer
    // is unchanged by the fold — every one of these was `=> 0` here and is `=> 0`
    // in glue — so a regression check on the value alone would pass against a
    // dispatcher that had lost the arms entirely. What it cannot pass against is
    // the number hop being wrong, which is why each is asserted beside the
    // asm-generic constant it must reach. x86_64 102-108 — the whole credential
    // block — lands in asm-generic's timer/module block, where this build
    // already answers one number for real: `nr::SETITIMER` is 103, and
    // `sys_setitimer` reads two `struct itimerval` pointers out of `args[1]` and
    // `args[2]`. A `getuid` arriving there would hand it whatever its unset
    // argument registers held. So a missed hop is a *different arm*, not a
    // missing one — the failure mode the `symlink` bug below already cost this
    // target months.
    //
    // `akuma_syscalls_linux::nr` is the asm-generic table by name; writing 174
    // here would test the transcription rather than the mapping.
    //
    // Unrolled rather than looped so a failure names the syscall it belongs to;
    // six identically-labelled checks would report which *count* broke, not which
    // arm.
    use akuma_syscalls_linux::nr;
    let hop = |call: Syscall, x86: u64, generic: u64| {
        call.to_x86_64() == x86 && call.to_aarch64() == generic
    };
    t.check("dispatch: getuid 102 -> 174", hop(Syscall::Getuid, 102, nr::GETUID));
    t.check("dispatch: getgid 104 -> 176", hop(Syscall::Getgid, 104, nr::GETGID));
    t.check("dispatch: geteuid 107 -> 175", hop(Syscall::Geteuid, 107, nr::GETEUID));
    t.check("dispatch: getegid 108 -> 177", hop(Syscall::Getegid, 108, nr::GETEGID));
    t.check("dispatch: setuid 105 -> 146", hop(Syscall::Setuid, 105, nr::SETUID));
    t.check("dispatch: setgid 106 -> 144", hop(Syscall::Setgid, 106, nr::SETGID));

    let d = |nr_x86: u64| syscall_dispatch(nr_x86, 0, 0, 0, 0, 0, 0);
    t.check_eq("dispatch: glue answers getuid 0", d(102), 0);
    t.check_eq("dispatch: glue answers getgid 0", d(104), 0);
    t.check_eq("dispatch: glue answers geteuid 0", d(107), 0);
    t.check_eq("dispatch: glue answers getegid 0", d(108), 0);
    t.check_eq("dispatch: glue accepts setuid", d(105), 0);
    t.check_eq("dispatch: glue accepts setgid", d(106), 0);
    // `getgroups` is the pair that makes the two-number shape earn itself:
    // asm-generic 158 is `getgroups`, x86_64 158 is `arch_prctl`, and this
    // kernel answers both. `size == 0` is the probe form and must report the
    // count (zero here) without touching the buffer; a negative size is EINVAL.
    t.check("dispatch: getgroups 115 -> 158", hop(Syscall::Getgroups, 115, nr::GETGROUPS));
    t.check_eq("dispatch: glue reports 0 supplementary groups", d(115), 0);
    t.check_eq(
        "dispatch: getgroups(-1) is EINVAL",
        syscall_dispatch(115, (-1i64) as u64, 0, 0, 0, 0, 0),
        (-22i64) as u64,
    );

    // Batch 3. Unlike the credentials above, **both of these change their
    // answer**, so these are value checks with a working negative control: the
    // arm they replaced would fail each one.
    t.check("dispatch: prlimit64 302 -> 261", hop(Syscall::Prlimit64, 302, nr::PRLIMIT64));
    t.check("dispatch: getrandom 318 -> 278", hop(Syscall::Getrandom, 318, nr::GETRANDOM));

    // `prlimit64(0, RLIMIT_STACK, NULL, &old)`. The old arm was `=> 0` and
    // wrote nothing, so the sentinel is what catches it — a return-value check
    // alone passes against the bug, exactly as it did for `symlink` below.
    // `RLIM_INFINITY` is the sentinel because it is the one value the arm is
    // *not* supposed to produce for this resource.
    const RLIMIT_STACK: u64 = 3;
    let mut rlim = [u64::MAX; 2];
    let prlimit_rc = syscall_dispatch(302, 0, RLIMIT_STACK, 0, rlim.as_mut_ptr() as u64, 0, 0);
    t.check_eq("dispatch: glue accepts prlimit64", prlimit_rc, 0);
    t.check(
        "dispatch: and writes a real RLIMIT_STACK (the old arm wrote nothing)",
        rlim[0] == (ELF_STACK_PAGES * 4096) as u64 && rlim[1] == rlim[0],
    );
    // The limit reported must be the stack a program actually gets, and this is
    // the check that says which one that is. `exec_runtime.rs` supplied the
    // *kernel* stack here until the fold made anyone read it — a placeholder
    // that was correct only because it was dead.
    t.check(
        "dispatch: RLIMIT_STACK is the user stack, not the kernel one",
        rlim[0] != crate::sched::STACK_SIZE as u64,
    );

    // `getrandom(buf, len, 0)` through glue, which reaches `RDRAND` only via
    // the `akuma_primitives::rng` hook `boot::install_shared_sinks` registers.
    // Without that hook glue asks the virtio-rng device, which no rig of this
    // target has, and every call is `EIO` — so this is the check that the seam
    // is wired, not just that the number hops.
    t.check("dispatch: an entropy source is registered", akuma_primitives::rng::is_registered());
    let mut draw1 = [0u8; 32];
    let mut draw2 = [0u8; 32];
    let got1 = syscall_dispatch(318, draw1.as_mut_ptr() as u64, draw1.len() as u64, 0, 0, 0, 0);
    let got2 = syscall_dispatch(318, draw2.as_mut_ptr() as u64, draw2.len() as u64, 0, 0, 0, 0);
    t.check_eq("dispatch: glue getrandom returns the full length", got1, draw1.len() as u64);
    t.check_eq("dispatch: and again", got2, draw2.len() as u64);
    // Two draws differing is what separates a wired source from a loop that
    // returned the byte count over an untouched buffer — which is what the old
    // arm did whenever `RDRAND` gave up, and it reported success either way.
    t.check("dispatch: two draws differ", draw1 != draw2);
    t.check("dispatch: and neither is all-zero", draw1 != [0u8; 32] && draw2 != [0u8; 32]);
    // Larger than glue's 256-byte chunk, because the arm this replaced capped
    // at one chunk and returned `min(len, 256)`. A caller asking for 300 got
    // 256 and had to loop; glue loops for it.
    let mut big = [0u8; 300];
    t.check_eq(
        "dispatch: getrandom(300) is not truncated to one 256-byte chunk",
        syscall_dispatch(318, big.as_mut_ptr() as u64, big.len() as u64, 0, 0, 0, 0),
        big.len() as u64,
    );
    t.check("dispatch: and the tail past 256 was written", big[256..] != [0u8; 44]);

    if !have_fs {
        t.note("dispatch: no filesystem; symlink round trip skipped", 0);
        return;
    }

    let target = b"/probe.txt\0";
    let link = b"/dispatch-symlink-probe\0";
    // The disk image survives a boot, so clear any leftover before creating it.
    let _ = syscall_dispatch(87, link.as_ptr() as u64, 0, 0, 0, 0, 0);

    // Straight through the dispatcher, by number, the way userspace arrives —
    // all four calls of the round trip. Since 4b step 2 batch 1 the `*at`
    // family is glue's, so this now exercises glue's arms end to end from the
    // boot task: `AT_FDCWD` with no registered process resolves against `/`
    // (glue's own default), and the absolute paths never need an fd table.
    let link_rc = syscall_dispatch(88, target.as_ptr() as u64, link.as_ptr() as u64, 0, 0, 0, 0);
    t.check_eq("dispatch: x86_64 88 is symlink(2) and succeeds", link_rc, 0);
    let mut buf = [0u8; 64];
    let n = syscall_dispatch(89, link.as_ptr() as u64, buf.as_mut_ptr() as u64, buf.len() as u64, 0, 0, 0);
    t.check_eq("dispatch: the link reads back its target length", n, 10);
    t.check("dispatch: and the target itself", &buf[..10] == b"/probe.txt");
    // Cleanup through the real arm: x86_64 87 is unlink(2), unlinkat's shim.
    let unlink_rc = syscall_dispatch(87, link.as_ptr() as u64, 0, 0, 0, 0, 0);
    t.check_eq("dispatch: unlink removes the link", unlink_rc, 0);
    // And it is gone: a second readlink is ENOENT (glue distinguishes it from
    // the EINVAL a non-symlink gets — the answer the local arm never made).
    let gone_rc = syscall_dispatch(89, link.as_ptr() as u64, buf.as_mut_ptr() as u64, buf.len() as u64, 0, 0, 0);
    t.check_eq(
        "dispatch: readlink on a missing path is ENOENT",
        gone_rc,
        (-2i64) as u64,
    );
    // The half that only glue answers right: `/probe.txt` **exists** and is
    // not a symlink, so readlink is `EINVAL` — the local arm collapsed both
    // answers to `ENOENT`, so this check is red against the pre-fold kernel
    // by construction. That is what makes it a differentiator and not a
    // re-statement of the check above.
    let notlink = syscall_dispatch(89, target.as_ptr() as u64, buf.as_mut_ptr() as u64, buf.len() as u64, 0, 0, 0);
    t.check_eq(
        "dispatch: readlink on a non-symlink is EINVAL, not ENOENT",
        notlink,
        (-22i64) as u64,
    );
}

#[cfg(not(feature = "no-tests"))]
/// **The fault path's lookup, priced.**
///
/// 5b slice 4 moved `with_current_regions` / `with_current_address_space` /
/// `cow_swap_frame` off a per-CPU field plus one array index and onto
/// `akuma-exec`'s process table. Those three run on every demand-paging fault
/// and every CoW break, so the honest question about that slice is not whether
/// it is tidier but what it costs, and this is the instrument that answers it —
/// permanently, in every boot, rather than once in a session.
///
/// # Why it is measured here and not with `mem_fault_cost`
///
/// It cannot be measured from ring 3 on this target. `userspace/memprobe/c/`
/// exists and both its instruments build for `x86_64-linux-musl`, but every
/// arm of them is timed with `clock_gettime(CLOCK_MONOTONIC)` and **this
/// kernel's clock has 10 ms granularity**: `net::uptime_us` is
/// `lapic::ticks() * US_PER_TICK`, `US_PER_TICK` is 10 000, and every clock on
/// the target derives from it. Measured 2026-09-08 on QEMU/TCG: the
/// 1000-iteration `mmap_lazy` control reads exactly 10 000 ns per rep (one
/// tick) and every 512-fault bracket reads **0 ns**. A per-fault cost is
/// nanoseconds; the finest thing userspace can see here is ten milliseconds.
/// So the probe is not "not run", it is not *runnable*, and the difference
/// matters — see `scripts/benchmarks/amd64_fault_cost.py`, which will show you.
///
/// TSC has the resolution the tick clock lacks, and the kernel is where the TSC
/// is reachable.
///
/// # What the two numbers are
///
/// * `slot` — [`current_proc_slot`], the per-CPU `UserCtx` field read. This is
///   what the **old** lookup was, plus one bounds-checked array index that no
///   measurement here could separate from noise.
/// * `process` — [`current_process`], the whole new lookup: the same per-CPU
///   read of the task slot, then `THREAD_IDENTITY[tid]`'s cached process-table
///   slot and generation, then `SlotTable::ref_if_current`.
///
/// Both are reported per call, in **TSC ticks**, uncalibrated — the comparison
/// is the point and a frequency would add a second thing to get wrong. Under
/// TCG they are emulation ticks and only the ratio means anything; on real
/// silicon they are cycles.
///
/// # And the check that is not a timing
///
/// A number can drift; what must not is *which path* the lookup takes. The
/// check asserts that `IDENTITY_FALLBACKS` does not move across the whole
/// measured loop — every resolution was a cache hit, not the 256-slot table
/// scan the naive fold would have paid. That is the property the hand-off
/// warned about, and unlike a nanosecond count it cannot pass by luck.
pub fn identity_cost_test(t: &mut Suite) {
    /// Enough iterations that the loop dominates the two `rdtsc`s, and few
    /// enough to cost nothing on a TCG boot.
    const ITERS: u64 = 20_000;

    fn tsc() -> u64 {
        // SAFETY: `rdtsc` is unprivileged and baseline on x86_64.
        unsafe { core::arch::x86_64::_rdtsc() }
    }

    let free_before = akuma_pmm::free_count();
    // The boot task is registered nowhere — that is the whole boot-suite window
    // this target has tripped over three times — so measuring the cached path
    // from it needs an identity to cache. Register one, measure, retire it.
    // A real `UserAddressSpace`, because that is what a real process carries and
    // `register_exec_process` takes ownership of one; it is never activated.
    let Some(space) = UserAddressSpace::new() else {
        t.check("identity: probe address space built", false);
        return;
    };
    let image = Image { space, entry: 0, stack: 0, regions: Vec::new() };
    let pid = alloc_pid();
    let task = crate::sched::current_task();
    register_exec_process(pid, 1, task, image, 0, "cost-probe", b"cost-probe\0", None);

    if !t.check("identity: the running task resolves to its process", current_process().is_some()) {
        finish_test_process(pid, task);
        return;
    }

    let fallbacks_before =
        akuma_exec::process::IDENTITY_FALLBACKS.load(Ordering::Relaxed);

    let t0 = tsc();
    for _ in 0..ITERS {
        core::hint::black_box(current_proc_slot());
    }
    let t1 = tsc();
    for _ in 0..ITERS {
        core::hint::black_box(current_process().is_some());
    }
    let t2 = tsc();

    let fallbacks = akuma_exec::process::IDENTITY_FALLBACKS
        .load(Ordering::Relaxed)
        .saturating_sub(fallbacks_before);

    let slot_ticks = t1.saturating_sub(t0) / ITERS;
    let proc_ticks = t2.saturating_sub(t1) / ITERS;
    t.note("identity: per-call TSC ticks, per-CPU slot read (the old lookup)", slot_ticks);
    t.note("identity: per-call TSC ticks, registered-process lookup", proc_ticks);
    t.note("identity: added ticks per fault-path lookup", proc_ticks.saturating_sub(slot_ticks));
    // The load-bearing assertion. A fallback is the slow path — a `THREAD_PID_MAP`
    // walk and a scan of up to 256 process slots — and one per fault is exactly
    // the regression this slice had to avoid. Zero, not "few": every iteration
    // resolves the same live process from the same live thread, so any fallback
    // at all means the cache is not being consulted.
    t.check_eq("identity: every lookup was a cache hit, not a table scan", fallbacks, 0);

    finish_test_process(pid, task);
    t.check_eq(
        "identity: probe teardown leaks nothing",
        akuma_pmm::free_count() as u64,
        free_before as u64,
    );
}

#[cfg(not(feature = "no-tests"))]
/// Run two isolated processes concurrently and prove they interleave.
pub fn smoke_test(t: &mut Suite) {
    const ROUNDS: u32 = 3;
    const MSG_A: &[u8] = b"    [ring3 A] round\n";
    const MSG_B: &[u8] = b"    [ring3 B] round\n";

    let free_before = akuma_pmm::free_count();

    let (Some(a), Some(b)) = (
        Image::new(MSG_A, ROUNDS, 0, 0x0A),
        Image::new(MSG_B, ROUNDS, 0, 0x0B),
    ) else {
        t.check("ring3: processes built", false);
        return;
    };

    // The isolation property, checked before either runs: the same virtual
    // address resolves to different frames, and to nothing in the kernel's space.
    let pa_a = a.space.translate(USER_CODE_VA);
    let pa_b = b.space.translate(USER_CODE_VA);
    let pa_k = crate::paging::translate(USER_CODE_VA);

    let (Some(a), Some(b)) = (
        start_test_process(0, a, 0, "ring3-a"),
        start_test_process(1, b, 0, "ring3-b"),
    ) else {
        t.check("ring3: processes spawned", false);
        return;
    };
    t.check("ring3: processes spawned", true);

    serial::puts("  -- userspace output follows --\n");
    // Drive the round-robin from the boot task until both processes finish.
    let mut spins = 0u64;
    while !crate::sched::all_user_tasks_finished() && spins < 10_000 {
        spins += 1;
        crate::sched::yield_now();
    }

    t.check("ring3: both spaces map the test VA", pa_a.is_some() && pa_b.is_some());
    t.check("ring3: same VA, different frames", pa_a != pa_b);
    t.check("ring3: kernel space does not map it", pa_k.is_none());
    t.check_eq(
        "ring3: writes served",
        WRITE_SEQ_LEN.load(Ordering::Relaxed),
        u64::from(ROUNDS) * 2,
    );

    // The multitasking claim. Counts alone cannot distinguish "A ran three
    // times then B ran three times" from real interleaving, so this looks for a
    // change of task between consecutive writes.
    let len = WRITE_SEQ_LEN.load(Ordering::Relaxed).min(WRITE_SEQ.len() as u64) as usize;
    let switches = (1..len)
        .filter(|&i| {
            WRITE_SEQ[i].load(Ordering::Relaxed) != WRITE_SEQ[i - 1].load(Ordering::Relaxed)
        })
        .count();
    t.check_eq(
        "ring3: processes interleaved",
        switches as u64,
        u64::from(ROUNDS) * 2 - 1,
    );

    // Both tasks have finished; nothing else names these processes.
    finish_test_process(a.0, a.1);
    finish_test_process(b.0, b.1);
    t.check_eq(
        "ring3: address-space teardown leaks nothing",
        akuma_pmm::free_count() as u64,
        free_before as u64,
    );
}

#[cfg(not(feature = "no-tests"))]
/// Two processes that never yield, interleaved by the timer alone.
///
/// The distinction from [`smoke_test`] is the whole point: those processes call
/// `sched_yield`, so interleaving proves only that the scheduler works. These
/// spin in ring 3 with no syscall between writes, so the *only* way control can
/// leave one is the timer interrupt taking it — which is preemption.
pub fn preempt_test(t: &mut Suite) {
    const ROUNDS: u32 = 3;
    // Long enough to span at least one tick on both machines. Emulation and real
    // silicon differ by ~17x here (§3.9), so this is sized for the slower-to-tick
    // of the two — the Ryzen, at roughly 2.1M spins per tick.
    const DELAY: u32 = 8_000_000;

    let base = WRITE_SEQ_LEN.load(Ordering::Relaxed);
    let free_before = akuma_pmm::free_count();

    let (Some(c), Some(d)) = (
        Image::new(b"    [ring3 C] spinning, never yields\n", ROUNDS, DELAY, 0x0C),
        Image::new(b"    [ring3 D] spinning, never yields\n", ROUNDS, DELAY, 0x0D),
    ) else {
        t.check("preempt: processes built", false);
        return;
    };

    let (Some(c), Some(d)) = (
        start_test_process(2, c, 0, "ring3-c"),
        start_test_process(3, d, 0, "ring3-d"),
    ) else {
        t.check("preempt: processes spawned", false);
        return;
    };
    t.check("preempt: processes spawned", true);

    serial::puts("  -- userspace output follows (no yields) --\n");
    crate::lapic::start_timer();
    let mut spins = 0u64;
    while !crate::sched::all_user_tasks_finished() && spins < 100_000 {
        spins += 1;
        crate::sched::yield_now();
    }
    crate::lapic::stop_timer();

    let len = WRITE_SEQ_LEN.load(Ordering::Relaxed).min(WRITE_SEQ.len() as u64);
    let switches = (base + 1..len)
        .filter(|&i| {
            let i = i as usize;
            WRITE_SEQ[i].load(Ordering::Relaxed) != WRITE_SEQ[i - 1].load(Ordering::Relaxed)
        })
        .count();

    t.check_eq("preempt: writes served", len - base, u64::from(ROUNDS) * 2);
    // Only >= 1 is asserted. How *often* the timer lands inside a spin depends on
    // the tick period against the delay loop, which differs by an order of
    // magnitude between QEMU and real silicon; requiring an exact count would be
    // asserting on the host's speed rather than on preemption.
    t.check("preempt: timer interleaved two non-yielding processes", switches >= 1);
    t.note("preempt: task switches observed between writes", switches as u64);

    // Both tasks have finished; nothing else names these processes.
    finish_test_process(c.0, c.1);
    finish_test_process(d.0, d.1);
    t.check_eq(
        "preempt: teardown leaks nothing",
        akuma_pmm::free_count() as u64,
        free_before as u64,
    );
}

#[cfg(not(feature = "no-tests"))]
/// Two spinning processes with every core running: do they end up on different
/// cores?
///
/// The ring-3 half of `smp::smoke_test`. Its kernel workers proved two *kernel*
/// tasks can execute at once by dropping the BKL around a spin; this proves the
/// thing the BKL design is for — that user code needs no such favour. The same
/// two non-yielding programs as [`preempt_test`], with each `write` tagged by
/// the core that served it ([`WRITE_CPU`]): with a second core idling next to a
/// BSP that is busy driving the test, the idle core's tick picks one of them
/// up, and the writes arrive from two cores. Skipped, and said so, on one core.
pub fn smp_parallel_test(t: &mut Suite) {
    const ROUNDS: u32 = 3;
    const DELAY: u32 = 8_000_000;

    if crate::smp::online_cpus() < 2 {
        t.note("smp: one core online, ring-3 parallelism check skipped", 1);
        return;
    }

    let base = WRITE_SEQ_LEN.load(Ordering::Relaxed);
    let free_before = akuma_pmm::free_count();

    let (Some(c), Some(d)) = (
        Image::new(b"    [ring3 E] spinning on some core\n", ROUNDS, DELAY, 0x0E),
        Image::new(b"    [ring3 F] spinning on some core\n", ROUNDS, DELAY, 0x0F),
    ) else {
        t.check("smp ring3: processes built", false);
        return;
    };

    // Slots 2 and 3 are free again: `preempt_test` reaped its processes.
    let (Some(c), Some(d)) = (
        start_test_process(2, c, 0, "ring3-e"),
        start_test_process(3, d, 0, "ring3-f"),
    ) else {
        t.check("smp ring3: processes spawned", false);
        return;
    };
    t.check("smp ring3: processes spawned", true);

    serial::puts("  -- userspace output follows (two cores) --\n");
    crate::lapic::start_timer();
    let mut spins = 0u64;
    while !crate::sched::all_user_tasks_finished() && spins < 100_000_000 {
        spins += 1;
        crate::sched::yield_now();
    }
    crate::lapic::stop_timer();

    let len = WRITE_SEQ_LEN.load(Ordering::Relaxed).min(WRITE_SEQ.len() as u64);
    let cpus = (base..len).fold(0u64, |acc, i| {
        let c = WRITE_CPU[i as usize].load(Ordering::Relaxed);
        if c < 64 { acc | (1 << c) } else { acc }
    });

    t.check_eq("smp ring3: writes served", len - base, u64::from(ROUNDS) * 2);
    t.note("smp ring3: cpu mask the writes came from", cpus);
    t.check("smp ring3: two processes ran on two cores", cpus.count_ones() >= 2);

    // Both tasks have finished; nothing else names these processes.
    finish_test_process(c.0, c.1);
    finish_test_process(d.0, d.1);
    t.check_eq(
        "smp ring3: teardown leaks nothing",
        akuma_pmm::free_count() as u64,
        free_before as u64,
    );
}

/// Call `f(p_vaddr, p_memsz)` for every non-empty `PT_LOAD` program header in
/// an ELF64 image.
///
/// Reads the fields it needs at their architectural offsets rather than going
/// through the `elf` crate, which is the point: what the loader did is checked
/// against a segment list derived independently of the code that produced it.
/// A parser bug that dropped a segment would otherwise agree with itself — and
/// since C1 step 6 the loader *is* the `elf` crate, so an independent reader
/// here is the only thing left that can disagree with it.
///
/// `p_memsz == 0` segments are skipped, because a segment that occupies no
/// memory is mapped by neither loader and counting one would make the caller's
/// comparison fail for a reason that is not about placement. No image in this
/// tree carries one; the filter is so that the first that does fails loudly
/// somewhere better than here.
#[cfg(not(feature = "no-tests"))]
fn for_each_pt_load(image: &[u8], mut f: impl FnMut(u64, u64)) {
    const PT_LOAD: u32 = 1;
    let u16_at = |off: usize| u16::from_le_bytes([image[off], image[off + 1]]) as usize;
    let u64_at = |b: &[u8]| {
        u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
    };
    let phoff = u64_at(&image[32..40]) as usize;
    let phentsize = u16_at(54);
    let phnum = u16_at(56);

    for i in 0..phnum {
        let at = phoff + i * phentsize;
        // An ELF64 phdr is 56 bytes: p_type(4) p_flags(4) p_offset(8)
        // p_vaddr(8) p_paddr(8) p_filesz(8) p_memsz(8).
        let Some(ph) = image.get(at..at + 56) else { continue };
        if u32::from_le_bytes([ph[0], ph[1], ph[2], ph[3]]) != PT_LOAD {
            continue;
        }
        let vaddr = u64_at(&ph[16..24]);
        let memsz = u64_at(&ph[40..48]);
        if memsz != 0 {
            f(vaddr, memsz);
        }
    }
}

/// Count non-empty `PT_LOAD` program headers. See [`for_each_pt_load`].
#[cfg(not(feature = "no-tests"))]
fn count_pt_load(image: &[u8]) -> u64 {
    let mut n = 0;
    for_each_pt_load(image, |_, _| n += 1);
    n
}

#[cfg(not(feature = "no-tests"))]
/// Load a linked ELF image and run it.
///
/// The distinction from the two tests above is the same one Stage F drew
/// against Stage E: those run a program *this file assembled*, so they can only
/// ever exercise code the kernel already knew how to emit. This one runs an
/// image `rustc` produced and the kernel had to parse — headers it did not
/// write, segment placement it did not choose, and an entry point it read out
/// of the file.
///
/// # What the exit status carries
///
/// `hello.rs` checks six properties of the load and reports them as bits, and
/// the expectation here is spelled out as named constants rather than as `0x3F`
/// so that a partial failure names itself. Reporting through the status rather
/// than through `write` is what makes a bad load fail the boot: a program that
/// printed its verdict would still have "passed" by running at all.
pub fn elf_test(t: &mut Suite) {
    /// `.data` arrived with its linked contents.
    const DATA_OK: u64 = 1 << 0;
    /// `.bss` is zero across all 32 KiB of it.
    const BSS_OK: u64 = 1 << 1;
    /// The `PF_W` segment really is writable.
    const WRITABLE_OK: u64 = 1 << 2;
    /// `argc` is what the stack builder wrote.
    const ARGC_OK: u64 = 1 << 3;
    /// `argv[0]` points at the expected NUL-terminated string.
    const ARGV_OK: u64 = 1 << 4;
    /// `AT_PAGESZ` is present in the auxiliary vector and is 4096.
    const AUXV_OK: u64 = 1 << 5;
    /// A syscall preserved every register the Linux x86_64 ABI promises. This
    /// bit is the one that failed on its first run — see `syscall_entry`.
    const REGS_OK: u64 = 1 << 6;
    const ALL_OK: u64 =
        DATA_OK | BSS_OK | WRITABLE_OK | ARGC_OK | ARGV_OK | AUXV_OK | REGS_OK;

    let free_before = akuma_pmm::free_count();

    // Rejection first, and before anything is allocated: a loader is judged as
    // much by what it refuses as by what it loads, and these cost nothing to
    // check because each fails before a frame is touched.
    reject_test(t);

    // From the filesystem when there is one. This is the whole point of the
    // stage: the bytes came off a disk the kernel discovered, through a
    // filesystem it mounted, found by path — rather than out of its own `.rodata`.
    let from_disk = crate::fs::read_file("/bin/hello");
    let image: &[u8] = if let Ok(bytes) = from_disk.as_deref() {
        {
            // The embedded copy and the on-disk copy come from two different
            // build steps (`build.rs` into OUT_DIR, `mkdisk.sh` into the image).
            // They are supposed to be the same file; asserting it is what turns
            // that into a checked fact rather than a convention, and a mismatch
            // would mean the image is stale — which would otherwise show up as
            // the previous run's program silently running again.
            // Reported, not scored. This asserts that two *build steps* agreed
            // — `build.rs` into OUT_DIR and `mkdisk.sh` into the image — which
            // is true when both ran on the same machine from the same tree and
            // false the moment they did not. On the bare-metal box they
            // routinely do not: the kernel is cross-built on a laptop and
            // shipped, while `root.img` stays whatever the last image build
            // made. A scored check there fails for a reason that says nothing
            // about the kernel, and `run_shell = passed && have_fs` then
            // withholds sshd over it. The mismatch is still worth *seeing* —
            // it means the program that ran is not the one you just built.
            t.note("elf: on-disk image size", bytes.len() as u64);
            if bytes != HELLO_ELF {
                serial::puts(
                    "  elf:  NOTE on-disk /bin/hello differs from the embedded copy \
                     — root.img is from a different build than this kernel\n",
                );
            }
            serial::puts("  elf:  loading /bin/hello from ext2\n");
            bytes
        }
    } else {
        serial::puts("  elf:  no filesystem; loading the embedded image\n");
        HELLO_ELF
    };
    t.check("elf: image came from the filesystem", from_disk.is_ok());

    let (proc, img) = match Image::from_elf(image) {
        Ok(p) => p,
        Err(e) => {
            t.check("elf: image loaded", false);
            serial::puts("  elf: load failed: ");
            serial::puts(e);
            serial::puts("\n");
            return;
        }
    };
    t.check("elf: image loaded", true);

    // Every PT_LOAD in the file was placed — asked of the **page tables**, not
    // of a count the loader reported about itself. Before C1 step 6 this was
    // `img.segments == count_pt_load(image)`; `akuma_elf::LoadedElf` reports no
    // segment count, and re-deriving one from the same headers the loader read
    // would have made the check agree with itself. Both ends of each segment
    // are probed, so a loader that mapped a segment's first page and stopped
    // fails here rather than in ring 3.
    //
    // Counted out of the image rather than written as a literal: how many
    // segments lld emits is its decision, not ours — `user.ld` names three
    // output sections and the current link produces four LOADs, because `-z
    // relro` splits .data from .bss. A literal here would turn any future
    // linker flag into a test failure, while this catches the thing that
    // matters: a loader that skipped one would produce a program that runs
    // right up until it touches the segment that is missing.
    //
    // `hello` is `ET_EXEC`, so its `p_vaddr`s are its runtime addresses and
    // there is no load bias to add. A PIE probe here would need the base.
    let mapped_segments = {
        let space = &proc.space;
        let mut n = 0u64;
        for_each_pt_load(image, |vaddr, memsz| {
            let first = (vaddr & !0xfff) as usize;
            let last = ((vaddr + memsz - 1) & !0xfff) as usize;
            if space.is_mapped(first) && space.is_mapped(last) {
                n += 1;
            }
        });
        n
    };
    t.check_eq("elf: every PT_LOAD is mapped", mapped_segments, count_pt_load(image));
    t.check(
        "elf: image ends above its entry point",
        img.end_va > img.entry && img.end_va % 4096 == 0,
    );
    // .bss is 32 KiB, so the writable segment alone is 9 pages; with .text,
    // .rodata and two stack pages the total cannot be a single-page accident.
    t.check(
        "elf: frames owned covers image and stack",
        proc.space.user_frame_count() >= 12,
    );

    // The entry point is what the file said, not what the kernel assumed. Read
    // straight out of the image's `e_entry` field so a loader that ignored it
    // and jumped at the first segment would fail here rather than by crashing.
    let want_entry = u64::from_le_bytes([
        image[24], image[25], image[26], image[27],
        image[28], image[29], image[30], image[31],
    ]);
    t.check_eq("elf: entry point is e_entry", proc.entry, want_entry);

    // Permissions read back out of the page tables — what the hardware will do,
    // not what the loader believes it did. The entry page must be executable and
    // not writable; the stack must be the reverse. Both are W^X, from opposite
    // ends.
    let entry_prot = proc.space.pte_prot(proc.entry as usize & !0xfff).map(|(p, _)| p);
    t.check(
        "elf: entry page is user-executable and not writable",
        entry_prot == Some(PteProt::USER_RX),
    );
    let stack_prot = proc.space.pte_prot((proc.stack as usize) & !0xfff).map(|(p, _)| p);
    t.check(
        "elf: stack page is user-writable and not executable",
        stack_prot == Some(PteProt::USER_RW),
    );

    // The stack is a separate mapping from the image, not an extension of it.
    t.check(
        "elf: stack is above the image and mapped",
        proc.stack < ELF_STACK_TOP && proc.stack >= ELF_STACK_TOP - (ELF_STACK_PAGES as u64 * 4096),
    );

    let Some(started) = start_test_process(4, proc, img.end_va, "hello") else {
        t.check("elf: process spawned", false);
        return;
    };
    t.check("elf: process spawned", true);

    EXIT_STATUS.store(u64::MAX, Ordering::Relaxed);
    serial::puts("  -- userspace output follows (from an ELF image) --\n");
    let mut spins = 0u64;
    while !crate::sched::all_user_tasks_finished() && spins < 10_000 {
        spins += 1;
        crate::sched::yield_now();
    }

    let status = EXIT_STATUS.load(Ordering::Relaxed);
    t.check_eq("elf: program ran and reported every check", status, ALL_OK);
    if status != ALL_OK && status != u64::MAX {
        // Name the failures individually. A bare "got 0x2F, want 0x3F" makes the
        // reader decode a bitmask to learn that argv[0] was wrong.
        t.check("elf:   .data holds its linked contents", status & DATA_OK != 0);
        t.check("elf:   .bss is zero-filled", status & BSS_OK != 0);
        t.check("elf:   the PF_W segment is writable", status & WRITABLE_OK != 0);
        t.check("elf:   argc is on the stack", status & ARGC_OK != 0);
        t.check("elf:   argv[0] points at its string", status & ARGV_OK != 0);
        t.check("elf:   auxv carries AT_PAGESZ", status & AUXV_OK != 0);
        t.check("elf:   a syscall preserved the ABI's registers", status & REGS_OK != 0);
    }

    finish_test_process(started.0, started.1);
    t.check_eq(
        "elf: teardown leaks nothing",
        akuma_pmm::free_count() as u64,
        free_before as u64,
    );
}

#[cfg(not(feature = "no-tests"))]
/// Images the loader must refuse, and the reason each one exists.
///
/// Every case is a mutation of the *real* image rather than a hand-written
/// header, so a change to how `hello` is linked cannot leave these testing a
/// shape the loader no longer sees. They check that a rejection happens, not
/// which message comes back — the messages are diagnostics and pinning them
/// would make rewording one a test failure.
fn reject_test(t: &mut Suite) {
    let free_before = akuma_pmm::free_count();

    // A buffer big enough for the header mutations. Only the first 64 bytes are
    // ever changed, and every case below is rejected during header validation,
    // so the truncated image is never actually placed.
    let mut buf = [0u8; 256];
    buf.copy_from_slice(&HELLO_ELF[..256]);

    let cases: [(&str, usize, &[u8]); 4] = [
        // e_ident[EI_MAG0..4]: not an ELF at all.
        ("elf: rejects a non-ELF image", 0, &[0x7f, b'E', b'L', b'G']),
        // e_type at offset 16: ET_DYN (3). Static-PIE (`ET_DYN`, no
        // `PT_INTERP`) is accepted since 2026-09-04 — see the module header's
        // "Static-PIE" section — so this case is no longer refused *for being
        // ET_DYN*; it is refused because `HELLO_ELF[..256]` truncates its
        // first `PT_LOAD` segment's file data (`p_offset=0x1000`,
        // `p_filesz=500`, past this test's 256-byte buffer), which the
        // relocated-at-`PIE_BASE` load path reaches and rejects on exactly
        // the same "segment data past end of image" check an `ET_EXEC` load
        // of the same truncated bytes would hit. Kept under this name because
        // the outcome this suite actually checks (refused) is unchanged, and
        // per this function's own doc it checks that, not why — a dedicated
        // "refuses `PT_INTERP`" case would need a hand-built image rather
        // than a `HELLO_ELF` mutation, which is real, not-yet-written
        // coverage rather than something this case still stands in for.
        ("elf: rejects ET_DYN", 16, &[3, 0]),
        // e_machine at offset 18: EM_AARCH64 (183). The other architecture in
        // this tree, which is the mistake actually available to make.
        ("elf: rejects a non-x86-64 machine", 18, &[183, 0]),
        // e_ident[EI_CLASS] at offset 4: ELFCLASS32.
        ("elf: rejects ELF32", 4, &[1]),
    ];

    for (name, at, bytes) in cases {
        let mut img = buf;
        img[at..at + bytes.len()].copy_from_slice(bytes);
        // The address space is the loader's to build and to drop since C1
        // step 6, so a refused load releases whatever it had already placed
        // without this loop naming it — which is what the `free_count`
        // assertion below is actually testing now.
        let refused = loader::load(&img).is_err();
        t.check(name, refused);
    }

    // A truncated image: the header claims segments the file does not contain.
    t.check("elf: rejects a truncated image", loader::load(&HELLO_ELF[..48]).is_err());

    t.check_eq(
        "elf: rejected loads leak nothing",
        akuma_pmm::free_count() as u64,
        free_before as u64,
    );
}

#[cfg(not(feature = "no-tests"))]
/// Run the file/memory syscall probe in ring 3.
///
/// `fd::smoke_test` and `mm::smoke_test` call the same functions from ring 0,
/// where a user pointer is just a pointer. This runs them across the privilege
/// boundary through the `syscall` instruction with the real x86_64 numbers,
/// which is the only place a wrong argument register shows up — `r10` versus
/// `rcx` for the fourth argument in particular, which the `syscall` instruction
/// forces and which no ring-0 test can catch.
pub fn fdprobe_test(t: &mut Suite) {
    /// Every bit the probe sets when the whole surface works. See its header for
    /// what each one claims.
    const ALL_OK: u64 = 0xFFF;

    let Ok(image) = crate::fs::read_file("/bin/fdprobe") else {
        t.note("fdprobe: not on the disk; skipped", 0);
        return;
    };

    let free_before = akuma_pmm::free_count();
    let (proc, _img) = match Image::from_elf(&image) {
        Ok(p) => p,
        Err(e) => {
            t.check("fdprobe: image loaded", false);
            serial::puts("  fdprobe: load failed: ");
            serial::puts(e);
            serial::puts("\n");
            return;
        }
    };
    let Some(started) = start_test_process(5, proc, _img.end_va, "fdprobe") else {
        t.check("fdprobe: spawned", false);
        return;
    };
    t.check("fdprobe: spawned", true);

    EXIT_STATUS.store(u64::MAX, Ordering::Relaxed);
    let mut spins = 0u64;
    while !crate::sched::all_user_tasks_finished() && spins < 10_000 {
        spins += 1;
        crate::sched::yield_now();
    }

    let status = EXIT_STATUS.load(Ordering::Relaxed);
    t.check_eq("fdprobe: every syscall claim held", status, ALL_OK);
    if status != ALL_OK && status != u64::MAX {
        // Name the failures individually rather than making the reader decode a
        // 12-bit mask.
        let claims: [(&str, u64); 12] = [
            ("fdprobe:   openat returns a descriptor", 1 << 0),
            ("fdprobe:   read returns the file's bytes", 1 << 1),
            ("fdprobe:   lseek(SEEK_SET) rewinds", 1 << 2),
            ("fdprobe:   lseek(SEEK_END) reports the size", 1 << 3),
            ("fdprobe:   reading at EOF returns 0", 1 << 4),
            ("fdprobe:   fstat reports the size", 1 << 5),
            ("fdprobe:   close invalidates the descriptor", 1 << 6),
            ("fdprobe:   a missing path is ENOENT", 1 << 7),
            ("fdprobe:   mmap returns zeroed memory", 1 << 8),
            ("fdprobe:   that memory holds a write", 1 << 9),
            ("fdprobe:   munmap succeeds", 1 << 10),
            ("fdprobe:   a file-backed mmap is refused", 1 << 11),
        ];
        for (label, bit) in claims {
            t.check(label, status & bit != 0);
        }
    }

    finish_test_process(started.0, started.1);
    // The probe mmaps and munmaps, so its frames must come back too — a leak
    // here is `mm::sys_munmap` failing to free rather than the loader.
    t.check_eq(
        "fdprobe: teardown leaks nothing",
        akuma_pmm::free_count() as u64,
        free_before as u64,
    );
}

/// Load `path` and give it the console.
///
/// Not a self-test — it never returns while the shell is running, and it blocks
/// on the UART waiting for a keystroke. It is the last thing the boot does, and
/// only when the binary is on the disk, so a machine without it still reaches
/// the "all self-tests passed" line and halts as before.
///
/// Selected by `init=` on the boot command line. `paws` is the shell; `httpd`
/// is a server that never reads the console. Both are the same binaries the
/// aarch64 devbox runs, compiled for `x86_64-unknown-none` against a ported
/// `libakuma`.
pub fn run_init(path: &str, args: &[&str]) -> bool {
    let Ok(image) = crate::fs::read_file(path) else {
        serial::puts("  [init] not on the disk: ");
        serial::puts(path);
        serial::puts("\n");
        return false;
    };
    // argv[0] is the path; `args` (from `initargs=`) follow. A multicall binary
    // like busybox dispatches on `argv[1]` when `argv[0]`'s basename is
    // `busybox`, so `initargs=uname,-a` runs its `uname` applet.
    let mut argv_owned: alloc::vec::Vec<&[u8]> = alloc::vec::Vec::with_capacity(1 + args.len());
    argv_owned.push(path.as_bytes());
    for a in args {
        argv_owned.push(a.as_bytes());
    }
    // Init has no `Spawn` entry, so `/proc/1` reads its argv from here.
    set_init_cmdline(argv_owned.iter().copied());
    let (proc, ld) = match Image::from_elf_argv(&image, &argv_owned) {
        Ok(p) => p,
        Err(e) => {
            serial::puts("  [init] failed to load: ");
            serial::puts(e);
            serial::puts("\n");
            return false;
        }
    };
    let init_image_top = ld.end_va;
    let root = proc.space.ttbr0();
    let Some(task_slot) = spawn_process_task(6, root) else {
        serial::puts("  [init] no task slot\n");
        return false;
    };
    // 5b slice 1: init is pid 1 on this target — `sshd`'s `getpid` says so, and
    // every process not in the process table falls back to the same answer — so
    // the registered identity names that pid. Nothing unregisters it: init runs
    // until the machine stops, and the boot loop after this never returns while
    // it lives.
    //
    // 5b slice 4: the image goes in with it, and `publish_task` comes after —
    // `run_process` reads init's entry point and stack out of this
    // registration, so publishing first would race a task with nowhere to start
    // against the register that gives it one.
    register_exec_process(1, 0, task_slot, proc, init_image_top, path, &INIT_CMDLINE.lock().clone(), None);
    crate::sched::publish_task(task_slot);
    // The sign-on banner, last thing before the init program starts: on the HP
    // box the console is a television, and this is what is on it when sshd comes
    // up.
    crate::banner::print();
    serial::puts("-- running ");
    serial::puts(path);
    serial::puts(" --\n");
    // Drive the round-robin from the boot task. Unbounded on purpose: a shell
    // runs until it exits, and the spin cap the self-tests use would kill it
    // mid-session.
    while !crate::sched::all_user_tasks_finished() {
        crate::sched::yield_now();
    }
    serial::puts("\n-- init exited --\n");
    true
}


#[cfg(not(feature = "no-tests"))]
/// `clone(CLONE_VM|CLONE_THREAD)` and `futex`, exercised from ring 3.
///
/// Wired 2026-09-06 with the syscalls themselves
/// (`docs/archive/AKUMA_AMD64_RUST_STD.md`). A boot check rather than only the
/// `ruststd` probe, because that one is built by a cross toolchain the kernel's
/// own build does not own and runs only when someone asks for it — a
/// regression in threads has to fail a boot, not wait to be noticed.
///
/// Runs in `PROCS` slot 5, next to `elf_test`'s slot 4 and by the same shape:
/// load, spawn, drive the scheduler until it finishes, read the exit status,
/// tear down, assert the frame count came back.
pub fn thread_test(t: &mut Suite) {
    /// `clone` returned a plausible new tid.
    const TID_OK: u64 = 1 << 0;
    /// `CLONE_PARENT_SETTID` wrote it into the parent's word.
    const PARENT_SETTID_OK: u64 = 1 << 1;
    /// The child ran, and its own `gettid` agrees.
    const CHILD_TID_OK: u64 = 1 << 2;
    /// The child read the parent's memory: `CLONE_VM` shares.
    const SHARE_P2C_OK: u64 = 1 << 3;
    /// The parent read the child's write to the same page.
    const SHARE_C2P_OK: u64 = 1 << 4;
    /// `FUTEX_WAIT` was released by the child's `FUTEX_WAKE`.
    const FUTEX_OK: u64 = 1 << 5;
    /// `CLONE_CHILD_CLEARTID` zeroed the join word on thread exit.
    const CLEARTID_OK: u64 = 1 << 6;
    /// The kernel's own wake on that word released a second wait.
    const JOIN_WAKE_OK: u64 = 1 << 7;
    /// `mmap` still works alongside all of it.
    const MMAP_OK: u64 = 1 << 8;
    const ALL_OK: u64 = TID_OK
        | PARENT_SETTID_OK
        | CHILD_TID_OK
        | SHARE_P2C_OK
        | SHARE_C2P_OK
        | FUTEX_OK
        | CLEARTID_OK
        | JOIN_WAKE_OK
        | MMAP_OK;

    let free_before = akuma_pmm::free_count();

    let (proc, _img) = match Image::from_elf(THREADPROBE_ELF) {
        Ok(p) => p,
        Err(e) => {
            t.check("thread: probe loaded", false);
            serial::puts("  thread: load failed: ");
            serial::puts(e);
            serial::puts("\n");
            return;
        }
    };
    t.check("thread: probe loaded", true);
    let Some(started) = start_test_process(5, proc, _img.end_va, "threadprobe") else {
        t.check("thread: probe spawned", false);
        return;
    };
    t.check("thread: probe spawned", true);

    EXIT_STATUS.store(u64::MAX, Ordering::Relaxed);
    let mut spins = 0u64;
    while !crate::sched::all_user_tasks_finished() && spins < 200_000 {
        spins += 1;
        crate::sched::yield_now();
    }

    let status = EXIT_STATUS.load(Ordering::Relaxed);
    t.check_eq("thread: probe reported every check", status, ALL_OK);
    if status != ALL_OK && status != u64::MAX {
        // Named individually: a bitmask in a boot log is a puzzle, and each of
        // these fails for a different reason in a different file.
        t.check("thread:   clone returned a new tid", status & TID_OK != 0);
        t.check("thread:   CLONE_PARENT_SETTID wrote it", status & PARENT_SETTID_OK != 0);
        t.check("thread:   the child ran and gettid agrees", status & CHILD_TID_OK != 0);
        t.check("thread:   the child sees the parent's memory", status & SHARE_P2C_OK != 0);
        t.check("thread:   the parent sees the child's", status & SHARE_C2P_OK != 0);
        t.check("thread:   FUTEX_WAIT was woken by FUTEX_WAKE", status & FUTEX_OK != 0);
        t.check("thread:   CLONE_CHILD_CLEARTID zeroed the join word", status & CLEARTID_OK != 0);
        t.check("thread:   the join word's wake was delivered", status & JOIN_WAKE_OK != 0);
        t.check("thread:   mmap still works", status & MMAP_OK != 0);
    }

    // No thread of the probe may outlive it. `run_process` drains before it
    // finishes, so a non-zero count here means that drain is not working —
    // which is the use-after-free of a page table that `crate::thread`'s
    // lifetime rule exists to prevent, seen before it can happen.
    t.check_eq("thread: no thread outlived the process", crate::thread::live_count(5) as u64, 0);

    finish_test_process(started.0, started.1);
    t.check_eq(
        "thread: teardown leaks nothing",
        akuma_pmm::free_count() as u64,
        free_before as u64,
    );
}
