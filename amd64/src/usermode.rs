//! Ring 3, and the `syscall`/`sysret` transition.
//!
//! Stage F. This is the first code in the amd64 port that runs *unprivileged*,
//! and the first use of `Prot::USER_RX` / `Prot::USER_RW` — which have existed
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

use akuma_selftest::Suite;

use crate::gdt;
use crate::loader::{self, FrameSet};
use crate::paging::{self, MemAttr, Prot};
use crate::phys::phys_ptr;
use crate::serial;
use core::sync::atomic::{AtomicU64, Ordering};

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
const USER_CODE_VA: usize = 0x40_0000;
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
const ELF_STACK_PAGES: usize = 128;

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
const HELLO_ELF: &[u8] = include_bytes!(env!("USER_HELLO_ELF"));

/// Where a task's kernel stack and saved user stack live.
///
/// One per task. `syscall_entry` reaches the running task's through
/// [`CURRENT_UCTX`]; the scheduler repoints that on every switch. They were two
/// globals until multitasking made that wrong — a syscall taken by one process
/// would have written another's saved stack pointer.
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
    /// The `PROCS` slot this task is running, or `usize::MAX` for a task that is
    /// not an ELF process (the boot task). Offset 24 — past everything the
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
        }
    }
}

/// The running task's [`UserCtx`]. Repointed by the scheduler on every switch.
#[unsafe(no_mangle)]
pub static mut CURRENT_UCTX: *mut UserCtx = core::ptr::null_mut();

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
static WRITE_SEQ: [AtomicU64; 16] = [const { AtomicU64::new(u64::MAX) }; 16];
static WRITE_SEQ_LEN: AtomicU64 = AtomicU64::new(0);

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
     * Every saved slot is per-task, reached through CURRENT_UCTX. With more
     * than one process, globals would be wrong twice over: a syscall by task A
     * would overwrite task B's saved user stack, and a context switch *inside*
     * a syscall would corrupt whichever kernel stack the two shared. */
    mov [rip + SYSCALL_SCRATCH], rax
    mov rax, [rip + CURRENT_UCTX]
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
    mov rax, [rip + SYSCALL_SCRATCH]

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

    sub rsp, 8                      /* System V wants rsp 16-aligned at `call` */

    /* Linux arg registers into System V positions:
     *   Linux:    nr=rax  a1=rdi  a2=rsi  a3=rdx  a4=r10  a5=r8   a6=r9
     *   System V: 1 =rdi  2 =rsi  3 =rdx  4 =rcx  5 =r8   6 =r9
     *
     * Six of the seven are passed on; a6 is dropped because no syscall this
     * kernel implements takes one (mmap's sixth is its offset, and only
     * file-backed mappings use it — this target has none).
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
    mov rcx, [rip + CURRENT_UCTX]
    cmp qword ptr [rcx + 16], 0     /* uctx.leave */
    jne .Lexit_to_kernel

    pop r11                         /* user rflags */
    pop rcx                         /* user rip    */
    /* rax holds the result and must survive the stack switch; rcx and r11 are
     * now live for sysretq, so the scratch slot is the only place left. */
    mov [rip + SYSCALL_SCRATCH], rax
    mov rax, [rip + CURRENT_UCTX]
    mov rsp, [rax + 8]              /* back to this task's user stack */
    mov rax, [rip + SYSCALL_SCRATCH]
    sysretq

.Lexit_to_kernel:
    /* Leave ring 3 for good. Restore what enter_user_mode saved and return from
     * it; rax still holds the handler's result. rcx already points at the
     * uctx. */
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

    mov rax, [rip + CURRENT_UCTX]
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

    mov rax, [rip + CURRENT_UCTX]  /* rax = this task's uctx, kept until the end */
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
    sysretq

    .section .bss
    .align 16
SYSCALL_SCRATCH:
    .skip 8
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
extern "C" fn syscall_handler(nr: u64, a1: u64, a2: u64, a3: u64, a4: u64, a5: u64) -> u64 {
    CALLS.fetch_add(1, Ordering::Relaxed);
    let r = syscall_dispatch(nr, a1, a2, a3, a4, a5);
    if SYSCALL_TRACE.load(Ordering::Relaxed) {
        serial::puts("[sc] nr=");
        serial::put_dec(nr);
        serial::puts(" -> 0x");
        serial::put_hex(r);
        serial::puts("\n");
    }
    r
}

fn syscall_dispatch(nr: u64, a1: u64, a2: u64, a3: u64, a4: u64, a5: u64) -> u64 {
    use crate::fd::errno;

    // Akuma's own syscalls, before the Linux table.
    //
    // They live at `0x1000 +` their AArch64 number (see `libakuma`'s
    // `AKUMA_PRIVATE_BASE`), far above any allocated Linux number. Checking this
    // range first is what keeps a shell's keystroke poll from dispatching into
    // whatever Linux happens to have at 313 — `finit_module`, as it turns out.
    const AKUMA_PRIVATE_BASE: u64 = 0x1000;
    if nr >= AKUMA_PRIVATE_BASE {
        return match nr - AKUMA_PRIVATE_BASE {
            // `spawn(path, argv, envp, stdin, stdin_len[, flags])` — the sixth
            // argument (the PTY flag) is dropped by `syscall_entry` and this
            // target ignores it anyway (a pipe has no line discipline).
            301 => sys_spawn(a1, a2, a3, a4, a5),
            313 => crate::fd::sys_poll_input_event(a1, a2, a3),
            303 => sys_waitpid(a1, a2, a3),
            326 => sys_close_child_stdin(a1),
            // `kill` is accepted as a no-op success: `sshd` sends SIGHUP/SIGTERM
            // to a session's shell on teardown, and there is nothing here to
            // deliver a signal to, but failing the call makes it log an error.
            302 => 0,
            _ => errno::ENOSYS,
        };
    }

    // Linux syscalls handled by raw x86_64 number rather than through the
    // cross-architecture `Syscall` enum — either because they are x86-only
    // (`arch_prctl`) or because their behaviour on a kernel with no users and no
    // signals is a one-liner that does not earn an enum variant and four match
    // arms. Where the AArch64 kernel does more (real signal masking, real
    // credentials), that is noted at the site.
    match nr {
        // x86_64 158: the TLS-base primitive. No aarch64 number.
        158 => return sys_arch_prctl(a1, a2),
        // Path-based `struct stat`. `stat` (4) and `lstat` (6) are x86-only —
        // `asm-generic` dropped them, so aarch64 has no number and they cannot
        // go through the `Syscall` enum; `newfstatat` (262) exists on both but
        // is grouped here with its siblings. `stat` follows a final symlink,
        // `lstat` does not (`AT_SYMLINK_NOFOLLOW` == 0x100) — on this target
        // that changes nothing (see `fd::sys_newfstatat`). `AT_FDCWD` is -100.
        // busybox `sh` stats every PATH entry before it will run an applet —
        // without this it saw `ENOSYS` and reported "Function not implemented"
        // for a working builtin.
        4 => return crate::fd::sys_newfstatat((-100i64) as u64, a1, a2, 0),
        6 => return crate::fd::sys_newfstatat((-100i64) as u64, a1, a2, 0x100),
        262 => return crate::fd::sys_newfstatat(a1, a2, a3, a4),
        // `open(path, flags, mode)` — x86_64 2. x86_64 musl issues this directly
        // (it only falls back to `openat` on architectures without `open`, like
        // aarch64), so `busybox cat` hit `ENOSYS` here until now. `openat`
        // ignores the dirfd for absolute paths and treats a relative one as
        // root-relative, which is what `AT_FDCWD` means on a target with no cwd.
        2 => return crate::fd::sys_openat((-100i64) as u64, a1, a2, a3),
        // `access`/`faccessat(dirfd, path, mode[, flags])` — existence only.
        // This target has one user (root) and no per-file exec tracking worth
        // trusting, so "the path resolves" is the honest answer; a real
        // permission check would be a guess.
        21 => return crate::fd::sys_access(a1),
        269 => return crate::fd::sys_access(a2),
        // `poll(fds, nfds, timeout_ms)` — x86_64 7. An interactive `busybox sh`
        // polls its stdin on every keystroke; `ENOSYS` here was a forever-loop
        // of "sh: poll: Function not implemented".
        7 => return crate::fd::sys_poll(a1, a2, a3),
        // `ppoll(fds, nfds, *timespec, sigmask, sigsetsize)` — x86_64 271. Same
        // core; a NULL timespec means wait forever, otherwise fold sec+nsec to
        // milliseconds (this target has no finer clock to honour anyway).
        271 => {
            let timeout_ms = if a3 == 0 {
                (-1i64) as u64
            } else {
                // SAFETY: user `struct timespec` { i64 tv_sec, i64 tv_nsec }.
                let (sec, nsec) = unsafe {
                    ((a3 as *const i64).read_volatile(), (a3 as *const i64).add(1).read_volatile())
                };
                (sec.max(0) as u64)
                    .saturating_mul(1000)
                    .saturating_add((nsec.max(0) as u64) / 1_000_000)
            };
            return crate::fd::sys_poll(a1, a2, timeout_ms);
        }
        // `execve(path, argv, envp)` — x86_64 59: the current (spawned or
        // forked) task replaces its own image in place. See `sys_execve`.
        59 => return sys_execve(a1, a2, a3),
        // `fork` (57) / `vfork` (58) — a real eager-copy fork; see `sys_fork`
        // (`vfork` gets the same, its "don't touch the parent" contract is moot
        // once the address space is copied). `clone` (56) is a fork only when
        // `CLONE_VM` is clear; with it set it means threads, not done here.
        57 | 58 => return sys_fork(),
        56 => {
            const CLONE_VM: u64 = 0x0000_0100;
            if a1 & CLONE_VM != 0 {
                return errno::ENOSYS;
            }
            return sys_fork();
        }
        // `wait4(pid, wstatus, options, rusage)` — x86_64 61. Route into the
        // Akuma-private `waitpid` table, but **block** (unless `WNOHANG`): a
        // forked shell calls `wait4(pid, &st, 0, 0)` expecting to sleep until
        // the child is done, where `sys_waitpid` alone just returns 0.
        61 => {
            const WNOHANG: u64 = 0x0000_0001;
            loop {
                // 0 = a matching child exists but has not exited; anything else
                // is a reaped pid or `-ESRCH`.
                let r = sys_waitpid(a1, a2, a3);
                if r != 0 || a3 & WNOHANG != 0 {
                    return r;
                }
                crate::sched::yield_now();
            }
        }
        // uname(2). Same static-.rodata answer the aarch64 kernel gives, machine
        // string aside — see `akuma_syscalls_glue::proc::sys_uname`.
        63 => return sys_uname(a1),
        // Credentials. One user, uid 0 — the same answer `src/syscall` gives.
        102 | 104 | 107 | 108 => return 0, // get{uid,gid,euid,egid}
        105 | 106 => return 0,             // set{uid,gid}: already root, accept
        // Signals: this kernel has none, so "the mask is empty and stays empty"
        // is the correct result, not a stub. `rt_sigprocmask` writes the old
        // (empty) set back if asked.
        13 => return 0, // rt_sigaction
        14 => {
            if a3 != 0 {
                let n = (a4 as usize).min(8);
                // SAFETY: a user pointer to a sigset_t, bounded by sigsetsize.
                unsafe { core::ptr::write_bytes(a3 as *mut u8, 0, n) };
            }
            return 0;
        }
        // Best-effort robustness/rlimit hooks musl pokes on startup.
        273 => return 0,          // set_robust_list
        302 => return 0,          // prlimit64
        // Nothing here is a symlink.
        89 | 267 => return errno::EINVAL, // readlink / readlinkat
        // Process-group / session ids. One process, so it is its own group and
        // session leader; `setpgid`/`setsid` accept and report id 1.
        110 => return 1,                  // getppid
        111 | 121 | 124 => return 1,      // getpgrp / getpgid / getsid
        109 | 112 => return 0,            // setpgid / setsid
        // `getcwd(buf, size)` — this target has no per-process cwd; it is always
        // root. Linux returns the length *including* the NUL.
        79 => {
            if a1 == 0 || a2 < 2 {
                return errno::EINVAL;
            }
            // SAFETY: a user buffer of at least `a2` bytes, `a2 >= 2` checked.
            unsafe {
                (a1 as *mut u8).write_volatile(b'/');
                (a1 as *mut u8).add(1).write_volatile(0);
            }
            return 2;
        }
        // `mprotect` — W^X is enforced at map time and there is no region table
        // to re-permission against, so this accepts and does nothing. A caller
        // asking for *more* access than it has still sees the original (never
        // less permissive) mapping; one asking for less is not honoured. Real
        // `mprotect` for spawned processes needs the per-space region table the
        // loader also wants (§3.18.8 / §3.24.5).
        10 => return 0,
        _ => {}
    }

    let Some(call) = Syscall::from_x86_64(nr) else {
        return errno::ENOSYS;
    };

    match call {
        Syscall::Write => sys_write(a1, a2, a3),
        Syscall::Read => crate::fd::sys_read(a1, a2, a3),
        // busybox prints through `writev`, not `write`. Walk the iovec array and
        // forward each segment; a short write on any segment stops the walk, as
        // `writev(2)` specifies.
        Syscall::Writev => sys_writev(a1, a2, a3),
        Syscall::Readv => sys_readv(a1, a2, a3),
        Syscall::Openat => crate::fd::sys_openat(a1, a2, a3, a4),
        Syscall::Close => crate::fd::sys_close(a1),
        Syscall::Lseek => crate::fd::sys_lseek(a1, a2, a3),
        Syscall::Fstat => crate::fd::sys_fstat(a1, a2),
        Syscall::Ioctl => crate::fd::sys_ioctl(a1, a2, a3),
        // `getdents64(fd, dirp, count)` — x86_64 217. `ls`/`find`.
        Syscall::Getdents64 => crate::fd::sys_getdents64(a1, a2, a3),
        // `a5` is mmap's fd and is deliberately unused: only anonymous mappings
        // are supported, so a file-backed request must fail rather than quietly
        // return zeroed memory that the caller believes holds a file.
        Syscall::Mmap => crate::mm::sys_mmap(a1, a2, a3, a4, a5),
        Syscall::Munmap => crate::mm::sys_munmap(a1, a2),
        Syscall::Socket => crate::sock::sys_socket(a1, a2, a3),
        Syscall::Bind => crate::sock::sys_bind(a1, a2, a3),
        Syscall::Listen => crate::sock::sys_listen(a1, a2),
        Syscall::Accept => crate::sock::sys_accept(a1, a2, a3),
        Syscall::Connect => crate::sock::sys_connect(a1, a2, a3),
        Syscall::Sendto => crate::sock::sys_sendto(a1, a2, a3),
        Syscall::Recvfrom => crate::sock::sys_recvfrom(a1, a2, a3),
        Syscall::Setsockopt => crate::sock::sys_setsockopt(a1, a2, a3, a4, a5),
        Syscall::Exit | Syscall::ExitGroup => {
            EXIT_STATUS.store(a1, Ordering::Relaxed);
            // SAFETY: single core, interrupts off inside a syscall. The
            // running task's context is what `syscall_entry` will read on the
            // way out, and only this task can be inside a syscall.
            unsafe {
                let cur = &raw const CURRENT_UCTX;
                let uctx = *cur;
                if !uctx.is_null() {
                    (*uctx).leave = 1;
                }
            }
            a1
        }
        Syscall::Getpid => 1,
        Syscall::Fcntl => crate::fd::sys_fcntl(a1, a2, a3),
        Syscall::Getrandom => sys_getrandom(a1, a2),
        // No high-resolution sleep: this target has a coarse, uncalibrated
        // clock and a cooperative scheduler. Yield instead — `sshd`'s serve
        // loop only calls this to avoid a busy-spin when it did no work, and a
        // yield is exactly that with the preemption timer running.
        Syscall::Nanosleep => {
            crate::sched::yield_now();
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
        // SAFETY: user pointers to an iovec array; bad ones fault reportably.
        let (base, len) = unsafe {
            (
                (e as *const u64).read_volatile(),
                (e as *const u64).add(1).read_volatile(),
            )
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
        // SAFETY: as `sys_writev`.
        let (base, len) = unsafe {
            (
                (e as *const u64).read_volatile(),
                (e as *const u64).add(1).read_volatile(),
            )
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

/// `getrandom(buf, buflen, flags)` — bytes from `RDRAND` (or the loud
/// non-cryptographic fallback on a CPU without it; see `net::rng_fill`).
///
/// `flags` is ignored: `GRND_NONBLOCK` never applies because `RDRAND` does not
/// block, and `GRND_RANDOM` vs the urandom pool is a distinction this source
/// does not have. Bounded per call — `sshd` asks for 32 at a time.
/// `struct utsname`: six 65-byte NUL-padded fields, 390 bytes. The ABI's shape,
/// not this kernel's — Linux copies the same 390 bytes out of `init_uts_ns`.
const UTS_FIELD: usize = 65;
const UTS_LEN: usize = UTS_FIELD * 6;

const fn uts_set(mut b: [u8; UTS_LEN], field: usize, v: &[u8]) -> [u8; UTS_LEN] {
    let start = field * UTS_FIELD;
    let max = UTS_FIELD - 1;
    let n = if v.len() < max { v.len() } else { max };
    let mut i = 0;
    while i < n {
        b[start + i] = v[i];
        i += 1;
    }
    b
}

/// The answer `uname(2)` gives, assembled once in `.rodata`. `machine` is
/// `x86_64` here where the AArch64 kernel says `aarch64` — the one field that
/// actually differs between the two.
static UTSNAME: [u8; UTS_LEN] = {
    let b = [0u8; UTS_LEN];
    let b = uts_set(b, 0, b"Akuma"); // sysname
    let b = uts_set(b, 1, b"akuma"); // nodename
    let b = uts_set(b, 2, b"0.1.0-amd64"); // release
    let b = uts_set(b, 3, b"Akuma/amd64 (x86_64 bring-up)"); // version
    let b = uts_set(b, 4, b"x86_64"); // machine
    uts_set(b, 5, b"(none)") // domainname
};

fn sys_uname(buf: u64) -> u64 {
    if buf == 0 {
        return crate::fd::errno::EFAULT;
    }
    // SAFETY: a user pointer to a `struct utsname`; `CR4.SMAP` is off and a bad
    // pointer faults reportably.
    unsafe {
        core::ptr::copy_nonoverlapping(UTSNAME.as_ptr(), buf as *mut u8, UTS_LEN);
    }
    0
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
    const IA32_GS_BASE: u32 = 0xC000_0101;
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
                let cur = &raw const CURRENT_UCTX;
                let uctx = *cur;
                if !uctx.is_null() {
                    (*uctx).fs_base = addr;
                }
            }
            0
        }
        ARCH_SET_GS => {
            // SAFETY: as above for GS. Nothing in this kernel uses GS base.
            unsafe { wrmsr(IA32_GS_BASE, addr) };
            0
        }
        ARCH_GET_FS => {
            // SAFETY: a user pointer to a u64, per the ABI.
            unsafe { (addr as *mut u64).write_volatile(rdmsr(IA32_FS_BASE)) };
            0
        }
        ARCH_GET_GS => {
            // SAFETY: as above.
            unsafe { (addr as *mut u64).write_volatile(rdmsr(IA32_GS_BASE)) };
            0
        }
        _ => crate::fd::errno::EINVAL,
    }
}

fn sys_getrandom(buf: u64, len: u64) -> u64 {
    use crate::fd::errno;
    if buf == 0 {
        return errno::EFAULT;
    }
    let n = (len as usize).min(256);
    let mut tmp = [0u8; 256];
    crate::net::rng_fill(&mut tmp[..n]);
    crate::fd::copy_out(buf, &tmp[..n]) as u64
}

/// `write(fd, buf, len)` — fd 1 and 2 go to the serial console.
///
/// Kept here rather than moved into `fd` with its siblings because of the
/// `WRITE_SEQ` bookkeeping below: the multitasking and preemption tests prove
/// interleaving by recording *which task* performed each write, and that
/// instrumentation belongs next to the tests that read it.
///
/// Reading `buf` from ring 0 works because `CR4.SMAP` is not enabled; with SMAP
/// on this would need `stac`/`clac` around the access. That is a real gap and
/// not a hypothetical — see the module note.
fn sys_write(fd: u64, buf: u64, len: u64) -> u64 {
    const EBADF: u64 = (-9i64) as u64;
    const EFAULT: u64 = (-14i64) as u64;

    // A socket descriptor routes to the network stack, so a program written
    // against `write(2)` works on a connection without knowing it has one.
    if let Some(sock) = crate::fd::socket_index(fd) {
        return crate::sock::send(sock, buf, len, crate::fd::is_nonblocking(fd));
    }
    // An explicit pipe write end (a parent feeding a child's stdin).
    if let Some(p) = crate::fd::pipe_write_id(fd) {
        return crate::fd::write_pipe(p, buf, len as usize, crate::fd::is_nonblocking(fd));
    }
    if fd != 1 && fd != 2 {
        return EBADF;
    }
    // A spawned child's stdout/stderr is a pipe `sshd` drains, not the console.
    if let Some(p) = current_stdout_pipe() {
        return crate::fd::write_pipe(p, buf, len as usize, false);
    }
    if len > MAX_WRITE {
        return EFAULT;
    }
    for i in 0..len {
        // SAFETY: `buf` is a user pointer and this is the one place the kernel
        // dereferences one. It is bounded by MAX_WRITE above, and a fault here
        // would land in the #PF handler rather than silently corrupting — which
        // is the honest limit of what this can promise without a
        // copy_from_user that validates the range against the page tables.
        let byte = unsafe { (buf as *const u8).add(i as usize).read_volatile() };
        serial::putb(byte);
    }
    let n = WRITE_SEQ_LEN.fetch_add(1, Ordering::Relaxed) as usize;
    if let Some(slot) = WRITE_SEQ.get(n) {
        slot.store(crate::sched::current_task() as u64, Ordering::Relaxed);
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
        wrmsr(IA32_FMASK, (1 << 9) | (1 << 10));
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

/// A process: its own address space, the frames backing it, and where to start.
///
/// `entry` and `stack_top` are fields rather than the two module constants they
/// used to be. That was fine while every process was the same hand-assembled
/// blob at the same address; an ELF decides its own entry point, and its stack
/// has to go somewhere the image does not already occupy.
pub struct Process {
    space: paging::AddressSpace,
    /// Every leaf frame this process owns. `AddressSpace::free` releases page
    /// tables, not the pages they point at — this is the other half.
    frames: FrameSet,
    /// Where ring 3 starts executing.
    entry: u64,
    /// Initial `rsp`.
    stack: u64,
    /// Resume this process's **first** ring-3 entry with the parent's full
    /// register set (`enter_user_mode_forked`) rather than a fresh `_start`.
    ///
    /// `true` for a `fork` child: it is a register- and memory-complete copy of
    /// the parent and must continue from the parent's post-`fork` instruction.
    /// `execve` replaces it with a plain `Process`, so only that first entry
    /// takes the forked path.
    forked: bool,
}

impl Process {
    /// Build a process that prints `msg` `rounds` times then exits with
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
    fn new(msg: &[u8], rounds: u32, delay: u32, status: u32) -> Option<Self> {
        let space = paging::AddressSpace::new()?;
        let mut frames = FrameSet::new();

        let (Some(code), Some(stack)) = (akuma_pmm::alloc_page(), akuma_pmm::alloc_page()) else {
            space.free();
            return None;
        };
        // Recorded before anything can fail: a frame the set does not know
        // about is a frame that leaks.
        if !frames.push(code) || !frames.push(stack) {
            akuma_pmm::free_page(code, 0);
            akuma_pmm::free_page(stack, 0);
            space.free();
            return None;
        }

        // SAFETY: PMM frames are reachable through the physmap, so the program
        // can be staged *before* the address space that will hold it is ever
        // activated.
        unsafe {
            core::ptr::write_bytes(phys_ptr::<u8>(code as u64), 0, 4096);
            core::ptr::write_bytes(phys_ptr::<u8>(stack as u64), 0, 4096);
            let page = core::slice::from_raw_parts_mut(phys_ptr::<u8>(code as u64), 4096);
            build_user_program(page, USER_CODE_VA as u64, msg, rounds, delay, status);
        }

        if !space.map(USER_CODE_VA, code as u64, Prot::USER_RX, MemAttr::WriteBack)
            || !space.map(USER_STACK_VA, stack as u64, Prot::USER_RW, MemAttr::WriteBack)
        {
            frames.free_all();
            space.free();
            return None;
        }
        Some(Self {
            space,
            frames,
            entry: USER_CODE_VA as u64,
            stack: (USER_STACK_VA + 4096 - 16) as u64,
            forked: false,
        })
    }

    /// Build a process from a linked ELF image.
    ///
    /// The failure path frees the frames the loader recorded before it gave up —
    /// which is why [`loader::load`] records them as it goes and frees nothing
    /// itself. A half-loaded image whose frames the loader had reclaimed would
    /// leave this space's page tables pointing at memory the PMM has since
    /// handed to someone else.
    /// Returns the process and what the loader found, so a caller can check the
    /// placement as well as the outcome.
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
        let space = paging::AddressSpace::new().ok_or("no frame for a PML4")?;
        let mut frames = FrameSet::new();

        let built = loader::load(image, &space, &mut frames).and_then(|img| {
            loader::build_stack(
                &space,
                &mut frames,
                ELF_STACK_TOP,
                ELF_STACK_PAGES,
                argv,
                envp,
                img.entry,
            )
            .map(|rsp| (img, rsp))
        });

        match built {
            Ok((img, stack)) => {
                let entry = img.entry;
                Ok((Self { space, frames, entry, stack, forked: false }, img))
            }
            Err(e) => {
                frames.free_all();
                space.free();
                Err(e)
            }
        }
    }

    fn free(mut self) {
        // A `fork` child's `FrameSet` already holds *every* mapped page,
        // anonymous ones included (`fork_from` copied them). A loader-built
        // process's does not — its `mmap`/heap frames are untracked, so walk
        // the mmap window and free them before the tables go. Running that walk
        // for a `fork` child would double-free.
        if !self.forked {
            crate::mm::release_anon_frames(&self.space);
        }
        self.frames.free_all();
        self.space.free();
    }

    /// A `fork` child: a full eager copy of `parent`'s address space (every user
    /// page in fresh frames — no CoW on this target), resuming at `entry`/`stack`
    /// (the parent's post-`fork` RIP/RSP) as a register-complete copy.
    ///
    /// `None` if a frame runs out mid-copy or the child would need more frames
    /// than `MAX_PROC_FRAMES` — the shell then sees `fork` fail with `ENOMEM`,
    /// which is a survivable "can't fork" rather than a corrupt child.
    fn fork_from(parent: &Self, entry: u64, stack: u64) -> Option<Self> {
        let space = paging::AddressSpace::new()?;
        let mut frames = FrameSet::new();
        let mut ok = true;

        paging::for_each_user_leaf(parent.space.root(), |va, pa, prot| {
            if !ok {
                return;
            }
            let Some(frame) = akuma_pmm::alloc_page() else {
                ok = false;
                return;
            };
            // SAFETY: both frames are live and reached through the physmap;
            // exactly one page is copied.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    phys_ptr::<u8>(pa),
                    phys_ptr::<u8>(frame as u64),
                    4096,
                );
            }
            if !frames.push(frame) || !space.map(va, frame as u64, prot, MemAttr::WriteBack) {
                akuma_pmm::free_page(frame, 0);
                ok = false;
            }
        });

        if !ok {
            frames.free_all();
            space.free();
            return None;
        }
        Some(Self { space, frames, entry, stack, forked: true })
    }
}

/// The processes the tests run, reachable from their task entry points.
///
/// `extern "C" fn() -> !` takes no arguments, so a task entry cannot be handed
/// its process. A slot per process is the smallest thing that works on one core.
///
/// Slots 0..=6 are the self-tests and `run_init`; slots [`SPAWN_SLOT_BASE`]..
/// are for `sys_spawn`'d children (an `sshd` session's shell). 16 total leaves
/// nine concurrent spawns, which is past what the cooperative `sshd` build
/// serves.
static mut PROCS: [Option<Process>; 16] = [const { None }; 16];

/// First `PROCS` slot `sys_spawn` may use; everything below is the self-tests
/// and `run_init`.
pub const SPAWN_SLOT_BASE: usize = 7;

/// A fully-built replacement image parked by [`sys_execve`], keyed by `PROCS`
/// slot. `run_process` picks it up after `enter_user_mode` returns and does the
/// address-space swap on the kernel stack — outside the syscall asm, where a
/// `mov cr3` and a frame free belong.
static mut PENDING_EXEC: [Option<Process>; 16] = [const { None }; 16];

/// Take the replacement image for `idx`, if `execve` parked one.
fn take_pending_exec(idx: usize) -> Option<Process> {
    // SAFETY: raw-pointer access; single core, and only this task's own
    // `run_process` reads its slot.
    unsafe {
        let pending = &raw mut PENDING_EXEC;
        (*pending)[idx].take()
    }
}

/// Enter ring 3 for process `idx`, then mark the task finished.
///
/// The scheduler has already installed this task's address space by the time
/// this runs — `spawn_in_space` recorded the root, and `yield_now` writes `CR3`
/// before switching stacks.
///
/// The entry point and stack come from the slot rather than from module
/// constants. They were constants while every process was the same blob at the
/// same address; an ELF's entry is `e_entry` and its stack is wherever the
/// loader could put one.
fn run_process(idx: usize) -> ! {
    // SAFETY: single core; each slot is written once before its task is spawned
    // and read only by that task.
    let start = unsafe {
        let procs = &raw const PROCS;
        (*procs)[idx]
            .as_ref()
            .map(|p| (p.entry, p.stack, p.forked))
    };
    if let Some((entry, stack, forked)) = start {
        // A `fork` child's first entry re-enters ring 3 at the parent's
        // post-`fork` instruction with the parent's full register set; the
        // `execve` it usually does next swaps in a plain image, and every later
        // loop iteration uses the ordinary entry path.
        let mut forked_child = forked;
        // Tell the syscall path which process this is, so fd 0/1/2 route to
        // this task's pipes (if it is a spawned child) rather than the console.
        // SAFETY: single core; `CURRENT_UCTX` points at this task's own slot.
        unsafe {
            let cur = &raw const CURRENT_UCTX;
            let uctx = *cur;
            if !uctx.is_null() {
                (*uctx).proc_slot = idx;
            }
        }
        // SAFETY: both are addresses the loader (or `Process::new`) mapped
        // user-accessible in the address space the scheduler installed for this
        // task, and every program this kernel runs ends in exit_group.
        //
        // The loop is `execve`: when the last syscall parked a replacement
        // image, swap it into this slot, switch `CR3`, free the old image, and
        // re-enter ring 3 at the new entry — the same task, a new program.
        let (mut entry, mut stack) = (entry, stack);
        let status = loop {
            let status = if forked_child {
                forked_child = false;
                unsafe { enter_user_mode_forked(entry, stack) }
            } else {
                unsafe { enter_user_mode(entry, stack, 0) }
            };
            let Some(next) = take_pending_exec(idx) else {
                break status;
            };
            let new_root = next.space.root();
            (entry, stack) = (next.entry, next.stack);
            // SAFETY: raw-pointer access; single core. Replace before the CR3
            // switch so the slot always names the live image.
            let old = unsafe {
                let procs = &raw mut PROCS;
                (*procs)[idx].replace(next)
            };
            crate::sched::set_current_space_root(new_root);
            // The old image (the `fork` copy, or a previous `execve`'s) is
            // unreferenced now that CR3 points at the new space — hand it back.
            if let Some(old) = old {
                old.free();
            }
        };
        EXIT_STATUS.store(status, Ordering::Relaxed);
        if idx >= SPAWN_SLOT_BASE {
            spawn_record_exit(idx, status as i32);
        }
    }
    crate::sched::finish();
}

macro_rules! proc_entries {
    ($($name:ident => $idx:literal),* $(,)?) => {
        $(extern "C" fn $name() -> ! { run_process($idx); })*
    };
}
proc_entries! {
    proc0_entry => 0, proc1_entry => 1, proc2_entry => 2, proc3_entry => 3,
    proc4_entry => 4, proc5_entry => 5, proc6_entry => 6, proc7_entry => 7,
    proc8_entry => 8, proc9_entry => 9, proc10_entry => 10, proc11_entry => 11,
    proc12_entry => 12, proc13_entry => 13, proc14_entry => 14, proc15_entry => 15,
}

/// The task entry function for `PROCS` slot `idx`. `sys_spawn` needs to hand
/// `sched::spawn_in_space` a plain `fn` pointer and the slot is baked into each.
pub fn proc_entry_for(idx: usize) -> Option<extern "C" fn() -> !> {
    Some(match idx {
        7 => proc7_entry,
        8 => proc8_entry,
        9 => proc9_entry,
        10 => proc10_entry,
        11 => proc11_entry,
        12 => proc12_entry,
        13 => proc13_entry,
        14 => proc14_entry,
        15 => proc15_entry,
        _ => return None,
    })
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

/// One spawned child. Indexed by `proc_slot - SPAWN_SLOT_BASE`.
struct Spawn {
    pid: u32,
    /// The child writes fd 1/2 here; the parent's `stdout_fd` reads it.
    stdout_pipe: PipeId,
    /// The child reads fd 0 here; `/proc/<pid>/fd/0` writes it.
    stdin_pipe: PipeId,
    /// `Some` once the child has left ring 3. `waitpid` consumes it and frees
    /// the slot and both pipes.
    exit: Option<i32>,
    /// The pipes above belong to another `Spawn` (the `fork` parent's): this
    /// child shares its stdio and must not close or free them on teardown.
    borrowed_io: bool,
    /// fd 0/1/2 for this child are the **console**, not the pipes above — a
    /// `fork` child of a shell that itself runs on the console (`INIT=/bin/sh`
    /// on the serial line, no `sshd` in front). The `stdin_pipe`/`stdout_pipe`
    /// fields are unused then.
    console_io: bool,
}

const SPAWN_SLOTS: usize = 16 - SPAWN_SLOT_BASE;

/// The spawn table. `static mut` reached through raw pointers on one core, same
/// discipline as `PROCS`: the only writers run inside a syscall (non-preemptible
/// on this target) and none of them yield while touching it.
static mut SPAWN: [Option<Spawn>; SPAWN_SLOTS] = [const { None }; SPAWN_SLOTS];

/// Next pid to hand out. `sshd` itself is pid 1 (`Getpid` returns 1), so
/// children start at 2.
static NEXT_PID: AtomicU64 = AtomicU64::new(2);

fn spawn_table() -> *mut [Option<Spawn>; SPAWN_SLOTS] {
    &raw mut SPAWN
}

/// Read a NUL-terminated string from user memory, bounded.
fn user_cstr(ptr: u64, max: usize) -> Option<alloc::vec::Vec<u8>> {
    if ptr == 0 {
        return None;
    }
    let mut out = alloc::vec::Vec::new();
    for i in 0..max {
        // SAFETY: the same user-pointer contract as every other access on this
        // target — `CR4.SMAP` is off, and a bad pointer faults reportably.
        let b = unsafe { (ptr as *const u8).add(i).read_volatile() };
        if b == 0 {
            return Some(out);
        }
        out.push(b);
    }
    None
}

/// Parse a NULL-terminated array of C-string pointers into owned bytes.
fn user_argv(ptr: u64) -> alloc::vec::Vec<alloc::vec::Vec<u8>> {
    let mut argv = alloc::vec::Vec::new();
    if ptr == 0 {
        return argv;
    }
    for i in 0..loader::MAX_ARGV {
        // SAFETY: as `user_cstr`; the array is the caller's and NULL-terminated.
        let p = unsafe { (ptr as *const u64).add(i).read_volatile() };
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
        // SAFETY: as `user_cstr`; the array is the caller's and NULL-terminated.
        let p = unsafe { (ptr as *const u64).add(i).read_volatile() };
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
/// its `PROCS` slot, its pipes and its pid; only the image behind it changes.
///
/// The build happens here, on the syscall stack; the *swap* (replace the slot,
/// `mov cr3`, free the old image) happens in [`run_process`] after this returns
/// through the `leave` path, because a page-table switch and a frame free do not
/// belong inside the syscall asm. On success this does not really "return" — it
/// sets `leave` and the next thing the task does is re-enter ring 3 at the new
/// entry. On failure it returns a negative errno and the caller runs on.
fn sys_execve(path_ptr: u64, argv_ptr: u64, envp_ptr: u64) -> u64 {
    use crate::fd::errno;

    let slot = current_proc_slot();
    if slot == usize::MAX || slot >= 16 {
        // Not a slotted user task — nothing to replace.
        return errno::ENOSYS;
    }

    let Some(path_bytes) = user_cstr(path_ptr, 256) else {
        return errno::EFAULT;
    };
    let Ok(path) = core::str::from_utf8(&path_bytes) else {
        return errno::EINVAL;
    };
    let Some(image) = crate::fs::read_file(path) else {
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

    let (proc, _img) = match Process::from_elf_argv_envp(&image, &argv_refs, &envp_refs) {
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

    // Park the built image and ask the entry path to leave ring 3. `run_process`
    // does the swap.
    // SAFETY: raw-pointer access; single core, slot is this task's own.
    unsafe {
        let pending = &raw mut PENDING_EXEC;
        (*pending)[slot] = Some(proc);
    }
    // SAFETY: single core, interrupts off inside a syscall; `CURRENT_UCTX` is
    // this task's slot. Same `leave` mechanism `exit` uses.
    unsafe {
        let cur = &raw const CURRENT_UCTX;
        let uctx = *cur;
        if !uctx.is_null() {
            (*uctx).leave = 1;
        }
    }
    0
}

/// `fork` (57) / `vfork` (58) / plain `clone(SIGCHLD, 0)` (56).
///
/// A real fork: the child gets an **eager full copy** of the parent's address
/// space (every user page in fresh frames — there is no CoW on this target),
/// resumes at the parent's post-`fork` instruction as a register- and TLS-
/// complete copy, and runs as its own scheduler task. The parent is **not**
/// suspended — it gets the child pid back immediately and both run; a shell
/// blocks on the child itself, in `wait4`.
///
/// This is what an interactive `busybox sh` needs for every external command
/// (`fork(); if (child) execvp(...)`) and it is `uname -a` at a shell prompt
/// that it unblocks. The copy is thrown away microseconds later by the child's
/// `execve`, which is wasteful but correct — CoW is the optimisation, not the
/// semantics.
///
/// The rough edges are `MAX_PROC_FRAMES`: a `fork` needs one frame per mapped
/// user page, so a large program near that ceiling (busybox is ~400 pages) can
/// make `fork` fail with `ENOMEM` — the shell reports `can't fork` and carries
/// on. A two-child pipeline needs both copies live at once.
///
/// Returns the child pid in the parent; the child never returns from here.
fn sys_fork() -> u64 {
    use crate::fd::errno;

    let parent_slot = current_proc_slot();
    if parent_slot == usize::MAX || parent_slot >= 16 {
        return errno::ENOSYS;
    }

    // The point the child resumes from — the parent's own user RIP/RSP as
    // captured on the way into this syscall — plus its TLS base and register
    // snapshot.
    // SAFETY: raw-pointer read; single core. `CURRENT_UCTX` is this task's.
    let (user_rip, user_rsp, parent_fs_base, parent_regs) = unsafe {
        let cur = &raw const CURRENT_UCTX;
        let uctx = *cur;
        if uctx.is_null() {
            (0, 0, 0, [0u64; 12])
        } else {
            (
                (*uctx).user_rip,
                (*uctx).user_rsp,
                (*uctx).fs_base,
                (*uctx).saved_regs,
            )
        }
    };
    if user_rip == 0 || user_rsp == 0 {
        return errno::ENOSYS;
    }

    // A free child slot (`PROCS` and `SPAWN` share the index).
    // SAFETY: raw-pointer read; single core.
    let slot = unsafe {
        let procs = &raw const PROCS;
        let spawn = spawn_table();
        (SPAWN_SLOT_BASE..16)
            .find(|&s| (*procs)[s].is_none() && (*spawn)[s - SPAWN_SLOT_BASE].is_none())
    };
    let Some(slot) = slot else {
        return errno::ENOMEM;
    };

    // The child's fd 0/1/2 route wherever the parent's do.
    let (stdin_pipe, stdout_pipe, console_io) = match spawn_stdio(parent_slot) {
        Some((si, so)) => (si, so, false),
        None => (0, 0, true),
    };

    // Copy the parent's whole address space.
    let child = {
        // SAFETY: raw-pointer read; single core. The parent slot is occupied
        // (this task is running it).
        let maybe = unsafe {
            let procs = &raw const PROCS;
            (*procs)[parent_slot]
                .as_ref()
                .and_then(|parent| Process::fork_from(parent, user_rip, user_rsp))
        };
        match maybe {
            Some(c) => c,
            None => return errno::ENOMEM,
        }
    };
    let child_root = child.space.root();
    // SAFETY: raw-pointer write; single core, slot just found free.
    unsafe {
        let procs = &raw mut PROCS;
        (*procs)[slot] = Some(child);
    }

    let Some(entry_fn) = proc_entry_for(slot) else {
        take_proc_slot(slot);
        return errno::ENOMEM;
    };
    let Some(task_slot) = crate::sched::spawn_in_space(entry_fn, child_root) else {
        take_proc_slot(slot);
        return errno::ENOMEM;
    };
    crate::sched::seed_forked_task(task_slot, parent_fs_base, &parent_regs);

    let pid = NEXT_PID.fetch_add(1, Ordering::Relaxed) as u32;
    // SAFETY: raw-pointer write; single core.
    unsafe {
        (*spawn_table())[slot - SPAWN_SLOT_BASE] = Some(Spawn {
            pid,
            stdout_pipe,
            stdin_pipe,
            exit: None,
            borrowed_io: true,
            console_io,
        });
    }

    u64::from(pid)
}

/// Drop a `PROCS` slot on a `fork`/`spawn` bail-out. The child task was never
/// scheduled, so nothing else touches it.
fn take_proc_slot(slot: usize) {
    // SAFETY: raw-pointer access; single core.
    unsafe {
        let procs = &raw mut PROCS;
        if let Some(p) = (*procs)[slot].take() {
            p.free();
        }
    }
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

    let Some(image) = crate::fs::read_file(path) else {
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

    // A free PROCS slot in the spawn range.
    let slot = {
        // SAFETY: raw-pointer read; single core.
        let procs = unsafe {
            let p = &raw const PROCS;
            &*p
        };
        (SPAWN_SLOT_BASE..16).find(|&s| procs[s].is_none())
    };
    let Some(slot) = slot else {
        return errno::ENOMEM;
    };

    let (proc, _img) = match Process::from_elf_argv(&image, &argv_refs) {
        Ok(p) => p,
        Err(e) => {
            serial::puts("  [spawn] load failed: ");
            serial::puts(e);
            serial::puts("\n");
            return errno::ENOMEM;
        }
    };

    let (Some(stdout_pipe), Some(stdin_pipe)) = (pipe::alloc(), pipe::alloc()) else {
        proc.free();
        return errno::ENOMEM;
    };

    // Seed the child's stdin, if the caller supplied any (`spawn_with_stdin`).
    if stdin_ptr != 0 && stdin_len != 0 {
        let seed = crate::fd::copy_in(stdin_ptr, stdin_len.min(64 * 1024));
        pipe::write(stdin_pipe, &seed);
    }

    let root = proc.space.root();
    // SAFETY: raw-pointer write; single core, slot was just found free.
    unsafe {
        let procs = &raw mut PROCS;
        (*procs)[slot] = Some(proc);
    }

    let Some(entry) = proc_entry_for(slot) else {
        cleanup_spawn_slot(slot, stdout_pipe, stdin_pipe);
        return errno::ENOMEM;
    };
    if crate::sched::spawn_in_space(entry, root).is_none() {
        cleanup_spawn_slot(slot, stdout_pipe, stdin_pipe);
        return errno::ENOMEM;
    }

    let pid = NEXT_PID.fetch_add(1, Ordering::Relaxed) as u32;
    // SAFETY: raw-pointer write; single core.
    unsafe {
        (*spawn_table())[slot - SPAWN_SLOT_BASE] = Some(Spawn {
            pid,
            stdout_pipe,
            stdin_pipe,
            exit: None,
            borrowed_io: false,
            console_io: false,
        });
    }

    let stdout_fd = crate::fd::alloc_pipe_fd(stdout_pipe, false);
    let Some(stdout_fd) = stdout_fd else {
        // The child is already running; it will just write into a pipe nobody
        // reads. Report the failure — `sshd` drops the session.
        return errno::EMFILE;
    };

    u64::from(pid) | (stdout_fd << 32)
}

fn cleanup_spawn_slot(slot: usize, stdout_pipe: PipeId, stdin_pipe: PipeId) {
    pipe::free(stdout_pipe);
    pipe::free(stdin_pipe);
    // SAFETY: raw-pointer access; single core. The task was never spawned (or
    // failed to), so nothing else touches this slot.
    unsafe {
        let procs = &raw mut PROCS;
        if let Some(p) = (*procs)[slot].take() {
            p.free();
        }
    }
}

/// Called from `run_process` when a spawned child leaves ring 3.
pub fn spawn_record_exit(proc_slot: usize, status: i32) {
    // SAFETY: raw-pointer access; single core.
    unsafe {
        if let Some(Some(s)) = (*spawn_table()).get_mut(proc_slot - SPAWN_SLOT_BASE) {
            s.exit = Some(status);
            // A `fork` child that shares its parent's stdio must not close the
            // parent's stdout — the parent (and every later command it runs)
            // still writes there.
            if !s.borrowed_io {
                pipe::close_write(s.stdout_pipe);
            }
        }
    }
}

/// The stdin/stdout pipe a task running `PROCS` slot `proc_slot` reads/writes as
/// fd 0 / fd 1. `None` for a task that is not a spawned child, or a `vfork`
/// child of a console shell — in both cases fd 0/1/2 are the console.
fn spawn_stdio(proc_slot: usize) -> Option<(PipeId, PipeId)> {
    if proc_slot < SPAWN_SLOT_BASE {
        return None;
    }
    // SAFETY: raw-pointer read; single core.
    unsafe {
        let s = (*spawn_table())
            .get(proc_slot - SPAWN_SLOT_BASE)?
            .as_ref()?;
        if s.console_io {
            return None;
        }
        Some((s.stdin_pipe, s.stdout_pipe))
    }
}

/// fd 0 for the current task: its stdin pipe, if it is a spawned child.
pub fn current_stdin_pipe() -> Option<PipeId> {
    spawn_stdio(current_proc_slot()).map(|(stdin, _)| stdin)
}

/// fd 1/2 for the current task: its stdout pipe, if it is a spawned child.
pub fn current_stdout_pipe() -> Option<PipeId> {
    spawn_stdio(current_proc_slot()).map(|(_, stdout)| stdout)
}

fn current_proc_slot() -> usize {
    // SAFETY: single core; `CURRENT_UCTX` points at the running task's slot.
    unsafe {
        let cur = &raw const CURRENT_UCTX;
        let uctx = *cur;
        if uctx.is_null() {
            usize::MAX
        } else {
            (*uctx).proc_slot
        }
    }
}

/// The stdin pipe write end for pid `pid`, for `fd::sys_openat`'s
/// `/proc/<pid>/fd/0` handling.
pub fn stdin_pipe_for_pid(pid: u32) -> Option<PipeId> {
    // SAFETY: raw-pointer read; single core.
    unsafe {
        (*spawn_table())
            .iter()
            .flatten()
            .find(|s| s.pid == pid)
            .map(|s| s.stdin_pipe)
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
    let want = pid as u32;
    // `wait4(-1)` / `waitpid(0)` — any child. `-1` arrives as `u32::MAX`.
    let any = want == u32::MAX || want == 0;

    // SAFETY: raw-pointer read; single core.
    let table = unsafe { &*spawn_table() };

    // Does any matching child exist at all? (For the `-ESRCH` vs `0` decision.)
    let exists = table
        .iter()
        .any(|e| e.as_ref().is_some_and(|s| any || s.pid == want));
    if !exists {
        return errno::ESRCH;
    }

    // A matching child that has exited — reap the first one found.
    let exited = table.iter().position(|e| {
        e.as_ref()
            .is_some_and(|s| (any || s.pid == want) && s.exit.is_some())
    });
    let Some(slot_off) = exited else {
        return 0; // matching child(ren) exist, none has exited yet
    };

    // SAFETY: `slot_off` is in bounds and occupied with `exit == Some`.
    let (code, stdin_pipe, child_pid, borrowed_io) = unsafe {
        let s = (*spawn_table())[slot_off].as_ref().unwrap();
        (s.exit.unwrap(), s.stdin_pipe, s.pid, s.borrowed_io)
    };

    if status_ptr != 0 {
        let raw = ((u64::from((code as u32) & 0xff)) << 8) as i32;
        // SAFETY: a user pointer to an int, per the wait(2) ABI.
        unsafe { (status_ptr as *mut i32).write_volatile(raw) };
    }

    // A `vfork` child borrows the parent's stdio — leave those pipes alone.
    // Otherwise free the **stdin** pipe (nobody reads it now); the **stdout**
    // pipe outlives this call so `sshd`'s bridge can drain the last bytes.
    if !borrowed_io {
        pipe::free(stdin_pipe);
    }
    // SAFETY: raw-pointer access; single core. The child task is Finished — it
    // called `sched::finish()` in `run_process` after recording its exit. A
    // `vfork` child's `Process` is borrowed, so `free` is a no-op for it.
    unsafe {
        (*spawn_table())[slot_off] = None;
        let procs = &raw mut PROCS;
        if let Some(p) = (*procs)[SPAWN_SLOT_BASE + slot_off].take() {
            p.free();
        }
    }
    u64::from(child_pid)
}

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

    if crate::fs::read_file("/bin/busybox").is_none() {
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

/// Stage T: `execve` with no `fork`.
///
/// `busybox sh -c "uname"` is spawned; ash resolves `uname` on `PATH`,
/// `stat`s `/bin/uname` (the path-`stat` half of this stage) and `execve`s it
/// **in place** — no fork. The spawned task keeps its slot, pid and stdout
/// pipe, so `uname`'s output ("Akuma") comes back the same way the shell's would
/// and `waitpid` reaps one child, not two.
pub fn execve_test(t: &mut Suite) {
    const ERRNO_FLOOR: u64 = 0xFFFF_FFFF_FFFF_F000;

    if crate::fs::read_file("/bin/busybox").is_none() || crate::fs::read_file("/bin/sh").is_none() {
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

/// Stage T: `fork` (with `vfork` semantics) + `execve` + `wait4`.
///
/// `busybox sh -c "uname; echo DONE"` — the `;` makes ash a command list, and
/// it **forks** to run `uname` (an external command, not the last in the list)
/// before running the `echo` builtin. So this exercises the whole path: the
/// shell forks, the child `execve`s `/bin/uname` in the shared address space,
/// the parent blocks in `wait4` until the child is done, then finishes the
/// list. Output must carry both `Akuma` (from the forked `uname`) and `DONE`
/// (from the parent shell), and nothing may leak.
pub fn fork_test(t: &mut Suite) {
    const ERRNO_FLOOR: u64 = 0xFFFF_FFFF_FFFF_F000;

    if crate::fs::read_file("/bin/busybox").is_none() || crate::fs::read_file("/bin/sh").is_none() {
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

/// Run two isolated processes concurrently and prove they interleave.
pub fn smoke_test(t: &mut Suite) {
    const ROUNDS: u32 = 3;
    const MSG_A: &[u8] = b"    [ring3 A] round\n";
    const MSG_B: &[u8] = b"    [ring3 B] round\n";

    let free_before = akuma_pmm::free_count();

    let (Some(a), Some(b)) = (
        Process::new(MSG_A, ROUNDS, 0, 0x0A),
        Process::new(MSG_B, ROUNDS, 0, 0x0B),
    ) else {
        t.check("ring3: processes built", false);
        return;
    };

    // The isolation property, checked before either runs: the same virtual
    // address resolves to different frames, and to nothing in the kernel's space.
    let pa_a = a.space.translate(USER_CODE_VA);
    let pa_b = b.space.translate(USER_CODE_VA);
    let pa_k = paging::translate(USER_CODE_VA);
    let (root_a, root_b) = (a.space.root(), b.space.root());

    // SAFETY: single core; written before the tasks that read them exist.
    unsafe {
        let procs = &raw mut PROCS;
        (*procs)[0] = Some(a);
        (*procs)[1] = Some(b);
    }

    let spawned = crate::sched::spawn_in_space(proc0_entry, root_a).is_some()
        && crate::sched::spawn_in_space(proc1_entry, root_b).is_some();
    if !t.check("ring3: processes spawned", spawned) {
        return;
    }

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

    // SAFETY: both tasks have finished; nothing else touches these slots.
    unsafe {
        let procs = &raw mut PROCS;
        for slot in 0..2 {
            if let Some(p) = (*procs)[slot].take() {
                p.free();
            }
        }
    }
    t.check_eq(
        "ring3: address-space teardown leaks nothing",
        akuma_pmm::free_count() as u64,
        free_before as u64,
    );
}

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
        Process::new(b"    [ring3 C] spinning, never yields\n", ROUNDS, DELAY, 0x0C),
        Process::new(b"    [ring3 D] spinning, never yields\n", ROUNDS, DELAY, 0x0D),
    ) else {
        t.check("preempt: processes built", false);
        return;
    };
    let (root_c, root_d) = (c.space.root(), d.space.root());

    // SAFETY: single core; written before the tasks that read them exist.
    unsafe {
        let procs = &raw mut PROCS;
        (*procs)[2] = Some(c);
        (*procs)[3] = Some(d);
    }

    let spawned = crate::sched::spawn_in_space(proc2_entry, root_c).is_some()
        && crate::sched::spawn_in_space(proc3_entry, root_d).is_some();
    if !t.check("preempt: processes spawned", spawned) {
        return;
    }

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

    // SAFETY: both tasks have finished; nothing else touches these slots.
    unsafe {
        let procs = &raw mut PROCS;
        for slot in 2..4 {
            if let Some(p) = (*procs)[slot].take() {
                p.free();
            }
        }
    }
    t.check_eq(
        "preempt: teardown leaks nothing",
        akuma_pmm::free_count() as u64,
        free_before as u64,
    );
}

/// Count `PT_LOAD` program headers in an ELF64 image.
///
/// Reads the four fields it needs at their architectural offsets rather than
/// going through the `elf` crate, which is the point: the loader's segment
/// count is checked against a number derived independently of the code that
/// produced it. A parser bug that dropped a segment would otherwise agree with
/// itself.
fn count_pt_load(image: &[u8]) -> u64 {
    const PT_LOAD: u32 = 1;
    let u16_at = |off: usize| u16::from_le_bytes([image[off], image[off + 1]]) as usize;
    let phoff = u64::from_le_bytes([
        image[32], image[33], image[34], image[35],
        image[36], image[37], image[38], image[39],
    ]) as usize;
    let phentsize = u16_at(54);
    let phnum = u16_at(56);

    (0..phnum)
        .filter(|i| {
            let at = phoff + i * phentsize;
            image
                .get(at..at + 4)
                .is_some_and(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) == PT_LOAD)
        })
        .count() as u64
}

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
    let image: &[u8] = if let Some(bytes) = from_disk.as_deref() {
        {
            // The embedded copy and the on-disk copy come from two different
            // build steps (`build.rs` into OUT_DIR, `mkdisk.sh` into the image).
            // They are supposed to be the same file; asserting it is what turns
            // that into a checked fact rather than a convention, and a mismatch
            // would mean the image is stale — which would otherwise show up as
            // the previous run's program silently running again.
            t.check_eq("elf: on-disk and embedded images agree in size",
                       bytes.len() as u64, HELLO_ELF.len() as u64);
            t.check("elf: on-disk and embedded images are identical", bytes == HELLO_ELF);
            serial::puts("  elf:  loading /bin/hello from ext2\n");
            bytes
        }
    } else {
        serial::puts("  elf:  no filesystem; loading the embedded image\n");
        HELLO_ELF
    };
    t.check("elf: image came from the filesystem", from_disk.is_some());

    let (proc, img) = match Process::from_elf(image) {
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

    // Every PT_LOAD in the file was placed. Counted out of the image rather
    // than written as a literal: how many segments lld emits is its decision,
    // not ours — `user.ld` names three output sections and the current link
    // produces four LOADs, because `-z relro` splits .data from .bss. A literal
    // here would turn any future linker flag into a test failure, while this
    // catches the thing that matters: a loader that skipped one would produce a
    // program that runs right up until it touches the segment that is missing.
    t.check_eq(
        "elf: every PT_LOAD was placed",
        img.segments as u64,
        count_pt_load(image),
    );
    t.check(
        "elf: image ends above its entry point",
        img.end_va > img.entry && img.end_va % 4096 == 0,
    );
    // .bss is 32 KiB, so the writable segment alone is 9 pages; with .text,
    // .rodata and two stack pages the total cannot be a single-page accident.
    t.check(
        "elf: frames owned covers image and stack",
        proc.frames.len() >= 12 && proc.frames.len() <= loader::MAX_PROC_FRAMES,
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
    let entry_prot = proc.space.prot(proc.entry as usize & !0xfff);
    t.check(
        "elf: entry page is user-executable and not writable",
        entry_prot == Some(Prot::USER_RX),
    );
    let stack_prot = proc.space.prot((proc.stack as usize) & !0xfff);
    t.check(
        "elf: stack page is user-writable and not executable",
        stack_prot == Some(Prot::USER_RW),
    );

    // The stack is a separate mapping from the image, not an extension of it.
    t.check(
        "elf: stack is above the image and mapped",
        proc.stack < ELF_STACK_TOP && proc.stack >= ELF_STACK_TOP - (ELF_STACK_PAGES as u64 * 4096),
    );

    let root = proc.space.root();
    // SAFETY: single core; the slot is written before the task that reads it
    // exists.
    unsafe {
        let procs = &raw mut PROCS;
        (*procs)[4] = Some(proc);
    }

    if !t.check(
        "elf: process spawned",
        crate::sched::spawn_in_space(proc4_entry, root).is_some(),
    ) {
        return;
    }

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

    // SAFETY: the task has finished; nothing else touches this slot.
    unsafe {
        let procs = &raw mut PROCS;
        if let Some(p) = (*procs)[4].take() {
            p.free();
        }
    }
    t.check_eq(
        "elf: teardown leaks nothing",
        akuma_pmm::free_count() as u64,
        free_before as u64,
    );
}

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
        // e_type at offset 16: ET_DYN (3). A PIE needs relocation this kernel
        // cannot do, and loading one places it at its link-time zero.
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
        let Some(space) = paging::AddressSpace::new() else {
            t.check(name, false);
            continue;
        };
        let mut frames = FrameSet::new();
        let refused = loader::load(&img, &space, &mut frames).is_err();
        frames.free_all();
        space.free();
        t.check(name, refused);
    }

    // A truncated image: the header claims segments the file does not contain.
    t.check("elf: rejects a truncated image", {
        let Some(space) = paging::AddressSpace::new() else {
            return;
        };
        let mut frames = FrameSet::new();
        let refused = loader::load(&HELLO_ELF[..48], &space, &mut frames).is_err();
        frames.free_all();
        space.free();
        refused
    });

    t.check_eq(
        "elf: rejected loads leak nothing",
        akuma_pmm::free_count() as u64,
        free_before as u64,
    );
}

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

    let Some(image) = crate::fs::read_file("/bin/fdprobe") else {
        t.note("fdprobe: not on the disk; skipped", 0);
        return;
    };

    let free_before = akuma_pmm::free_count();
    let (proc, _img) = match Process::from_elf(&image) {
        Ok(p) => p,
        Err(e) => {
            t.check("fdprobe: image loaded", false);
            serial::puts("  fdprobe: load failed: ");
            serial::puts(e);
            serial::puts("\n");
            return;
        }
    };
    let root = proc.space.root();
    // SAFETY: single core; the slot is written before the task that reads it
    // exists.
    unsafe {
        let procs = &raw mut PROCS;
        (*procs)[5] = Some(proc);
    }
    if !t.check(
        "fdprobe: spawned",
        crate::sched::spawn_in_space(proc5_entry, root).is_some(),
    ) {
        return;
    }

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

    // SAFETY: the task has finished; nothing else touches this slot.
    unsafe {
        let procs = &raw mut PROCS;
        if let Some(p) = (*procs)[5].take() {
            p.free();
        }
    }
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
    let Some(image) = crate::fs::read_file(path) else {
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
    let (proc, _img) = match Process::from_elf_argv(&image, &argv_owned) {
        Ok(p) => p,
        Err(e) => {
            serial::puts("  [init] failed to load: ");
            serial::puts(e);
            serial::puts("\n");
            return false;
        }
    };
    let root = proc.space.root();
    // SAFETY: single core; the slot is written before the task that reads it
    // exists.
    unsafe {
        let procs = &raw mut PROCS;
        (*procs)[6] = Some(proc);
    }
    if crate::sched::spawn_in_space(proc6_entry, root).is_none() {
        serial::puts("  [init] no task slot\n");
        return false;
    }
    serial::puts("\n-- running ");
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
